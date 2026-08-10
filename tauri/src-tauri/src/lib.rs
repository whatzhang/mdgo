// macOS 链接器对齐段警告（tract-onnx 固有，可安全忽略）
#![allow(linker_messages)]
mod commands;
mod core;
mod services;
mod tray;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use commands::llm::TaskRegistry;
use commands::system::SystemMonitorState;
use log::LevelFilter;
use simplelog::{ColorChoice, ConfigBuilder, TerminalMode, TermLogger, WriteLogger};
use tauri::{Emitter, Manager};
use crate::core::{ConfigStore, Indexer, IndexerConfig, WatcherService};
use crate::core::skill::{SkillRegistry, SkillStore};
use crate::core::skill::metrics::SkillMetrics;
use crate::services::prompt::PromptStore;

use std::sync::RwLock;

/// LLM 连接配置（中央化，由前端保存后通过命令更新）
#[derive(Clone, Debug)]
pub struct LlmConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            model: String::new(),
            api_key: String::new(),
        }
    }
}

/// Tauri 托管的应用级共享状态
pub struct AppState {
    pub config_store: Arc<ConfigStore>,
    pub indexer: Arc<Indexer>,
    pub watcher: Arc<WatcherService>,
    /// 按目录路径缓存的 ChatStore 实例（惰性创建）
    pub chat_stores: Mutex<HashMap<String, Arc<services::chat::ChatStore>>>,
    /// 按目录路径缓存的 AiHistoryStore 实例（惰性创建）
    pub ai_history_stores: Mutex<HashMap<String, Arc<services::ai_history::AiHistoryStore>>>,
    /// LLM 连接配置（中央化，由前端保存后通过 kb_update_llm_config 更新）
    pub llm_config: RwLock<LlmConfig>,
    /// LLM 客户端缓存（按配置指纹复用 reqwest 连接池；配置变化后自动重建）
    pub llm_client_cache: tokio::sync::Mutex<Option<(String, services::llm::LLMClient)>>,
    /// Skill 注册表（内存读写分离 + DB 缓存同步）
    pub skill_registry: Arc<SkillRegistry>,
    
    /// Skill 执行指标收集器（环形缓冲 + 聚合统计）
    pub skill_metrics: Arc<SkillMetrics>,
}

