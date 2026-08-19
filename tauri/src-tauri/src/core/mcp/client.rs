//! MCP stdio 客户端（v3：并发分发 + 常驻读线程）。
//!
//! 实现 MCP 官方 stdio transport（JSON-RPC 2.0 over stdio，`Content-Length` 帧）：
//! - `initialize` 握手 → `notifications/initialized`
//! - `tools/list` 获取工具清单（支持 cursor 分页，由注册表侧驱动）
//! - `tools/call` 调用工具
//!
//! # v3 架构（对齐主流 Agent 客户端，修复 v2 的三处正确性缺陷）
//!
//! v2 的单 IO 线程「串行处理请求」存在三个问题：
//! 1. **请求串行**：一次只处理一个请求，慢工具调用阻塞后续所有请求
//!    → v3 拆分为 **写线程 + 读线程**，请求按 id 并发分发，响应乱序到达也按 id 回传；
//! 2. **空闲时收不到服务端通知**：v2 空闲时阻塞在通道接收，服务端推送的
//!    `tools/list_changed` / `notifications/message` / `ping` 全被管道缓冲住，
//!    长会话下服务端写阻塞可致死锁
//!    → v3 读线程 **常驻消费所有入站帧**：响应按 id 分发、通知走事件回调、
//!      服务端请求（`ping` / `roots/list`）即时应答；
//! 3. **进程退出无感知**：子进程空闲时退出，状态仍显示 connected
//!    → v3 增加 **子进程存活监控线程**（1s 轮询 try_wait），退出即上报 `Closed` 事件，
//!      由注册表标记 failed 并触发自动重连。
//!
//! 另外补齐：
//! - **stderr 捕获**：子进程 stderr 逐行转发为 `LogMessage` 事件（服务端报错可排查）；
//! - **roots 应答**：客户端携带工作区根目录，应答服务端发起的 `roots/list` 请求
//!   （对齐 Claude Code / Codex 暴露 workspace 的能力）；
//! - **Windows 进程树终止**：断开时 `taskkill /T /F`，避免 `cmd /C` 包装后
//!   孙进程（node 等）残留。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

/// 单次调用超时（秒）：tools/call 等请求的最长等待。
pub const MCP_CALL_TIMEOUT_SECS: u64 = 60;
/// 本客户端声明的最新 MCP 协议版本。
pub const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
/// 协商降级使用的旧协议版本（部分服务端只认 2024-11-05）。
pub const MCP_PROTOCOL_VERSION_FALLBACK: &str = "2024-11-05";
/// 请求通道容量（并发在途请求上限）。
const CTL_CHANNEL_CAPACITY: usize = 64;
/// 子进程存活监控轮询间隔。
const CHILD_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 服务端 → 客户端事件（注册表据此消费通知）。
#[derive(Debug, Clone)]
pub enum McpServerEvent {
    /// 服务端工具清单变化（notifications/tools/list_changed）：需重拉 tools/list。
    ToolsListChanged,
    /// 服务端日志消息（notifications/message 或子进程 stderr 行）。
    LogMessage { level: String, data: String },
    /// 传输中断（进程退出 / 流结束 / 读失败）：连接已不可用。
    Closed,
}

/// 事件回调签名（std 线程调用，异步工作由回调内部 spawn）。
pub type McpEventHandler = dyn Fn(McpServerEvent) + Send + Sync;

/// 工作区根（应答服务端 roots/list 请求）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootInfo {
    pub uri: String,
    pub name: String,
}

/// MCP 传输抽象（SOLID：stdio / streamable HTTP 各自实现，注册表与 Agent 只面向 trait）。
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// 发起 JSON-RPC 请求并等待响应（默认超时）。
    async fn call(&self, method: &str, params: Value) -> Result<Value, String>;
    /// 发起 JSON-RPC 请求并等待响应（自定义超时秒数；握手等慢操作可放宽）。
    async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        secs: u64,
    ) -> Result<Value, String> {
        let _ = secs;
        self.call(method, params).await
    }
    /// 发送 notification（不等待响应；失败仅记日志）。
    fn send_notification(&self, method: &str, params: Value);
    /// 断开连接（终止子进程 / 取消接收流）。
    fn disconnect(&self);
    /// 设置服务端 → 客户端事件回调（连接建立后、首轮请求前设置）。
    fn set_event_handler(&self, _handler: Arc<McpEventHandler>) {}
    /// 传输是否已中断（进程退出 / 流结束）。
    fn is_closed(&self) -> bool {
        false
    }
    /// 建立服务端 → 客户端的推送接收通道。
    ///
    /// stdio 在 connect 时已启动读线程（默认空实现）；streamable HTTP 需在
    /// initialize 握手完成后调用（此时已捕获 mcp-session-id）。
    async fn start_receiver(&self) {}
}

