use std::collections::HashMap;

use serde_json::Value;
use tauri::{AppHandle, State};
use tauri_plugin_store::StoreExt;

use crate::AppState;

const STORE_FILE: &str = "app_settings.json";

/// 读取所有配置（返回 key-value Map）
///
/// 可用配置项：
/// - graphql_endpoint, nginx_conf_path
/// - local_llm_endpoint, local_llm_token, local_llm_model, local_llm_context_length
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

/// 更新 LLM 连接配置（中央化入口）
///
/// 前端在保存设置后调用此命令，更新后端内存中的 LLM 配置，
/// 同时将配置持久化到 `.mdgo/setting.json`。
#[tauri::command]
pub async fn kb_update_llm_config(
    state: State<'_, AppState>,
    dir_path: String,
    endpoint: String,
    model: String,
    api_key: String,
) -> Result<(), String> {
    // 1. 更新内存配置
    {
        let mut cfg = state.llm_config.write().unwrap_or_else(|e| e.into_inner());
        cfg.endpoint = endpoint.clone();
        cfg.model = model.clone();
        cfg.api_key = api_key.clone();
    }

    // 2. 持久化到 .mdgo/setting.json
    let setting_path = std::path::Path::new(&dir_path)
        .join(".mdgo")
        .join("setting.json");

    // 读取现有配置，只更新 LLM 字段（不覆盖其它配置）
    let mut config: serde_json::Value = std::fs::read_to_string(&setting_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    if let Some(obj) = config.as_object_mut() {
        obj.insert("localLlmEndpoint".into(), Value::String(endpoint.clone()));
        obj.insert("localLlmModel".into(), Value::String(model.clone()));
        obj.insert("localLlmToken".into(), Value::String(api_key.clone()));
    }

    let json_str = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("序列化配置失败: {}", e))?;

    // 确保目录存在
    if let Some(parent) = setting_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建配置目录失败: {}", e))?;
    }

    std::fs::write(&setting_path, &json_str)
        .map_err(|e| format!("写入配置文件失败: {}", e))?;

    log::info!("[config] LLM 配置已更新: endpoint={}, model={}", setting_path.display(), &model);
    Ok(())
}