impl AppState {
    /// 获取或创建指定目录的 ChatStore
    pub fn get_chat_store(&self, dir_path: &str) -> Result<Arc<services::chat::ChatStore>, String> {
        let mut stores = self.chat_stores.lock().map_err(|e| e.to_string())?;
        if let Some(store) = stores.get(dir_path) {
            return Ok(Arc::clone(store));
        }
        // 聊天数据存储在 {dir_path}/.mdgo/mdgo.db
        let db_dir = std::path::Path::new(dir_path)
            .join(".mdgo");
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
        // AI 历史数据存储在 {dir_path}/.mdgo/mdgo.db
        let db_dir = std::path::Path::new(dir_path)
            .join(".mdgo");
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
    let on_changed: Arc<dyn Fn(&[String]) + Send + Sync> = Arc::new(|_paths: &[String]| {});
    let watcher = Arc::new(WatcherService::new(indexer.clone(), on_error, on_changed));

    // ── 后台预下载 embedding 模型 ──
    // 模型不随安装包内置，首次启动时静默在后台从 HuggingFace 仓库逐文件下载
    // （ModelScope → hf-mirror 镜像 → HuggingFace 主站依次回退），不阻塞 UI；
    // 即使失败也不影响启动，首次使用 embedding 时自动重试。
    std::thread::spawn(|| {
        match crate::core::db::utils::ensure_model_ready() {
            Ok(p) => log::info!("[startup] embedding 模型已就绪: {}", p.display()),
            Err(e) => log::warn!(
                "[startup] embedding 模型后台预下载失败: {}（首次使用时将重试）",
                e
            ),
        }
    });

    // ── 后台预热 Jieba 中文分词器 ──
    // 词典加载约 1-2 秒，在启动时预热，避免首个 BM25 检索/索引请求被阻塞
    std::thread::spawn(|| {
        crate::core::db::bm25::warmup();
    });

    // ── Skill 体系初始化 ──
    // 注册表（内存）+ 指标收集器。
    // Skill 目录监控已合并到 WatcherService 中，由 watcher.start() 时自动启动。
    // 首次打开目录时由 skill_list 懒加载注册表；全局目录不存在时自动创建。
    let skill_registry = Arc::new(SkillRegistry::new());
    let skill_metrics = Arc::new(SkillMetrics::new());
    watcher.set_skill_registry(skill_registry.clone());
    if let Err(e) = std::fs::create_dir_all(SkillStore::global_skills_dir()) {
        log::warn!("[skill] 创建全局技能目录失败: {}", e);
    }

    // ── Prompt 存储初始化 ──
    let prompt_store = PromptStore::new()
        .expect("初始化 PromptStore 失败");

    // 托盘创建是否成功：决定 Windows/Linux 关闭按钮是否拦截为「隐藏到托盘」。
    // 托盘不可用（如 Linux 无托盘宿主）时走系统原生关闭逻辑，避免窗口隐藏后无法恢复。
    let tray_ok = Arc::new(AtomicBool::new(false));
    #[cfg(not(target_os = "macos"))]
    let tray_ok_for_close = tray_ok.clone();

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .manage(SystemMonitorState::new())
        .manage(TaskRegistry::new())
        .manage(prompt_store)
        .manage(AppState {
            config_store,
            indexer,
            watcher,
            chat_stores: Mutex::new(HashMap::new()),
            ai_history_stores: Mutex::new(HashMap::new()),
            llm_config: RwLock::new(LlmConfig::default()),
            llm_client_cache: tokio::sync::Mutex::new(None),
            skill_registry,
            skill_metrics,
        })
        .setup(move |app| {
            // 注入 skill:changed 事件：AppHandle 就绪后替换 watcher 回调
            let handle = app.handle().clone();
            {
                let watcher = app.state::<AppState>().watcher.clone();
                watcher.set_on_skill_changed(Arc::new(move || {
                    let _ = handle.emit("skill:changed", ());
                }));
            }
            // 初始化系统托盘（关闭到托盘 / 左键显示 / 右键菜单：显示、退出）。
            // 托盘是外围功能：创建失败不阻断启动（如 Linux 无托盘宿主），降级为无托盘模式。
            match crate::tray::setup_tray(app) {
                Ok(()) => tray_ok.store(true, Ordering::Relaxed),
                Err(e) => log::warn!("[tray] 系统托盘创建失败，应用将以无托盘模式运行: {}", e),
            }
            // 启动 WebSocket 通信桥（替代 Tauri 事件/命令，Rust 工具闭包 ↔ 前端）
            tauri::async_runtime::spawn(async move {
                match crate::core::bridge::start_server().await {
                    Ok(port) => log::info!("[setup] WebSocket 桥已启动: 127.0.0.1:{}", port),
                    Err(e) => log::error!("[setup] WebSocket 桥启动失败: {}", e),
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::fs::scan_dir_full,
            commands::fs::read_dir,
            commands::fs::read_file,
            commands::fs::read_file_binary,
            commands::fs::write_file,
            commands::fs::write_file_binary,
            commands::fs::delete,
            commands::fs::move_dir_to_trash,
            commands::fs::restore_dir_from_trash,
            commands::fs::clear_trash,
            commands::fs::rename,
            commands::fs::create_dir,
            commands::fs::exists,
            commands::fs::get_file_meta,
            commands::open_url::open_url,
            commands::open_url::show_file_dir_window,
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
            commands::system::set_log_level,
            commands::clipboard::copy_to_clipboard,
            commands::knowledge::kb_index,
            commands::knowledge::kb_index_unindexed,
            commands::knowledge::kb_search_hybrid,
            commands::knowledge::kb_status,
            commands::knowledge::kb_clear,
            commands::knowledge::kb_dashboard_stats,
            commands::knowledge::kb_embedding_info,
            commands::knowledge::kb_get_indexer_config,
            commands::knowledge::kb_update_indexer_config,
            commands::config::kb_config_read,
            commands::config::kb_config_write,
            commands::config::kb_config_delete,
            commands::config::kb_update_llm_config,
            commands::fs_watcher::kb_start_watcher,
            commands::fs_watcher::kb_stop_watcher,
            commands::fs_watcher::kb_set_indexing_enabled,
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
            // LLM 命令
            commands::llm::agent_query,
            commands::llm::kb_llm_query,
            commands::llm::kb_cancel_task,
            // 前端通信桥命令（WebSocket）
            commands::bridge::get_bridge_port,
            // Skill 管理命令
            commands::skill::skill_list,
            commands::skill::skill_get,
            commands::skill::skill_create,
            commands::skill::skill_update,
            commands::skill::skill_delete,
            commands::skill::skill_set_enabled,
            commands::skill::skill_attach,
            commands::skill::skill_detach,
            commands::skill::skill_get_attached,
            commands::skill::skill_metrics,
            // Prompt 模板命令
            commands::prompt::prompt_list,
            commands::prompt::prompt_create,
            commands::prompt::prompt_update,
            commands::prompt::prompt_delete,
        ]);

    // Windows/Linux：拦截主窗口关闭请求，点击右上角关闭按钮 → 隐藏到系统托盘。
    // 仅当托盘创建成功时才拦截；托盘不可用时走系统原生关闭逻辑，避免窗口隐藏后无法恢复。
    // macOS：遵循系统原生逻辑（红 X 关闭窗口，进程保留、Dock 可见），不注册拦截。
    #[cfg(not(target_os = "macos"))]
    let builder = builder.on_window_event(move |window, event| {
        if window.label() == "main" {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if tray_ok_for_close.load(Ordering::Relaxed) {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        }
    });

    builder
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app_handle, _event| {
            // macOS：点击 Dock 图标 → 恢复/重新打开主窗口（系统原生习惯）
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = _event {
                crate::tray::show_main_window(_app_handle);
            }
        });
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

    // 创建文件日志（允许所有级别，由 log::set_max_level_filter 统一控制）
    // Lance 向量库内部 I/O 的 Debug 日志（如读取文件批次细节）过于频繁，
    // 按 target 前缀屏蔽，避免终端与日志文件被刷屏；
    // 不影响应用层 [rag_query]/[llm_trace] 等自有日志。
    let log_config = ConfigBuilder::new()
        .add_filter_ignore_str("lance")
        .add_filter_ignore_str("tantivy")
        .add_filter_ignore_str("datafusion")
        .add_filter_ignore_str("sqlparser")
        .add_filter_ignore_str("tao::platform_imp")
        .build();
    let has_file_logger;
    let file_logger = match std::fs::create_dir_all(&log_dir)
        .and_then(|_| std::fs::File::create(&log_path))
    {
        Ok(file) => {
            has_file_logger = true;
            Some(WriteLogger::new(
                LevelFilter::Trace,
                log_config.clone(),
                file,
            ))
        }
        Err(_) => {
            has_file_logger = false;
            None
        }
    };

    // 创建终端日志（允许所有级别，同上）
    let term_logger = TermLogger::new(
        LevelFilter::Trace,
        log_config,
        TerminalMode::Mixed,
        ColorChoice::Auto,
    );

    let mut loggers: Vec<Box<dyn simplelog::SharedLogger>> = Vec::with_capacity(2);
    loggers.push(term_logger);
    if let Some(file) = file_logger {
        loggers.push(file);
    }

    let _ = simplelog::CombinedLogger::init(loggers);

    // dev: Debug 级别方便调试；release: 只记录 Warn 以上
    if cfg!(debug_assertions) {
        log::set_max_level(LevelFilter::Debug);
    } else {
        log::set_max_level(LevelFilter::Warn);
    }

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
            .map(|p| std::path::PathBuf::from(p).join("com.mdgo").join("logs"))
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
