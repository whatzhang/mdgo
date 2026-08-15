//! 日程规划器（单一职责）：时间排布相关纯函数。
//!
//! 项目独有特性：可跳过休息日/节假日/调休班日（基于 [`crate::core::schedule::lunar::DayInfoProvider`]），
//! 对齐中文用户的日程习惯（避开节假日安排会议等）。
//!
//! - [`next_available`]：查找下一个可安排时间段
//! - [`plan_tasks`]：把 AI 拆解出的任务（标题 + 小时数）排布到 deadline 前的空闲时段
//! - [`suggest_alternatives`]：冲突时给出备选时间段建议（不自动覆盖/移动，只给建议）

use chrono::{Duration, NaiveDate, NaiveDateTime, Timelike};

use super::lunar::DayInfoProvider;
use super::rules::{find_conflicts, parse_local_time};
use super::ScheduleEvent;

/// 待排布任务（AI 拆解产物：标题 + 预估小时数）
#[derive(Debug, Clone)]
pub struct PlannedTask {
    pub title: String,
    pub hours: f64,
}

/// 排布结果：建议时间块
#[derive(Debug, Clone)]
pub struct PlannedSlot {
    pub title: String,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
}

/// 把任务按顺序排入 `deadline` 前的空闲时段（按天，每天在 `[work_start_hour, work_end_hour)` 工作窗口内）。
///
/// - 任务不跨天：当天剩余窗口不足一个任务时顺延到次日；`skip_rest_days` 时跳过休息日；
/// - 冲突任务段被跳过（跳到冲突事件结束之后）；
/// - 返回与 `tasks` 等长的结果：`Some(slot)` 表示排布成功，`None` 表示 deadline 前排不下（由上层如实告知用户）。
pub fn plan_tasks(
    events: &[ScheduleEvent],
    provider: &dyn DayInfoProvider,
    tasks: &[PlannedTask],
    deadline: NaiveDate,
    work_start_hour: u32,
    work_end_hour: u32,
    skip_rest_days: bool,
    from: NaiveDateTime,
) -> Vec<Option<PlannedSlot>> {
    let ws_h = work_start_hour.clamp(0, 23);
    let we_h = work_end_hour.clamp(ws_h + 1, 24);
    let limit = deadline.and_hms_opt(23, 59, 0).unwrap();
    let mut cursor = from;
    let mut results = Vec::with_capacity(tasks.len());
    for task in tasks {
        let minutes = Duration::minutes((task.hours * 60.0).round().max(1.0) as i64);
        let mut placed: Option<PlannedSlot> = None;
        'search: while cursor <= limit {
            let day = cursor.date();
            let ws = day.and_hms_opt(ws_h, 0, 0).unwrap();
            let we = day.and_hms_opt(we_h, 0, 0).unwrap();
            if skip_rest_days && provider.day_info(day).is_rest_day {
                cursor = next_workday_start(day, ws, provider, skip_rest_days);
                continue;
            }
            if cursor < ws {
                cursor = ws;
                continue;
            }
            // 当天剩余窗口不足一个任务 → 顺延次日
            if cursor + minutes > we {
                cursor = next_workday_start(day, ws, provider, skip_rest_days);
                continue;
            }
            let cand_end = cursor + minutes;
            if cand_end > limit {
                break 'search; // deadline 前放不下
            }
            let conflicts = find_conflicts(events, cursor, cand_end, None);
            if conflicts.is_empty() {
                placed = Some(PlannedSlot {
                    title: task.title.clone(),
                    start: cursor,
                    end: cand_end,
                });
                cursor = cand_end;
                break 'search;
            }
            cursor = conflicts
                .iter()
                .filter_map(|e| parse_local_time(&e.end))
                .max()
                .map(|end| if end > cursor { end } else { cursor + Duration::minutes(30) })
                .unwrap_or(cursor + Duration::minutes(30));
        }
        results.push(placed);
    }
    results
}

/// 计算下一可用工作日起点：`day` 之后第一个非休息日（或休息日不跳过时即 `day` 次日）的 `ws` 时刻
fn next_workday_start(
    day: NaiveDate,
    ws: NaiveDateTime,
    provider: &dyn DayInfoProvider,
    skip_rest_days: bool,
) -> NaiveDateTime {
    let mut d = day + Duration::days(1);
    while skip_rest_days && provider.day_info(d).is_rest_day {
        d += Duration::days(1);
    }
    d.and_hms_opt(ws.hour(), ws.minute(), 0).unwrap()
}

