use std::collections::HashMap;

use serde_json::Value;
use tauri::AppHandle;
use tauri_plugin_store::StoreExt;

const STORE_FILE: &str = "app_settings.json";

/// 读取所有配置（返回 key-value Map）
///
/// 可用配置项：
/// - graphql_endpoint, nginx_conf_path
/// - local_llm_endpoint, local_llm_token, local_llm_model, local_llm_context_length
/// - embedding_endpoint, embedding_token, embedding_model, embedding_dimension
/// - dir_blacklist, file_blacklist, random_dir_blacklist, html_code_show_blacklist
#[tauri::command]
pub async fn kb_config_read(app: AppHandle) -> Result<HashMap<String, Value>, String> {
    let store = app
        .store(STORE_FILE)
        .map_err(|e| format!("打开配置存储失败: {}", e))?;

    let mut map = HashMap::new();
    for key in store.keys() {
        if let Some(val) = store.get(&key) {
            map.insert(key.clone(), val.clone());
        }
    }
    Ok(map)
}

/// 写入配置（批量设置 key-value）
#[tauri::command]
pub async fn kb_config_write(
    app: AppHandle,
    settings: HashMap<String, Value>,
) -> Result<(), String> {
    let store = app
        .store(STORE_FILE)
        .map_err(|e| format!("打开配置存储失败: {}", e))?;

    for (key, value) in &settings {
        store.set(key.clone(), value.clone());
    }

    store.save().map_err(|e| format!("保存配置失败: {}", e))?;
    Ok(())
}

/// 删除指定配置项
#[tauri::command]
pub async fn kb_config_delete(app: AppHandle, key: String) -> Result<(), String> {
    let store = app
        .store(STORE_FILE)
        .map_err(|e| format!("打开配置存储失败: {}", e))?;

    store.delete(&key);
    store.save().map_err(|e| format!("保存配置失败: {}", e))?;
    Ok(())
}
