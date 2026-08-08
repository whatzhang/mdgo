use tauri::State;

use crate::services::prompt::{PromptItem, PromptStore, UpsertPromptRequest};

/// 列出所有 prompt 模板
#[tauri::command]
pub fn prompt_list(
    store: State<'_, PromptStore>,
) -> Result<Vec<PromptItem>, String> {
    store.list()
}

/// 创建 prompt 模板
#[tauri::command]
pub fn prompt_create(
    store: State<'_, PromptStore>,
    name: String,
    prompt: String,
) -> Result<PromptItem, String> {
    if name.trim().is_empty() {
        return Err("名称不能为空".to_string());
    }
    if prompt.trim().is_empty() {
        return Err("内容不能为空".to_string());
    }
    store.create(&UpsertPromptRequest {
        name: name.trim().to_string(),
        prompt: prompt.trim().to_string(),
    })
}

/// 更新 prompt 模板
#[tauri::command]
pub fn prompt_update(
    store: State<'_, PromptStore>,
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
    store.update(&id, &UpsertPromptRequest {
        name: name.trim().to_string(),
        prompt: prompt.trim().to_string(),
    })
}

/// 删除 prompt 模板
#[tauri::command]
pub fn prompt_delete(
    store: State<'_, PromptStore>,
    id: String,
) -> Result<(), String> {
    store.delete(&id)
}