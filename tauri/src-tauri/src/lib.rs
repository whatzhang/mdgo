mod commands;
mod db;

use commands::fs_watcher::WatcherHandle;
use commands::system::SystemMonitorState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 设置 HuggingFace 镜像源（解决国内下载慢/失败问题）
    // 优先使用用户已设置的环境变量，否则使用国内镜像
    if std::env::var("HF_ENDPOINT").is_err() {
        // SAFETY: 在程序启动时设置环境变量是安全的
        unsafe {
            std::env::set_var("HF_ENDPOINT", "https://hf-mirror.com");
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .manage(SystemMonitorState::new())
        .manage(WatcherHandle::new())
        .invoke_handler(tauri::generate_handler![
            commands::fs::read_dir_recursive,
            commands::fs::read_dir,
            commands::fs::read_file,
            commands::fs::read_file_binary,
            commands::fs::write_file,
            commands::fs::write_file_binary,
            commands::fs::delete,
            commands::fs::rename,
            commands::fs::create_dir,
            commands::fs::exists,
            commands::fs::get_file_meta,
            commands::open_url::open_url,
            commands::git::git_log,
            commands::git::git_status_matrix,
            commands::git::git_checkout,
            commands::git::git_parse_refs,
            commands::git::git_diff_tree,
            commands::git::git_read_blob,
            commands::git::git_add,
            commands::git::git_reset,
            commands::git::git_commit,
            commands::git::git_resolve_ref,
            commands::system::start_monitor,
            commands::system::stop_monitor,
            commands::clipboard::copy_to_clipboard,
            commands::knowledge::kb_index,
            commands::knowledge::kb_search_hybrid,
            commands::knowledge::kb_status,
            commands::knowledge::kb_clear,
            commands::config::kb_config_read,
            commands::config::kb_config_write,
            commands::config::kb_config_delete,
            commands::fs_watcher::kb_start_watcher,
            commands::fs_watcher::kb_stop_watcher,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
