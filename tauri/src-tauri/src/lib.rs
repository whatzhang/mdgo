#![allow(linker_messages)]
mod commands;
mod services;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use commands::system::SystemMonitorState;
use log::LevelFilter;
use simplelog::{ColorChoice, Config, TerminalMode, TermLogger, WriteLogger};
use mdgo_core::{ConfigStore, Indexer, IndexerConfig, WatcherService};

/// Tauri 托管的应用级共享状态
pub struct AppState {
    pub config_store: Arc<ConfigStore>,
    pub indexer: Arc<Indexer>,
    pub watcher: Arc<WatcherService>,
    /// 按目录路径缓存的 ChatStore 实例（惰性创建）
    pub chat_stores: Mutex<HashMap<String, Arc<services::chat::ChatStore>>>,
    /// 按目录路径缓存的 AiHistoryStore 实例（惰性创建）
    pub ai_history_stores: Mutex<HashMap<String, Arc<services::ai_history::AiHistoryStore>>>,
}

impl AppState {
    /// 获取或创建指定目录的 ChatStore
    pub fn get_chat_store(&self, dir_path: &str) -> Result<Arc<services::chat::ChatStore>, String> {
        let mut stores = self.chat_stores.lock().map_err(|e| e.to_string())?;
        if let Some(store) = stores.get(dir_path) {
            return Ok(Arc::clone(store));
        }
        // 聊天数据存储在 {dir_path}/.mdgo/data/chat.db
        let db_dir = std::path::Path::new(dir_path)
            .join(".mdgo")
            .join("data");
        let store = Arc::new(
            services::chat::ChatStore::new(
                &db_dir.to_string_lossy(),
            )?,
        );
        stores.insert(dir_path.to_string(), Arc::clone(&store));
        Ok(store)
    }

    /// 获取或创建指定目录的 AiHistoryStore
    pub fn get_ai_history_store(
        &self,
        dir_path: &str,
    ) -> Result<Arc<services::ai_history::AiHistoryStore>, String> {
        let mut stores = self.ai_history_stores.lock().map_err(|e| e.to_string())?;
        if let Some(store) = stores.get(dir_path) {
            return Ok(Arc::clone(store));
        }
        // AI 历史数据存储在 {dir_path}/.mdgo/data/ai_history.db
        let db_dir = std::path::Path::new(dir_path)
            .join(".mdgo")
            .join("data");
        let store = Arc::new(
            services::ai_history::AiHistoryStore::new(&db_dir.to_string_lossy())?,
        );
        stores.insert(dir_path.to_string(), Arc::clone(&store));
        Ok(store)
    }
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
    // watcher 变更回调（初始无操作，启动时由 fs_watcher 注入真实 Tauri 事件发射器）
    let on_changed: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
    let watcher = Arc::new(WatcherService::new(indexer.clone(), on_error, on_changed));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .manage(SystemMonitorState::new())
        .manage(AppState {
            config_store,
            indexer,
            watcher,
            chat_stores: Mutex::new(HashMap::new()),
            ai_history_stores: Mutex::new(HashMap::new()),
        })
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
            commands::knowledge::kb_dashboard_stats,
            commands::config::kb_config_read,
            commands::config::kb_config_write,
            commands::config::kb_config_delete,
            commands::fs_watcher::kb_start_watcher,
            commands::fs_watcher::kb_stop_watcher,
            // AI 历史记录命令
            commands::ai_history::ai_history_add,
            commands::ai_history::ai_history_list,
            commands::ai_history::ai_history_delete,
            commands::ai_history::ai_history_toggle_favorite,
            commands::ai_history::ai_history_update_access_time,
            commands::ai_history::ai_history_update_file_path,
            commands::ai_history::ai_history_stats,
            // AI 聊天历史命令
            commands::chat::chat_session_list,
            commands::chat::chat_session_create,
            commands::chat::chat_session_delete,
            commands::chat::chat_session_rename,
            commands::chat::chat_session_toggle_favorite,
            commands::chat::chat_session_messages,
            commands::chat::chat_history_search,
            commands::chat::chat_message_save,
            commands::chat::chat_session_clear_messages,
            commands::chat::chat_message_sources_save,
            commands::chat::chat_messages_sources,
            commands::chat::chat_session_index_current,
            commands::chat::kb_chat_stats,
            commands::chat::chat_session_set_last,
            commands::chat::chat_session_get_last,
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
