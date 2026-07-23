use tauri::State;

use crate::AppState;

#[tauri::command]
pub async fn ai_history_add(
    state: State<'_, AppState>,
    dir_path: String,
    item: crate::services::ai_history::AddAiHistoryRequest,
) -> Result<crate::services::ai_history::AiHistoryItem, String> {
    let store = state.get_ai_history_store(&dir_path)?;
    tokio::task::spawn_blocking(move || store.add(&item))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

#[tauri::command]
pub async fn ai_history_list(
    state: State<'_, AppState>,
    dir_path: String,
    limit: Option<u32>,
    offset: Option<u32>,
) -> Result<Vec<crate::services::ai_history::AiHistoryItem>, String> {
    let store = state.get_ai_history_store(&dir_path)?;
    tokio::task::spawn_blocking(move || {
        store.list(limit.unwrap_or(100), offset.unwrap_or(0))
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))?
}

#[tauri::command]
pub async fn ai_history_delete(
    state: State<'_, AppState>,
    dir_path: String,
    id: String,
) -> Result<bool, String> {
    let store = state.get_ai_history_store(&dir_path)?;
    tokio::task::spawn_blocking(move || store.delete(&id))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

#[tauri::command]
pub async fn ai_history_toggle_favorite(
    state: State<'_, AppState>,
    dir_path: String,
    id: String,
) -> Result<bool, String> {
    let store = state.get_ai_history_store(&dir_path)?;
    tokio::task::spawn_blocking(move || store.toggle_favorite(&id))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

#[tauri::command]
pub async fn ai_history_update_access_time(
    state: State<'_, AppState>,
    dir_path: String,
    id: String,
) -> Result<(), String> {
    let store = state.get_ai_history_store(&dir_path)?;
    tokio::task::spawn_blocking(move || store.update_access_time(&id))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

#[tauri::command]
pub async fn ai_history_update_file_path(
    state: State<'_, AppState>,
    dir_path: String,
    old_file_path: String,
    new_file_name: String,
    new_file_path: String,
) -> Result<(), String> {
    let store = state.get_ai_history_store(&dir_path)?;
    tokio::task::spawn_blocking(move || {
        store.update_file_path(&old_file_path, &new_file_name, &new_file_path)
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))?
}

#[tauri::command]
pub async fn ai_history_stats(
    state: State<'_, AppState>,
    dir_path: String,
) -> Result<crate::services::ai_history::AiHistoryStats, String> {
    let store = state.get_ai_history_store(&dir_path)?;
    tokio::task::spawn_blocking(move || store.get_stats())
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}
