//! 日程规则引擎（纯函数 · 单一职责）。
//!
//! 全部为无副作用函数，输入输出确定，便于契约测试对齐前端既有行为：
//! - [`parse_local_time`]：解析 `YYYY-MM-DDTHH:MM` → `NaiveDateTime`
//! - [`validate_time`]：起止时间合法性（结束必须晚于开始）
//! - [`expand_cron_times`]：Cron 事件在指定日期内的命中时间点（裁剪到事件起止区间）
//! - [`next_cron_time`]：Cron 事件下一次命中时间
//! - [`events_on_date`]：某日事件列表（普通事件 + Cron 展开），对齐前端 `getEventsForDate`
//! - [`find_conflicts`]：与现有事件的区间重叠检测
//! - [`due_reminders`]：到点应提醒的事件（普通事件开始时间命中 + Cron 事件当前分钟命中）
//!
//! Cron 解析用 `cron` crate（现成框架）：前端为 5 字段（分 时 日 月 周），
//! 适配层补秒字段（`0 <expr>`）后交给 cron；迭代按系统本地时区（无 DST 歧义）。

use chrono::{Duration, Local, NaiveDate, NaiveDateTime, TimeZone};
use cron::Schedule;
use std::str::FromStr;

use super::ScheduleEvent;

/// Cron 命中事件的单次占用时长（分钟）。
///
/// 统一口径：Rust 与前端（`_expandCronEventsForDate` 虚拟事件 30 分钟）一致。
/// cron 事件 `start/end` 仅作"每天可命中的时间区间边界"，每次命中按固定时长占档。
pub const CRON_HIT_DURATION_MINUTES: i64 = 30;

/// 解析 `YYYY-MM-DDTHH:MM` 本地时间字符串
pub fn parse_local_time(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%dT%H:%M").ok()
}

/// 起止时间校验：可解析且结束晚于开始（公开 API，供工具层/后续提醒扩展使用）
#[allow(dead_code)]
pub fn validate_time(start: &str, end: &str) -> Result<(), String> {
    let s = parse_local_time(start).ok_or_else(|| format!("开始时间格式无效: {}", start))?;
    let e = parse_local_time(end).ok_or_else(|| format!("结束时间格式无效: {}", end))?;
    if e <= s {
        return Err("结束时间必须晚于开始时间".into());
    }
    Ok(())
}

/// 将前端 5 字段 Cron 补秒后解析为 `cron` crate 的 Schedule
fn parse_cron(cron_expr: &str) -> Result<Schedule, String> {
    let expr = format!("0 {}", cron_expr.trim());
    Schedule::from_str(&expr).map_err(|e| format!("Cron 表达式无效: {}", e))
}

/// Cron 事件在指定日期（本地）内命中的所有时间点，裁剪到事件起止区间。
/// 对齐前端 `_expandCronEventsForDate`：只返回当天且在 [start, end] 内的命中。
pub fn expand_cron_times(event: &ScheduleEvent, date: NaiveDate) -> Vec<NaiveDateTime> {
    if event.cron.trim().is_empty() {
        return Vec::new();
    }
    let Ok(schedule) = parse_cron(&event.cron) else {
        return Vec::new();
    };
    let Some(start_dt) = parse_local_time(&event.start) else {
        return Vec::new();
    };
    let Some(end_dt) = parse_local_time(&event.end) else {
        return Vec::new();
    };
    // 当天区间 ∩ 事件区间
    let day_start = date.and_hms_opt(0, 0, 0).unwrap();
    let day_end = day_start + Duration::days(1) - Duration::seconds(1);
    let range_start = day_start.max(start_dt);
    let range_end = day_end.min(end_dt);
    if range_end < range_start {
        return Vec::new();
    }
    // 从 range_start 前 1 秒开始迭代，确保起点整点命中（cron after 为严格大于）
    let Some(from) = Local
        .from_local_datetime(&(range_start - Duration::seconds(1)))
        .single()
    else {
        return Vec::new();
    };
    schedule
        .after(&from)
        .take_while(|t| t.naive_local() <= range_end)
        .map(|t| t.naive_local())
        .collect()
}

/// Cron 事件在 `after` 之后的下一次命中时间（本地时间；公开 API，供提醒调度/前端 upcoming 使用）
#[allow(dead_code)]
pub fn next_cron_time(event: &ScheduleEvent, after: NaiveDateTime) -> Option<NaiveDateTime> {
    if event.cron.trim().is_empty() {
        return None;
    }
    let schedule = parse_cron(&event.cron).ok()?;
    let from = Local.from_local_datetime(&after).single()?;
    schedule.after(&from).next().map(|t| t.naive_local())
}