/// 工具定义（tools/list 结果项）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
}

/// MCP 服务器配置（stdio：command + args + env；url 为 HTTP）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// streamable HTTP transport（url 非空时优先于 command）
    #[serde(default)]
    pub url: Option<String>,
    /// HTTP 请求头（与 .mcp.json 标准字段往返一致）
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

impl McpServerConfig {
    pub fn is_stdio(&self) -> bool {
        self.url.as_ref().is_none_or(|u| u.trim().is_empty())
    }
}

/// 写线程指令。
enum Ctl {
    /// 写入一帧（请求 / 通知 / 对服务端请求的应答）
    Write { msg: Value },
    /// 终止写线程并关闭。
    Shutdown,
}

/// 启动 stdio 子进程。
///
/// Windows 兼容（关键）：`Command::new("npx")` 只解析 `npx.exe`，而 npm 安装的是
/// `npx.cmd`（CreateProcess 不解析 PATHEXT 的 `.cmd/.bat`），直接 spawn 必然
/// `program not found`。因此：Windows 下先按原样尝试（exe），失败则回退
/// `cmd /C` 包装整条命令（覆盖 npx.cmd / uvx / 各类脚本启动器）。
fn spawn_stdio_command(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<Child, String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：从 GUI 进程 spawn 控制台程序（npx/cmd 等）时不闪现黑色窗口
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // 1) 直接尝试（.exe 可执行文件）
        {
            let mut c = Command::new(command);
            c.creation_flags(CREATE_NO_WINDOW);
            c.args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for (k, v) in env {
                c.env(k, v);
            }
            if let Ok(child) = c.spawn() {
                return Ok(child);
            }
        }
        // 2) 回退 cmd /C：把命令与参数拼成单条命令行（含空格参数加引号）
        let quoted: Vec<String> = args
            .iter()
            .map(|a| {
                if a.contains(' ') && !a.starts_with('"') {
                    format!("\"{}\"", a)
                } else {
                    a.clone()
                }
            })
            .collect();
        let mut parts = vec![command.to_string()];
        parts.extend(quoted);
        let mut c = Command::new("cmd");
        c.creation_flags(CREATE_NO_WINDOW);
        c.arg("/C")
            .arg(parts.join(" "))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            c.env(k, v);
        }
        c.spawn().map_err(|e| format!("启动进程失败 ({}): {}", command, e))
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new(command);
        c.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            c.env(k, v);
        }
        c.spawn().map_err(|e| format!("启动进程失败 ({}): {}", command, e))
    }
}

/// 已建立的 stdio 客户端（Arc 共享，跨线程安全）。
pub struct StdioMcpClient {
    tx: SyncSender<Ctl>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
    handler: Arc<Mutex<Option<Arc<McpEventHandler>>>>,
    child: Arc<Mutex<Option<Child>>>,
}

