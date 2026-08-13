// macOS 链接器对齐段警告（tract-onnx 固有，可安全忽略）
#![allow(linker_messages)]
mod commands;
mod core;
mod services;
mod tray;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use commands::llm::TaskRegistry;
use commands::system::SystemMonitorState;
use log::LevelFilter;
use tauri::{Emitter, Manager};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::prelude::*;
use crate::core::subagent::LruResultStore;
use crate::core::{ConfigStore, Indexer, IndexerConfig, WatcherService};
use crate::core::skill::{SkillRegistry, SkillStore};
use crate::core::skill::metrics::SkillMetrics;
use crate::core::approval::policy::DestructiveWritePolicy;
use crate::core::approval::transport::IpcApprovalTransport;
use crate::core::agent::planner::PlanDecision;

use crate::core::approval::{ApprovalGate, ApprovalOutcome};
use crate::services::prompt::PromptStore;

use std::sync::RwLock;
use std::time::Duration;

/// LLM 连接配置（中央化，由前端保存后通过命令更新）
#[derive(Clone, Debug)]
pub struct LlmConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    /// 协议：openai（Chat Completions，默认）/ anthropic（Messages API）
    pub protocol: String,
    /// 规划用小模型（P0-6：可选；None = 主模型）
    pub planner_model: Option<String>,
    /// 摘要用小模型（P0-6：可选；None = 主模型）
    pub summary_model: Option<String>,
    /// 推理努力等级（P2-18：可选；low/medium/high，透传 additional_params）
    pub reasoning_effort: Option<String>,
    /// 最大输出 token（P3：可选；None/0 = 不发送，由服务器/模型默认；>0 时显式发送，
    /// 避免本地模型（LM Studio 等）使用过小的默认输出上限导致回答截断）
    pub max_tokens: Option<u32>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            model: String::new(),
            api_key: String::new(),
            protocol: "openai".to_string(),
            planner_model: None,
            summary_model: None,
            reasoning_effort: None,
            max_tokens: None,
        }
    }
}

/// 模型角色（P0-6 路由：规划/摘要用轻量模型、生成用主模型）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRole {
    /// 主生成模型（`cfg.model`）
    Main,
    /// 规划模型（`cfg.planner_model`；None 时回退 Main）
    Planner,
    /// 摘要模型（`cfg.summary_model`；None 时回退 Main）
    Summary,
}