/// 事件是否与某日重合（start 与 date 同日；含跨夜事件按 start 日归属，对齐前端 `todoIsSameDay` 语义）
fn same_day(start: &str, date: NaiveDate) -> bool {
    parse_local_time(start)
        .map(|dt| dt.date() == date)
        .unwrap_or(false)
}

/// 某日事件列表：普通事件（start 同日）+ Cron 事件当天展开，按开始时间排序。
/// 对齐前端 `getEventsForDate`。
///
/// Cron 命中事件统一生成为 `start=命中时刻`、`end=命中时刻+[`CRON_HIT_DURATION_MINUTES`]` 的
/// 独立事件（与前端 30 分钟虚拟时长对齐），使空闲块 / 复盘 / 统计等下游按单次时长计算，
/// 避免"cron 事件把原始长区间（如 09:00-18:00）整体当作占用"的失真。
pub fn events_on_date(events: &[ScheduleEvent], date: NaiveDate) -> Vec<ScheduleEvent> {
    let mut normal: Vec<ScheduleEvent> = events
        .iter()
        .filter(|e| e.cron.trim().is_empty() && same_day(&e.start, date))
        .cloned()
        .collect();
    let mut cron_hits: Vec<ScheduleEvent> = events
        .iter()
        .filter(|e| !e.cron.trim().is_empty())
        .flat_map(|e| {
            expand_cron_times(e, date)
                .into_iter()
                .map(|t| ScheduleEvent {
                    start: t.format("%Y-%m-%dT%H:%M").to_string(),
                    end: (t + Duration::minutes(CRON_HIT_DURATION_MINUTES))
                        .format("%Y-%m-%dT%H:%M")
                        .to_string(),
                    ..e.clone()
                })
        })
        .collect();
    normal.append(&mut cron_hits);
    normal.sort_by_key(|e| parse_local_time(&e.start).unwrap_or_else(|| date.and_hms_opt(0, 0, 0).unwrap()));
    normal
}

/// Cron 事件是否在查询区间 `[start, end)` 内存在命中占用（命中时刻 + 单次时长 与区间重叠）。
///
/// 供 [`find_conflicts`] 使用：cron 事件按**当天命中时刻**判定冲突，而不是把原始长区间
/// （如每天 09:00-18:00）整体当作占用，保证空闲查询 / 排期 / 冲突检测的准确性。
/// 最多扫描查询区间涉及的天数（上限 31 天，防止跨多年 cron 拖垮性能）。
fn cron_overlaps(event: &ScheduleEvent, start: NaiveDateTime, end: NaiveDateTime) -> bool {
    let mut day = start.date();
    let last_day = end.date();
    let mut guard = 0;
    while day <= last_day && guard < 31 {
        for t in expand_cron_times(event, day) {
            if t < end && t + Duration::minutes(CRON_HIT_DURATION_MINUTES) > start {
                return true;
            }
        }
        day += Duration::days(1);
        guard += 1;
    }
    false
}

/// 与现有事件的时间重叠检测（半开区间 [start, end) 重叠）。
/// Cron 事件按当天命中时刻（+单次时长）判定；普通/提醒事件按原始区间判定。
pub fn find_conflicts(
    events: &[ScheduleEvent],
    start: NaiveDateTime,
    end: NaiveDateTime,
    ignore_id: Option<&str>,
) -> Vec<ScheduleEvent> {
    events
        .iter()
        .filter(|e| Some(e.id.as_str()) != ignore_id)
        .filter(|e| {
            if !e.cron.trim().is_empty() {
                return cron_overlaps(e, start, end);
            }
            let Some(es) = parse_local_time(&e.start) else { return false };
            let Some(ee) = parse_local_time(&e.end) else { return false };
            es < end && start < ee // 重叠判定
        })
        .cloned()
        .collect()
}