/// 冲突时的备选时间段建议（最多 2 个，**只建议不自动创建**）：
///
/// 1. `after` 之后第一个可安排空档（如冲突窗口结束后）；
/// 2. 次日 09:00 起的第一个可安排空档（若与第 1 个重复则跳过）。
pub fn suggest_alternatives(
    events: &[ScheduleEvent],
    provider: &dyn DayInfoProvider,
    duration_minutes: i64,
    after: NaiveDateTime,
    skip_rest_days: bool,
) -> Vec<NaiveDateTime> {
    let mut out = Vec::new();
    if let Some(t) = next_available(events, provider, duration_minutes, after, skip_rest_days) {
        out.push(t);
    }
    let tomorrow = (after.date() + Duration::days(1)).and_hms_opt(9, 0, 0);
    if let Some(t2) = tomorrow
        .and_then(|d| next_available(events, provider, duration_minutes, d, skip_rest_days))
        .filter(|t| !out.contains(t))
    {
        out.push(t2);
    }
    out
}

/// 查找下一个可安排时间段（从 `start_after` 起，30 分钟步进探测，最多向后 30 天）。
///
/// - `duration_minutes`：所需时长
/// - `skip_rest_days`：为 true 时跳过休息日（周末/法定节假日；含调休班日的判断由 DayInfoProvider 决定）
/// - 返回 `Some(起点)` 或 `None`（30 天内无空档）
pub fn next_available(
    events: &[ScheduleEvent],
    provider: &dyn DayInfoProvider,
    duration_minutes: i64,
    start_after: NaiveDateTime,
    skip_rest_days: bool,
) -> Option<NaiveDateTime> {
    let duration = Duration::minutes(duration_minutes.max(1));
    let limit = start_after + Duration::days(30);
    let mut cursor = start_after;
    while cursor < limit {
        let candidate_end = cursor + duration;
        if skip_rest_days && provider.day_info(cursor.date()).is_rest_day {
            cursor += Duration::minutes(30);
            continue;
        }
        let conflicts = find_conflicts(events, cursor, candidate_end, None);
        if conflicts.is_empty() {
            return Some(cursor);
        }
        // 跳到冲突事件结束之后（比固定步进更快收敛）
        cursor = conflicts
            .iter()
            .filter_map(|e| parse_local_time(&e.end))
            .max()
            .map(|end| if end > cursor { end } else { cursor + Duration::minutes(30) })
            .unwrap_or(cursor + Duration::minutes(30));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::schedule::lunar::SimpleDayInfo;
    use chrono::{NaiveDate, Timelike};

    fn event(id: &str, start: &str, end: &str) -> ScheduleEvent {
        ScheduleEvent {
            id: id.into(),
            title: id.into(),
            start: start.into(),
            end: end.into(),
            notify: true,
            ..Default::default()
        }
    }

    #[test]
    fn returns_start_when_no_conflict() {
        let provider = SimpleDayInfo;
        let after = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap().and_hms_opt(9, 0, 0).unwrap(); // 周一
        let next = next_available(&[], &provider, 60, after, true).unwrap();
        assert_eq!(next, after);
    }

    #[test]
    fn skips_conflicting_window() {
        let provider = SimpleDayInfo;
        let busy = event("b", "2026-08-10T09:00", "2026-08-10T10:00");
        let after = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap().and_hms_opt(8, 30, 0).unwrap();
        let next = next_available(&[busy], &provider, 60, after, true).unwrap();
        // 8:30 起 60 分钟会撞 9:00-10:00，应跳到 10:00
        assert_eq!(next.hour(), 10);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn skips_rest_days() {
        let provider = SimpleDayInfo;
        let sat = NaiveDate::from_ymd_opt(2026, 8, 15).unwrap().and_hms_opt(9, 0, 0).unwrap(); // 周六
        let next = next_available(&[], &provider, 60, sat, true).unwrap();
        // 周六跳过 → 周一（8/17）09:00
        assert_eq!(next.date(), NaiveDate::from_ymd_opt(2026, 8, 17).unwrap());
        // skip_rest_days=false 时周六可用
        let next2 = next_available(&[], &provider, 60, sat, false).unwrap();
        assert_eq!(next2, sat);
    }

    #[test]
    fn plan_tasks_fills_work_window() {
        let provider = SimpleDayInfo;
        // 周一 08:30 开始排：两个任务（2h + 1h），工作日窗口 09:00-18:00
        let from = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap().and_hms_opt(8, 30, 0).unwrap();
        let tasks = vec![
            PlannedTask { title: "RAG 优化".into(), hours: 2.0 },
            PlannedTask { title: "Embedding 测试".into(), hours: 1.0 },
        ];
        let results = plan_tasks(&[], &provider, &tasks, NaiveDate::from_ymd_opt(2026, 8, 14).unwrap(), 9, 18, true, from);
        assert_eq!(results.len(), 2);
        let s0 = results[0].as_ref().unwrap();
        assert_eq!(s0.start, NaiveDate::from_ymd_opt(2026, 8, 10).unwrap().and_hms_opt(9, 0, 0).unwrap());
        assert_eq!(s0.end, NaiveDate::from_ymd_opt(2026, 8, 10).unwrap().and_hms_opt(11, 0, 0).unwrap());
        let s1 = results[1].as_ref().unwrap();
        assert_eq!(s1.start, NaiveDate::from_ymd_opt(2026, 8, 10).unwrap().and_hms_opt(11, 0, 0).unwrap());
        assert_eq!(s1.end, NaiveDate::from_ymd_opt(2026, 8, 10).unwrap().and_hms_opt(12, 0, 0).unwrap());
    }

    #[test]
    fn plan_tasks_skips_conflicts_and_rest_days() {
        let provider = SimpleDayInfo;
        // 周一 10:00-11:00 已有会议；从周一 08:30 排 2h 任务 → 先排 09:00-10:00，冲突后跳到 11:00-13:00？不跨天限制 → 顺延
        let busy = event("meeting", "2026-08-10T10:00", "2026-08-10T11:00");
        let from = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap().and_hms_opt(8, 30, 0).unwrap();
        let tasks = vec![PlannedTask { title: "深度工作".into(), hours: 2.0 }];
        let results = plan_tasks(&[busy], &provider, &tasks, NaiveDate::from_ymd_opt(2026, 8, 14).unwrap(), 9, 18, true, from);
        let slot = results[0].as_ref().unwrap();
        // 09:00-11:00 撞会议 → 顺延到 11:00 起
        assert_eq!(slot.start, NaiveDate::from_ymd_opt(2026, 8, 10).unwrap().and_hms_opt(11, 0, 0).unwrap());
        assert_eq!(slot.end, NaiveDate::from_ymd_opt(2026, 8, 10).unwrap().and_hms_opt(13, 0, 0).unwrap());

        // 周五排 8h（09:00-17:00 占满当天）→ 下一个任务顺延到周一
        let friday = NaiveDate::from_ymd_opt(2026, 8, 14).unwrap().and_hms_opt(9, 0, 0).unwrap();
        let tasks2 = vec![
            PlannedTask { title: "A".into(), hours: 8.0 },
            PlannedTask { title: "B".into(), hours: 2.0 },
        ];
        let results2 = plan_tasks(&[], &provider, &tasks2, NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(), 9, 18, true, friday);
        let a = results2[0].as_ref().unwrap();
        let b = results2[1].as_ref().unwrap();
        assert_eq!(a.start.date(), NaiveDate::from_ymd_opt(2026, 8, 14).unwrap()); // 周五
        assert_eq!(b.start.date(), NaiveDate::from_ymd_opt(2026, 8, 17).unwrap()); // 周一（跳过周末）
    }

    #[test]
    fn plan_tasks_reports_unplaceable_before_deadline() {
        let provider = SimpleDayInfo;
        // deadline 就在当天，任务 15h 远超当日 9h 工作窗口 → 顺延次日已超过 deadline → 排不下
        let from = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap().and_hms_opt(9, 0, 0).unwrap();
        let tasks = vec![PlannedTask { title: "大任务".into(), hours: 15.0 }];
        let results = plan_tasks(&[], &provider, &tasks, NaiveDate::from_ymd_opt(2026, 8, 10).unwrap(), 9, 18, true, from);
        assert!(results[0].is_none());
    }

    #[test]
    fn suggest_alternatives_offers_two_options() {
        let provider = SimpleDayInfo;
        let busy = event("meeting", "2026-08-10T10:00", "2026-08-10T11:00");
        let after = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap().and_hms_opt(10, 0, 0).unwrap();
        let alts = suggest_alternatives(&[busy], &provider, 60, after, true);
        // 方案1：冲突结束后 11:00；方案2：次日（周一后是周二）09:00 —— 但 8/11 是周二（工作日）
        assert!(!alts.is_empty());
        assert!(alts[0] >= NaiveDate::from_ymd_opt(2026, 8, 10).unwrap().and_hms_opt(11, 0, 0).unwrap());
        // 去重：两个方案不应相同
        assert!(alts.len() <= 2);
        if alts.len() == 2 {
            assert_ne!(alts[0], alts[1]);
        }
    }
}
