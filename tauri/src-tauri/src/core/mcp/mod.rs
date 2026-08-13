//! MCP 服务器注册表（v3：对齐主流 Agent 客户端能力）。
//!
//! 职责：
//! - 配置读写：`{dir}/.mdgo/mcp.json` / `mcp.yaml` / `mcp.yml`（MCP 官方格式）
//! - 生命周期：连接（拉起 stdio 子进程 / streamable HTTP → initialize 协议协商 →
//!   initialized → tools/list 分页）、断开、重启
//! - 事件处理：`tools/list_changed` 重拉工具、`notifications/message` 与 stderr
//!   记录日志、`Closed` 标记 failed + 指数退避自动重连（对齐 Claude Code 连接状态机）
//! - 工具注册：已连接服务器的工具清单暴露给 Agent（`mcp:<server>:<tool>`）
//! - roots 能力：携带工作区根目录，应答服务端发起的 `roots/list` 请求
//!
//! 设计（SOLID）：
//! - `McpRegistry` 为单一入口（聚合配置 + 状态 + 客户端），命令层与 Agent 工具闭包共用；
//! - 每服务器状态独立加锁（`Arc<Mutex<McpServerState>>`），互不阻塞；
//! - 连接失败不阻断其它服务器与应用启动（状态标记 failed + 错误信息）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::sync::Mutex;

pub mod client;
pub mod http;
pub use crate::core::mcp::client::{
    extract_result, McpServerConfig, McpServerEvent, McpToolDef, McpTransport, RootInfo,
    StdioMcpClient, MCP_PROTOCOL_VERSION, MCP_PROTOCOL_VERSION_FALLBACK,
};
pub use crate::core::mcp::http::HttpStreamableClient;
use crate::core::agent::KbSearchConfig;
use crate::core::agent::limits::MCP_MAX_OUTPUT_CHARS;

/// 服务器状态枚举（字符串，直接序列化给前端）。
pub const STATUS_STOPPED: &str = "stopped";
pub const STATUS_CONNECTING: &str = "connecting";
pub const STATUS_CONNECTED: &str = "connected";
pub const STATUS_FAILED: &str = "failed";

/// 单条运行期日志（服务端 message 通知 / stderr / 连接事件）。
#[derive(Clone, serde::Serialize)]
pub struct McpLogEntry {
    pub level: String,
    pub message: String,
    pub ts: u64,
}

/// 单个服务器的运行时状态。
pub struct McpServerState {
    pub name: String,
    pub config: McpServerConfig,
    pub status: String,
    pub tools: Vec<McpToolDef>,
    pub error: Option<String>,
    pub updated_at: u64,
    /// 传输客户端（stdio / streamable HTTP，面向 McpTransport trait）
    pub client: Option<Arc<dyn McpTransport>>,
    /// 运行期日志（环形有界，最新在前展示由前端负责排序）
    pub logs: Vec<McpLogEntry>,
    /// 自动重连是否已耗尽（3 次重试全失败后置 true）：此后 Closed 事件不再自动
    /// 重连，保持 failed，直到用户手动「连接」/「断开」重置（避免无限自动重试）。
    pub auto_reconnect_exhausted: bool,
}

impl McpServerState {
    fn new(name: String, config: McpServerConfig) -> Self {
        Self {
            name,
            config,
            status: STATUS_STOPPED.to_string(),
            tools: Vec::new(),
            error: None,
            updated_at: now_secs(),
            client: None,
            logs: Vec::new(),
            auto_reconnect_exhausted: false,
        }
    }

    /// 追加一条日志（环形有界：超上限丢弃最旧）。
    ///
    /// 统一出口脱敏：stderr 行 / 服务端 message / 连接事件可能内嵌 URL userinfo，
    /// 全部经 redact_all_urls 剥离凭据后再进运行日志（SOLID：脱敏收敛于唯一写入点）。
    fn push_log(&mut self, level: &str, message: String) {
        const MAX_LOGS: usize = 100;
        self.logs.push(McpLogEntry {
            level: level.to_string(),
            message: redact_all_urls(&message),
            ts: now_secs(),
        });
        if self.logs.len() > MAX_LOGS {
            let excess = self.logs.len() - MAX_LOGS;
            self.logs.drain(..excess);
        }
    }
}

/// 前端列表项。
#[derive(Clone, serde::Serialize)]
pub struct McpServerInfo {
    pub name: String,
    pub status: String,
    pub tool_count: usize,
    pub error: Option<String>,
    pub enabled: bool,
}

/// 前端详情（不含运行日志：日志体积大且变化频繁，改为按需经 mcp_logs 单独拉取）。
#[derive(Clone, serde::Serialize)]
pub struct McpServerDetail {
    pub name: String,
    pub config: McpServerConfig,
    pub status: String,
    pub tools: Vec<McpToolDef>,
    pub error: Option<String>,
}

