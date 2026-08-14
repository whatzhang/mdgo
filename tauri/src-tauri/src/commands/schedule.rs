//! 日程命令层（IPC 薄壳：参数编解码 → 委托 `core::schedule`，不含业务逻辑）。
//!
//! 存储按知识库目录惰性创建（`JsonFileStore` 读写 `{dir}/.mdgo/index_schedule.json`）；
//! 农历/节假日服务为全局单例（调休缓存于 `%APPDATA%/com.mdgo/schedule_cache`）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{Local, NaiveDate, NaiveDateTime};
use tauri::{AppHandle, Manager};

use crate::core::schedule::lunar::{DayInfo, DayInfoProvider, HolidayService};
use crate::core::schedule::planner;
use crate::core::schedule::rules;
use crate::core::schedule::store::{EventStore, JsonFileStore};
use crate::core::schedule::{ScheduleEvent, ScheduleEventInput};
use crate::AppState;

type StoreRef = Arc<Mutex<JsonFileStore>>;

/// 按目录获取（或惰性创建）日程存储
fn store_for(state: &AppState, dir_path: &str) -> StoreRef {
    let mut map = state.schedule_stores.lock().unwrap();
    map.entry(dir_path.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(JsonFileStore::new(dir_path))))
        .clone()
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
    store_for(&state, &dir_path).lock().unwrap().list()
}

/// 单事件详情
#[tauri::command]
pub async fn schedule_get(
    app: AppHandle,
    dir_path: String,
    id: String,
) -> Result<ScheduleEvent, String> {
    let state = app.state::<AppState>();
    store_for(&state, &dir_path)
        .lock()
        .unwrap()
        .list()?
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
    // 冲突检测（非 Cron 事件）
    let conflicts = if event.cron.trim().is_empty() {
        let s = rules::parse_local_time(&event.start)
            .ok_or_else(|| "开始时间格式无效".to_string())?;
        let e = rules::parse_local_time(&event.end)
            .ok_or_else(|| "结束时间格式无效".to_string())?;
        let list = store_for(&state, &dir_path).lock().unwrap().list()?;
        rules::find_conflicts(&list, s, e, None)
    } else {
        Vec::new()
    };
    store_for(&state, &dir_path).lock().unwrap().upsert(event.clone())?;
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
    let store = store_for(&state, &dir_path);
    let mut events = store.lock().unwrap().list()?;
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
    drop(store);
    store_for(&state, &dir_path).lock().unwrap().replace_all(events)?;
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
    store_for(&state, &dir_path).lock().unwrap().remove(&id)
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
    let events = store_for(&state, &dir_path).lock().unwrap().list()?;
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
    let events = store_for(&state, &dir_path).lock().unwrap().list()?;
    Ok(rules::find_conflicts(&events, s, e, ignore_id.as_deref()))
}

/// 到点应提醒的事件（前端/系统提醒调度轮询）
#[tauri::command]
pub async fn schedule_remind(app: AppHandle, dir_path: String) -> Result<Vec<ScheduleEvent>, String> {
    let state = app.state::<AppState>();
    let events = store_for(&state, &dir_path).lock().unwrap().list()?;
    Ok(rules::due_reminders(&events, now_local()))
}

/// 某日农历 / 节假日 / 调休信息
#[tauri::command]
pub async fn schedule_lunar(app: AppHandle, dir_path: String, date: String) -> Result<DayInfo, String> {
    let state = app.state::<AppState>();
    let _ = dir_path;
    let d = parse_date(&date)?;
    Ok(state.schedule_day_info.day_info(d))
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
    let events = store_for(&state, &dir_path).lock().unwrap().list()?;
    let next = planner::next_available(
        &events,
        state.schedule_day_info.as_ref(),
        duration_minutes,
        start,
        skip_rest_days.unwrap_or(true),
    );
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
