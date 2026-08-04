//! SkillWatcherService：热更新不重启的独立监控服务（单一职责原则）。
//!
//! 只负责一件事：监控 Skill 目录（用户全局 + 用户项目）的文件变更，
//! 800ms 防抖合并后触发注册表重建并通知前端刷新。
//! 解析、Schema 校验、写库等职责均在 `core/skill.rs`，不混入本服务。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, oneshot};

use crate::core::skill::SkillRegistry;
use crate::core::skill::SkillStore;

/// 防抖静默期：同一文件在 800ms 内的多次事件只触发一次重建
const DEBOUNCE_DELAY: Duration = Duration::from_millis(800);

/// 目录变更事件（带时间戳，用于防抖排序）
#[derive(Debug)]
struct SkillDirEvent {
    timestamp: Instant,
}

pub struct SkillWatcherService {
    registry: Arc<SkillRegistry>,
    /// notify watcher 实例（存入结构体避免资源泄漏）
    notify_watcher: Mutex<Option<RecommendedWatcher>>,
    /// 事件通道发送端（notify 原生线程 → 防抖任务）
    event_tx: Mutex<Option<mpsc::UnboundedSender<SkillDirEvent>>>,
    /// 停止信号
    stop_tx: Mutex<Option<oneshot::Sender<()>>>,
    /// 防抖任务句柄
    debounce_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// 当前打开目录（注册表重建的上下文）
    current_dir: RwLock<Option<String>>,
    /// 变更通知回调（前端刷新 `skill:changed`）
    on_changed: Mutex<Arc<dyn Fn() + Send + Sync>>,
    /// 是否活跃
    running: AtomicBool,
}

impl SkillWatcherService {
    pub fn new(registry: Arc<SkillRegistry>, on_changed: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            registry,
            notify_watcher: Mutex::new(None),
            event_tx: Mutex::new(None),
            stop_tx: Mutex::new(None),
            debounce_handle: Mutex::new(None),
            current_dir: RwLock::new(None),
            on_changed: Mutex::new(on_changed),
            running: AtomicBool::new(false),
        }
    }

    /// 替换变更回调（用于在 AppHandle 可用后注入 Tauri 事件）
    pub fn set_on_changed(&self, on_changed: Arc<dyn Fn() + Send + Sync>) {
        *self.on_changed.lock().unwrap_or_else(|e| e.into_inner()) = on_changed;
    }

    /// 设置当前打开目录并（重新）启动监控。
    ///
    /// 幂等：目录未变化时不重启。监控目标：
    /// - 用户全局 Skill 目录（始终）
    /// - 用户项目 Skill 目录（存在时；不存在则监听 `.mdgo` 父目录，创建技能时会触发）
    pub fn set_current_dir(&self, dir_path: &str) {
        {
            let mut cur = self.current_dir.write().unwrap_or_else(|e| e.into_inner());
            if cur.as_deref() == Some(dir_path) {
                return;
            }
            *cur = Some(dir_path.to_string());
        }
        self.restart(dir_path);
    }

    /// 替换当前目录并重启监控
    pub fn restart(&self, dir_path: &str) {
        self.stop_inner();

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
            log::debug!("[skill-watcher] 无 Skill 目录可监听，跳过启动");
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
                    log::error!("[skill-watcher] 监听错误: {}", e);
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
                log::error!("[skill-watcher] 创建监听器失败: {}", e);
                return;
            }
        };
        for path in &watch_paths {
            if let Err(e) = notify_watcher.watch(path, RecursiveMode::Recursive) {
                log::error!("[skill-watcher] 监听目录失败 ({}): {}", path.display(), e);
            }
        }

        // ── 防抖任务（tokio 协程）──
        let registry = self.registry.clone();
        let dir = dir_path.to_string();
        let on_changed = self.on_changed.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let handle = tokio::spawn(async move {
            run_skill_debounce_loop(rx, stop_rx, registry, dir, on_changed).await;
        });

        *self.notify_watcher.lock().unwrap_or_else(|e| e.into_inner()) = Some(notify_watcher);
        *self.event_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        *self.stop_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(stop_tx);
        *self.debounce_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        self.running.store(true, Ordering::Release);

        log::info!(
            "[skill-watcher] 技能目录监听已启动: {:?}",
            watch_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()
        );
    }

    /// 停止监控（释放 OS 资源）
    pub fn stop(&self) {
        self.stop_inner();
    }

    fn stop_inner(&self) {
        *self.notify_watcher.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(tx) = self.stop_tx.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.debounce_handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                if !handle.is_finished() {
                    handle.abort();
                }
            });
        }
        *self.event_tx.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.running.store(false, Ordering::Release);
        log::info!("[skill-watcher] 技能目录监听已停止");
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
}

/// 防抖主循环：800ms 静默期后触发一次注册表重建 + 通知回调
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
                log::info!("[skill-watcher] 防抖循环收到停止信号，退出");
                return;
            }
        }

        // 事件到达后，等待 800ms 静默期（期间新事件刷新计时）
        loop {
            let silent = match pending {
                Some(t) => t + DEBOUNCE_DELAY,
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
                    log::info!("[skill-watcher] 防抖循环收到停止信号，退出");
                    return;
                }
                _ = tokio::time::sleep(wait) => {}
            }
        }

        pending = None;
        log::debug!("[skill-watcher] 检测到 Skill 目录变更，重建注册表");
        match registry.reload(&dir_path) {
            Ok(_) => on_changed(),
            Err(e) => log::error!("[skill-watcher] 注册表重建失败: {}", e),
        }
    }
}
