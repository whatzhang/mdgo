//! 时间分析器（单一职责）：为空闲查询 / 时间优化 / 日复盘提供确定性统计。
//!
//! 设计原则：**Rust 引擎只做确定性计算**（空闲块、时长聚合、类型分布），
//! 结论与建议（为什么下午会议多、怎么改）由 AI 基于这些数据生成。
//!
//! - [`available_blocks`]：某日工作窗口内的空闲时间段（today_plan / focus 用）
//! - [`analyze_range`]：时间段内的投入统计（optimize 用）
//! - [`day_summary`]：某日事件按状态归类（review 用）

use std::collections::HashMap;

use chrono::{NaiveDate, NaiveDateTime, Timelike};

use super::rules::{events_on_date, parse_local_time};
use super::ScheduleEvent;

/// 一个连续时间段（半开区间 [start, end)）
#[derive(Debug, Clone, PartialEq)]
pub struct TimeBlock {
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
}

impl TimeBlock {
    /// 分钟数
    pub fn minutes(&self) -> i64 {
        (self.end - self.start).num_minutes()
    }
}

/// 某日工作窗口 `[work_start_hour, work_end_hour)` 内的空闲时间段
/// （去除普通事件 + Cron 展开命中事件的占用，按开始时间排序）。
pub fn available_blocks(
    events: &[ScheduleEvent],
    date: NaiveDate,
    work_start_hour: u32,
    work_end_hour: u32,
) -> Vec<TimeBlock> {
    let ws_h = work_start_hour.clamp(0, 23);
    let we_h = work_end_hour.clamp(ws_h + 1, 24);
    let Some(ws) = date.and_hms_opt(ws_h, 0, 0) else {
        return Vec::new();
    };
    let Some(we) = date.and_hms_opt(we_h, 0, 0) else {
        return Vec::new();
    };
    let mut blocks = Vec::new();
    let mut cursor = ws;
    for e in events_on_date(events, date) {
        let (Some(es), Some(ee)) = (parse_local_time(&e.start), parse_local_time(&e.end)) else {
            continue;
        };
        // 裁剪到工作窗口内（窗口外事件忽略）
        let es = es.max(ws);
        let ee = ee.min(we);
        if ee <= ws || es >= we {
            continue;
        }
        if es > cursor {
            blocks.push(TimeBlock { start: cursor, end: es });
        }
        cursor = cursor.max(ee);
    }
    if cursor < we {
        blocks.push(TimeBlock { start: cursor, end: we });
    }
    blocks
}

/// 范围投入统计（optimize 用）
#[derive(Debug, Clone, Default)]
pub struct RangeStats {
    /// 覆盖事件数
    pub event_count: usize,
    /// 总投入分钟数
    pub total_minutes: i64,
    /// 按类型聚合（分钟数降序）：(type, 分钟)
    pub by_type: Vec<(String, i64)>,
    /// 按天分布：(日期, 分钟, 事件数)
    pub by_day: Vec<(NaiveDate, i64, usize)>,
    /// 会议类投入分钟
    pub meeting_minutes: i64,
    /// 深度工作投入分钟（type=focus/work 或 ai.energy=deep_work）
    pub deep_work_minutes: i64,
    /// 下午（13:00 起）会议分钟
    pub evening_meeting_minutes: i64,
    /// 平均每个有投入工作日的投入小时数（无投入工作日不计入分母）
    pub avg_workday_hours: f64,
}

