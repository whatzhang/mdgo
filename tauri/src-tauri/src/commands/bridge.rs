//! 前端通信桥命令：暴露 WebSocket 服务端端口，供前端建立连接。
//!
//! 命令层为 [`crate::core::bridge`] 的薄适配层：仅做参数透传，
//! 全部协议逻辑（请求关联、超时、清理）在桥内部完成（单一职责）。
//!
//! WebSocket 模式下，前端连接后直接通过 WebSocket 收发消息，
//! 不再需要 Tauri 事件/命令作为中间层。

use crate::core::bridge;

/// 获取 WebSocket 服务端端口（前端据此建立连接）。
#[tauri::command]
pub fn get_bridge_port() -> Result<u16, String> {
    bridge::get_port().ok_or_else(|| "WebSocket 服务端尚未启动".to_string())
}