/// MCP 注册表（挂入 AppState，进程内单例）。
pub struct McpRegistry {
    dir_path: Mutex<Option<PathBuf>>,
    servers: Mutex<HashMap<String, Arc<Mutex<McpServerState>>>>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl McpRegistry {
    pub fn new() -> Self {
        Self {
            dir_path: Mutex::new(None),
            servers: Mutex::new(HashMap::new()),
        }
    }

    fn config_paths(dir: &Path) -> Vec<(PathBuf, bool)> {
        let base = dir.join(".mdgo");
        vec![
            (base.join("mcp.json"), true),
            (base.join("mcp.yaml"), false),
            (base.join("mcp.yml"), false),
        ]
    }

    /// 读取配置（json 优先；yaml/yml 兜底；均不存在返回空表）。
    fn load_configs(dir: &Path) -> HashMap<String, McpServerConfig> {
        for (path, is_json) in Self::config_paths(dir) {
            if !path.exists() {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(raw) => {
                    let parsed: Result<serde_json::Value, String> = if is_json {
                        serde_json::from_str(&raw).map_err(|e| e.to_string())
                    } else {
                        serde_yaml::from_str(&raw).map_err(|e| e.to_string())
                    };
                    match parsed {
                        Ok(v) => {
                            let servers = v.get("mcpServers").cloned().unwrap_or(serde_json::json!({}));
                            if let Ok(map) = serde_json::from_value::<HashMap<String, McpServerConfig>>(servers) {
                                log::info!("[mcp] 已加载配置: {}（{} 个服务器）", path.display(), map.len());
                                return map;
                            }
                            log::warn!("[mcp] 配置格式不合法（缺少 mcpServers）: {}", path.display());
                        }
                        Err(e) => log::warn!("[mcp] 配置解析失败: {} ({})", path.display(), e),
                    }
                }
                Err(e) => log::warn!("[mcp] 配置读取失败: {} ({})", path.display(), e),
            }
        }
        HashMap::new()
    }

    /// 持久化配置（写入 json；servers 空时删除文件避免残留空配置）。
    fn save_configs(dir: &Path, servers: &HashMap<String, McpServerConfig>) -> Result<(), String> {
        let json_path = dir.join(".mdgo").join("mcp.json");
        if servers.is_empty() {
            let _ = std::fs::remove_file(&json_path);
            return Ok(());
        }
        if let Some(parent) = json_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {}", e))?;
        }
        let payload = serde_json::json!({ "mcpServers": servers });
        let raw = serde_json::to_string_pretty(&payload)
            .map_err(|e| format!("序列化配置失败: {}", e))?;
        std::fs::write(&json_path, raw).map_err(|e| format!("写入配置失败: {}", e))
    }

