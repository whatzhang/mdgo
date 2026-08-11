//! 规划确认命令：前端对 `plan:request` 的用户决定回传。
//!
//! 协议与审批通道（approval_respond）同构：后端挂起表（`AppState.plan_pending`）
//! 持有 oneshot sender，前端经 `invoke("plan_respond", ...)` 回传决定。

use crate::core::agent::planner::PlanDecision;
use crate::AppState;

/// 回传用户对任务计划的决定（`plan:request` 的配套响应）。
///
/// 从 `AppState.plan_pending` 取对应 oneshot sender 回传；未知 `plan_id`
/// （已超时/已被处理）返回错误，前端可忽略。
#[tauri::command]
pub async fn plan_respond(
    state: tauri::State<'_, AppState>,
    plan_id: String,
    approved: bool,
    reason: Option<String>,
) -> Result<(), String> {
    let decision = if approved {
        PlanDecision::Approved
    } else {
        PlanDecision::Denied(reason.unwrap_or_else(|| "用户未批准计划".to_string()))
    };
    let sender = state
        .plan_pending
        .lock()
        .map_err(|e| e.to_string())?
        .remove(&plan_id);
    match sender {
        Some(tx) => tx.send(decision).map_err(|_| "规划请求已超时或已被处理".to_string()),
        None => Err(format!("未知的规划请求: {}", plan_id)),
    }
}