impl StdioMcpClient {
    /// 拉起子进程并启动 写线程 + 读线程 + 存活监控线程。
    pub fn connect(cfg: &McpServerConfig, roots: Vec<RootInfo>) -> Result<Self, String> {
        if !cfg.is_stdio() {
            return Err("streamable HTTP transport 需通过 HttpStreamableClient 连接".into());
        }
        let command = cfg.command.clone().ok_or_else(|| "缺少 command".to_string())?;
        let mut child = spawn_stdio_command(&command, &cfg.args, &cfg.env)?;
        let stdin = child.stdin.take().ok_or_else(|| "无法获取子进程 stdin".to_string())?;
        let stdout = child.stdout.take().ok_or_else(|| "无法获取子进程 stdout".to_string())?;
        let stderr = child.stderr.take().ok_or_else(|| "无法获取子进程 stderr".to_string())?;

        let (tx, rx): (SyncSender<Ctl>, Receiver<Ctl>) = sync_channel(CTL_CHANNEL_CAPACITY);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let handler: Arc<Mutex<Option<Arc<McpEventHandler>>>> = Arc::new(Mutex::new(None));
        let child_arc = Arc::new(Mutex::new(Some(child)));

        // ── 写线程：消费指令，逐帧写入 stdin ──
        {
            let pending_w = pending.clone();
            let closed_w = closed.clone();
            let handler_w = handler.clone();
            let tx_w = tx.clone();
            std::thread::Builder::new()
                .name("mcp-stdio-writer".into())
                .spawn(move || {
                    let mut stdin = stdin;
                    for ctl in rx {
                        match ctl {
                            Ctl::Shutdown => break,
                            Ctl::Write { msg } => {
                                if let Err(e) = write_frame(&mut stdin, &msg) {
                                    log::warn!("[mcp] 写入帧失败，关闭连接: {}", e);
                                    mark_closed(&closed_w, &handler_w, &pending_w, Some(e));
                                    let _ = tx_w.try_send(Ctl::Shutdown);
                                    break;
                                }
                            }
                        }
                    }
                })
                .map_err(|e| format!("启动写线程失败: {}", e))?;
        }

        // ── 读线程：常驻消费所有入站帧（响应分发 / 通知回调 / 服务端请求应答）──
        {
            let pending_r = pending.clone();
            let closed_r = closed.clone();
            let handler_r = handler.clone();
            let tx_r = tx.clone();
            let roots_r = roots.clone();
            std::thread::Builder::new()
                .name("mcp-stdio-reader".into())
                .spawn(move || {
                    let mut reader = BufReader::new(stdout);
                    loop {
                        match read_frame(&mut reader) {
                            Ok(Some(frame)) => {
                                let has_id = frame.get("id").is_some();
                                let has_method = frame.get("method").is_some();
                                if has_id && !has_method {
                                    // 请求响应：按 id 分发
                                    if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                                        if let Some(tx) =
                                            pending_r.lock().ok().and_then(|mut p| p.remove(&id))
                                        {
                                            let _ = tx.send(resp_to_result(frame));
                                        }
                                    }
                                } else if has_id && has_method {
                                    // 服务端主动请求（ping / roots/list）：即时应答
                                    if let Some(resp) = server_request_response(&frame, &roots_r) {
                                        let _ = tx_r.try_send(Ctl::Write { msg: resp });
                                    }
                                } else if has_method {
                                    // 服务端通知
                                    handle_notification(&frame, &handler_r);
                                }
                            }
                            Ok(None) => {
                                // EOF：进程退出 / 管道关闭
                                mark_closed(
                                    &closed_r,
                                    &handler_r,
                                    &pending_r,
                                    Some("MCP 服务器连接已关闭（进程退出）".into()),
                                );
                                break;
                            }
                            Err(e) => {
                                mark_closed(
                                    &closed_r,
                                    &handler_r,
                                    &pending_r,
                                    Some(format!("读取 MCP 响应失败: {}", e)),
                                );
                                break;
                            }
                        }
                    }
                })
                .map_err(|e| format!("启动读线程失败: {}", e))?;
        }

