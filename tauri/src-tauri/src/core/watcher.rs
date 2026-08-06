use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, oneshot};

use crate::core::agent::tools::invalidate_file_list_cache;
use crate::core::db::utils;
use crate::core::indexer::Indexer;
use crate::core::skill::{SkillRegistry, SkillStore};

/// 文件变更事件（带时间戳，用于防抖排序）
struct FileEvent {
    rel_path: String,
    is_remove: bool,
    timestamp: Instant,
}

/// 防抖循环内部控制消息
enum DebounceCmd {
    /// 清空 pending 事件表（index_all 完成后调用，避免处理过期事件）
    ClearPending,
}

/// Skill 目录变更事件（带时间戳，用于防抖排序）
#[derive(Debug)]
struct SkillDirEvent {
    timestamp: Instant,
}

/// Skill 防抖静默期
const SKILL_DEBOUNCE_DELAY: Duration = Duration::from_millis(800);

/// 文件监听服务（带防抖合并 + 暂停机制 + Skill 热更新监控）
///
/// 设计要点：
/// 1. 单一 watcher 实例，整个应用生命周期内只启动一次
/// 2. 防抖合并：同一文件在 800ms 内的多次事件只触发最后一次处理
/// 3. 原生通知线程 → mpsc Channel → Tokio 异步处理（避免 tokio::spawn panic）
/// 4. 支持运行时暂停/恢复（index_all 期间暂停增量处理，避免竞态）
/// 5. 支持运行时停止/重启
/// 6. notify watcher 存入结构体，restart 时正确 drop 旧实例
/// 7. resume 时自动清空暂停期间收集的过期事件（index_all 已全量重建）
/// 8. 内置 Skill 目录监控（SKILL.md 变更 → 注册表重建 + 前端通知）
pub struct WatcherService {
    indexer: Arc<Indexer>,
    /// notify watcher 实例（存入结构体避免 mem::forget 泄漏）
    notify_watcher: Mutex<Option<RecommendedWatcher>>,
    /// 事件通道发送端（notify 原生线程 → debounce actor）
    event_tx: Mutex<Option<mpsc::UnboundedSender<FileEvent>>>,
    /// 控制通道发送端（用于清空 pending 等命令）
    cmd_tx: Mutex<Option<mpsc::UnboundedSender<DebounceCmd>>>,
    /// 停止信号
    stop_tx: Mutex<Option<oneshot::Sender<()>>>,
    /// 防抖循环的 JoinHandle（重启时用于确保旧任务退出）
    debounce_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// 暂停信号（true = 暂停增量处理，index_all 期间使用）
    paused: Arc<AtomicBool>,
    /// 索引开关（true = 对文件变更执行增量索引；false = 仅监听并失效缓存，不索引）
    indexing_enabled: Arc<AtomicBool>,
    /// 当前监听目录
    watch_dir: Mutex<Option<String>>,
    /// 当前是否活跃
    running: AtomicBool,
    /// 增量索引错误回调（用于通知前端）
    on_error: Mutex<Arc<dyn Fn(&str) + Send + Sync>>,
    /// 增量索引成功回调（通知前端刷新面板）
    on_changed: Mutex<Arc<dyn Fn() + Send + Sync>>,

    // ── Skill 监控 ──
    /// Skill 注册表（通过 set_skill_registry 注入）
    skill_registry: Mutex<Option<Arc<SkillRegistry>>>,
    /// Skill 事件通道发送端（notify 原生线程 → skill debounce loop）
    skill_event_tx: Mutex<Option<mpsc::UnboundedSender<SkillDirEvent>>>,
    /// Skill 停止信号
    skill_stop_tx: Mutex<Option<oneshot::Sender<()>>>,
    /// Skill 防抖任务句柄
    skill_debounce_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Skill 当前监听目录（用于注册表重建上下文）
    skill_cur_dir: RwLock<Option<String>>,
    /// Skill 是否活跃
    skill_running: AtomicBool,
    /// Skill 变更通知回调（前端刷新 `skill:changed`）
    on_skill_changed: Mutex<Arc<dyn Fn() + Send + Sync>>,
}