    /// 工作区根（应答服务端 roots/list 请求，对齐 Claude Code / Codex 暴露 workspace）。
    fn roots_for_dir(dir: &Path) -> Vec<RootInfo> {
        let p = dir.to_string_lossy().replace('\\', "/");
        let uri = if p.starts_with('/') {
            format!("file://{}", p)
        } else {
            format!("file:///{}", p)
        };
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "workspace".to_string());
        vec![RootInfo { uri, name }]
    }

    /// initialize 参数（声明 roots 能力；协议版本由调用方指定，供降级协商）。
    fn init_params(version: &str) -> Value {
        serde_json::json!({
            "protocolVersion": version,
            "capabilities": { "roots": { "listChanged": true } },
            "clientInfo": { "name": "mdgo", "version": "1.0.0" },
        })
    }

    /// tools/list 全量拉取（cursor 分页循环，防御异常服务端无限翻页）。
    async fn list_all_tools(client: &Arc<dyn McpTransport>) -> Result<Vec<McpToolDef>, String> {
        let mut all: Vec<McpToolDef> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0u32;
        const MAX_PAGES: u32 = 50;
        loop {
            let params = match &cursor {
                Some(c) => serde_json::json!({ "cursor": c }),
                None => serde_json::json!({}),
            };
            let resp = client.call("tools/list", params).await.and_then(extract_result)?;
            if let Some(arr) = resp.get("tools").and_then(Value::as_array) {
                for item in arr {
                    if let Ok(def) = serde_json::from_value::<McpToolDef>(item.clone()) {
                        all.push(def);
                    }
                }
            }
            cursor = resp
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            pages += 1;
            if cursor.is_none() || pages >= MAX_PAGES {
                break;
            }
        }
        Ok(all)
    }

    /// 设置当前根目录：加载配置（同目录幂等）。
    ///
    /// 安全：**仅加载不自动连接**——`.mdgo/mcp.json` 中的 command 可执行任意本地命令，
    /// 打开外部仓库时自动拉起子进程属于无确认执行（注入面）。连接需用户在 MCP
    /// 管理页显式点击「连接」（upsert 为用户主动配置，保留其自动连接行为）。
    pub async fn set_root(&self, dir_path: &str) {
        {
            let mut cur = self.dir_path.lock().await;
            if cur.as_deref().is_some_and(|p| p.to_string_lossy() == dir_path) {
                return; // 相同目录不重复加载
            }
            *cur = Some(PathBuf::from(dir_path));
        }
        let dir = PathBuf::from(dir_path);
        let configs = Self::load_configs(&dir);
        let mut servers = self.servers.lock().await;
        servers.clear();
        for (name, cfg) in configs {
            let state = Arc::new(Mutex::new(McpServerState::new(name.clone(), cfg.clone())));
            servers.insert(name.clone(), state);
        }
    }

    /// 服务器列表（前端列表视图）。
    pub async fn list(&self) -> Vec<McpServerInfo> {
        let servers = self.servers.lock().await;
        let mut out: Vec<McpServerInfo> = Vec::new();
        for (name, state) in servers.iter() {
            let guard = state.lock().await;
            out.push(McpServerInfo {
                name: name.clone(),
                status: guard.status.clone(),
                tool_count: guard.tools.len(),
                error: guard.error.clone(),
                enabled: guard.config.enabled,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// 服务器详情（含配置、工具清单与运行期日志）。
    pub async fn get(&self, name: &str) -> Option<McpServerDetail> {
        let servers = self.servers.lock().await;
        let state = servers.get(name)?;
        let guard = state.lock().await;
        Some(McpServerDetail {
            name: guard.name.clone(),
            config: guard.config.clone(),
            status: guard.status.clone(),
            tools: guard.tools.clone(),
            error: guard.error.clone(),
        })
    }

    /// 运行日志（按需拉取；前端点击「加载运行日志」后调用）。
    pub async fn logs(&self, name: &str) -> Option<Vec<McpLogEntry>> {
        let servers = self.servers.lock().await;
        let state = servers.get(name)?;
        let guard = state.lock().await;
        Some(guard.logs.clone())
    }

    /// 新增/更新配置（写盘 + 重连）。
    pub async fn upsert(self: &Arc<Self>, name: &str, config: McpServerConfig) -> Result<(), String> {
        if name.trim().is_empty() {
            return Err("服务器名称不能为空".into());
        }
        let dir = self.dir_path.lock().await.clone();
        let dir = dir.ok_or_else(|| "尚未设置根目录".to_string())?;
        let servers = self.servers.lock().await;
        let mut configs: HashMap<String, McpServerConfig> = HashMap::new();
        for (k, v) in servers.iter() {
            match v.try_lock() {
                Ok(guard) => {
                    configs.insert(k.clone(), guard.config.clone());
                }
                Err(_) => {
                    // try_lock 失败（锁被占用）：记录警告，跳过该服务器
                    // 不使用 default() 避免配置丢失，仅记录日志
                    log::warn!("[mcp] 服务器 {} 配置锁定失败（并发冲突），跳过配置保存", k);
                }
            }
        }
        let previous = configs.get(name).cloned();
        configs.insert(name.to_string(), config.clone());
        drop(servers);
        Self::save_configs(&dir, &configs)?;

        let state = {
            let mut servers = self.servers.lock().await;
            match servers.get(name) {
                Some(s) => {
                    let mut g = s.lock().await;
                    g.config = config.clone();
                    g.status = STATUS_STOPPED.to_string();
                    g.error = None;
                    s.clone()
                }
                None => {
                    let s = Arc::new(Mutex::new(McpServerState::new(name.to_string(), config.clone())));
                    servers.insert(name.to_string(), s.clone());
                    s
                }
            }
        };
        // 配置变化时若旧连接存在则断开
        let changed = previous.as_ref().map(|p| p.command != config.command || p.args != config.args || p.env != config.env).unwrap_or(true);
        if changed {
            let mut g = state.lock().await;
            if let Some(client) = g.client.take() {
                client.disconnect();
            }
        }
        if config.enabled {
            drop(state);
            let _ = self.connect(name).await;
        }
        Ok(())
    }

    /// 删除服务器（断开 + 移除配置）。
    pub async fn delete(&self, name: &str) -> Result<(), String> {
        let dir = self.dir_path.lock().await.clone();
        let dir = dir.ok_or_else(|| "尚未设置根目录".to_string())?;
        let removed = self.servers.lock().await.remove(name);
        if let Some(state) = removed {
            let mut g = state.lock().await;
            if let Some(client) = g.client.take() {
                client.disconnect();
            }
        }
        let servers = self.servers.lock().await;
        let mut configs: HashMap<String, McpServerConfig> = HashMap::new();
        for (k, v) in servers.iter() {
            match v.try_lock() {
                Ok(guard) => {
                    configs.insert(k.clone(), guard.config.clone());
                }
                Err(_) => {
                    // try_lock 失败（锁被占用）：记录警告，跳过该服务器
                    // 不使用 default() 避免配置丢失，仅记录日志
                    log::warn!("[mcp] 服务器 {} 配置锁定失败（并发冲突），跳过配置保存", k);
                }
            }
        }
        drop(servers);
        Self::save_configs(&dir, &configs)
    }

    /// 连接服务器：拉起进程 → initialize（协议协商）→ initialized → tools/list（分页）。
    ///
    /// `self: &Arc<Self>`：事件回调需要持有注册表句柄以触发自动重连。
    pub async fn connect(self: &Arc<Self>, name: &str) -> Result<(), String> {
        let state = {
            let servers = self.servers.lock().await;
            servers.get(name).cloned().ok_or_else(|| format!("服务器不存在: {}", name))?
        };
        // 防重入：连接中直接拒绝，避免并发连接互相覆盖（后完成者杀先建 client）
        {
            let g = state.lock().await;
            if g.status == STATUS_CONNECTING {
                return Err(format!("服务器正在连接中: {}", name));
            }
        }
        {
            let mut g = state.lock().await;
            g.status = STATUS_CONNECTING.to_string();
            g.error = None;
            // 用户手动发起连接：重置自动重连耗尽标记（重新获得自动重连机会）
            g.auto_reconnect_exhausted = false;
        }
        let (cfg, old_client) = {
            let mut g = state.lock().await;
            (g.config.clone(), g.client.take())
        };
        if let Some(c) = old_client {
            c.disconnect();
        }
        if !cfg.enabled {
            let mut g = state.lock().await;
            g.status = STATUS_STOPPED.to_string();
            return Ok(());
        }
        // 工作区根（应答服务端 roots/list 请求）
        let roots = {
            let dir = self.dir_path.lock().await;
            Self::roots_for_dir(dir.as_deref().unwrap_or(Path::new("")))
        };
        // 事件回调：工具清单变更 → 重拉；日志 → 记录；连接中断 → 标记 failed + 自动重连。
        // 回调运行于 std 线程（读线程/stderr/监控），异步工作经 Tauri 运行时派发。
        let handler: Arc<crate::core::mcp::client::McpEventHandler> = {
            let reg = self.clone();
            let name = name.to_string();
            Arc::new(move |event: McpServerEvent| {
                let reg = reg.clone();
                let name = name.clone();
                tauri::async_runtime::spawn(async move {
                    reg.on_server_event(&name, event).await;
                });
            })
        };
        let result = async {
            // 按传输类型创建客户端：stdio（command）或 streamable HTTP（url）
            let client: Arc<dyn McpTransport> = if cfg.is_stdio() {
                let cmd = cfg.command.clone().unwrap_or_default();
                log::info!(
                    "[mcp] {} 启动 stdio 进程: {} {:?}",
                    name, cmd, redact_args(&cfg.args)
                );
                Arc::new(StdioMcpClient::connect(&cfg, roots)?)
            } else {
                let url = cfg.url.clone().unwrap_or_default();
                log::info!("[mcp] {} 连接 streamable HTTP: {}", name, redact_url(&url));
                let c = HttpStreamableClient::new(&cfg, roots);
                c.connect().await?;
                Arc::new(c)
            };
            client.set_event_handler(handler.clone());
            // initialize 握手（协议版本协商）：声明最新 2025-03-26，失败降级 2024-11-05。
            // 放宽至 180s：首次运行 npx 需下载依赖包，可能较慢。
            let init = match client
                .call_with_timeout("initialize", Self::init_params(MCP_PROTOCOL_VERSION), 180)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    log::info!(
                        "[mcp] {} 最新协议版本握手失败（{}），降级 {} 重试",
                        name, redact_all_urls(&e), MCP_PROTOCOL_VERSION_FALLBACK
                    );
                    client
                        .call_with_timeout(
                            "initialize",
                            Self::init_params(MCP_PROTOCOL_VERSION_FALLBACK),
                            180,
                        )
                        .await?
                }
            };
            let init = extract_result(init)?;
            let negotiated = init
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(MCP_PROTOCOL_VERSION);
            log::info!(
                "[mcp] {} initialize 成功: protocolVersion={} serverInfo={}",
                name,
                negotiated,
                init.get("serverInfo").cloned().unwrap_or(serde_json::json!({}))
            );
            // initialized 通知（无 id 的 JSON-RPC notification，不等待响应）
            client.send_notification("notifications/initialized", serde_json::json!({}));
            // HTTP：握手完成后建立 SSE 接收流（携带会话 ID，保证通知推送到同一会话）
            client.start_receiver().await;
            // 工具清单（cursor 分页）
            let tool_list = Self::list_all_tools(&client).await?;
            Ok::<(Arc<dyn McpTransport>, Vec<McpToolDef>, String), String>(
                (client, tool_list, negotiated.to_string()),
            )
        }
        .await;

        match result {
            Ok((client, tools, negotiated)) => {
                let mut g = state.lock().await;
                g.client = Some(client);
                g.tools = tools;
                g.status = STATUS_CONNECTED.to_string();
                g.error = None;
                g.updated_at = now_secs();
                let count = g.tools.len();
                g.push_log("info", format!("已连接（协议 {}，{} 个工具）", negotiated, count));
                log::info!("[mcp] {} 已连接，工具数={}（协议 {}）", name, count, negotiated);
                Ok(())
            }
            Err(e) => {
                // 兜底脱敏：传输层错误（reqwest 等）可能内嵌完整 URL（含 userinfo）
                let e = redact_all_urls(&e);
                let mut g = state.lock().await;
                g.status = STATUS_FAILED.to_string();
                g.error = Some(e.clone());
                g.updated_at = now_secs();
                g.push_log("error", format!("连接失败: {}", e));
                log::warn!("[mcp] {} 连接失败: {}", name, e);
                Err(e)
            }
        }
    }

    /// 断开服务器。
    pub async fn disconnect(&self, name: &str) -> Result<(), String> {
        let servers = self.servers.lock().await;
        let state = servers.get(name).cloned().ok_or_else(|| format!("服务器不存在: {}", name))?;
        drop(servers);
        let mut g = state.lock().await;
        if let Some(client) = g.client.take() {
            client.disconnect();
        }
        g.status = STATUS_STOPPED.to_string();
        g.tools.clear();
        g.error = None;
        // 用户手动断开：重置自动重连耗尽标记（断开后再手动连接可恢复自动重连）
        g.auto_reconnect_exhausted = false;
        g.updated_at = now_secs();
        g.push_log("info", "已断开连接".to_string());
        Ok(())
    }

    /// 重启服务器。
    pub async fn restart(self: &Arc<Self>, name: &str) -> Result<(), String> {
        self.disconnect(name).await?;
        self.connect(name).await
    }

    /// 测试连接（不落盘、不影响现有状态）：返回工具数。
    ///
    /// 错误按阶段包装上下文，便于用户在管理页定位问题（SOLID：错误上下文构造
    /// 内聚在本方法，传输/握手/列清单各阶段职责单一）：
    /// 1. 传输建立（stdio 启动进程 / HTTP URL 校验）；
    /// 2. initialize 握手（最新协议失败降级重试，两次错误一并展示）；
    /// 3. 工具清单拉取。
    pub async fn test(&self, config: McpServerConfig) -> Result<usize, String> {
        let roots = Vec::new();
        // 阶段 1：建立传输客户端
        let client: Arc<dyn McpTransport> = if config.is_stdio() {
            let cmd = config.command.clone().unwrap_or_default();
            // 错误信息展示用脱敏参数（防 --api-key=xxx 等敏感值进日志/前端）
            let args_redacted = redact_args(&config.args);
            let c = StdioMcpClient::connect(&config, roots).map_err(|e| {
                redact_all_urls(&format!(
                    "启动 MCP 服务器进程失败（command={} args={:?}）: {}",
                    cmd, args_redacted, e
                ))
            })?;
            Arc::new(c)
        } else {
            let url = config.url.clone().unwrap_or_default();
            let c = HttpStreamableClient::new(&config, roots);
            c.connect()
                .await
                .map_err(|e| {
                    redact_all_urls(&format!(
                        "HTTP 传输初始化失败（url={}）: {}",
                        redact_url(&url),
                        e
                    ))
                })?;
            Arc::new(c)
        };
        // 阶段 2：initialize 握手（协议版本协商，与 connect 同语义但不建立 SSE 流——纯探测）
        let init = match client
            .call_with_timeout("initialize", Self::init_params(MCP_PROTOCOL_VERSION), 180)
            .await
        {
            Ok(v) => v,
            Err(err_latest) => {
                match client
                    .call_with_timeout(
                        "initialize",
                        Self::init_params(MCP_PROTOCOL_VERSION_FALLBACK),
                        180,
                    )
                    .await
                {
                    Ok(v) => v,
                    Err(err_fallback) => {
                        return Err(redact_all_urls(&format!(
                            "MCP 初始化握手失败：最新协议（{}）错误「{}」；降级协议（{}）错误「{}」",
                            MCP_PROTOCOL_VERSION,
                            truncate_output(&err_latest, 500),
                            MCP_PROTOCOL_VERSION_FALLBACK,
                            truncate_output(&err_fallback, 500)
                        )))
                    }
                }
            }
        };
        let _ =
            extract_result(init).map_err(|e| format!("initialize 响应异常: {}", e))?;
        // 阶段 3：工具清单
        let tools = Self::list_all_tools(&client)
            .await
            .map_err(|e| {
                redact_all_urls(&format!(
                    "获取工具清单失败（tools/list）: {}",
                    truncate_output(&e, 500)
                ))
            })?;
        let count = tools.len();
        client.disconnect();
        Ok(count)
    }

    /// 服务端 → 客户端事件入口（由传输回调经 Tauri 运行时派发）。
    pub async fn on_server_event(self: &Arc<Self>, name: &str, event: McpServerEvent) {
        match event {
            McpServerEvent::ToolsListChanged => {
                log::info!("[mcp] {} 工具清单已变更，重拉 tools/list", name);
                let client = {
                    let servers = self.servers.lock().await;
                    let Some(state) = servers.get(name) else { return };
                    state.lock().await.client.clone()
                };
                let Some(client) = client else { return };
                match Self::list_all_tools(&client).await {
                    Ok(tools) => {
                        let servers = self.servers.lock().await;
                        let Some(state) = servers.get(name) else { return };
                        let mut g = state.lock().await;
                        g.tools = tools;
                        g.updated_at = now_secs();
                        let count = g.tools.len();
                        g.push_log("info", format!("工具清单已刷新（{} 个工具）", count));
                        log::info!("[mcp] {} 工具清单刷新完成: {} 个工具", name, count);
                        // 提示用户工具列表已更新，建议重新发起对话以使用新工具
                        log::warn!("[mcp] ⚠️ 工具列表已动态更新。当前 Agent 会话仍在使用旧工具列表。新工具将在下一次对话中生效。");
                    }
                    Err(e) => log::warn!("[mcp] {} 重拉工具清单失败: {}", name, e),
                }
            }
            McpServerEvent::LogMessage { level, data } => {
                let servers = self.servers.lock().await;
                let Some(state) = servers.get(name) else { return };
                let mut g = state.lock().await;
                g.push_log(&level, data);
            }
            McpServerEvent::Closed => {
                self.handle_closed(name).await;
            }
        }
    }

    /// 连接中断处理：标记 failed + 释放客户端 + 指数退避自动重连（3s/6s/12s）。
    ///
    /// 语义对齐主流 Agent：明确断开（status=stopped）不重连；重连期间被手动
    /// 重连/断开则放弃后续尝试；全部失败后保持 failed，由用户手动重连。
    ///
    /// 返回装箱 future（非 async fn）：切断「connect → 事件回调 → handle_closed →
    /// connect」的 opaque 类型递归，避免 E0391 循环依赖（`Pin<Box<dyn Future + Send>>`
    /// 的 Send 由 trait bound 保证，编译器无需再下钻内部具体类型）。
    fn handle_closed(
        self: &Arc<Self>,
        name: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
        let me = self.clone();
        let name = name.to_string();
        Box::pin(async move {
            let should_reconnect = {
                let servers = me.servers.lock().await;
                let Some(state) = servers.get(&name) else { return };
                let mut g = state.lock().await;
                // 明确断开（stopped）或自动重连已耗尽（3 次失败）时不再自动重连
                if g.status == STATUS_STOPPED || g.auto_reconnect_exhausted {
                    false
                } else {
                    if let Some(c) = g.client.take() {
                        c.disconnect();
                    }
                    g.status = STATUS_FAILED.to_string();
                    g.error = Some("连接中断（服务器进程退出或传输关闭），正在尝试自动重连…".to_string());
                    g.updated_at = now_secs();
                    g.push_log("error", "连接中断，开始自动重连（退避 3s/6s/12s，最多 3 次）".to_string());
                    true
                }
            };
            if !should_reconnect {
                return;
            }
            for (attempt, backoff) in [3u64, 6, 12].iter().enumerate() {
                tokio::time::sleep(std::time::Duration::from_secs(*backoff)).await;
                // 期间被手动断开（stopped）或已重连成功（connected）则放弃
                let current = {
                    let servers = me.servers.lock().await;
                    match servers.get(&name) {
                        Some(s) => Some(s.lock().await.status.clone()),
                        None => None,
                    }
                };
                if current.as_deref() != Some(STATUS_FAILED) {
                    return;
                }
                match me.connect(&name).await {
                    Ok(()) => {
                        log::info!("[mcp] {} 自动重连成功（第 {} 次）", name, attempt + 1);
                        return;
                    }
                    Err(e) => {
                        log::warn!("[mcp] {} 自动重连失败（第 {} 次）: {}", name, attempt + 1, e);
                    }
                }
            }
            // 3 次重试全部失败：标记自动重连已耗尽，此后 Closed 事件不再自动重试，
            // 保持 failed 直到用户手动「连接」/「断开」重置（重启需用户主动操作）。
            {
                let servers = me.servers.lock().await;
                if let Some(state) = servers.get(&name) {
                    let mut g = state.lock().await;
                    g.auto_reconnect_exhausted = true;
                    g.error = Some("自动重连 3 次均失败，已停止自动重连；请手动点击「连接」重试".to_string());
                    g.updated_at = now_secs();
                    g.push_log(
                        "error",
                        "自动重连 3 次均失败，已停止自动重连；请手动点击「连接」重试".to_string(),
                    );
                }
            }
            log::warn!("[mcp] {} 自动重连 3 次均失败，已停止自动重连（请手动连接）", name);
        })
    }

    /// Agent 调用 MCP 工具（`mcp:<server>:<tool>`）。
    pub async fn call_tool(&self, server: &str, tool: &str, args: Value) -> Result<String, String> {
        let servers = self.servers.lock().await;
        let state = servers.get(server).cloned().ok_or_else(|| format!("MCP 服务器不存在: {}", server))?;
        drop(servers);
        let client = {
            let g = state.lock().await;
            if g.status != STATUS_CONNECTED {
                return Err(format!("MCP 服务器未连接: {}", server));
            }
            g.client.clone().ok_or_else(|| "MCP 客户端不可用".to_string())?
        };
        // 传输层实时检查：进程退出/流结束可能先于状态更新（事件回调经异步派发有延迟），快速失败
        if client.is_closed() {
            log::warn!("[mcp] {} 调用 {} 时传输已关闭，标记 failed", server, tool);
            let mut g = state.lock().await;
            if let Some(c) = g.client.take() {
                c.disconnect();
            }
            g.status = STATUS_FAILED.to_string();
            g.error = Some("连接已中断（传输关闭）".to_string());
            g.push_log("error", "连接已中断（传输关闭），调用已取消".to_string());
            return Err(format!("MCP 服务器连接已中断: {}", server));
        }
        let result = client
            .call("tools/call", serde_json::json!({ "name": tool, "arguments": args }))
            .await
            .and_then(extract_result);
        let resp = match result {
            Ok(r) => r,
            Err(e) => {
                // 传输层故障（超时 / 通道关闭 / 队列满 / 读取失败 / HTTP 错误）：
                // 标记 failed 并释放客户端（终止进程），用户可一键重启恢复。
                // 错误可能内嵌完整 URL（含 userinfo），统一脱敏后再落库/返回（含 Agent 上下文）
                let e = redact_all_urls(&e);
                if is_transport_error(&e) {
                    log::warn!("[mcp] {} 调用 {} 传输故障，标记 failed: {}", server, tool, e);
                    let mut g = state.lock().await;
                    if let Some(c) = g.client.take() {
                        c.disconnect();
                    }
                    g.status = STATUS_FAILED.to_string();
                    g.error = Some(format!("调用失败: {}", e));
                    g.push_log("error", format!("调用 {} 传输故障: {}", tool, e));
                }
                return Err(e);
            }
        };
        // 结果格式化：content 数组（text / image / resource 块）+ structuredContent（JSON）
        let mut parts: Vec<String> = Vec::new();
        if let Some(content) = resp.get("content").and_then(Value::as_array) {
            for c in content {
                match c.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = c.get("text").and_then(Value::as_str) {
                            parts.push(t.to_string());
                        }
                    }
                    Some("image") => parts.push("[图片资源（image）]".to_string()),
                    Some("resource") => {
                        if let Some(t) = c
                            .get("resource")
                            .and_then(|r| r.get("text"))
                            .and_then(Value::as_str)
                        {
                            parts.push(t.to_string());
                        } else {
                            parts.push("[资源（resource）]".to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(sc) = resp.get("structuredContent") {
            if !sc.is_null() {
                parts.push(format!(
                    "（结构化数据）{}",
                    serde_json::to_string(sc).unwrap_or_default()
                ));
            }
        }
        let mut text = parts.join("\n");
        if text.trim().is_empty() {
            text = serde_json::to_string(&resp).unwrap_or_else(|_| "（空结果）".to_string());
        }
        // 输出上限（防撑爆模型上下文，对齐内置工具 MAX_* 护栏）
        text = truncate_output(&text, MCP_MAX_OUTPUT_CHARS);
        if resp.get("isError").and_then(Value::as_bool).unwrap_or(false) {
            let msg = if text.trim().is_empty() {
                "MCP 工具执行失败".to_string()
            } else {
                text
            };
            return Err(msg);
        }
        Ok(text)
    }
}

/// 判断错误是否属于「传输层故障」（应标记 failed 并释放客户端）。
fn is_transport_error(e: &str) -> bool {
    const KEYS: &[&str] = &[
        "超时",
        "已关闭",
        "已断开",
        "队列已满",
        "通道断开",
        "响应通道已关闭",
        "读取 MCP 响应失败",
        "读取失败",
        "SSE 流结束",
        "SSE 读取失败",
        "HTTP 请求失败",
        "HTTP 错误",
    ];
    KEYS.iter().any(|k| e.contains(k))
}

/// 按字符数截断输出（保留头部 + 截断提示）。
fn truncate_output(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{}…（输出已截断：{} 字符）", head, s.chars().count())
}

/// 命令行参数敏感值脱敏（错误信息/日志展示用）：
/// - `--key=value` / `-key=value` 形态：key 命中敏感词时值替换为 `***`；
/// - `--key value` 空格分隔形态：敏感参数名后的独立值参数替换为 `***`；
/// - 其它参数原样保留（保证错误信息可读性，同时避免密钥进入日志/前端）。
fn redact_args(args: &[String]) -> Vec<String> {
    const SENSITIVE: &[&str] = &[
        "key", "token", "secret", "password", "passwd", "pwd", "api_key", "apikey", "auth",
        "credential", "bearer", "authorization", "pass",
    ];
    let is_sensitive = |k: &str| {
        let kk = k.trim_start_matches('-').to_lowercase();
        SENSITIVE.iter().any(|s| kk.contains(s))
    };
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0usize;
    while i < args.len() {
        let a = &args[i];
        if let Some((k, _v)) = a.split_once('=') {
            if is_sensitive(k) {
                out.push(format!("{}=***", k));
            } else {
                out.push(a.clone());
            }
        } else if is_sensitive(a) {
            // `--api-key value`：参数名本身不含值，保留；紧随其后的独立值参数脱敏
            out.push(a.clone());
            if i + 1 < args.len() {
                out.push("***".to_string());
                i += 1;
            }
        } else {
            out.push(a.clone());
        }
        i += 1;
    }
    out
}

/// URL 展示脱敏：剥离 userinfo 段（`http://user:pass@host` → `http://***@host`），
/// 避免 URL 内嵌凭据进入错误信息/日志。
fn redact_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let after_scheme = &url[scheme_end + 3..];
        if let Some(at) = after_scheme.find('@') {
            return format!("{}***@{}", &url[..scheme_end + 3], &after_scheme[at + 1..]);
        }
    }
    url.to_string()
}

/// 兜底脱敏：扫描文本（如 reqwest 错误串 `... for url (http://user:pass@host/...)`）
/// 中所有 `scheme://userinfo@` 形态并剥离凭据；无匹配时原样返回。
fn redact_all_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(scheme_end) = rest.find("://") else {
            out.push_str(rest);
            return out;
        };
        let after = &rest[scheme_end + 3..];
        if let Some(at) = after.find('@') {
            let userinfo = &after[..at];
            // @ 前若含空白/引号/括号等分隔符，则不是 URL userinfo，跳过
            let is_userinfo = !userinfo.is_empty()
                && !userinfo
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '(' | ')' | '<' | '>' | '`'));
            if is_userinfo {
                out.push_str(&rest[..scheme_end + 3]);
                out.push_str("***@");
                rest = &after[at + 1..];
                continue;
            }
        }
        // 未找到有效 userinfo：越过本段继续向后扫描
        out.push_str(&rest[..scheme_end + 3]);
        rest = after;
    }
}