        // ── stderr 捕获线程：逐行转发为 LogMessage 事件 ──
        {
            let handler_e = handler.clone();
            let closed_e = closed.clone();
            std::thread::Builder::new()
                .name("mcp-stdio-stderr".into())
                .spawn(move || {
                    let mut reader = BufReader::new(stderr);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let data = line.trim_end_matches(['\r', '\n']).to_string();
                                if !data.is_empty() && !closed_e.load(Ordering::Relaxed) {
                                    fire(
                                        &handler_e,
                                        McpServerEvent::LogMessage {
                                            level: "stderr".to_string(),
                                            data,
                                        },
                                    );
                                }
                            }
                        }
                    }
                })
                .map_err(|e| format!("启动 stderr 线程失败: {}", e))?;
        }

        // ── 存活监控线程：1s 轮询 try_wait，进程退出即上报 Closed ──
        {
            let child_m = child_arc.clone();
            let closed_m = closed.clone();
            let handler_m = handler.clone();
            let pending_m = pending.clone();
            std::thread::Builder::new()
                .name("mcp-stdio-monitor".into())
                .spawn(move || loop {
                    if closed_m.load(Ordering::Relaxed) {
                        break;
                    }
                    let exited = child_m
                        .lock()
                        .ok()
                        .and_then(|mut guard| {
                            guard.as_mut().and_then(|c| c.try_wait().ok().flatten())
                        })
                        .is_some();
                    if exited {
                        mark_closed(
                            &closed_m,
                            &handler_m,
                            &pending_m,
                            Some("MCP 服务器进程已退出".into()),
                        );
                        break;
                    }
                    std::thread::sleep(CHILD_POLL_INTERVAL);
                })
                .map_err(|e| format!("启动监控线程失败: {}", e))?;
        }

        Ok(Self {
            tx,
            pending,
            next_id: AtomicU64::new(1),
            closed,
            handler,
            child: child_arc,
        })
    }

    /// 发起 JSON-RPC 请求并等待响应（默认超时 MCP_CALL_TIMEOUT_SECS）。
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.call_with_timeout(method, params, MCP_CALL_TIMEOUT_SECS)
            .await
    }

    /// 发起 JSON-RPC 请求并等待响应（自定义超时）。
    ///
    /// 高可用语义：
    /// - 请求写入后由读线程按 id 回传，超时仅终止等待（pending 表移除该项，
    ///   迟到响应被读线程丢弃，不产生线程泄漏）；
    /// - 并发请求互不阻塞（v3 核心改进）。
    pub async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        secs: u64,
    ) -> Result<Value, String> {
        if self.closed.load(Ordering::Relaxed) {
            return Err("MCP 客户端已关闭（服务器连接中断）".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            p.insert(id, reply_tx);
        }
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(e) = self.tx.try_send(Ctl::Write { msg }) {
            self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            return Err(match e {
                TrySendError::Full(_) => "MCP 请求队列已满，请稍后重试".into(),
                TrySendError::Disconnected(_) => "MCP 客户端已关闭（写入通道断开）".into(),
            });
        }
        match tokio::time::timeout(Duration::from_secs(secs), reply_rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => Err("MCP 响应通道已关闭（服务器可能已退出）".into()),
            Err(_) => {
                self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                Err(format!(
                    "MCP 调用超时（{}s）: {}（服务器未响应；若为首次运行 npx 下载依赖，可等待后重试或检查网络）",
                    secs, method
                ))
            }
        }
    }

    /// 发送 JSON-RPC notification（fire-and-forget）。
    pub fn send_notification(&self, method: &str, params: Value) {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        if self.tx.try_send(Ctl::Write { msg }).is_err() {
            log::warn!("[mcp] notification 发送失败（队列已满或已关闭）: {}", method);
        }
    }

    /// 设置事件回调（读/stderr/监控线程读取）。
    pub fn set_event_handler(&self, handler: Arc<McpEventHandler>) {
        if let Ok(mut h) = self.handler.lock() {
            *h = Some(handler);
        }
    }

    /// 断开：终止写线程 → 终止进程树 → 取消全部挂起请求。
    pub fn disconnect(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = self.tx.try_send(Ctl::Shutdown);
        if let Ok(mut guard) = self.child.lock() {
            if let Some(mut child) = guard.take() {
                kill_process_tree(&mut child);
            }
        }
        // 唤醒所有等待中的调用（不再有响应）：drain 移出所有权，断开即清空挂起表
        let mut pend = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        for (_, tx) in pend.drain() {
            let _ = tx.send(Err("MCP 客户端已断开".to_string()));
        }
    }
}

impl Drop for StdioMcpClient {
    fn drop(&mut self) {
        self.disconnect();
    }
}

#[async_trait]
impl McpTransport for StdioMcpClient {
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        StdioMcpClient::call(self, method, params).await
    }
    async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        secs: u64,
    ) -> Result<Value, String> {
        StdioMcpClient::call_with_timeout(self, method, params, secs).await
    }
    fn send_notification(&self, method: &str, params: Value) {
        StdioMcpClient::send_notification(self, method, params);
    }
    fn disconnect(&self) {
        StdioMcpClient::disconnect(self);
    }
    fn set_event_handler(&self, handler: Arc<McpEventHandler>) {
        StdioMcpClient::set_event_handler(self, handler);
    }
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

