//! MCP 服务器注册表（v2：最基础协议级接入，独立管理页数据源）。
//!
//! 职责：
//! - 配置读写：`{dir}/.mdgo/mcp.json` / `mcp.yaml` / `mcp.yml`（MCP 官方格式）
//! - 生命周期：连接（拉起 stdio 子进程 → initialize → initialized → tools/list）、断开、重启
//! - 工具注册：已连接服务器的工具清单暴露给 Agent（`mcp:<server>:<tool>`）
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
    extract_result, McpServerConfig, McpToolDef, McpTransport, StdioMcpClient,
};
pub use crate::core::mcp::http::HttpStreamableClient;

/// 服务器状态枚举（字符串，直接序列化给前端）。
pub const STATUS_STOPPED: &str = "stopped";
pub const STATUS_CONNECTING: &str = "connecting";
pub const STATUS_CONNECTED: &str = "connected";
pub const STATUS_FAILED: &str = "failed";

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

/// 前端详情。
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

    /// 服务器详情（含配置与工具清单）。
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

    /// 新增/更新配置（写盘 + 重连）。
    pub async fn upsert(&self, name: &str, config: McpServerConfig) -> Result<(), String> {
        if name.trim().is_empty() {
            return Err("服务器名称不能为空".into());
        }
        let dir = self.dir_path.lock().await.clone();
        let dir = dir.ok_or_else(|| "尚未设置根目录".to_string())?;
        let servers = self.servers.lock().await;
        let mut configs: HashMap<String, McpServerConfig> =
            servers.iter().map(|(k, v)| (k.clone(), v.try_lock().map(|g| g.config.clone()).unwrap_or_default())).collect();
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
        let configs: HashMap<String, McpServerConfig> =
            servers.iter().map(|(k, v)| (k.clone(), v.try_lock().map(|g| g.config.clone()).unwrap_or_default())).collect();
        drop(servers);
        Self::save_configs(&dir, &configs)
    }

    /// 连接服务器：拉起进程 → initialize → initialized → tools/list。
    pub async fn connect(&self, name: &str) -> Result<(), String> {
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
        let result = async {
            // 按传输类型创建客户端：stdio（command）或 streamable HTTP（url）
            let (client, protocol_version): (Arc<dyn McpTransport>, &str) = if cfg.is_stdio() {
                let cmd = cfg.command.clone().unwrap_or_default();
                log::info!("[mcp] {} 启动 stdio 进程: {} {:?}", name, cmd, cfg.args);
                (Arc::new(StdioMcpClient::connect(&cfg)?), "2024-11-05")
            } else {
                let url = cfg.url.clone().unwrap_or_default();
                log::info!("[mcp] {} 连接 streamable HTTP: {}", name, url);
                let c = HttpStreamableClient::new(&cfg);
                c.connect().await?;
                (Arc::new(c), "2025-03-26")
            };
            // initialize 握手（放宽至 180s：首次运行 npx 需下载依赖包，可能较慢）
            let init = client
                .call_with_timeout("initialize", serde_json::json!({
                    "protocolVersion": protocol_version,
                    "capabilities": {},
                    "clientInfo": { "name": "mdgo", "version": "1.0.0" },
                }), 180)
                .await?;
            let init = extract_result(init)?;
            log::info!("[mcp] {} initialize 成功: serverInfo={}", name, init.get("serverInfo").cloned().unwrap_or(serde_json::json!({})));
            // initialized 通知（无 id 的 JSON-RPC notification，不等待响应）
            client.send_notification("notifications/initialized", serde_json::json!({}));
            // 工具清单
            let tools = client
                .call("tools/list", serde_json::json!({}))
                .await
                .and_then(extract_result)?;
            let tool_list: Vec<McpToolDef> = tools
                .get("tools")
                .and_then(|t| serde_json::from_value(t.clone()).ok())
                .unwrap_or_default();
            Ok::<(Arc<dyn McpTransport>, Vec<McpToolDef>), String>((client, tool_list))
        }
        .await;

        match result {
            Ok((client, tools)) => {
                let mut g = state.lock().await;
                g.client = Some(client);
                g.tools = tools;
                g.status = STATUS_CONNECTED.to_string();
                g.error = None;
                g.updated_at = now_secs();
                log::info!("[mcp] {} 已连接，工具数={}", name, g.tools.len());
                Ok(())
            }
            Err(e) => {
                let mut g = state.lock().await;
                g.status = STATUS_FAILED.to_string();
                g.error = Some(e.clone());
                g.updated_at = now_secs();
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
        g.updated_at = now_secs();
        Ok(())
    }

    /// 重启服务器。
    pub async fn restart(&self, name: &str) -> Result<(), String> {
        self.disconnect(name).await?;
        self.connect(name).await
    }

    /// 测试连接（不落盘、不影响现有状态）：返回工具数。
    pub async fn test(&self, config: McpServerConfig) -> Result<usize, String> {
        let (client, protocol_version): (Arc<dyn McpTransport>, &str) = if config.is_stdio() {
            (Arc::new(StdioMcpClient::connect(&config)?), "2024-11-05")
        } else {
            let c = HttpStreamableClient::new(&config);
            c.connect().await?;
            (Arc::new(c), "2025-03-26")
        };
        let init = client
            .call_with_timeout("initialize", serde_json::json!({
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": { "name": "mdgo", "version": "1.0.0" },
            }), 180)
            .await
            .and_then(extract_result)?;
        let _ = init;
        let tools = client
            .call("tools/list", serde_json::json!({}))
            .await
            .and_then(extract_result)?;
        let count = tools.get("tools").and_then(|t| t.as_array()).map(|a| a.len()).unwrap_or(0);
        client.disconnect();
        Ok(count)
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
        let result = client
            .call("tools/call", serde_json::json!({ "name": tool, "arguments": args }))
            .await
            .and_then(extract_result);
        let resp = match result {
            Ok(r) => r,
            Err(e) => {
                // 传输层故障（超时 / 通道关闭 / 队列满 / 读取失败）：
                // 标记 failed 并释放客户端（终止进程），用户可一键重启恢复
                if is_transport_error(&e) {
                    log::warn!("[mcp] {} 调用 {} 传输故障，标记 failed: {}", server, tool, e);
                    let mut g = state.lock().await;
                    if let Some(c) = g.client.take() {
                        c.disconnect();
                    }
                    g.status = STATUS_FAILED.to_string();
                    g.error = Some(format!("调用失败: {}", e));
                }
                return Err(e);
            }
        };
        // MCP 结果：content 数组（text 块拼接）
        let content = resp.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
        let text = content
            .iter()
            .filter_map(|c| {
                if c.get("type").and_then(Value::as_str) == Some("text") {
                    c.get("text").and_then(Value::as_str).map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if resp.get("isError").and_then(Value::as_bool).unwrap_or(false) {
            let msg = if text.is_empty() { "MCP 工具执行失败".to_string() } else { text };
            return Err(msg);
        }
        Ok(text)
    }
}

/// 判断错误是否属于「传输层故障」（应标记 failed 并释放客户端）。
fn is_transport_error(e: &str) -> bool {
    e.contains("超时")
        || e.contains("通道已关闭")
        || e.contains("队列已满")
        || e.contains("连接已关闭")
        || e.contains("读取 MCP 响应失败")
        || e.contains("读取失败")
        || e.contains("SSE 流结束")
        || e.contains("HTTP 请求失败")
}

/// 构建 MCP 工具的 rig DynamicTool（供 Agent 使用）。
///
/// 注册名规范化为 `mcp_<server>_<tool>`（下划线）：冒号不符合 OpenAI function name
/// 约束（`^[a-zA-Z0-9_-]{1,64}$`），部分严格服务端会拒绝含冒号的工具定义。
/// 闭包内仍用原始 server/tool 名调用 MCP（registry.call_tool 按原始名路由）。
pub fn build_mcp_tool(
    server: String,
    def: McpToolDef,
    registry: Arc<McpRegistry>,
) -> rig_agent::tool::DynamicTool {
    use rig_agent::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
    let normalized = format!("mcp_{}_{}", server.replace([' ', ':'], "_"), def.name.replace([' ', ':'], "_"));
    let full_name = normalized;
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
    DynamicTool::new(
        full_name.clone(),
        description,
        schema,
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let registry = registry.clone();
            let server = server.clone();
            let tool = def.name.clone();
            Box::pin(async move {
                match registry.call_tool(&server, &tool, args).await {
                    Ok(text) => Ok(ToolOutput::text(text)),
                    Err(e) => Err(ToolExecutionError::other(e)),
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
}
