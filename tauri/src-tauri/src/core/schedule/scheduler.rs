//! 日程提醒调度器（Rust 侧：定时循环 + 提醒判定 + 事件推送）。
//!
//! - 每 60 秒 tick 一次，对当前激活的知识库目录执行 `rules::due_reminders`，
//!   到点事件经 `schedule:reminder` 事件推给前端（前端只负责弹窗展示）。
//! - 活动目录由前端打开日历时经 `schedule_set_active_dir` 设置（依赖倒置：调度器不感知 UI）。
//! - 替换前端 `todoStartLocalScheduler` 的 60s 轮询判定（`_shouldTriggerReminder` 等逻辑全部移除）。

use std::sync::RwLock;

use chrono::Local;
use tauri::{AppHandle, Emitter};

use super::rules;
use super::store::{EventStore, JsonFileStore};

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
                    if let Err(e) = Self::tick(&app, &dir) {
                        log::warn!("[schedule] 提醒调度 tick 失败: {}", e);
                    }
                }
            }
        });
    }

    /// 单次 tick：查当前激活目录的到点提醒并推送事件
    fn tick(app: &AppHandle, dir: &str) -> Result<(), String> {
        let store = JsonFileStore::new(dir);
        let events = store.list()?;
        let due = rules::due_reminders(&events, Local::now().naive_local());
        if !due.is_empty() {
            let _ = app.emit(
                "schedule:reminder",
                serde_json::json!({ "dir_path": dir, "events": due }),
            );
        }
        Ok(())
    }
}
