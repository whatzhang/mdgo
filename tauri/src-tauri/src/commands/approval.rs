//! 审批结果回传命令:前端确认框点击后经 Tauri IPC 回传用户决定。
//!
//! 与 [`crate::core::approval::transport::IpcApprovalTransport`] 共享
//! AppState 中的挂起表(单一数据源):transport 侧注册并 emit `approval:request`
//! 事件,本命令消费前端的 `invoke("approval_respond", ...)` 回传并完成 oneshot。

use crate::core::approval::{ApprovalDenial, ApprovalOutcome, DenialCategory};
use crate::AppState;

/// 前端回传审批结果。
///
/// - `request_id`:transport 随 `approval:request` 事件下发的请求 ID
/// - `approved`:`true` 允许执行;`false` 拒绝(归为 [`DenialCategory::UserRejected`])
/// - `reason`:拒绝时的人类可读原因(可空)
#[tauri::command]
pub async fn approval_respond(
    state: tauri::State<'_, AppState>,
    request_id: String,
    approved: bool,
    reason: Option<String>,
) -> Result<(), String> {
    let outcome = if approved {
        ApprovalOutcome::Approved
    } else {
        ApprovalOutcome::Denied(ApprovalDenial {
            category: DenialCategory::UserRejected,
            reason: reason.unwrap_or_else(|| "用户拒绝了此操作".to_string()),
        })
    };
    let sender = state
        .approval_pending
        .lock()
        .map_err(|e| format!("审批挂起表锁异常: {}", e))?
        .remove(&request_id);
    match sender {
        Some(tx) => tx
            .send(outcome)
            .map_err(|_| "审批请求已超时或已被处理".to_string()),
        None => Err(format!("未知的审批请求: {}", request_id)),
    }
}