/// 一次性标记 closed 并触发 Closed 事件 + 取消全部挂起请求。
/// 返回 `true` 表示本次调用真正完成了「首次关闭」动作（幂等防重入）。
fn mark_closed(
    closed: &Arc<AtomicBool>,
    handler: &Arc<Mutex<Option<Arc<McpEventHandler>>>>,
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    reason: Option<String>,
) -> bool {
    if closed.swap(true, Ordering::Relaxed) {
        return false; // 已被其它线程关闭
    }
    if let Some(reason) = &reason {
        log::warn!("[mcp] stdio 连接关闭: {}", reason);
    }
    let msg = reason.unwrap_or_else(|| "MCP 连接已关闭".to_string());
    let mut pend = pending.lock().unwrap_or_else(|e| e.into_inner());
    for (_, tx) in pend.drain() {
        let _ = tx.send(Err(msg.clone()));
    }
    drop(pend);
    fire(handler, McpServerEvent::Closed);
    true
}

/// 触发事件回调（回调缺失时静默）。
fn fire(handler: &Arc<Mutex<Option<Arc<McpEventHandler>>>>, event: McpServerEvent) {
    if let Ok(guard) = handler.lock() {
        if let Some(h) = guard.as_ref() {
            h(event);
        }
    }
}

/// 消费服务端通知（tools/list_changed / message / 其它忽略）。
///
/// 供 http.rs 的 SSE 接收流复用（stdio 读线程与 HTTP GET 流共用同一分发逻辑）。
pub(crate) fn handle_notification(frame: &Value, handler: &Arc<Mutex<Option<Arc<McpEventHandler>>>>) {
    let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "notifications/tools/list_changed" => fire(handler, McpServerEvent::ToolsListChanged),
        "notifications/message" => {
            let params = frame.get("params").unwrap_or(&Value::Null);
            let level = params
                .get("level")
                .and_then(Value::as_str)
                .unwrap_or("info")
                .to_string();
            let data = params
                .get("data")
                .map(|d| serde_json::to_string(d).unwrap_or_else(|_| d.to_string()))
                .unwrap_or_default();
            fire(handler, McpServerEvent::LogMessage { level, data });
        }
        _ => {} // progress 等其它通知暂不消费
    }
}

/// 应答服务端主动请求（ping / roots/list）。不支持的方法返回 MethodNotFound 错误。
///
/// 供 http.rs 的 SSE 接收流复用（服务端经 GET 流发起的请求同样需回传应答）。
pub(crate) fn server_request_response(frame: &Value, roots: &[RootInfo]) -> Option<Value> {
    let id = frame.get("id").and_then(Value::as_u64)?;
    let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
    let msg = match method {
        "ping" => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        "roots/list" => {
            let roots: Vec<Value> = roots
                .iter()
                .map(|r| serde_json::json!({ "uri": r.uri, "name": r.name }))
                .collect();
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "roots": roots } })
        }
        _ => serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": -32601, "message": format!("Method not found: {}", method) }
        }),
    };
    Some(msg)
}

/// 响应帧 → Result（error 帧转为 Err）。
fn resp_to_result(frame: Value) -> Result<Value, String> {
    if let Some(err) = frame.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("MCP 错误")
            .to_string();
        return Err(msg);
    }
    Ok(frame.get("result").cloned().unwrap_or(frame))
}

/// 从响应值提取结果（兼容旧接口：直接透传；`__mcp_error__` 标记兜底）。
pub fn extract_result(resp: Value) -> Result<Value, String> {
    if let Some(msg) = resp.get("__mcp_error__").and_then(Value::as_str) {
        return Err(msg.to_string());
    }
    Ok(resp)
}

