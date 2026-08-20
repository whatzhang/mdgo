//! 用户澄清提问（ask_user_question 工具）回传命令。
//!
//! 协议与审批/规划确认通道同构：工具侧把 oneshot sender 注册进
//! `AppState.user_question_pending` 并 emit `question:request` 事件；
//! 前端弹窗收集用户回答后经 `invoke("question_respond", ...)` 回传，
//! 本命令按 question_id 取回 sender 并完成（answer=None 表示用户取消/未答）。

use crate::AppState;

/// 回传用户对澄清提问的回答（`question:request` 的配套响应）。
///
/// - `question_id`:工具随 `question:request` 事件下发的请求 ID
/// - `answer`:用户输入/选择的回答文本；`null` 表示用户取消或未回答
#[tauri::command]
pub async fn question_respond(
    state: tauri::State<'_, AppState>,
    question_id: String,
    answer: Option<String>,
) -> Result<(), String> {
    let sender = state
        .user_question_pending
        .lock()
        .map_err(|e| format!("提问挂起表锁异常: {}", e))?
        .remove(&question_id);
    match sender {
        Some(tx) => tx
            .send(answer)
            .map_err(|_| "提问请求已超时或已被处理".to_string()),
        None => Err(format!("未知的提问请求: {}", question_id)),
    }
}
