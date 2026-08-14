//! 日程命令层（IPC 薄壳：参数编解码 → 委托 `core::schedule`，不含业务逻辑）。
//!
//! 存储按知识库目录惰性创建（`SqliteStore` 指向全局共用 DB `%APPDATA%/com.mdgo/mdgo.db`，
//! `dir_path` 列隔离各知识库数据）；
//! 农历/节假日服务为全局单例（调休缓存于 `%APPDATA%/com.mdgo/schedule_cache`）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{Local, NaiveDate, NaiveDateTime};
use tauri::{AppHandle, Manager};

use crate::core::schedule::lunar::{DayInfo, DayInfoProvider, HolidayService};
use crate::core::schedule::planner;
use crate::core::schedule::rules;
use crate::core::schedule::sqlite::SqliteStore;
use crate::core::schedule::store::EventStore;
use crate::core::schedule::{ScheduleEvent, ScheduleEventInput};
use crate::AppState;

type StoreRef = Arc<Mutex<SqliteStore>>;

/// 按目录获取（或惰性创建）日程存储——所有读写路径（IPC/工具/调度器）共用同一 Arc<Mutex>
fn store_for(state: &AppState, dir_path: &str) -> Result<StoreRef, String> {
    state.schedule_store(dir_path)
}

/// 加锁（poison 恢复：锁被污染时继续服务而非 panic，保证高可用）
fn lock_store(store: &StoreRef) -> std::sync::MutexGuard<'_, SqliteStore> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_local() -> NaiveDateTime {
    Local::now().naive_local()
}

fn fmt_time(dt: NaiveDateTime) -> String {
    dt.format("%Y-%m-%dT%H:%M").to_string()
}

fn parse_date(s: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .map_err(|_| format!("日期格式无效: {}（应为 YYYY-MM-DD）", s))
}

