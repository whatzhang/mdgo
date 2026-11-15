//! 使用行为统计命令（行为分析页）。
//!
//! 与 [`crate::services::usage`] 分工：服务层只碰 SQLite，命令层负责路径规范化、
//! 惰性 store 缓存与阻塞任务隔离。

use tauri::State;

use crate::services::usage::{
    UsageEvent, UsageFileList, UsageRecordResult, UsageSummary,
};
use crate::AppState;

/// 批量累计使用事件（前端每 5s / 页面隐藏时 flush 一次）。
#[tauri::command]
pub async fn usage_record(
    state: State<'_, AppState>,
    dir_path: String,
    events: Vec<UsageEvent>,
    prune_before: Option<String>,
) -> Result<UsageRecordResult, String> {
    let store = state.get_usage_store(&dir_path)?;
    let prune = prune_before.unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        crate::core::db::with_busy_retry(3, || {
            store.record(&events, if prune.is_empty() { None } else { Some(prune.as_str()) })
        })
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))?
}

/// 区间汇总（含上一等长周期，供 KPI 环比）。
///
/// 区间由前端按本地日历计算（`from`/`to`/`prev_from`/`prev_to` 均为 `YYYY-MM-DD`），
/// 后端不做时区换算。
#[tauri::command]
pub async fn usage_summary(
    state: State<'_, AppState>,
    dir_path: String,
    from: String,
    to: String,
    prev_from: Option<String>,
    prev_to: Option<String>,
) -> Result<UsageSummary, String> {
    let store = state.get_usage_store(&dir_path)?;
    let prev_from = prev_from.unwrap_or_default();
    let prev_to = prev_to.unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        crate::core::db::with_busy_retry(3, || store.summary(&from, &to, &prev_from, &prev_to))
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))?
}

/// 区间内文件维度聚合（TopN + 不重复打开文件数）。
#[tauri::command]
pub async fn usage_files(
    state: State<'_, AppState>,
    dir_path: String,
    from: String,
    to: String,
    limit: Option<u32>,
) -> Result<UsageFileList, String> {
    let store = state.get_usage_store(&dir_path)?;
    let limit = limit.unwrap_or(500);
    tokio::task::spawn_blocking(move || {
        crate::core::db::with_busy_retry(3, || store.files(&from, &to, limit))
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))?
}