/// 统计 `[from, to)` 区间内事件的时间投入（与区间重叠部分计入）。
pub fn analyze_range(events: &[ScheduleEvent], from: NaiveDateTime, to: NaiveDateTime) -> RangeStats {
    let mut total = 0i64;
    let mut count = 0usize;
    let mut by_type: HashMap<String, i64> = HashMap::new();
    let mut by_day: HashMap<NaiveDate, (i64, usize)> = HashMap::new();
    let mut meeting = 0i64;
    let mut deep = 0i64;
    let mut evening_meeting = 0i64;

    for e in events {
        let (Some(es), Some(ee)) = (parse_local_time(&e.start), parse_local_time(&e.end)) else {
            continue;
        };
        let s = es.max(from);
        let t = ee.min(to);
        if t <= s {
            continue;
        }
        let mins = (t - s).num_minutes();
        total += mins;
        count += 1;
        let ty = if e.event_type.trim().is_empty() { "other" } else { e.event_type.trim() };
        *by_type.entry(ty.to_string()).or_insert(0) += mins;
        let day_entry = by_day.entry(s.date()).or_insert((0, 0));
        day_entry.0 += mins;
        day_entry.1 += 1;
        if ty == "meeting" {
            meeting += mins;
            if s.hour() >= 13 {
                evening_meeting += mins;
            }
        }
        if ty == "focus" || ty == "work" || e.ai.energy == "deep_work" {
            deep += mins;
        }
    }

    let mut by_type: Vec<(String, i64)> = by_type.into_iter().collect();
    by_type.sort_by(|a, b| b.1.cmp(&a.1));
    let mut by_day: Vec<(NaiveDate, i64, usize)> = by_day
        .into_iter()
        .map(|(d, (m, c))| (d, m, c))
        .collect();
    by_day.sort_by_key(|(d, _, _)| *d);
    let workdays = by_day.len().max(1) as f64;

    RangeStats {
        event_count: count,
        total_minutes: total,
        by_type,
        by_day,
        meeting_minutes: meeting,
        deep_work_minutes: deep,
        evening_meeting_minutes: evening_meeting,
        avg_workday_hours: (total as f64 / 60.0) / workdays,
    }
}

/// 日复盘归类（review 用）：按 `now` 划分已结束 / 进行中 / 未开始。
#[derive(Debug, Clone, Default)]
pub struct DaySummary {
    /// 已结束（end <= now）
    pub done: Vec<ScheduleEvent>,
    /// 进行中（start <= now < end）
    pub ongoing: Vec<ScheduleEvent>,
    /// 未开始（start > now）
    pub upcoming: Vec<ScheduleEvent>,
    /// 当天投入总分钟（按事件起止统计，Cron 命中事件按命中时间计算）
    pub total_minutes: i64,
}

/// 某日事件按状态归类（含 Cron 展开，按开始时间排序）。
pub fn day_summary(events: &[ScheduleEvent], date: NaiveDate, now: NaiveDateTime) -> DaySummary {
    let mut summary = DaySummary::default();
    for e in events_on_date(events, date) {
        let (Some(es), Some(ee)) = (parse_local_time(&e.start), parse_local_time(&e.end)) else {
            continue;
        };
        if ee > es {
            summary.total_minutes += (ee - es).num_minutes();
        }
        if now >= ee {
            summary.done.push(e);
        } else if now >= es {
            summary.ongoing.push(e);
        } else {
            summary.upcoming.push(e);
        }
    }
    summary
}

/// 把分钟数格式化为 "x小时y分"（工具输出用）
pub fn fmt_minutes(mins: i64) -> String {
    let h = mins / 60;
    let m = mins % 60;
    if h == 0 {
        format!("{}分钟", m)
    } else if m == 0 {
        format!("{}小时", h)
    } else {
        format!("{}小时{}分", h, m)
    }
}

