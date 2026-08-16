//! 日程提醒调度器（Rust 侧：定时循环 + 提醒判定 + 事件推送）。
//!
//! - 每 60 秒 tick 一次，对当前激活的知识库目录执行 `rules::due_reminders`，
//!   到点事件经 `schedule:reminder` 事件推给前端（前端只负责弹窗展示）。
//! - 去重：同一事件的同一触发点（普通/提醒按 `start`，Cron 按当前命中分钟）经
//!   [`store::EventStore::record_reminder`] 幂等记录，只推送一次——避免提前提醒的
//!   长窗口内每 tick 重复推送、以及应用重启后窗口期内重复弹窗。
//! - 活动目录由前端打开日历时经 `schedule_set_active_dir` 设置（依赖倒置：调度器不感知 UI）。
//! - 替换前端 `todoStartLocalScheduler` 的 60s 轮询判定（`_shouldTriggerReminder` 等逻辑全部移除）。

use std::sync::RwLock;

use chrono::{Duration, Local};
use tauri::{AppHandle, Emitter, Manager};

use super::rules;
use super::store::EventStore;

/// 提醒推送日志保留窗口（天）：清理更早的记录，防止日志表无限增长
const REMINDER_LOG_RETENTION_DAYS: i64 = 30;

/// 提醒调度器（AppState 持有；`spawn` 启动后台循环）
pub struct ScheduleScheduler {
    active_dir: RwLock<Option<String>>,
}

impl ScheduleScheduler {
    pub fn new() -> Self {
        Self {
            active_dir: RwLock::new(None),
        }
    }

    /// 设置当前激活的知识库目录（None 停止提醒）
    pub fn set_active_dir(&self, dir: Option<String>) {
        *self.active_dir.write().unwrap() = dir;
    }

    fn active_dir(&self) -> Option<String> {
        self.active_dir.read().unwrap().clone()
    }

    /// 启动后台调度循环（setup 中调用；tokio interval 每 60s tick，首次立即检查）。
    pub fn spawn(self: std::sync::Arc<Self>, app: AppHandle) {
        tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                if let Some(dir) = self.active_dir() {
                    // SQLite 读 + 提醒日志写为阻塞 IO，移入 blocking 线程避免占住 tokio worker
                    let app = app.clone();
                    let dir_clone = dir.clone();
                    if let Err(e) = tokio::task::spawn_blocking(move || Self::tick_blocking(&app, &dir_clone)).await {
                        log::warn!("[schedule] 提醒调度 tick 失败: {}", e);
                    }
                }
            }
        });
    }

    /// 单次 tick（blocking 线程执行）：查当前激活目录的到点提醒，
    /// 经幂等日志去重后推送 `schedule:reminder` 事件（走共享存储锁，与 IPC/工具并发安全）。
    fn tick_blocking(app: &AppHandle, dir: &str) -> Result<(), String> {
        let state = app.state::<crate::AppState>();
        let store = state.schedule_store(dir)?;
        let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let now = Local::now().naive_local();
        let events = guard.list()?;
        let due = rules::due_reminders(&events, now);
        let mut to_emit: Vec<crate::core::schedule::ScheduleEvent> = Vec::new();
        if !due.is_empty() {
            for e in &due {
                // 触发点键：Cron 事件按当前命中分钟（每次命中唯一）；普通/提醒事件按 start（窗口内只推一次）
                let trigger = if !e.cron.trim().is_empty() {
                    now.format("%Y-%m-%dT%H:%M").to_string()
                } else {
                    e.start.clone()
                };
                if guard.record_reminder(&e.id, &trigger)? {
                    to_emit.push(e.clone());
                }
            }
            if !to_emit.is_empty() {
                let _ = app.emit(
                    "schedule:reminder",
                    serde_json::json!({ "dir_path": dir, "events": to_emit }),
                );
            }
        }
        // 顺带清理过期推送日志
        let cutoff = (now - Duration::days(REMINDER_LOG_RETENTION_DAYS))
            .format("%Y-%m-%dT%H:%M")
            .to_string();
        let _ = guard.cleanup_reminders(&cutoff);
        Ok(())
    }
}
