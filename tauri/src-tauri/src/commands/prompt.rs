use tauri::{AppHandle, Emitter, Manager};

use crate::services::prompt::{GlobalPromptStore, PromptItem, PromptStore, UpsertPromptRequest};
use crate::AppState;

/// 广播 Prompt 变更事件（前端监听 `prompt:changed` 自动刷新）
fn emit_changed(app: &AppHandle) {
    let _ = app.emit("prompt:changed", ());
}

/// 按知识库目录获取项目 Prompt 存储（知识库级：`{dir}/.mdgo/mdgo.db`）
fn store_for(app: &AppHandle, dir_path: &str) -> Result<std::sync::Arc<PromptStore>, String> {
    app.state::<AppState>().prompt_store_for(dir_path)
}

/// 获取全局 Prompt 存储（用户数据目录，进程内缓存单例）
fn global_store(app: &AppHandle) -> Result<std::sync::Arc<GlobalPromptStore>, String> {
    app.state::<AppState>().global_prompt_store(app)
}

/// 列出所有前端可见 prompt（系统 + 全局 + 项目 三层合并，可选用 scope 过滤）。
///
/// - system：全局 DB 中 `scope='system'` 且 `display=1` 的行（seed.sql 初始化写入）；
/// - global：全局 DB 中 `scope='global'` 的行（前端创建）；
/// - project：{dir}/.mdgo/mdgo.db 的 prompts 表（display=1）。
/// `display=0` 的系统内置 prompt（用户无感知）不返回。
#[tauri::command]
pub async fn prompt_list(
    app: AppHandle,
    dir_path: String,
    scope: Option<String>,
) -> Result<Vec<PromptItem>, String> {
    let store = store_for(&app, &dir_path)?;
    let g_store = global_store(&app)?;
    let (global, project) = tokio::task::spawn_blocking(move || {
        let g = g_store.list_visible()?;
        let p = store.list_visible()?;
        Ok::<_, String>((g, p))
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))??;
    let mut all = global;
    all.extend(project);

    // 作用域过滤（scope 为空 = 全部）
    if let Some(sc) = scope {
        if !sc.is_empty() {
            all.retain(|p| p.scope == sc);
        }
    }
    Ok(all)
}

/// 创建 prompt 模板（仅 global / project 作用域；display 默认 true）
#[tauri::command]
pub async fn prompt_create(
    app: AppHandle,
    dir_path: String,
    scope: String,
    name: String,
    prompt: String,
) -> Result<PromptItem, String> {
    if name.trim().is_empty() {
        return Err("名称不能为空".to_string());
    }
    if prompt.trim().is_empty() {
        return Err("内容不能为空".to_string());
    }
    let req = UpsertPromptRequest {
        name: name.trim().to_string(),
        prompt: prompt.trim().to_string(),
    };
    let item = match scope.as_str() {
        "global" => {
            let g_store = global_store(&app)?;
            tokio::task::spawn_blocking(move || crate::core::db::with_busy_retry(3, || g_store.create(&req)))
                .await
                .map_err(|e| format!("任务执行失败: {}", e))??
        }
        "project" => {
            let store = store_for(&app, &dir_path)?;
            tokio::task::spawn_blocking(move || crate::core::db::with_busy_retry(3, || store.create(&req)))
                .await
                .map_err(|e| format!("任务执行失败: {}", e))??
        }
        other => return Err(format!("作用域非法: {}（应为 global/project）", other)),
    };
    emit_changed(&app);
    Ok(item)
}

/// 更新 prompt 模板（仅 global / project；system 只读拒绝）
#[tauri::command]
pub async fn prompt_update(
    app: AppHandle,
    dir_path: String,
    scope: String,
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
    let req = UpsertPromptRequest {
        name: name.trim().to_string(),
        prompt: prompt.trim().to_string(),
    };
    let item = match scope.as_str() {
        "global" => {
            let g_store = global_store(&app)?;
            tokio::task::spawn_blocking(move || crate::core::db::with_busy_retry(3, || g_store.update(&id, &req)))
                .await
                .map_err(|e| format!("任务执行失败: {}", e))??
        }
        "project" => {
            let store = store_for(&app, &dir_path)?;
            tokio::task::spawn_blocking(move || crate::core::db::with_busy_retry(3, || store.update(&id, &req)))
                .await
                .map_err(|e| format!("任务执行失败: {}", e))??
        }
        other => return Err(format!("作用域非法: {}（应为 global/project）", other)),
    };
    emit_changed(&app);
    Ok(item)
}

/// 删除 prompt 模板（仅 global / project；system 只读拒绝）
#[tauri::command]
pub async fn prompt_delete(
    app: AppHandle,
    dir_path: String,
    scope: String,
    id: String,
) -> Result<(), String> {
    match scope.as_str() {
        "global" => {
            let g_store = global_store(&app)?;
            tokio::task::spawn_blocking(move || crate::core::db::with_busy_retry(3, || g_store.delete(&id)))
                .await
                .map_err(|e| format!("任务执行失败: {}", e))??
        }
        "project" => {
            let store = store_for(&app, &dir_path)?;
            tokio::task::spawn_blocking(move || crate::core::db::with_busy_retry(3, || store.delete(&id)))
                .await
                .map_err(|e| format!("任务执行失败: {}", e))??
        }
        other => return Err(format!("作用域非法: {}（应为 global/project）", other)),
    }
    emit_changed(&app);
    Ok(())
}
