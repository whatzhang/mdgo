use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager};

use crate::core::IndexerConfig;
use crate::AppState;

/// 启动文件监听（带防抖）
///
/// Idempotent：已在监听同一目录时直接返回；更换目录自动重启。
/// 黑名单写入 ConfigStore，供 Indexer 扫描时使用。
#[tauri::command]
pub async fn kb_start_watcher(
    app: AppHandle,
    dir_path: String,
    dir_blacklist: Vec<String>,
    file_blacklist: Vec<String>,
) -> Result<(), String> {
    let state = app.state::<AppState>();

    // 同步黑名单到全局 ConfigStore（保留已有分块/检索参数）
    state.config_store.update(IndexerConfig {
        dir_blacklist: dir_blacklist.clone(),
        file_blacklist: file_blacklist.clone(),
        ..state.config_store.read()
    });

    // 注入带 Tauri 事件发射的错误回调（覆盖初始的仅 eprintln 版本）
    let app_clone = app.clone();
    state.watcher.set_on_error(Arc::new(move |msg: &str| {
        log::error!("[watcher-err] {}", msg);
        let _ = app_clone.emit("watcher-error", msg.to_string());
    }));

    // 注入变更通知回调：索引更新后发射 kb-watcher-event，携带变更文件相对路径列表，通知前端刷新 
    let app_clone2 = app.clone();
    state.watcher.set_on_changed(Arc::new(move |paths: &[String]| {
        let _ = app_clone2.emit("kb-watcher-event", paths.to_vec());
    }));

    // 启动 watcher（Idempotent）
    state.watcher.start(&dir_path, &dir_blacklist, &file_blacklist)
}

/// 停止文件监听
#[tauri::command]
pub async fn kb_stop_watcher(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.watcher.stop();
    Ok(())
}

/// 设置索引开关（true = 对文件变更执行增量索引；false = 仅监听并失效缓存）
///
/// 即使索引关闭，watcher 仍持续运行并失效文件列表缓存，保证 ls 数据新鲜。
#[tauri::command]
pub async fn kb_set_indexing_enabled(app: AppHandle, enabled: bool) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.watcher.set_indexing_enabled(enabled);
    Ok(())
}
