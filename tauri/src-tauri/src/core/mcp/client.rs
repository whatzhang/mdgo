//! MCP stdio 客户端（v2：最基础协议级接入）。
//!
//! 实现 MCP 官方 stdio transport（JSON-RPC 2.0 over stdio，`Content-Length` 帧）：
//! - `initialize` 握手 → `notifications/initialized`
//! - `tools/list` 获取工具清单
//! - `tools/call` 调用工具
//!
//! 设计（SOLID）：
//! - 专用 IO 线程负责「写请求帧 → 阻塞读响应帧 → 按 id 匹配回传」，调用侧用
//!   `tokio::time::timeout` 做超时（跨平台，不依赖子进程句柄超时能力）；
//! - 响应未就绪时调用侧超时返回错误，线程继续消费后续帧（不会死锁）；
//! - `disconnect` 终止子进程并停止 IO 线程。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 单次调用超时（秒）：tools/call 等请求的最长等待。
pub const MCP_CALL_TIMEOUT_SECS: u64 = 60;

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

/// MCP 服务器配置（stdio：command + args + env；url 预留 HTTP）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// 预留：streamable HTTP transport（本期不支持，声明时报错）
    #[serde(default)]
    pub url: Option<String>,
    /// HTTP 请求头（预留，与 .mcp.json 标准字段往返一致；本期 stdio 传输不使用）
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