/// Windows：终止进程树（cmd /C 包装的孙进程一并清理）。
fn kill_process_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let pid = child.id();
        let mut cmd = Command::new("taskkill");
        // CREATE_NO_WINDOW：避免 taskkill 弹出控制台窗口
        cmd.creation_flags(0x0800_0000);
        let _ = cmd
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .output();
        let _ = child.kill();
        let _ = child.wait();
    }
    #[cfg(not(windows))]
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// 写 JSON-RPC 帧（Content-Length 头 + \r\n + JSON）。
fn write_frame(stdin: &mut ChildStdin, msg: &Value) -> Result<(), String> {
    let body = serde_json::to_string(msg).map_err(|e| format!("序列化失败: {}", e))?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin
        .write_all(header.as_bytes())
        .and_then(|_| stdin.write_all(body.as_bytes()))
        .and_then(|_| stdin.flush())
        .map_err(|e| format!("写入 MCP 请求失败: {}", e))
}

/// 读一帧：解析 Content-Length 头，返回 JSON 体（EOF 返回 None）。
fn read_frame(reader: &mut BufReader<ChildStdout>) -> std::io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // 空行 = 头结束
        }
        if let Some(rest) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = rest.trim().parse::<usize>().ok();
        }
    }
    let len = content_length.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "缺少 Content-Length 头")
    })?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    let text = String::from_utf8_lossy(&body);
    serde_json::from_str(&text).map(Some).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("JSON 解析失败: {}", e))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resp_to_result_extracts_result_and_error() {
        let ok = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}});
        assert_eq!(resp_to_result(ok).unwrap(), serde_json::json!({"ok":true}));
        let err = serde_json::json!({"jsonrpc":"2.0","id":2,"error":{"message":"boom"}});
        assert_eq!(resp_to_result(err).unwrap_err(), "boom");
        // 无 result 的响应回退为整帧（宽松兼容）
        assert_eq!(
            resp_to_result(serde_json::json!({"jsonrpc":"2.0","id":3})).unwrap(),
            serde_json::json!({"jsonrpc":"2.0","id":3})
        );
    }

    #[test]
    fn extract_result_marks_errors() {
        assert_eq!(extract_result(serde_json::json!({"ok": 1})).unwrap(), serde_json::json!({"ok": 1}));
        assert!(extract_result(serde_json::json!({"__mcp_error__": "boom"})).is_err());
    }

    #[test]
    fn server_request_response_answers_ping_and_roots() {
        let roots = vec![
            RootInfo { uri: "file:///workspace".into(), name: "workspace".into() },
        ];
        let ping = serde_json::json!({"jsonrpc":"2.0","id":7,"method":"ping"});
        let resp = server_request_response(&ping, &roots).unwrap();
        assert_eq!(resp.get("id"), Some(&serde_json::json!(7)));
        assert!(resp.get("result").is_some());

        let list_roots = serde_json::json!({"jsonrpc":"2.0","id":8,"method":"roots/list"});
        let resp = server_request_response(&list_roots, &roots).unwrap();
        let r = resp.get("result").and_then(|v| v.get("roots")).and_then(Value::as_array).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["uri"], serde_json::json!("file:///workspace"));

        // 不支持的方法 → MethodNotFound
        let unknown = serde_json::json!({"jsonrpc":"2.0","id":9,"method":"sampling/createMessage"});
        let resp = server_request_response(&unknown, &roots).unwrap();
        assert_eq!(resp["error"]["code"], serde_json::json!(-32601));
    }

    #[test]
    fn notification_dispatch_hits_handler() {
        use std::sync::atomic::AtomicU32;
        let handler: Arc<Mutex<Option<Arc<McpEventHandler>>>> = Arc::new(Mutex::new(None));
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let fired = Arc::new(AtomicU32::new(0));
        {
            let events = events.clone();
            let fired = fired.clone();
            *handler.lock().unwrap() = Some(Arc::new(move |e: McpServerEvent| {
                fired.fetch_add(1, Ordering::Relaxed);
                if let McpServerEvent::LogMessage { level, data } = e {
                    events.lock().unwrap().push(format!("{level}:{data}"));
                }
            }));
        }
        let list_changed = serde_json::json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"});
        handle_notification(&list_changed, &handler);
        let msg = serde_json::json!({
            "jsonrpc":"2.0","method":"notifications/message",
            "params":{"level":"warning","data":{"text":"disk full"}}
        });
        handle_notification(&msg, &handler);
        assert_eq!(fired.load(Ordering::Relaxed), 2);
        assert_eq!(events.lock().unwrap()[0], "warning:{\"text\":\"disk full\"}");
    }
}
