use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager};

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

    // 同步黑名单到全局 ConfigStore
    state.config_store.update(crate::services::IndexerConfig {
        dir_blacklist: dir_blacklist.clone(),
        file_blacklist: file_blacklist.clone(),
    });

    // 注入带 Tauri 事件发射的错误回调（覆盖初始的仅 eprintln 版本）
    let app_clone = app.clone();
    state.watcher.set_on_error(Arc::new(move |msg: &str| {
        log::error!("[watcher-err] {}", msg);
        let _ = app_clone.emit("watcher-error", msg.to_string());
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