/// IO 线程控制指令。
enum Ctl {
    Call {
        method: String,
        params: Value,
        /// 响应回传通道：调用侧超时后 drop receiver，此处 send 失败即忽略
        reply: tokio::sync::oneshot::Sender<Result<Value, String>>,
    },
    /// notification：只写帧、不读响应（服务器不回复，避免阻塞 IO 线程）
    Notification { params: Value },
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
        // 1) 直接尝试（.exe 可执行文件）
        {
            let mut c = Command::new(command);
            c.args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
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
        c.arg("/C")
            .arg(parts.join(" "))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
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
            .stderr(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        c.spawn().map_err(|e| format!("启动进程失败 ({}): {}", command, e))
    }
}

/// 已建立的 stdio 客户端（Arc 共享，跨线程安全）。
pub struct StdioMcpClient {
    ctl_tx: SyncSender<Ctl>,
    child: Arc<std::sync::Mutex<Option<Child>>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl StdioMcpClient {
    /// 拉起子进程并启动 IO 线程。
    pub fn connect(cfg: &McpServerConfig) -> Result<Self, String> {
        if !cfg.is_stdio() {
            return Err("streamable HTTP transport 尚未支持（本期仅 stdio）".into());
        }
        let command = cfg.command.clone().ok_or_else(|| "缺少 command".to_string())?;
        let child = spawn_stdio_command(&command, &cfg.args, &cfg.env)?;
        let mut child = child;
        let stdin = child.stdin.take().ok_or_else(|| "无法获取子进程 stdin".to_string())?;
        let stdout = child.stdout.take().ok_or_else(|| "无法获取子进程 stdout".to_string())?;

        let (ctl_tx, ctl_rx): (SyncSender<Ctl>, Receiver<Ctl>) = sync_channel(16);
        let child_arc = Arc::new(std::sync::Mutex::new(Some(child)));
        let child_io = child_arc.clone();

        std::thread::Builder::new()
            .name("mcp-io".into())
            .spawn(move || {
                io_loop(ctl_rx, stdin, stdout, child_io);
            })
            .map_err(|e| format!("启动 IO 线程失败: {}", e))?;

        Ok(Self {
            ctl_tx,
            child: child_arc,
            next_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// 发起 JSON-RPC 请求并等待响应（默认超时 MCP_CALL_TIMEOUT_SECS）。
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.call_with_timeout(method, params, MCP_CALL_TIMEOUT_SECS)
            .await
    }

    /// 发起 JSON-RPC 请求并等待响应（自定义超时）。
    ///
    /// 与旧实现对比（高可用修复）：
    /// - 用 `tokio::sync::oneshot` 等待响应，超时后 receiver 直接 drop，
    ///   **不产生阻塞线程泄漏**（IO 线程稍后 send 失败被忽略）；
    /// - 超时仅终止等待，不中断 IO 线程（服务器最终响应到达时按 id 丢弃）。
    pub async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        secs: u64,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.ctl_tx
            .try_send(Ctl::Call { method: method.to_string(), params: msg, reply: reply_tx })
            .map_err(|_| "MCP 客户端已关闭或请求队列已满，请稍后重试".to_string())?;
        match tokio::time::timeout(Duration::from_secs(secs), reply_rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => Err("MCP 响应通道已关闭（服务器可能已退出）".into()),
            Err(_) => Err(format!(
                "MCP 调用超时（{}s）: {}（服务器未响应；若为首次运行 npx 下载依赖，可等待后重试或检查网络）",
                secs, method
            )),
        }
    }

    /// 终止子进程（IO 线程随后退出）。
    pub fn disconnect(&self) {
        let _ = self.ctl_tx.try_send(Ctl::Shutdown);
        if let Ok(mut guard) = self.child.lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// 发送 JSON-RPC notification（无 id、不等待响应）。IO 线程只写帧不读响应，
    /// 不会阻塞请求-响应通道；发送失败仅记日志（notification 不要求确认）。
    pub fn send_notification(&self, method: &str, params: Value) {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let result = self
            .ctl_tx
            .try_send(Ctl::Notification { params: msg });
        if result.is_err() {
            log::warn!("[mcp] notification 发送失败（队列已满或已关闭）: {}", method);
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
    fn send_notification(&self, method: &str, params: Value) {
        StdioMcpClient::send_notification(self, method, params);
    }
    fn disconnect(&self) {
        StdioMcpClient::disconnect(self);
    }
}

/// IO 线程主循环：读控制指令 → 写帧 → 阻塞读帧直到 id 匹配 → 回传。
fn io_loop(
    ctl_rx: Receiver<Ctl>,
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    _child: Arc<std::sync::Mutex<Option<Child>>>,
) {
    let mut reader = BufReader::new(stdout);
    for ctl in ctl_rx {
        match ctl {
            Ctl::Shutdown => break,
            Ctl::Notification { params } => {
                if let Err(e) = write_frame(&mut stdin, &params) {
                    log::warn!("[mcp] notification 写入失败: {}", e);
                }
            }
            Ctl::Call { method, params, reply } => {
                let id = params.get("id").and_then(Value::as_u64).unwrap_or(0);
                if let Err(e) = write_frame(&mut stdin, &params) {
                    let _ = reply.send(Err(e));
                    continue;
                }
                // 阻塞读响应帧，直到 id 匹配（跳过 notifications 与其它请求响应）
                let mut found: Option<Result<Value, String>> = None;
                while found.is_none() {
                    match read_frame(&mut reader) {
                        Ok(Some(frame)) => {
                            if let Some(resp) = match_response(&frame, id) {
                                found = Some(Ok(resp));
                            }
                        }
                        Ok(None) => {
                            found = Some(Err(format!("MCP 服务器连接已关闭（{}）", method)));
                            break;
                        }
                        Err(e) => {
                            found = Some(Err(format!("读取 MCP 响应失败: {}", e)));
                            break;
                        }
                    }
                }
                let _ = reply.send(found.unwrap_or_else(|| Err("MCP 调用失败".into())));
            }
        }
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

/// 匹配响应帧：id 一致且是 response/error；非匹配帧（notification/其它请求）返回 None。
fn match_response(frame: &Value, id: u64) -> Option<Value> {
    let frame_id = frame.get("id").and_then(Value::as_u64)?;
    if frame_id != id {
        return None;
    }
    if let Some(err) = frame.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("MCP 错误")
            .to_string();
        return Some(serde_json::json!({ "__mcp_error__": msg }));
    }
    frame.get("result").cloned()
}

/// 从响应值提取结果（识别 __mcp_error__ 标记）。
pub fn extract_result(resp: Value) -> Result<Value, String> {
    if let Some(msg) = resp.get("__mcp_error__").and_then(Value::as_str) {
        return Err(msg.to_string());
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_response_filters_notifications() {
        let notification = serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert!(match_response(&notification, 1).is_none());
        let resp = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}});
        assert_eq!(match_response(&resp, 1), Some(serde_json::json!({"ok":true})));
        let wrong = serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"ok":true}});
        assert!(match_response(&wrong, 1).is_none());
    }

    #[test]
    fn extract_result_marks_errors() {
        assert_eq!(extract_result(serde_json::json!({"ok": 1})).unwrap(), serde_json::json!({"ok": 1}));
        assert!(extract_result(serde_json::json!({"__mcp_error__": "boom"})).is_err());
    }
}