/// 轻量参数校验：检查 schema.required 声明的字段是否齐全（不校验类型，容错）。
fn validate_args(schema: &Value, args: &Value) -> Result<(), String> {
    let Some(required) = schema.get("required").and_then(Value::as_array) else {
        return Ok(());
    };
    let obj = match args {
        Value::Object(m) => m,
        _ => return Err("MCP 工具参数必须是 JSON 对象".to_string()),
    };
    for field in required {
        if let Some(name) = field.as_str() {
            if !obj.contains_key(name) {
                return Err(format!("缺少必需参数: {}", name));
            }
        }
    }
    Ok(())
}

/// 构建 MCP 工具的 rig DynamicTool（供 Agent 使用）。
///
/// 注册名规范化为 `mcp_<server>_<tool>`（下划线）：冒号不符合 OpenAI function name
/// 约束（`^[a-zA-Z0-9_-]{1,64}$`），部分严格服务端会拒绝含冒号的工具定义。
/// 闭包内仍用原始 server/tool 名调用 MCP（registry.call_tool 按原始名路由）。
///
/// 与内置工具对齐（P2-15）：
/// - 参数 schema 校验（required 缺失 → invalid_args，不进执行链路）；
/// - 工具调用轨迹（record_tool_call / record_tool_result，前端 agent:tool_call 事件）；
/// - 审批门由 rig `ApprovalGateHook` 统一拦截（策略层已支持 `mcp_*` 通配）。
pub fn build_mcp_tool(
    server: String,
    def: McpToolDef,
    registry: Arc<McpRegistry>,
    cfg: KbSearchConfig,
) -> rig_agent::tool::DynamicTool {
    use crate::core::agent::tools::{record_tool_call, record_tool_result};
    use rig_agent::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
    let normalized = format!(
        "mcp_{}_{}",
        server.replace([' ', ':'], "_"),
        def.name.replace([' ', ':'], "_")
    );
    let description = if def.description.trim().is_empty() {
        format!("MCP 工具（服务器 {}）", server)
    } else {
        format!("{}（MCP 服务器 {}）", def.description, server)
    };
    let schema = if def.input_schema.is_null() || def.input_schema.as_object().is_none() {
        serde_json::json!({ "type": "object", "properties": {} })
    } else {
        def.input_schema.clone()
    };
    let name_arg = normalized.clone();
    DynamicTool::new(
        name_arg,
        description,
        schema.clone(),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let registry = registry.clone();
            let server = server.clone();
            let tool = def.name.clone();
            let cfg = cfg.clone();
            let schema = schema.clone();
            let full_name = normalized.clone();
            Box::pin(async move {
                // 参数 schema 校验：required 字段缺失直接拒绝（无效调用不进执行链路）
                if let Err(e) = validate_args(&schema, &args) {
                    return Err(ToolExecutionError::invalid_args(e));
                }
                let preview = truncate_output(&serde_json::to_string(&args).unwrap_or_default(), 120);
                record_tool_call(&cfg, &full_name, &preview, Some(&args));
                match registry.call_tool(&server, &tool, args).await {
                    Ok(text) => {
                        record_tool_result(
                            &cfg,
                            &full_name,
                            true,
                            &truncate_output(&text, 200),
                            Some(&text),
                        );
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(
                            &cfg,
                            &full_name,
                            false,
                            &truncate_output(&e, 200),
                            Some(&e),
                        );
                        Err(ToolExecutionError::other(e))
                    }
                }
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_serde_roundtrip() {
        let cfg: McpServerConfig = serde_json::from_value(serde_json::json!({
            "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
            "env": { "A": "1" }, "enabled": true
        })).unwrap();
        assert_eq!(cfg.command.as_deref(), Some("npx"));
        assert_eq!(cfg.args.len(), 3);
        assert!(cfg.is_stdio());
        assert!(cfg.enabled);
    }

    #[test]
    fn config_yaml_parse() {
        let yaml = "mcpServers:\n  fs:\n    command: npx\n    args:\n      - -y\n      - '@modelcontextprotocol/server-filesystem'\n";
        let v: serde_json::Value = serde_yaml::from_str(yaml).unwrap();
        let servers = v.get("mcpServers").unwrap();
        let map: HashMap<String, McpServerConfig> = serde_json::from_value(servers.clone()).unwrap();
        assert!(map.contains_key("fs"));
        assert_eq!(map["fs"].args.len(), 2);
    }

    #[test]
    fn roots_for_dir_builds_file_uri() {
        let roots = McpRegistry::roots_for_dir(Path::new("G:\\gitProject\\mdgo"));
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].uri, "file:///G:/gitProject/mdgo");
        assert_eq!(roots[0].name, "mdgo");
        let roots2 = McpRegistry::roots_for_dir(Path::new("/home/user/proj"));
        assert_eq!(roots2[0].uri, "file:///home/user/proj");
    }

    #[test]
    fn validate_args_checks_required() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        });
        assert!(validate_args(&schema, &serde_json::json!({"path": "/a"})).is_ok());
        assert!(validate_args(&schema, &serde_json::json!({})).is_err());
        assert!(validate_args(&schema, &serde_json::json!("not-an-object")).is_err());
        // 无 required → 不校验
        assert!(validate_args(&serde_json::json!({"type": "object"}), &serde_json::json!({})).is_ok());
    }

    #[test]
    fn truncate_output_caps_chars() {
        let s = "abcdefghij";
        let t = truncate_output(s, 5);
        assert!(t.starts_with("abcde"));
        assert!(t.contains("截断"));
        assert_eq!(truncate_output(s, 100), s);
    }

    #[test]
    fn redact_args_masks_sensitive_values_only() {
        let args = vec![
            "-y".to_string(),
            "@modelcontextprotocol/server-everything".to_string(),
            "--api-key=abc123".to_string(),
            "--path=/tmp".to_string(),
            "-token=xyz".to_string(),
        ];
        let r = redact_args(&args);
        assert_eq!(r[0], "-y");
        assert_eq!(r[1], "@modelcontextprotocol/server-everything");
        assert_eq!(r[2], "--api-key=***");
        assert_eq!(r[3], "--path=/tmp");
        assert_eq!(r[4], "-token=***");
    }

    #[test]
    fn redact_args_masks_space_separated_values() {
        // `--api-key value` 空格分隔：参数名保留，独立值脱敏
        let args = vec![
            "--api-key".to_string(),
            "abc123".to_string(),
            "--auth".to_string(),
            "xyz".to_string(),
            "--verbose".to_string(),
        ];
        let r = redact_args(&args);
        assert_eq!(r, vec!["--api-key", "***", "--auth", "***", "--verbose"]);
        // 敏感参数名位于末尾且无值：仅保留参数名
        let r2 = redact_args(&["--token".to_string()].to_vec());
        assert_eq!(r2, vec!["--token"]);
    }

    #[test]
    fn redact_url_strips_userinfo() {
        assert_eq!(
            redact_url("http://user:pass@example.com/mcp"),
            "http://***@example.com/mcp"
        );
        assert_eq!(redact_url("https://example.com/mcp"), "https://example.com/mcp");
        assert_eq!(redact_url(""), "");
    }

    #[test]
    fn redact_all_urls_masks_userinfo_in_error_text() {
        // reqwest 风格错误串：内嵌完整 URL（含 userinfo）
        let s = "error sending request for url (http://user:pass@example.com/mcp)";
        assert_eq!(
            redact_all_urls(s),
            "error sending request for url (http://***@example.com/mcp)"
        );
        // 多个 URL 均脱敏
        let s2 = "a http://u1:p1@h1/x b https://u2:p2@h2/y c";
        assert_eq!(redact_all_urls(s2), "a http://***@h1/x b https://***@h2/y c");
        // 无 userinfo 的 URL 原样保留；普通文本不变
        assert_eq!(redact_all_urls("see https://example.com/mcp"), "see https://example.com/mcp");
        assert_eq!(redact_all_urls("无 URL 的普通文本"), "无 URL 的普通文本");
    }
}
