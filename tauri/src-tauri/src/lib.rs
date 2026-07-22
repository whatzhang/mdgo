#![allow(linker_messages)]
mod commands;
mod db;
mod services;

use std::sync::Arc;

use commands::system::SystemMonitorState;
use log::LevelFilter;
use services::{ConfigStore, Indexer, IndexerConfig, WatcherService};
use simplelog::{ColorChoice, Config, TerminalMode, TermLogger, WriteLogger};

/// Tauri 托管的应用级共享状态
pub struct AppState {
    pub config_store: Arc<ConfigStore>,
    pub indexer: Arc<Indexer>,
    pub watcher: Arc<WatcherService>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // ── 初始化日志（文件 + 终端双输出）──
    init_logging();

    log::info!("[startup] 启动 mdgo...");

    // 初始化共享服务
    let config_store = Arc::new(ConfigStore::new(IndexerConfig::default()));
    let indexer = Arc::new(Indexer::new(config_store.clone()));

    // watcher 错误回调：通过日志输出
    let on_error: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(|msg: &str| {
        log::error!("[watcher-err] {}", msg);
    });
    let watcher = Arc::new(WatcherService::new(indexer.clone(), on_error));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .manage(SystemMonitorState::new())
        .manage(AppState { config_store, indexer, watcher })
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

/// 初始化日志系统：文件日志（Debug）+ 终端日志（Info）双输出。
///
/// 日志文件路径：
/// - macOS/Linux: `~/Library/Logs/mdgo/mdgo.log` / `~/.cache/mdgo/logs/mdgo.log`
/// - Windows: `%APPDATA%/mdgo/logs/mdgo.log`
///
/// **注意**：日志目录不会在项目目录内，避免触发 Tauri 开发服务器的文件监听重建循环。
fn init_logging() {
    let log_dir = log_dir_global();
    let log_path = log_dir.join("mdgo.log");

    // 创建文件日志
    let has_file_logger;
    let file_logger = match std::fs::create_dir_all(&log_dir)
        .and_then(|_| std::fs::File::create(&log_path))
    {
        Ok(file) => {
            has_file_logger = true;
            Some(WriteLogger::new(LevelFilter::Debug, Config::default(), file))
        }
        Err(_) => {
            has_file_logger = false;
            None
        }
    };

    // 创建终端日志（dev 模式下终端可用）
    let term_logger = TermLogger::new(
        LevelFilter::Info,
        Config::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    );

    let mut loggers: Vec<Box<dyn simplelog::SharedLogger>> = Vec::with_capacity(2);
    loggers.push(term_logger);
    if let Some(file) = file_logger {
        loggers.push(file);
    }

    let _ = simplelog::CombinedLogger::init(loggers);

    if has_file_logger {
        log::info!("日志文件: {}", log_path.display());
    }
}

/// 跨平台日志根目录（不依赖 Tauri API，纯标准库实现）。
///
/// - Windows: `%APPDATA%/mdgo/logs/`
/// - macOS:   `~/Library/Logs/mdgo/`
/// - Linux:   `~/.cache/mdgo/logs/`
fn log_dir_global() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA")
            .map(|p| std::path::PathBuf::from(p).join("mdgo").join("logs"))
            .unwrap_or_else(|_| dirs_fallback())
    }
    #[cfg(target_os = "macos")]
    {
        std::path::PathBuf::from(
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
        )
        .join("Library")
        .join("Logs")
        .join("mdgo")
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        std::path::PathBuf::from(
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
        )
        .join(".cache")
        .join("mdgo")
        .join("logs")
    }
}

/// 兜底日志路径（所有平台都无法获取标准目录时使用）
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn dirs_fallback() -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp/mdgo/logs")
}
