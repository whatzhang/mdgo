//! 审批通道的具体实现(依赖倒置的「具体侧」)。
//!
//! 生产通道走 **Tauri IPC**(与 `chat-index-error` 等事件同模式),不再使用 WebSocket 桥:
//! 1. `app.emit("approval:request", payload)` → 前端 `listen("approval:request")` 弹确认框
//! 2. 前端点击后 `invoke("approval_respond", {requestId, approved, reason})` 回传
//! 3. 本通道通过共享挂起表(oneshot)等待回传,超时/通道异常默认拒绝(fail-closed)
//!
//! 前端未监听时 IPC 事件会静默丢失,由超时兜底(归为 [`DenialCategory::Timeout`]);
//! 事件发送失败或响应通道异常归为 [`DenialCategory::ChannelUnavailable`]。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tauri::Emitter;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::{
    ApprovalDenial, ApprovalOutcome, ApprovalRequest, ApprovalTransport, DenialCategory,
};

/// 审批挂起表:request_id → 等待中的结果通道。
///
/// 由组装点(lib.rs)创建并共享给本通道与 `approval_respond` command,
/// 单一数据源,避免全局静态状态。
pub type PendingApprovals = Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalOutcome>>>>;

/// Tauri IPC 通道:emit 事件请前端确认,经 command 回传结果。
pub struct IpcApprovalTransport {
    app: tauri::AppHandle,
    pending: PendingApprovals,
}

impl IpcApprovalTransport {
    pub fn new(app: tauri::AppHandle, pending: PendingApprovals) -> Self {
        Self { app, pending }
    }

    fn remove(&self, request_id: &str) {
        if let Ok(mut map) = self.pending.lock() {
            map.remove(request_id);
        }
    }
}

/// RAII 清理：`request_approval` 的 future 若被父链取消而 drop
/// （审批弹窗显示期间用户点"停止"），自动移除挂起条目，避免 `approval_pending`
/// 残留（正常路径已手动 remove，此处幂等无害）。
struct RemovePendingOnDrop {
    pending: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, oneshot::Sender<ApprovalOutcome>>>>,
    id: String,
}

impl Drop for RemovePendingOnDrop {
    fn drop(&mut self) {
        if let Ok(mut map) = self.pending.lock() {
            map.remove(&self.id);
        }
    }
}

#[async_trait]
impl ApprovalTransport for IpcApprovalTransport {
    async fn request_approval(
        &self,
        req: &ApprovalRequest,
        timeout: Duration,
    ) -> ApprovalOutcome {
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<ApprovalOutcome>();
        {
            let mut map = match self.pending.lock() {
                Ok(m) => m,
                Err(e) => {
                    return ApprovalOutcome::Denied(ApprovalDenial {
                        category: DenialCategory::ChannelUnavailable,
                        reason: format!("审批挂起表锁异常: {}", e),
                    });
                }
            };
            map.insert(request_id.clone(), tx);
        }
        let _guard = RemovePendingOnDrop {
            pending: self.pending.clone(),
            id: request_id.clone(),
        };

        // 通过 IPC 事件把审批请求推给前端
        let payload = serde_json::json!({
            "request_id": request_id,
            "tool": req.tool,
            "summary": req.summary,
            "detail": req.detail,
        });
        if let Err(e) = self.app.emit("approval:request", payload) {
            self.remove(&request_id);
            return ApprovalOutcome::Denied(ApprovalDenial {
                category: DenialCategory::ChannelUnavailable,
                reason: format!("前端事件发送失败: {}", e),
            });
        }

        // 等待前端 invoke 回传;超时视为拒绝
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => {
                self.remove(&request_id);
                ApprovalOutcome::Denied(ApprovalDenial {
                    category: DenialCategory::ChannelUnavailable,
                    reason: "前端响应通道异常,默认拒绝".to_string(),
                })
            }
            Err(_elapsed) => {
                self.remove(&request_id);
                ApprovalOutcome::Denied(ApprovalDenial {
                    category: DenialCategory::Timeout,
                    reason: "用户确认超时,已按拒绝处理".to_string(),
                })
            }
        }
    }
}
