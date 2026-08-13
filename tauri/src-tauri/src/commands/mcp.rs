//! MCP 管理命令（v2：独立管理页的数据源，UI 参考 Skill 模块）。
//!
//! 命令面：
//! - `mcp_list`：服务器列表（名称/状态/工具数/错误）
//! - `mcp_get`：服务器详情（配置 + 工具清单）
//! - `mcp_upsert`：新增/更新配置（写盘 + 重连）
//! - `mcp_delete`：删除（断开 + 移除配置）
//! - `mcp_connect` / `mcp_disconnect` / `mcp_restart`：生命周期
//! - `mcp_test`：测试连接（不落盘，返回工具数）

use tauri::State;

use crate::AppState;
use crate::core::mcp::McpServerConfig;

/// 服务器列表（前端左侧列表）。
#[tauri::command]
pub async fn mcp_list(
    state: State<'_, AppState>,
    dir_path: String,
) -> Result<Vec<crate::core::mcp::McpServerInfo>, String> {
    state.mcp.set_root(&dir_path).await;
    Ok(state.mcp.list().await)
}

/// 服务器详情（含配置与工具清单）。
#[tauri::command]
pub async fn mcp_get(
    state: State<'_, AppState>,
    dir_path: String,
    name: String,
) -> Result<crate::core::mcp::McpServerDetail, String> {
    state.mcp.set_root(&dir_path).await;
    state
        .mcp
        .get(&name)
        .await
        .ok_or_else(|| format!("服务器不存在: {}", name))
}

/// 运行日志（按需加载：详情页不随 mcp_get 返回，用户点击后单独拉取）。
#[tauri::command]
pub async fn mcp_logs(
    state: State<'_, AppState>,
    dir_path: String,
    name: String,
) -> Result<Vec<crate::core::mcp::McpLogEntry>, String> {
    state.mcp.set_root(&dir_path).await;
    state
        .mcp
        .logs(&name)
        .await
        .ok_or_else(|| format!("服务器不存在: {}", name))
}

/// 新增/更新服务器（写 .mdgo/mcp.json + 重连）。
#[tauri::command]
pub async fn mcp_upsert(
    state: State<'_, AppState>,
    dir_path: String,
    name: String,
    config: McpServerConfig,
) -> Result<(), String> {
    state.mcp.set_root(&dir_path).await;
    state.mcp.upsert(&name, config).await
}

/// 删除服务器。
#[tauri::command]
pub async fn mcp_delete(
    state: State<'_, AppState>,
    dir_path: String,
    name: String,
) -> Result<(), String> {
    state.mcp.set_root(&dir_path).await;
    state.mcp.delete(&name).await
}

/// 连接服务器。
#[tauri::command]
pub async fn mcp_connect(
    state: State<'_, AppState>,
    dir_path: String,
    name: String,
) -> Result<(), String> {
    state.mcp.set_root(&dir_path).await;
    state.mcp.connect(&name).await
}

/// 断开服务器。
#[tauri::command]
pub async fn mcp_disconnect(
    state: State<'_, AppState>,
    dir_path: String,
    name: String,
) -> Result<(), String> {
    state.mcp.set_root(&dir_path).await;
    state.mcp.disconnect(&name).await
}

/// 重启服务器。
#[tauri::command]
pub async fn mcp_restart(
    state: State<'_, AppState>,
    dir_path: String,
    name: String,
) -> Result<(), String> {
    state.mcp.set_root(&dir_path).await;
    state.mcp.restart(&name).await
}

/// 测试连接（不落盘）：返回可用工具数。
#[tauri::command]
pub async fn mcp_test(
    state: State<'_, AppState>,
    config: McpServerConfig,
) -> Result<usize, String> {
    state.mcp.test(config).await
}
