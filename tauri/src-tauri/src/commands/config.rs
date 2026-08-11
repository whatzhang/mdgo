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
    // 规划模型（P0-6，可选；空串 = 使用主模型）
    planner_model: Option<String>,
    // 摘要模型（P0-6，可选；空串 = 使用主模型）
    summary_model: Option<String>,
    // 推理努力等级（P2-18，可选；low/medium/high，空串 = 不设置）
    reasoning_effort: Option<String>,
) -> Result<(), String> {
    // 归一化：空串视为 None（前端可能传空字符串）
    let normalize = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let planner_model = normalize(planner_model);
    let summary_model = normalize(summary_model);
    let reasoning_effort = normalize(reasoning_effort);

    // 1. 更新内存配置
    {
        let mut cfg = state.llm_config.write().unwrap_or_else(|e| e.into_inner());
        cfg.endpoint = endpoint.clone();
        cfg.model = model.clone();
        cfg.api_key = api_key.clone();
        cfg.planner_model = planner_model.clone();
        cfg.summary_model = summary_model.clone();
        cfg.reasoning_effort = reasoning_effort.clone();
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
        match &planner_model {
            Some(v) => obj.insert("localLlmPlannerModel".into(), Value::String(v.clone())),
            None => obj.remove("localLlmPlannerModel"),
        };
        match &summary_model {
            Some(v) => obj.insert("localLlmSummaryModel".into(), Value::String(v.clone())),
            None => obj.remove("localLlmSummaryModel"),
        };
        match &reasoning_effort {
            Some(v) => obj.insert("localLlmReasoningEffort".into(), Value::String(v.clone())),
            None => obj.remove("localLlmReasoningEffort"),
        };
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

// ─────────────────────────── 统一配置入口（O5） ───────────────────────────

/// 从全量设置对象中提取 LLM 段（camelCase 键，空串归一为 None）。
fn extract_llm_fields(settings: &serde_json::Value) -> (String, String, String, Option<String>, Option<String>, Option<String>) {
    let get = |k: &str| {
        settings
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let norm = |s: String| if s.is_empty() { None } else { Some(s) };
    (
        get("localLlmEndpoint"),
        get("localLlmModel"),
        get("localLlmToken"),
        norm(get("localLlmPlannerModel")),
        norm(get("localLlmSummaryModel")),
        norm(get("localLlmReasoningEffort")),
    )
}

/// 保存全量设置（O5：Tauri 模式统一配置入口）。
///
/// 写 `{dir}/.mdgo/setting.json`（全量 JSON，前端 settingsJson 原样落盘），
/// 并提取 LLM 段同步内存 `LlmConfig`（规划/摘要模型、推理等级一并生效）。
/// 本地模式（无 Tauri）不调用本命令，由前端 File System Access 直写 JSON 保持现状。
#[tauri::command]
pub async fn kb_save_setting(
    state: State<'_, AppState>,
    dir_path: String,
    settings: serde_json::Value,
) -> Result<(), String> {
    // 1. 写全量 setting.json（pretty）
    let setting_path = std::path::Path::new(&dir_path)
        .join(".mdgo")
        .join("setting.json");
    if let Some(parent) = setting_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建配置目录失败: {}", e))?;
    }
    let json_str = serde_json::to_string_pretty(&settings)
        .map_err(|e| format!("序列化配置失败: {}", e))?;
    std::fs::write(&setting_path, &json_str)
        .map_err(|e| format!("写入配置文件失败: {}", e))?;

    // 2. 提取 LLM 段同步内存（前端可能只保存非 LLM 段时 settings 缺 LLM 键 → 空，不覆盖）
    let (endpoint, model, api_key, planner, summary, effort) = extract_llm_fields(&settings);
    if !endpoint.is_empty() || !model.is_empty() {
        let mut cfg = state.llm_config.write().unwrap_or_else(|e| e.into_inner());
        cfg.endpoint = endpoint;
        cfg.model = model;
        cfg.api_key = api_key;
        cfg.planner_model = planner;
        cfg.summary_model = summary;
        cfg.reasoning_effort = effort;
    }

    log::info!("[config] 全量设置已保存: {}", setting_path.display());
    Ok(())
}

/// 读取全量设置（O5：Tauri 模式统一配置入口）。
///
/// 读 `{dir}/.mdgo/setting.json`（不存在返回空对象）；内存 `LlmConfig` 非空时
/// 用其覆盖 LLM 段（内存为最近一次保存的权威值，避免启动顺序导致文件旧值残留）。
#[tauri::command]
pub async fn kb_load_setting(
    state: State<'_, AppState>,
    dir_path: String,
) -> Result<serde_json::Value, String> {
    let setting_path = std::path::Path::new(&dir_path)
        .join(".mdgo")
        .join("setting.json");
    let mut config: serde_json::Value = std::fs::read_to_string(&setting_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    // 内存 LLM 配置优先（仅当已配置时覆盖，避免空配置误清文件值）
    let cfg = state.llm_config.read().unwrap_or_else(|e| e.into_inner()).clone();
    if !cfg.endpoint.is_empty() || !cfg.model.is_empty() {
        if let Some(obj) = config.as_object_mut() {
            obj.insert("localLlmEndpoint".into(), Value::String(cfg.endpoint));
            obj.insert("localLlmModel".into(), Value::String(cfg.model));
            obj.insert("localLlmToken".into(), Value::String(cfg.api_key));
            match &cfg.planner_model {
                Some(v) => obj.insert("localLlmPlannerModel".into(), Value::String(v.clone())),
                None => obj.remove("localLlmPlannerModel"),
            };
            match &cfg.summary_model {
                Some(v) => obj.insert("localLlmSummaryModel".into(), Value::String(v.clone())),
                None => obj.remove("localLlmSummaryModel"),
            };
            match &cfg.reasoning_effort {
                Some(v) => obj.insert("localLlmReasoningEffort".into(), Value::String(v.clone())),
                None => obj.remove("localLlmReasoningEffort"),
            };
        }
    }
    Ok(config)
}