impl WatcherService {
    pub fn new(
        indexer: Arc<Indexer>,
        on_error: Arc<dyn Fn(&str) + Send + Sync>,
        on_changed: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            indexer,
            notify_watcher: Mutex::new(None),
            event_tx: Mutex::new(None),
            cmd_tx: Mutex::new(None),
            stop_tx: Mutex::new(None),
            debounce_handle: Mutex::new(None),
            paused: Arc::new(AtomicBool::new(false)),
            indexing_enabled: Arc::new(AtomicBool::new(false)),
            watch_dir: Mutex::new(None),
            running: AtomicBool::new(false),
            on_error: Mutex::new(on_error),
            on_changed: Mutex::new(on_changed),
            skill_registry: Mutex::new(None),
            skill_event_tx: Mutex::new(None),
            skill_stop_tx: Mutex::new(None),
            skill_debounce_handle: Mutex::new(None),
            skill_cur_dir: RwLock::new(None),
            skill_running: AtomicBool::new(false),
            on_skill_changed: Mutex::new(Arc::new(|| {})),
        }
    }

    /// 当前正在监听的目录（watcher 未启动时为 None）
    pub fn get_watch_dir(&self) -> Option<String> {
        self.watch_dir
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 注入 Skill 注册表（构造后由 AppState 调用）
    pub fn set_skill_registry(&self, registry: Arc<SkillRegistry>) {
        *self.skill_registry.lock().unwrap_or_else(|e| e.into_inner()) = Some(registry);
    }

    /// 替换 Skill 变更回调（AppHandle 可用后注入 Tauri 事件）
    pub fn set_on_skill_changed(&self, on_skill_changed: Arc<dyn Fn() + Send + Sync>) {
        *self.on_skill_changed.lock().unwrap_or_else(|e| e.into_inner()) = on_skill_changed;
    }

    /// 启动文件监听
    ///
    /// Idempotent：如已启动且在监听同一目录，直接返回 Ok。
    /// 若目录变化则重新启动。
    pub fn start(&self, dir_path: &str, dir_blacklist: &[String], file_blacklist: &[String]) -> Result<(), String> {
        if self.running.load(Ordering::Acquire) {
            let current_dir = self.watch_dir.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ref d) = *current_dir {
                if d == dir_path {
                    return Ok(());
                }
            }
            self.stop_inner();
        }

        // 切换目录时索引开关默认关闭，需要用户手动开启
        self.indexing_enabled.store(false, Ordering::Release);

        let ignore = crate::core::db::utils::IgnoreMatcher::new(dir_blacklist, file_blacklist);
        let watch_dir = dir_path.to_string();
        let path = Path::new(&watch_dir).to_path_buf();
        if !path.exists() || !path.is_dir() {
            return Err(format!("目录不存在: {}", dir_path));
        }

        // 创建事件通道
        let (tx, rx) = mpsc::unbounded_channel::<FileEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<DebounceCmd>();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();

        let indexer = self.indexer.clone();
        let paused = self.paused.clone();

        // ── notify 事件处理器（在 notify 原生线程中执行）──
        let notify_tx = tx.clone();
        let event_handler = move |event: Result<notify::Event, notify::Error>| {
            let event = match event {
                Ok(e) => e,
                Err(e) => {
                    log::error!("[watcher] 文件监听错误: {}", e);
                    return;
                }
            };

            let is_modify = matches!(
                event.kind,
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
            );
            if !is_modify {
                return;
            }

            let is_remove = matches!(event.kind, EventKind::Remove(_));

            for event_path in &event.paths {
                let abs_path = event_path.to_string_lossy().to_string();

                // 排除 .mdgo 数据目录与垃圾箱目录（垃圾桶内文件不索引、不监听）
                if abs_path.contains(".mdgo")
                    || abs_path
                        .split(['\\', '/'])
                        .any(|c| c == utils::TRASH_DIR_NAME)
                {
                    continue;
                }

                // 跳过目录事件（仅处理文件）
                if event_path.is_dir() {
                    continue;
                }

                let rel = match abs_path.strip_prefix(&watch_dir) {
                    Some(s) => {
                        let s = s.trim_start_matches('\\').trim_start_matches('/');
                        if s.is_empty() { continue; }
                        s.replace('\\', "/")
                    }
                    None => continue,
                };

                // 检查黑白名单
                let name = event_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !ignore.is_kb_file_allowed(name, &rel) {
                    continue;
                }

                let _ = notify_tx.send(FileEvent {
                    rel_path: rel,
                    is_remove,
                    timestamp: Instant::now(),
                });
            }
        };

        // 创建 notify watcher
        let mut notify_watcher = notify::recommended_watcher(event_handler)
            .map_err(|e| format!("创建文件监听器失败: {}", e))?;

        notify_watcher
            .watch(&path, RecursiveMode::Recursive)
            .map_err(|e| format!("开始监听目录失败: {}", e))?;

        // ── 后台防抖任务 ──
        let debounce_indexer = indexer.clone();
        let debounce_dir = dir_path.to_string();
        let debounce_on_error = self.on_error.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let debounce_on_changed = self.on_changed.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let debounce_indexing_enabled = self.indexing_enabled.clone();
        let handle = tokio::spawn(async move {
            run_debounce_loop(rx, cmd_rx, stop_rx, debounce_indexer, &debounce_dir, paused, debounce_indexing_enabled, debounce_on_error, debounce_on_changed).await;
        });
        *self.debounce_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);

        // 保存状态（先 drop 旧的 notify watcher）
        *self.notify_watcher.lock().unwrap_or_else(|e| e.into_inner()) = Some(notify_watcher);
        *self.event_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        *self.cmd_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(cmd_tx);
        *self.stop_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(stop_tx);
        *self.watch_dir.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir_path.to_string());
        self.running.store(true, Ordering::Release);

        // 启动后失效文件列表缓存，确保下次 list_files 拿到最新数据
        invalidate_file_list_cache(dir_path);

        // ── 同步启动 Skill 监控 ──
        self.ensure_skill_watching(dir_path);

        log::info!("[watcher] 文件监听已启动: {}", dir_path);
        Ok(())
    }

    /// 停止文件监听（释放 OS 资源，含 Skill 监控）
    pub fn stop(&self) {
        self.stop_inner();
    }

    fn stop_inner(&self) {
        // drop notify watcher（释放 OS 监听句柄）
        *self.notify_watcher.lock().unwrap_or_else(|e| e.into_inner()) = None;

        // 通知防抖循环退出
        if let Some(tx) = self.stop_tx.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(());
        }

        // 等待防抖任务优雅退出，5 秒超时后强制中止
        if let Some(handle) = self.debounce_handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if !handle.is_finished() {
                    handle.abort();
                    log::warn!("[watcher] 防抖任务超时，强制中止");
                }
            });
        }

        // 清空通道
        *self.event_tx.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.cmd_tx.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.watch_dir.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.running.store(false, Ordering::Release);

        // ── 同步停止 Skill 监控 ──
        self.stop_skill_inner();

        log::info!("[watcher] 文件监听已停止");
    }

    /// 暂停增量处理（index_all 期间调用，避免并发写 DB）
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
        log::info!("[watcher] 增量处理已暂停");
    }

    /// 恢复增量处理（index_all 完成后调用）
    /// 同时清空暂停期间收集的过期事件（index_all 已全量重建，无需重复处理）
    ///
    /// 注意：先发送 ClearPending 再设置 paused=false，避免 tick 在清空前
    /// 误处理暂停期间收集的过期事件。
    pub fn resume(&self) {
        // 先清空 pending 事件，再恢复处理
        if let Some(ref cmd_tx) = *self.cmd_tx.lock().unwrap_or_else(|e| e.into_inner()) {
            let _ = cmd_tx.send(DebounceCmd::ClearPending);
        }
        self.paused.store(false, Ordering::Release);
        log::info!("[watcher] 增量处理已恢复（已清空过期事件）");
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// 替换错误回调（用于在 AppHandle 可用后注入 Tauri 事件）
    pub fn set_on_error(&self, on_error: Arc<dyn Fn(&str) + Send + Sync>) {
        *self.on_error.lock().unwrap_or_else(|e| e.into_inner()) = on_error;
    }

    /// 替换变更通知回调（用于在 AppHandle 可用后注入 Tauri 事件）
    pub fn set_on_changed(&self, on_changed: Arc<dyn Fn() + Send + Sync>) {
        *self.on_changed.lock().unwrap_or_else(|e| e.into_inner()) = on_changed;
    }

    /// 设置索引开关（true = 对文件变更执行增量索引；false = 仅监听并失效缓存）
    ///
    /// 即使索引关闭，watcher 仍持续运行并失效文件列表缓存，保证 list_files 数据新鲜。
    pub fn set_indexing_enabled(&self, enabled: bool) {
        self.indexing_enabled.store(enabled, Ordering::Release);
        log::info!("[watcher] 索引开关: {}", if enabled { "ON" } else { "OFF" });
    }

    /// 获取当前索引开关状态
    pub fn is_indexing_enabled(&self) -> bool {
        self.indexing_enabled.load(Ordering::Acquire)
    }

    // ─────────────── Skill 监控 ───────────────

    /// 确保 Skill 目录监控已启动（幂等）。
    ///
    /// 监控目标：
    /// - 用户全局 Skill 目录（始终）
    /// - 用户项目 Skill 目录（存在时；不存在则监听 `.mdgo` 父目录，创建技能时会触发）
    pub fn ensure_skill_watching(&self, dir_path: &str) {
        {
            let mut cur = self.skill_cur_dir.write().unwrap_or_else(|e| e.into_inner());
            if cur.as_deref() == Some(dir_path) && self.skill_running.load(Ordering::Acquire) {
                return;
            }
            *cur = Some(dir_path.to_string());
        }

        self.stop_skill_inner();

        let registry = match self.skill_registry.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            Some(r) => r,
            None => {
                log::debug!("[watcher] Skill 注册表未注入，跳过 Skill 监控");
                return;
            }
        };

        let mut watch_paths: Vec<std::path::PathBuf> = Vec::new();
        let global_dir = SkillStore::global_skills_dir();
        if global_dir.is_dir() {
            watch_paths.push(global_dir);
        }
        let project_dir = SkillStore::project_skills_dir(dir_path);
        if project_dir.is_dir() {
            watch_paths.push(project_dir);
        } else if let Some(parent) = project_dir.parent() {
            if parent.is_dir() {
                watch_paths.push(parent.to_path_buf());
            }
        }

        if watch_paths.is_empty() {
            log::debug!("[watcher] 无 Skill 目录可监听，跳过启动");
            return;
        }

        let (tx, rx) = mpsc::unbounded_channel::<SkillDirEvent>();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();

        // ── notify 事件处理器（原生线程）──
        let notify_tx = tx.clone();
        let event_handler = move |event: Result<notify::Event, notify::Error>| {
            let event = match event {
                Ok(e) => e,
                Err(e) => {
                    log::error!("[watcher] Skill 监听错误: {}", e);
                    return;
                }
            };
            let is_relevant = matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            );
            if !is_relevant {
                return;
            }
            // 只关心 SKILL.md 文件事件
            let has_skill_md = event
                .paths
                .iter()
                .any(|p| p.file_name().map(|n| n == "SKILL.md").unwrap_or(false));
            if has_skill_md {
                let _ = notify_tx.send(SkillDirEvent {
                    timestamp: Instant::now(),
                });
            }
        };

        let mut notify_watcher = match notify::recommended_watcher(event_handler) {
            Ok(w) => w,
            Err(e) => {
                log::error!("[watcher] 创建 Skill 监听器失败: {}", e);
                return;
            }
        };
        for path in &watch_paths {
            if let Err(e) = notify_watcher.watch(path, RecursiveMode::Recursive) {
                log::error!("[watcher] Skill 监听目录失败 ({}): {}", path.display(), e);
            }
        }

        // ── Skill 防抖任务 ──
        let dir = dir_path.to_string();
        let on_skill_changed = self.on_skill_changed.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let handle = tokio::spawn(async move {
            run_skill_debounce_loop(rx, stop_rx, registry, dir, on_skill_changed).await;
        });

        *self.skill_event_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        *self.skill_stop_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(stop_tx);
        *self.skill_debounce_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        self.skill_running.store(true, Ordering::Release);

        log::info!(
            "[watcher] Skill 目录监听已启动: {:?}",
            watch_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()
        );
    }

    fn stop_skill_inner(&self) {
        if let Some(tx) = self.skill_stop_tx.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.skill_debounce_handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                if !handle.is_finished() {
                    handle.abort();
                }
            });
        }
        *self.skill_event_tx.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.skill_running.store(false, Ordering::Release);
        log::info!("[watcher] Skill 目录监听已停止");
    }
}

