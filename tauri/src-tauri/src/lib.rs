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

    // 设置 HuggingFace 镜像源（解决国内下载慢/失败问题）
    // 优先使用用户已设置的环境变量，否则使用国内镜像
    if std::env::var("HF_ENDPOINT").is_err() {
        // SAFETY: 在程序启动时设置环境变量是安全的
        unsafe {
            std::env::set_var("HF_ENDPOINT", "https://hf-mirror.com");
        }
    }

    // 设置 fastembed 模型缓存目录
    // dev 模式下，模型可能已下载到 CARGO_MANIFEST_DIR/.fastembed_cache/
    // 需要显式指定绝对路径，避免因 CWD 不同或缓存结构校验失败导致重试下载
    if std::env::var("FASTEMBED_CACHE_DIR").is_err() {
        let search_paths = [
            // 通常 CWD = 项目根目录 或 src-tauri/
            std::env::current_dir().map(|p| p.join(".fastembed_cache")).ok(),
            // dev 模式下：cargo manifest dir = tauri/src-tauri/
            Some(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".fastembed_cache")),
        ];
        for path in search_paths.iter().flatten() {
            if path.exists() {
                unsafe {
                    std::env::set_var("FASTEMBED_CACHE_DIR", path.to_string_lossy().as_ref());
                }
                log::info!("[startup] FASTBED_CACHE_DIR 设为: {}", path.display());
                break;
            }
        }
    }

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
/// 日志文件路径：`%APPDATA%/mdgo/logs/app.log`（Windows）
/// 终端日志仅在附加了控制台时有效（dev 模式下可见）。
fn init_logging() {
    let log_dir = std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("mdgo")
        .join("logs");

    let log_path = log_dir.join("app.log");

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