impl LlmConfig {
    /// 按角色解析模型名（缺省回退主模型）
    pub fn model_for_role(&self, role: ModelRole) -> &str {
        match role {
            ModelRole::Main => &self.model,
            ModelRole::Planner => self.planner_model.as_deref().unwrap_or(&self.model),
            ModelRole::Summary => self.summary_model.as_deref().unwrap_or(&self.model),
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
    /// LLM 客户端缓存（按「endpoint|model|api_key|reasoning_effort」指纹复用；
    /// 配置变化后自动重建，多模型各占一项）
    pub llm_client_cache:
        tokio::sync::Mutex<std::collections::HashMap<String, services::llm::LLMClient>>,
    /// Skill 注册表（内存读写分离 + DB 缓存同步）
    pub skill_registry: Arc<SkillRegistry>,
    
    /// Skill 执行指标收集器（环形缓冲 + 聚合统计）
    pub skill_metrics: Arc<SkillMetrics>,
    /// 工具审批门（破坏性操作确认）；None = 未启用（保持原行为）
    pub approval_gate: Option<Arc<ApprovalGate>>,
    /// 审批挂起表（IPC 审批通道与 approval_respond 共享，单一数据源）
    pub approval_pending:
        Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<ApprovalOutcome>>>>,
    /// 规划确认挂起表（plan:request 与 plan_respond 共享，单一数据源）
    pub plan_pending:
        Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<PlanDecision>>>>,
    /// 子代理完整输出存储（LRU 有界：最多保留 16 条，按最近访问淘汰）
    pub subagent_results: Arc<LruResultStore>,
    /// 跨会话长期记忆存储（全局用户数据目录）
    pub memory_store: Arc<crate::core::memory::MemoryStore>,
    /// 记忆向量索引（O1：内存惰性增量，embedding 本地 BGE 模型）
    pub memory_vectors: Arc<crate::core::memory::vector::MemoryVectorIndex>,
    /// MCP 服务器注册表（v2：配置 + 生命周期 + 工具清单）
    pub mcp: Arc<crate::core::mcp::McpRegistry>,
}

impl AppState {
    /// 获取或创建 LLM 客户端（按配置指纹缓存，复用 reqwest 连接池；配置热更新后自动重建）。
    ///
    /// 供 commands 层与工具闭包（子代理深度调研）共用，避免 core 层反向依赖 commands 层。
    pub async fn llm_client_for(
        &self,
        endpoint: &str,
        model: &str,
        api_key: &str,
    ) -> Result<services::llm::LLMClient, String> {
        self.llm_client_for_cfg(endpoint, model, api_key, None).await
    }

    /// 按模型角色路由客户端（P0-6）：规划/摘要可用独立轻量模型，缺省回退主模型；
    /// reasoning_effort（P2-18）作为客户端属性参与指纹缓存。
    pub async fn llm_client_for_role(
        &self,
        cfg: &LlmConfig,
        role: ModelRole,
    ) -> Result<services::llm::LLMClient, String> {
        let model = cfg.model_for_role(role).to_string();
        self.llm_client_for_cfg(&cfg.endpoint, &model, &cfg.api_key, cfg.reasoning_effort.as_deref())
            .await
    }

    /// 带 reasoning_effort 的客户端工厂（供 commands 层主对话/聊天流式链路使用；
    /// effort 参与指纹缓存，配置热更新后自动重建）。
    pub(crate) async fn llm_client_for_cfg(
        &self,
        endpoint: &str,
        model: &str,
        api_key: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<services::llm::LLMClient, String> {
        let fingerprint = format!("{}|{}|{}|{}", endpoint, model, api_key, reasoning_effort.unwrap_or(""));
        let mut cache = self.llm_client_cache.lock().await;
        if let Some(client) = cache.get(&fingerprint) {
            return Ok(client.clone());
        }
        let client = services::llm::LLMClient::new(
            endpoint.to_string(),
            model.to_string(),
            api_key.to_string(),
            reasoning_effort.map(|s| s.to_string()),
        )?;
        // 容量治理：多模型缓存最多保留 8 项（超出清空，客户端重建成本低）
        const MAX_CACHED_CLIENTS: usize = 8;
        if cache.len() >= MAX_CACHED_CLIENTS {
            cache.clear();
        }
        cache.insert(fingerprint, client.clone());
        Ok(client)
    }

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
        .setup(move |app| {
            // ── 组装工具审批门（破坏性操作确认，需 AppHandle 以走前端桥）──
            // 策略：edit / delete 需用户确认；通道：WebSocket 桥调前端弹窗；
            // 超时 60s，超时/通道异常默认拒绝（fail-closed）。
            let mut policies: Vec<Box<dyn crate::core::approval::ApprovalPolicy>> =
                vec![Box::new(DestructiveWritePolicy::new(true))];
            // P2-19：配置驱动审批策略（%APPDATA%/com.mdgo/approval.yaml）。
            // allow/deny 规则短路默认策略（如"只读模式"= deny edit/delete）；
            // 配置缺失/解析失败保留默认（edit/delete 需确认），不阻断启动。
            match crate::core::approval::policy::load_approval_rules(
                &crate::core::approval::policy::default_rules_path(),
            ) {
                Ok(rules) if !rules.is_empty() => {
                    policies.insert(
                        0,
                        Box::new(crate::core::approval::policy::ConfigApprovalPolicy::new(rules)),
                    );
                    log::info!("[approval] 已加载配置审批策略（{} 条规则）", policies.len() - 1);
                }
                Ok(_) => {}
                Err(e) => log::warn!("[approval] 加载审批策略配置失败，使用默认策略: {}", e),
            }
            // 审批挂起表：IPC 通道与 approval_respond 共享（依赖注入，非全局静态）
            let approval_pending: Arc<
                Mutex<HashMap<String, tokio::sync::oneshot::Sender<ApprovalOutcome>>>,
            > = Arc::new(Mutex::new(HashMap::new()));
            let approval_gate = Arc::new(ApprovalGate::new(
                policies,
                Box::new(IpcApprovalTransport::new(
                    app.handle().clone(),
                    approval_pending.clone(),
                )),
                Duration::from_secs(60),
            ));
            app.manage(AppState {
                config_store,
                indexer,
                watcher,
                chat_stores: Mutex::new(HashMap::new()),
                ai_history_stores: Mutex::new(HashMap::new()),
                llm_config: RwLock::new(LlmConfig::default()),
                llm_client_cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
                skill_registry,
                skill_metrics,
                approval_gate: Some(approval_gate),
                approval_pending,
                plan_pending: Arc::new(Mutex::new(HashMap::new())),
                subagent_results: Arc::new(LruResultStore::new(16)),
                memory_store: Arc::new(
                    crate::core::memory::MemoryStore::new()
                        .expect("初始化 MemoryStore 失败"),
                ),
                memory_vectors: Arc::new(crate::core::memory::vector::MemoryVectorIndex::new(
                    Arc::new(crate::core::memory::vector::LocalEmbedder),
                )),
                mcp: Arc::new(crate::core::mcp::McpRegistry::new()),
            });

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
            commands::config::kb_save_setting,
            commands::config::kb_load_setting,
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
            commands::chat::chat_fork,
            // LLM 命令
            commands::llm::agent_query,
            commands::llm::kb_llm_query,
            commands::llm::kb_cancel_task,
            commands::approval::approval_respond,

            commands::plan::plan_respond,
            // 前端通信桥命令（WebSocket）
            commands::bridge::get_bridge_port,
            // Skill 管理命令
            commands::skill::skill_allowed_tools,
            commands::skill::skill_list,
            commands::skill::skill_get,
            commands::skill::skill_create,
            commands::skill::skill_update,
            commands::skill::skill_delete,
            commands::skill::skill_set_enabled,
            commands::skill::skill_attach,
            commands::skill::skill_set_mount_mode,
            commands::skill::skill_detach,
            commands::skill::skill_get_attached,
            commands::skill::skill_metrics,
            // Prompt 模板命令
            commands::prompt::prompt_list,
            commands::prompt::prompt_create,
            commands::prompt::prompt_update,
            commands::prompt::prompt_delete,
            // MCP 管理命令（v2：独立管理页）
            commands::mcp::mcp_list,
            commands::mcp::mcp_get,
            commands::mcp::mcp_logs,
            commands::mcp::mcp_upsert,
            commands::mcp::mcp_delete,
            commands::mcp::mcp_connect,
            commands::mcp::mcp_disconnect,
            commands::mcp::mcp_restart,
            commands::mcp::mcp_test,
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

/// tracing 侧日志过滤句柄（热重载；set_log_level 通过它更新默认级别，
/// 与 log::set_max_level 保持同步，rig span 与 log:: 桥接事件同受控制）。
pub static LOG_LEVEL_HANDLE: OnceLock<
    tracing_subscriber::reload::Handle<
        tracing_subscriber::filter::Targets,
        tracing_subscriber::registry::Registry,
    >,
> = OnceLock::new();

/// 构造日志过滤 Targets（init_logging 与 set_log_level 共用，单一来源）。
///
/// 级别规范化：
/// - 自己代码（`mdgo_lib` 前缀）按 `level`（默认 INFO；`set_log_level` 热切换可开 DEBUG/TRACE）
/// - 框架/第三方库统一 WARN（其 DEBUG/INFO 帧、连接日志为噪音，WARN/ERROR 保留）
/// - 高频噪音 target（lance/tantivy/datafusion/sqlparser/tao/ort）直接 OFF
pub fn log_filter_targets(
    level: tracing::level_filters::LevelFilter,
) -> tracing_subscriber::filter::Targets {
    // 级别策略（规范化）：自己代码（mdgo_lib）按 `level`（默认 INFO），
    // 框架/第三方库统一 WARN；高频噪音 target 直接 OFF。
    // `level` 为调用方传入的全局级别（init 默认 INFO；set_log_level 热切换控制 mdgo_lib）。
    Targets::new()
        // 自己代码：按 level（默认 INFO，热切换可开 DEBUG/TRACE）
        .with_target("mdgo_lib", level)
        .with_default(tracing::level_filters::LevelFilter::WARN)
}

/// 初始化日志系统：基于 tracing 的统一输出（文件 + 终端双输出）。
///
/// 日志文件路径：
/// - macOS/Linux: `~/Library/Logs/mdgo/mdgo.log` / `~/.cache/mdgo/logs/mdgo.log`
/// - Windows: `%APPDATA%/mdgo/logs/mdgo.log`
///
/// **注意**：日志目录不会在项目目录内，避免触发 Tauri 开发服务器的文件监听重建循环。
///
/// 与旧 simplelog 实现的行为对齐：
/// - 文件 + 终端双输出，文件创建失败降级为仅终端（sink）
/// - 按 target 前缀屏蔽高频第三方日志（lance/tantivy/datafusion/sqlparser/tao），
///   网络库（h2/hyper/reqwest/tower/want/mio）仅保留 INFO 及以上（DEBUG 帧/连接日志为噪音）
/// - 级别上限：dev=INFO，release=WARN
///
/// 新增收益：rig 内部的 tracing span/event 进入同一输出（此前 100% 丢失）；
/// 现有 `log::` 宏经 `tracing_log::LogTracer` 桥接继续工作。
fn init_logging() {
    let log_dir = log_dir_global();
    let log_path = log_dir.join("mdgo.log");

    // log:: → tracing 桥接：现有 log:: 宏进入统一 subscriber（target 保留）
    let _ = tracing_log::LogTracer::init();

    // 级别上限 + 高频第三方 target 屏蔽（行为与原 ignore 列表对齐）
    let level = if cfg!(debug_assertions) {
        tracing::level_filters::LevelFilter::INFO
    } else {
        tracing::level_filters::LevelFilter::WARN
    };
    let filter = crate::log_filter_targets(level);
    let (filter_layer, filter_handle) = tracing_subscriber::reload::Layer::new(filter);
    let _ = LOG_LEVEL_HANDLE.set(filter_handle);

    // 终端输出（彩色；含线程名/ID 便于多线程链路排查）
    let term_layer = tracing_subscriber::fmt::layer()
        .with_ansi(true)
        .with_thread_names(true)
        .with_thread_ids(true)
        .with_writer(std::io::stdout);

    // 文件输出（Mutex<Box<dyn Write>> 实现 MakeWriter；创建失败降级为 sink，仅终端输出）
    let mut has_file_logger = false;
    let file_writer: Mutex<Box<dyn std::io::Write + Send + Sync>> = match std::fs::create_dir_all(&log_dir)
        .and_then(|_| std::fs::File::create(&log_path))
    {
        Ok(file) => {
            has_file_logger = true;
            Mutex::new(Box::new(file))
        }
        Err(_) => Mutex::new(Box::new(std::io::sink())),
    };
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_thread_names(true)
        .with_thread_ids(true)
        .with_writer(file_writer);

    let subscriber = tracing_subscriber::registry()
        .with(filter_layer)
        .with(term_layer)
        .with(file_layer);
    let _ = tracing::subscriber::set_global_default(subscriber);

    // 仅约束 log:: 宏侧（与旧实现一致）；tracing 侧由 Targets 过滤
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
/// - Windows: `%APPDATA%/com.mdgo/logs/`
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