/// 日程列表
#[tauri::command]
pub async fn schedule_list(app: AppHandle, dir_path: String) -> Result<Vec<ScheduleEvent>, String> {
    let state = app.state::<AppState>();
    let store = store_for(&state, &dir_path)?;
    // SQLite 为阻塞 IO，移入 blocking 线程，避免占住 tokio worker
    tokio::task::spawn_blocking(move || lock_store(&store).list())
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

/// 单事件详情
#[tauri::command]
pub async fn schedule_get(
    app: AppHandle,
    dir_path: String,
    id: String,
) -> Result<ScheduleEvent, String> {
    let state = app.state::<AppState>();
    let store = store_for(&state, &dir_path)?;
    let events = tokio::task::spawn_blocking(move || lock_store(&store).list())
        .await
        .map_err(|e| format!("任务执行失败: {}", e))??;
    events
        .into_iter()
        .find(|e| e.id == id)
        .ok_or_else(|| "日程不存在".to_string())
}

/// 新建日程；返回创建的事件（含冲突事件列表，非阻塞）
#[tauri::command]
pub async fn schedule_add(
    app: AppHandle,
    dir_path: String,
    input: ScheduleEventInput,
) -> Result<ScheduleAddResult, String> {
    let state = app.state::<AppState>();
    let now = fmt_time(now_local());
    let event = ScheduleEvent {
        id: uuid::Uuid::new_v4().to_string(),
        title: input.title,
        start: input.start,
        end: input.end,
        color: input.color,
        desc: input.desc,
        cron: input.cron,
        notify: input.notify,
        created_at: now.clone(),
        updated_at: now,
    };
    event.validate()?;
    let store = store_for(&state, &dir_path)?;
    // 冲突检测（非 Cron 事件）+ 写入：SQLite 阻塞 IO 移入 blocking 线程
    let (conflicts, event) = tokio::task::spawn_blocking(move || -> Result<(Vec<ScheduleEvent>, ScheduleEvent), String> {
        let conflicts = if event.cron.trim().is_empty() {
            let s = rules::parse_local_time(&event.start)
                .ok_or_else(|| "开始时间格式无效".to_string())?;
            let e = rules::parse_local_time(&event.end)
                .ok_or_else(|| "结束时间格式无效".to_string())?;
            let list = lock_store(&store).list()?;
            rules::find_conflicts(&list, s, e, None)
        } else {
            Vec::new()
        };
        lock_store(&store).upsert(event.clone())?;
        Ok((conflicts, event))
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))??;
    Ok(ScheduleAddResult { event, conflicts })
}

/// 更新日程（按 id 全量替换字段）
#[tauri::command]
pub async fn schedule_update(
    app: AppHandle,
    dir_path: String,
    id: String,
    input: ScheduleEventInput,
) -> Result<ScheduleEvent, String> {
    let state = app.state::<AppState>();
    let store = store_for(&state, &dir_path)?;
    // 单锁内完成 读→改→写：消除锁窗口（并发写者在此期间插入/删除不会被覆盖）
    let updated = tokio::task::spawn_blocking(move || -> Result<ScheduleEvent, String> {
        let mut guard = lock_store(&store);
        let mut events = guard.list()?;
        let Some(existing) = events.iter_mut().find(|e| e.id == id) else {
            return Err("日程不存在".to_string());
        };
        existing.title = input.title;
        existing.start = input.start;
        existing.end = input.end;
        existing.color = input.color;
        existing.desc = input.desc;
        existing.cron = input.cron;
        existing.notify = input.notify;
        existing.updated_at = fmt_time(now_local());
        existing.validate()?;
        let updated = existing.clone();
        guard.replace_all(events)?;
        Ok(updated)
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))??;
    Ok(updated)
}

/// 删除日程
#[tauri::command]
pub async fn schedule_remove(
    app: AppHandle,
    dir_path: String,
    id: String,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let store = store_for(&state, &dir_path)?;
    tokio::task::spawn_blocking(move || lock_store(&store).remove(&id))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

/// 某日事件列表（含 Cron 展开，按时间排序）——前端日历视图取数
#[tauri::command]
pub async fn schedule_events_on_date(
    app: AppHandle,
    dir_path: String,
    date: String,
) -> Result<Vec<ScheduleEvent>, String> {
    let state = app.state::<AppState>();
    let d = parse_date(&date)?;
    let store = store_for(&state, &dir_path)?;
    let events = tokio::task::spawn_blocking(move || lock_store(&store).list())
        .await
        .map_err(|e| format!("任务执行失败: {}", e))??;
    Ok(rules::events_on_date(&events, d))
}

/// 冲突检测（与现有事件重叠）
#[tauri::command]
pub async fn schedule_conflicts(
    app: AppHandle,
    dir_path: String,
    start: String,
    end: String,
    ignore_id: Option<String>,
) -> Result<Vec<ScheduleEvent>, String> {
    let state = app.state::<AppState>();
    let s = rules::parse_local_time(&start).ok_or_else(|| "开始时间格式无效".to_string())?;
    let e = rules::parse_local_time(&end).ok_or_else(|| "结束时间格式无效".to_string())?;
    if e <= s {
        return Err("结束时间必须晚于开始时间".to_string());
    }
    let store = store_for(&state, &dir_path)?;
    let events = tokio::task::spawn_blocking(move || lock_store(&store).list())
        .await
        .map_err(|e| format!("任务执行失败: {}", e))??;
    Ok(rules::find_conflicts(&events, s, e, ignore_id.as_deref()))
}

/// 到点应提醒的事件（前端/系统提醒调度轮询）
#[tauri::command]
pub async fn schedule_remind(app: AppHandle, dir_path: String) -> Result<Vec<ScheduleEvent>, String> {
    let state = app.state::<AppState>();
    let store = store_for(&state, &dir_path)?;
    let events = tokio::task::spawn_blocking(move || lock_store(&store).list())
        .await
        .map_err(|e| format!("任务执行失败: {}", e))??;
    Ok(rules::due_reminders(&events, now_local()))
}

/// 某日农历 / 节假日 / 调休信息
#[tauri::command]
pub async fn schedule_lunar(app: AppHandle, dir_path: String, date: String) -> Result<DayInfo, String> {
    let state = app.state::<AppState>();
    let _ = dir_path;
    let d = parse_date(&date)?;
    // day_info 内部可能触发 timor.tech 网络请求（blocking client），须在 blocking 线程执行，
    // 避免在 async 上下文 drop 内部 runtime 导致 panic（Cannot drop a runtime ...）
    let provider = state.schedule_day_info.clone();
    tokio::task::spawn_blocking(move || provider.day_info(d))
        .await
        .map_err(|e| format!("农历/节假日计算失败: {}", e))
}

/// 查找下一个可安排时间段（可跳过休息日/节假日）——项目独有特性
#[tauri::command]
pub async fn schedule_next_available(
    app: AppHandle,
    dir_path: String,
    duration_minutes: i64,
    start_after: Option<String>,
    skip_rest_days: Option<bool>,
) -> Result<Option<String>, String> {
    let state = app.state::<AppState>();
    let start = match start_after {
        Some(s) => rules::parse_local_time(&s).ok_or_else(|| "开始时间格式无效".to_string())?,
        None => now_local(),
    };
    let store = store_for(&state, &dir_path)?;
    let events = tokio::task::spawn_blocking(move || lock_store(&store).list())
        .await
        .map_err(|e| format!("任务执行失败: {}", e))??;
    // planner::next_available 内部会调 day_info（可能触发 timor.tech blocking 网络），
    // 在 blocking 线程执行，避免 async 上下文 drop runtime panic
    let provider = state.schedule_day_info.clone();
    let next = tokio::task::spawn_blocking(move || {
        planner::next_available(
            &events,
            provider.as_ref(),
            duration_minutes,
            start,
            skip_rest_days.unwrap_or(true),
        )
    })
    .await
    .map_err(|e| format!("查找可安排时间失败: {}", e))?;
    Ok(next.map(|t| fmt_time(t)))
}

/// 设置提醒调度器的活动目录（打开日历 / 切换知识库时由前端调用）
#[tauri::command]
pub async fn schedule_set_active_dir(app: AppHandle, dir_path: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    state
        .schedule_scheduler
        .set_active_dir(Some(dir_path));
    Ok(())
}

/// 停止提醒调度（关闭日历 / 清空活动目录时调用）
#[tauri::command]
pub async fn schedule_clear_active_dir(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.schedule_scheduler.set_active_dir(None);
    Ok(())
}

/// 新建日程返回结构：事件 + 冲突提示
#[derive(serde::Serialize)]
pub struct ScheduleAddResult {
    pub event: ScheduleEvent,
    pub conflicts: Vec<ScheduleEvent>,
}

/// 供 AppState 初始化使用的全局节假日服务工厂
pub fn build_day_info_provider() -> Arc<dyn DayInfoProvider> {
    Arc::new(HolidayService::new())
}

/// 供 AppState 初始化的空存储缓存
pub fn empty_store_cache() -> Mutex<HashMap<String, StoreRef>> {
    Mutex::new(HashMap::new())
}
