use tauri::{AppHandle, Manager};

use crate::services::prompt::{PromptItem, PromptStore, UpsertPromptRequest};
use crate::AppState;

/// 按知识库目录获取 Prompt 模板存储（知识库级：`{dir}/.mdgo/mdgo.db`）
fn store_for(app: &AppHandle, dir_path: &str) -> Result<std::sync::Arc<PromptStore>, String> {
    app.state::<AppState>().prompt_store_for(dir_path)
}

/// 列出所有 prompt 模板
#[tauri::command]
pub async fn prompt_list(app: AppHandle, dir_path: String) -> Result<Vec<PromptItem>, String> {
    let store = store_for(&app, &dir_path)?;
    // SQLite 为阻塞 IO，移入 blocking 线程，避免占住 tokio worker
    tokio::task::spawn_blocking(move || store.list())
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

/// 创建 prompt 模板
#[tauri::command]
pub async fn prompt_create(
    app: AppHandle,
    dir_path: String,
    name: String,
    prompt: String,
) -> Result<PromptItem, String> {
    if name.trim().is_empty() {
        return Err("名称不能为空".to_string());
    }
    if prompt.trim().is_empty() {
        return Err("内容不能为空".to_string());
    }
    let store = store_for(&app, &dir_path)?;
    let req = UpsertPromptRequest {
        name: name.trim().to_string(),
        prompt: prompt.trim().to_string(),
    };
    tokio::task::spawn_blocking(move || crate::core::db::with_busy_retry(3, || store.create(&req)))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

/// 更新 prompt 模板
#[tauri::command]
pub async fn prompt_update(
    app: AppHandle,
    dir_path: String,
    id: String,
    name: String,
    prompt: String,
) -> Result<PromptItem, String> {
    if name.trim().is_empty() {
        return Err("名称不能为空".to_string());
    }
    if prompt.trim().is_empty() {
        return Err("内容不能为空".to_string());
    }
    let store = store_for(&app, &dir_path)?;
    let req = UpsertPromptRequest {
        name: name.trim().to_string(),
        prompt: prompt.trim().to_string(),
    };
    tokio::task::spawn_blocking(move || {
        crate::core::db::with_busy_retry(3, || store.update(&id, &req))
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))?
}

/// 删除 prompt 模板
#[tauri::command]
pub async fn prompt_delete(app: AppHandle, dir_path: String, id: String) -> Result<(), String> {
    let store = store_for(&app, &dir_path)?;
    tokio::task::spawn_blocking(move || crate::core::db::with_busy_retry(3, || store.delete(&id)))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}