/// 防抖处理主循环
///
/// 策略：
/// - 同一文件在 1200000ms（20分钟） 内收到多个事件时，只保留最后一次，文件变动事件 → 写入 pending 表（更新/覆盖同路径旧事件） 每 30000ms tick 触发 → 扫描 pending 表
/// - 对于 Modify/Remove 冲突（同路径），比较时间戳，最新的优先
/// - 暂停期间事件继续收集但延迟处理，恢复时收到 ClearPending 命令清空过期事件
/// - 通过 stop_rx 实现优雅退出
async fn run_debounce_loop(
    mut rx: mpsc::UnboundedReceiver<FileEvent>,
    mut cmd_rx: mpsc::UnboundedReceiver<DebounceCmd>,
    mut stop_rx: oneshot::Receiver<()>,
    indexer: Arc<Indexer>,
    dir_path: &str,
    paused: Arc<AtomicBool>,
    indexing_enabled: Arc<AtomicBool>,
    on_error: Arc<dyn Fn(&str) + Send + Sync>,
    on_changed: Arc<dyn Fn() + Send + Sync>,
) {
    let dir_path = dir_path.to_string();
    let debounce_delay = Duration::from_millis(1200000);
    let check_interval = Duration::from_millis(30000);

    // 待处理事件表：rel_path → (最后一次时间戳, 是否为 Remove)
    let mut pending: HashMap<String, (Instant, bool)> = HashMap::new();

    loop {
        let tick = tokio::time::sleep(check_interval);
        tokio::pin!(tick);

        tokio::select! {
            Some(event) = rx.recv() => {
                match pending.get(&event.rel_path) {
                    Some((existing_time, _)) if *existing_time > event.timestamp => {
                        // 旧事件，保留现有的
                    }
                    _ => {
                        pending.insert(event.rel_path, (event.timestamp, event.is_remove));
                    }
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    DebounceCmd::ClearPending => {
                        let cleared = pending.len();
                        pending.clear();
                        log::info!("[watcher] 清空 {} 条过期事件（index_all 已全量重建）", cleared);
                        // index_all 期间文件可能已变更，失效缓存确保下次 list_files 拿到最新数据
                        invalidate_file_list_cache(&dir_path);
                    }
                }
            }
            _ = &mut tick => {
                // 暂停期间只收集事件，不处理
                if paused.load(Ordering::Acquire) {
                    continue;
                }

                // 定时检查：找出已达到静默期的事件
                let now = Instant::now();
                let mut to_process: Vec<(String, bool)> = Vec::new();

                pending.retain(|path, (time, is_remove)| {
                    if now.duration_since(*time) >= debounce_delay {
                        to_process.push((path.clone(), *is_remove));
                        false
                    } else {
                        true
                    }
                });

                // 处理到期事件：索引开关关闭时仅跳过索引，不跳过缓存失效
                for (path, is_remove) in &to_process {
                    if !indexing_enabled.load(Ordering::Acquire) {
                        continue;
                    }
                    if *is_remove {
                        log::debug!("[watcher] 处理删除: {}", path);
                        if let Err(e) = indexer.remove_file(&dir_path, path).await {
                            log::error!("[watcher] 删除处理失败 ({}): {}", path, e);
                            on_error(&format!("增量删除失败 ({}): {}", path, e));
                        } else {
                            on_changed();
                        }
                    } else {
                        let abs_path = format!("{}/{}", dir_path, path);
                        if !Path::new(&abs_path).exists() {
                            // 文件已不存在，视作删除
                            log::debug!("[watcher] 文件已不存在，转删除: {}", path);
                            if let Err(e) = indexer.remove_file(&dir_path, path).await {
                                log::error!("[watcher] 删除处理失败 ({}): {}", path, e);
                                on_error(&format!("增量删除失败 ({}): {}", path, e));
                            } else {
                                on_changed();
                            }
                            continue;
                        }
                        log::debug!("[watcher] 处理修改: {}", path);
                        if indexer.is_reindex_in_progress() {
                            log::debug!("[watcher] 全量索引进行中，跳过增量索引: {}", path);
                            continue;
                        }
                        if let Err(e) = indexer.index_file(&dir_path, path, &abs_path).await {
                            log::error!("[watcher] 索引处理失败 ({}): {}", path, e);
                            on_error(&format!("增量索引失败 ({}): {}", path, e));
                        } else {
                            on_changed();
                        }
                    }
                }

                // 有文件变更时失效文件列表缓存（无论索引开关是否打开）
                if !to_process.is_empty() {
                    invalidate_file_list_cache(&dir_path);
                }
            }
            _ = &mut stop_rx => {
                log::info!("[watcher] 防抖循环收到停止信号，退出");
                // 停止时丢弃未处理事件（避免在关闭时还做 IO）
                return;
            }
        }
    }
}