/// 格式化某日空闲块列表为文本（today_plan / focus 输出用）
pub fn fmt_blocks(blocks: &[TimeBlock]) -> String {
    if blocks.is_empty() {
        return "当天工作窗口内无空闲时间段".to_string();
    }
    blocks
        .iter()
        .map(|b| {
            format!(
                "{} ~ {}（{}）",
                b.start.format("%H:%M"),
                b.end.format("%H:%M"),
                fmt_minutes(b.minutes())
            )
        })
        .collect::<Vec<_>>()
        .join("；")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, start: &str, end: &str, event_type: &str) -> ScheduleEvent {
        ScheduleEvent {
            id: id.into(),
            title: id.into(),
            start: start.into(),
            end: end.into(),
            event_type: event_type.into(),
            ..Default::default()
        }
    }

    fn dt(date: (i32, u32, u32), h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(date.0, date.1, date.2).unwrap().and_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn available_blocks_splits_work_window() {
        let d = (2026, 8, 10); // 周一
        let busy = event("m", "2026-08-10T10:00", "2026-08-10T11:00", "meeting");
        let blocks = available_blocks(&[busy], NaiveDate::from_ymd_opt(d.0, d.1, d.2).unwrap(), 9, 18);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].start, dt(d, 9, 0));
        assert_eq!(blocks[0].end, dt(d, 10, 0));
        assert_eq!(blocks[1].start, dt(d, 11, 0));
        assert_eq!(blocks[1].end, dt(d, 18, 0));
    }

    #[test]
    fn available_blocks_ignores_outside_window() {
        let d = (2026, 8, 10);
        let night = event("n", "2026-08-10T20:00", "2026-08-10T21:00", "personal");
        let blocks = available_blocks(&[night], NaiveDate::from_ymd_opt(d.0, d.1, d.2).unwrap(), 9, 18);
        // 夜间事件不占用工作窗口 → 整段空闲
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].minutes(), 9 * 60);
    }

    #[test]
    fn analyze_range_aggregates_types_and_days() {
        let from = dt((2026, 8, 10), 0, 0); // 周一
        let to = dt((2026, 8, 11), 0, 0); // 周二 0 点 → 只统计周一
        let events = vec![
            event("m1", "2026-08-10T10:00", "2026-08-10T11:00", "meeting"),
            event("m2", "2026-08-10T14:00", "2026-08-10T15:30", "meeting"),
            event("f1", "2026-08-10T09:00", "2026-08-10T12:00", "focus"),
        ];
        let stats = analyze_range(&events, from, to);
        assert_eq!(stats.event_count, 3);
        assert_eq!(stats.total_minutes, 60 + 90 + 180);
        assert_eq!(stats.meeting_minutes, 150);
        assert_eq!(stats.evening_meeting_minutes, 90); // 14:00 起的会议
        assert_eq!(stats.deep_work_minutes, 180);
        assert!(stats.by_type.iter().any(|(t, m)| t == "meeting" && *m == 150));
        assert_eq!(stats.by_day.len(), 1);
        assert_eq!(stats.by_day[0].0, NaiveDate::from_ymd_opt(2026, 8, 10).unwrap());
    }

    #[test]
    fn analyze_range_clips_to_window() {
        let from = dt((2026, 8, 10), 12, 0);
        let to = dt((2026, 8, 10), 13, 0);
        // 事件 11:00-14:00，只统计 [12:00, 13:00) 的 60 分钟
        let e = event("x", "2026-08-10T11:00", "2026-08-10T14:00", "work");
        let stats = analyze_range(&[e], from, to);
        assert_eq!(stats.total_minutes, 60);
        assert_eq!(stats.deep_work_minutes, 60);
    }

    #[test]
    fn day_summary_classifies_by_now() {
        let date = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
        let now = dt((2026, 8, 10), 11, 0);
        let events = vec![
            event("done", "2026-08-10T09:00", "2026-08-10T10:00", "work"),
            event("ongoing", "2026-08-10T10:30", "2026-08-10T12:00", "work"),
            event("upcoming", "2026-08-10T14:00", "2026-08-10T15:00", "meeting"),
        ];
        let s = day_summary(&events, date, now);
        assert_eq!(s.done.len(), 1);
        assert_eq!(s.done[0].id, "done");
        assert_eq!(s.ongoing.len(), 1);
        assert_eq!(s.ongoing[0].id, "ongoing");
        assert_eq!(s.upcoming.len(), 1);
        assert_eq!(s.upcoming[0].id, "upcoming");
        // 60 + 90 + 60 = 210 分钟
        assert_eq!(s.total_minutes, 210);
    }

    #[test]
    fn fmt_minutes_formats() {
        assert_eq!(fmt_minutes(30), "30分钟");
        assert_eq!(fmt_minutes(120), "2小时");
        assert_eq!(fmt_minutes(150), "2小时30分");
    }
}
