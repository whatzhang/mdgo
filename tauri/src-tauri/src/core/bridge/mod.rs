//! 前端通信桥（FrontendBridge）：Rust 工具闭包 ↔ 前端业务处理器的 WebSocket 通道。
//!
//! # 设计动机
//!
//! Rig Agent 的工具闭包在流式内部执行，无法直接调用前端 JS。本桥提供
//! WebSocket 双向通信通道，让任意 Rust 工具以一行代码调用前端已注册的业务处理器。
//!
//! # 协议时序
//!
//! ```text
//! 工具闭包 ── request(tool, action, args) ──▶ FrontendBridge
//!    │                                          │ WebSocket → 前端
//!    │◀── oneshot 应答 + 超时（5s）─────────────┤  {request_id, tool, action, args}
//!    │                                          ▼ 前端 handler
//!    │                              执行结果 ← WebSocket {type:"result", ...}
//!    │◀────────────────────── 结果回传 ──────────
//! ```
//!
//! # 高并发架构（DashMap 分片锁）
//!
//! - 挂起请求表使用 `DashMap<String, oneshot::Sender>` 替代 `Mutex<HashMap>`：
//!   分片锁技术让多个线程同时读写不同 request_id 的条目，锁竞争降到最低。
//! - 每个请求有独立的 `oneshot` 通道，互不干扰。
//! - 请求 ID 使用 UUID v4，全局唯一无需协调。
//!
//! # 高可用 / 不阻塞
//!
//! - 全程异步 await，不占用/阻塞 Rig 运行时线程
//! - 超时兜底：默认 5s，超时返回错误并清理挂起条目
//! - 不依赖就绪门控：`ready` 标记仅用于观测（日志/指标），
//!   请求决策唯一依赖超时——前端未就绪时自然超时，不会死锁
//! - 容量治理：挂起表超过上限自动清扫过期条目，防极端场景内存膨胀

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, OnceCell as TokioOnceCell};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

/// 默认请求超时：超过此时长前端未回传视为失败
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// 挂起表容量上限：超过后清扫过期条目，防止长会话内存膨胀
const MAX_PENDING: usize = 64;

// ── WebSocket 消息协议 ──

/// Rust → 前端：发起工具调用请求
#[derive(Serialize)]
struct WsRequest {
    #[serde(rename = "type")]
    msg_type: &'static str,
    request_id: String,
    tool: String,
    action: String,
    args: serde_json::Value,
}

/// 前端 → Rust：回传结果 / 就绪上报
#[derive(Deserialize)]
struct WsResponse {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    ready: Option<bool>,
}

// ── 内部结构 ──

struct BridgeReply {
    ok: bool,
    message: String,
}

struct Pending {
    tx: oneshot::Sender<BridgeReply>,
    created_at: Instant,
}

struct FrontendBridgeState {
    pending: DashMap<String, Pending>,
    ready: AtomicBool,
    /// WebSocket 发送通道（mpsc → ws_write 转发任务）
    ws_tx: tokio::sync::Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// 服务端端口（启动后写入一次）
    port: TokioOnceCell<u16>,
}

static BRIDGE: OnceLock<FrontendBridgeState> = OnceLock::new();

fn bridge() -> &'static FrontendBridgeState {
    BRIDGE.get_or_init(|| FrontendBridgeState {
        pending: DashMap::new(),
        ready: AtomicBool::new(false),
        ws_tx: tokio::sync::Mutex::new(None),
        port: TokioOnceCell::new(),
    })
}

// ── 公开 API ──

/// 获取 WebSocket 服务端端口（供前端连接）。
pub fn get_port() -> Option<u16> {
    bridge().port.get().copied()
}

/// 启动 WebSocket 服务端，返回监听端口。
///
/// 绑定 `127.0.0.1:0`（随机端口），仅接受本机连接。
/// 应在上层 Tauri `setup()` 中调用。
pub async fn start_server() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("WebSocket 服务端绑定失败: {}", e))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("获取端口失败: {}", e))?
        .port();

    bridge()
        .port
        .set(port)
        .map_err(|_| "端口已设置".to_string())?;

    log::info!("[bridge] WebSocket 服务端已启动: 127.0.0.1:{}", port);

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    log::info!("[bridge] WebSocket 客户端连接: {}", addr);
                    handle_connection(stream).await;
                }
                Err(e) => {
                    log::error!("[bridge] WebSocket 接受连接失败: {}", e);
                }
            }
        }
    });

    Ok(port)
}