/// Skill 防抖主循环：800ms 静默期后触发一次注册表重建 + 通知回调
async fn run_skill_debounce_loop(
    mut rx: mpsc::UnboundedReceiver<SkillDirEvent>,
    mut stop_rx: oneshot::Receiver<()>,
    registry: Arc<SkillRegistry>,
    dir_path: String,
    on_changed: Arc<dyn Fn() + Send + Sync>,
) {
    let mut pending: Option<Instant> = None;

    loop {
        // 等待下一个事件或停止信号
        tokio::select! {
            Some(event) = rx.recv() => {
                pending = Some(match pending {
                    Some(prev) if prev > event.timestamp => prev,
                    _ => event.timestamp,
                });
            }
            _ = &mut stop_rx => {
                log::info!("[watcher] Skill 防抖循环收到停止信号，退出");
                return;
            }
        }

        // 事件到达后，等待 800ms 静默期（期间新事件刷新计时）
        loop {
            let silent = match pending {
                Some(t) => t + SKILL_DEBOUNCE_DELAY,
                None => break,
            };
            let now = Instant::now();
            if now >= silent {
                break;
            }
            let wait = silent.saturating_duration_since(now);
            tokio::select! {
                Some(event) = rx.recv() => {
                    pending = Some(if let Some(prev) = pending {
                        if prev > event.timestamp { prev } else { event.timestamp }
                    } else { event.timestamp });
                }
                _ = &mut stop_rx => {
                    log::info!("[watcher] Skill 防抖循环收到停止信号，退出");
                    return;
                }
                _ = tokio::time::sleep(wait) => {}
            }
        }

        pending = None;
        log::debug!("[watcher] 检测到 Skill 目录变更，重建注册表");
        match registry.reload(&dir_path) {
            Ok(_) => on_changed(),
            Err(e) => log::error!("[watcher] Skill 注册表重建失败: {}", e),
        }
    }
}