/// 到点应触发提醒的事件。
/// - 普通事件：当前时间在提前提醒窗口内（`[start - notify_before, end)`；`notify_before=0` 时即
///   开始时间起提醒，与旧行为一致）
/// - 提醒（`event_type=reminder`，单点时间 start==end）：到点起 5 分钟窗口内触发
///   （前端 `_remindedEventIds` 会话内去重；窗口过期后不再触发，避免跨重启反复弹）
/// - Cron 事件：当前分钟命中 cron（对齐前端 `_cronMatches`；Cron 事件的提醒时机仍为命中时刻）
pub fn due_reminders(events: &[ScheduleEvent], now: NaiveDateTime) -> Vec<ScheduleEvent> {
    events
        .iter()
        .filter(|e| e.notify)
        .filter(|e| {
            let Some(es) = parse_local_time(&e.start) else { return false };
            let Some(ee) = parse_local_time(&e.end) else { return false };
            // 单点提醒（event_type=reminder, start==end）：到点起 5 分钟窗口内触发
            // （窗口可能在 end 之后，故 `now > ee` 的过期判定对提醒不适用）
            if e.event_type == "reminder" {
                return now >= es && now < es + Duration::minutes(5);
            }
            if now > ee {
                return false;
            }
            if !e.cron.trim().is_empty() {
                // cron 事件：当前分钟命中（起点前 1 秒迭代，包含整点命中）
                let Ok(schedule) = parse_cron(&e.cron) else { return false };
                let Some(from) = Local
                    .from_local_datetime(&(now - Duration::seconds(1)))
                    .single()
                else {
                    return false;
                };
                return schedule
                    .after(&from)
                    .next()
                    .map(|t| t.naive_local() <= now + Duration::minutes(1))
                    .unwrap_or(false);
            }
            // 普通事件：提前提醒窗口 [start - notify_before, end)
            let start_window = es - Duration::minutes(e.notify_before.max(0));
            now >= start_window && now < ee
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    fn event(id: &str, start: &str, end: &str, cron: &str) -> ScheduleEvent {
        ScheduleEvent {
            id: id.into(),
            title: id.into(),
            start: start.into(),
            end: end.into(),
            cron: cron.into(),
            notify: true,
            ..Default::default()
        }
    }

    #[test]
    fn parse_time_formats() {
        let dt = parse_local_time("2026-08-13T10:00").unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 8);
        assert_eq!(dt.day(), 13);
        assert_eq!(dt.hour(), 10);
        assert!(parse_local_time("bad").is_none());
    }

    #[test]
    fn validate_time_rejects_invalid() {
        assert!(validate_time("2026-08-13T10:00", "2026-08-13T11:00").is_ok());
        assert!(validate_time("2026-08-13T11:00", "2026-08-13T10:00").is_err());
        assert!(validate_time("x", "2026-08-13T10:00").is_err());
    }

    #[test]
    fn expand_cron_daily_every_minute_in_window() {
        // 每分钟的 cron，事件 09:00-09:02 → 当天命中 3 次（09:00/09:01/09:02）
        let e = event("e1", "2026-08-13T09:00", "2026-08-13T09:02", "* * * * *");
        let hits = expand_cron_times(&e, NaiveDate::from_ymd_opt(2026, 8, 13).unwrap());
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].hour(), 9);
        assert_eq!(hits[0].minute(), 0);
        assert_eq!(hits[2].minute(), 2);
        // 其他日期不命中（事件本身在该日之外）
        let other = expand_cron_times(&e, NaiveDate::from_ymd_opt(2026, 8, 14).unwrap());
        assert_eq!(other.len(), 0);
    }

    #[test]
    fn expand_cron_hourly() {
        // 每小时第 30 分，事件 08:00-12:00 → 命中 08:30/09:30/10:30/11:30
        let e = event("e1", "2026-08-13T08:00", "2026-08-13T12:00", "30 * * * *");
        let hits = expand_cron_times(&e, NaiveDate::from_ymd_opt(2026, 8, 13).unwrap());
        assert_eq!(hits.len(), 4);
        assert_eq!(hits[0].hour(), 8);
        assert_eq!(hits[3].hour(), 11);
    }

    #[test]
    fn next_cron_time_after_now() {
        let e = event("e1", "2026-08-13T08:00", "2026-08-13T18:00", "0 9 * * *");
        let after = NaiveDate::from_ymd_opt(2026, 8, 13)
            .unwrap()
            .and_hms_opt(8, 30, 0)
            .unwrap();
        let next = next_cron_time(&e, after).unwrap();
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 0);
        assert!(next > after);
    }

    #[test]
    fn events_on_date_mixes_normal_and_cron() {
        let normal = event("n1", "2026-08-13T10:00", "2026-08-13T11:00", "");
        let cron = event("c1", "2026-08-13T09:00", "2026-08-13T12:00", "0 9 * * *");
        let other_day = event("o1", "2026-08-14T10:00", "2026-08-14T11:00", "");
        let list = events_on_date(&[normal, cron, other_day], NaiveDate::from_ymd_opt(2026, 8, 13).unwrap());
        // normal(10:00) + cron 命中(09:00)，按时间排序
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "c1");
        assert_eq!(list[1].id, "n1");
        // cron 命中事件的 start 被替换为实际命中时间，end 为命中时刻 + 单次时长（30 分钟）
        assert_eq!(list[0].start, "2026-08-13T09:00");
        assert_eq!(list[0].end, "2026-08-13T09:30");
        // 普通事件保持原始起止
        assert_eq!(list[1].start, "2026-08-13T10:00");
        assert_eq!(list[1].end, "2026-08-13T11:00");
    }

    #[test]
    fn find_conflicts_checks_cron_by_hit_window() {
        // cron 事件 09:00-18:00（每天 09:00 命中）：09:15 的会议应冲突（命中 09:00-09:30 重叠），
        // 09:45 的会议不冲突（命中窗口已过）；不再把 09:00-18:00 整体当占用。
        let cron = event("c1", "2026-08-13T09:00", "2026-08-13T18:00", "0 9 * * *");
        let d = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap();
        let hit_overlap = d.and_hms_opt(9, 15, 0).unwrap();
        let hit_end = d.and_hms_opt(10, 0, 0).unwrap();
        let conflicts = find_conflicts(&[cron.clone()], hit_overlap, hit_end, None);
        assert_eq!(conflicts.len(), 1, "09:15-10:00 与命中窗口 09:00-09:30 重叠");

        let no_overlap = d.and_hms_opt(9, 45, 0).unwrap();
        let no_end = d.and_hms_opt(10, 45, 0).unwrap();
        assert!(
            find_conflicts(&[cron], no_overlap, no_end, None).is_empty(),
            "09:45 起与命中窗口无重叠，不应误报整天占用"
        );
    }

    #[test]
    fn find_conflicts_detects_overlap() {
        let a = event("a", "2026-08-13T10:00", "2026-08-13T11:00", "");
        let b = event("b", "2026-08-13T10:30", "2026-08-13T11:30", "");
        let c = event("c", "2026-08-13T12:00", "2026-08-13T13:00", "");
        let s = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap().and_hms_opt(10, 30, 0).unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap().and_hms_opt(12, 30, 0).unwrap();
        let conflicts = find_conflicts(&[a.clone(), b.clone(), c], s, e, None);
        // 半开区间 [10:30, 12:30)：a(10-11)、b(10:30-11:30)、c(12:00-13:00) 均重叠
        assert_eq!(conflicts.len(), 3);
        assert!(conflicts.iter().any(|x| x.id == "a"));
        assert!(conflicts.iter().any(|x| x.id == "b"));
        // ignore_id 排除自身
        let self_only = find_conflicts(&[a, b], s, e, Some("a"));
        assert_eq!(self_only.len(), 1);
        assert_eq!(self_only[0].id, "b");
    }

    #[test]
    fn due_reminders_fires_for_started_event() {
        let e = event("e1", "2026-08-13T10:00", "2026-08-13T11:00", "");
        let now = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap().and_hms_opt(10, 5, 0).unwrap();
        let due = due_reminders(&[e.clone()], now);
        assert_eq!(due.len(), 1);
        // 未开始不触发
        let before = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap().and_hms_opt(9, 59, 0).unwrap();
        assert!(due_reminders(&[e.clone()], before).is_empty());
        // 已结束不触发
        let after_end = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap().and_hms_opt(11, 1, 0).unwrap();
        assert!(due_reminders(&[e.clone()], after_end).is_empty());
    }

    #[test]
    fn due_reminders_supports_notify_before() {
        // notify_before=10：开始前 10 分钟起进入提醒窗口
        let mut e = event("e1", "2026-08-13T10:00", "2026-08-13T11:00", "");
        e.notify_before = 10;
        let in_window = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap().and_hms_opt(9, 52, 0).unwrap();
        assert_eq!(due_reminders(&[e.clone()], in_window).len(), 1);
        // 窗口外（早于 start-10min）不触发
        let too_early = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap().and_hms_opt(9, 49, 0).unwrap();
        assert!(due_reminders(&[e.clone()], too_early).is_empty());
        // notify=false 时不触发（即使进了窗口）
        e.notify = false;
        assert!(due_reminders(&[e], in_window).is_empty());
    }

    #[test]
    fn due_reminders_fires_for_reminder_type() {
        // 单点提醒（event_type=reminder, start==end）：到点起 5 分钟窗口内触发
        let mut e = event("r1", "2026-08-13T10:00", "2026-08-13T10:00", "");
        e.event_type = "reminder".into();
        let at = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap().and_hms_opt(10, 0, 0).unwrap();
        assert_eq!(due_reminders(&[e.clone()], at).len(), 1);
        // 窗口内（10:02）仍触发
        let within = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap().and_hms_opt(10, 2, 0).unwrap();
        assert_eq!(due_reminders(&[e.clone()], within).len(), 1);
        // 未到点不触发
        let before = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap().and_hms_opt(9, 59, 0).unwrap();
        assert!(due_reminders(&[e.clone()], before).is_empty());
        // 窗口过期（10:06）不再触发（避免跨重启反复弹）
        let after = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap().and_hms_opt(10, 6, 0).unwrap();
        assert!(due_reminders(&[e], after).is_empty());
    }
}