/// 发起一次前端调用并等待结果。
///
/// - `_app`：保留参数，兼容旧调用签名（WebSocket 模式下无需 Tauri AppHandle）
/// - `tool`：业务标识（前端据此查找 handler）
/// - `action`：业务动作
/// - `args`：动作参数（JSON 对象）
///
/// 返回前端 handler 回传的 `message`（String）。
/// 失败情形：超时（默认 5s）、前端回传 ok=false、WebSocket 未连接、应答通道异常。
pub async fn request(
    _app: &tauri::AppHandle,
    tool: &str,
    action: &str,
    args: serde_json::Value,
) -> Result<String, String> {
    let request_id = Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel::<BridgeReply>();

    // 容量治理：超过上限先清扫过期条目
    if bridge().pending.len() >= MAX_PENDING {
        let cutoff = Instant::now() - REQUEST_TIMEOUT;
        bridge().pending.retain(|_, p| p.created_at >= cutoff);
    }

    bridge().pending.insert(
        request_id.clone(),
        Pending {
            tx,
            created_at: Instant::now(),
        },
    );

    // 通过 WebSocket 发送请求
    let payload = serde_json::to_string(&WsRequest {
        msg_type: "request",
        request_id: request_id.clone(),
        tool: tool.to_string(),
        action: action.to_string(),
        args,
    })
    .map_err(|e| format!("序列化请求失败: {}", e))?;

    {
        let guard = bridge().ws_tx.lock().await;
        match guard.as_ref() {
            Some(tx) => {
                tx.send(payload)
                    .map_err(|_| "WebSocket 发送通道已关闭".to_string())?;
            }
            None => {
                bridge().pending.remove(&request_id);
                return Err("WebSocket 未连接，请确认前端已加载".to_string());
            }
        }
    }

    // 等待前端回传或超时
    let reply = tokio::time::timeout(REQUEST_TIMEOUT, rx).await;
    let reply = match reply {
        Err(_elapsed) => {
            bridge().pending.remove(&request_id);
            return Err(format!(
                "等待前端响应超时（{}s）：{tool}/{action}",
                REQUEST_TIMEOUT.as_secs()
            ));
        }
        Ok(Err(_)) => {
            bridge().pending.remove(&request_id);
            return Err(format!("前端响应通道异常：{tool}/{action}"));
        }
        Ok(Ok(r)) => r,
    };
    bridge().pending.remove(&request_id);

    if reply.ok {
        Ok(reply.message)
    } else {
        Err(format!("{tool}/{action} 执行失败：{}", reply.message))
    }
}

/// 前端回传业务请求结果（通过 WebSocket 接收）。
pub fn submit_result(request_id: &str, ok: bool, message: &str) {
    if let Some((_, pending)) = bridge().pending.remove(request_id) {
        let _ = pending.tx.send(BridgeReply {
            ok,
            message: message.to_string(),
        });
    }
}

/// 更新前端就绪状态（通过 WebSocket 接收）。
pub fn set_ready(ready: bool) {
    let was = bridge().ready.swap(ready, Ordering::Relaxed);
    if was != ready {
        log::info!("[bridge] 前端就绪: {}", ready);
    }
}

// ── WebSocket 连接处理 ──

async fn handle_connection(stream: tokio::net::TcpStream) {
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            log::error!("[bridge] WebSocket 握手失败: {}", e);
            return;
        }
    };

    let (mut ws_write, mut ws_read) = ws_stream.split();

    // 创建 mpsc 通道：request() → tx → send_task → ws_write → 前端
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    {
        let mut guard = bridge().ws_tx.lock().await;
        *guard = Some(tx);
    }

    // 发送任务：从 mpsc 读取 → 通过 WebSocket 发送给前端
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = ws_write.send(Message::Text(msg.into())).await {
                log::error!("[bridge] WebSocket 发送失败: {}", e);
                break;
            }
        }
    });

    // 接收任务：从 WebSocket 读取 → 分发到 submit_result / set_ready
    let recv_task = tokio::spawn(async move {
        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    let text_str = text.to_string();
                    match serde_json::from_str::<WsResponse>(&text_str) {
                        Ok(resp) => match resp.msg_type.as_str() {
                            "result" => {
                                if let (Some(rid), Some(ok), Some(msg)) =
                                    (resp.request_id, resp.ok, resp.message)
                                {
                                    submit_result(&rid, ok, &msg);
                                }
                            }
                            "ready" => {
                                if let Some(ready) = resp.ready {
                                    set_ready(ready);
                                }
                            }
                            _ => {
                                log::warn!("[bridge] 未知 WebSocket 消息类型: {}", resp.msg_type);
                            }
                        },
                        Err(e) => {
                            log::warn!("[bridge] 无法解析 WebSocket 消息: {} — {}", e, text_str);
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    log::info!("[bridge] WebSocket 客户端主动关闭");
                    break;
                }
                Err(e) => {
                    log::error!("[bridge] WebSocket 读取错误: {}", e);
                    break;
                }
                _ => {}
            }
        }
    });

    // 等待任一任务结束（连接断开时清理）
    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    // 清理：断开连接时清除发送通道
    {
        let mut guard = bridge().ws_tx.lock().await;
        *guard = None;
    }
    log::info!("[bridge] WebSocket 客户端断开");
}