//! 日程规划器（单一职责）：`next_available` —— 查找下一个可安排时间段。
//!
//! 项目独有特性：可跳过休息日/节假日/调休班日（基于 [`crate::core::schedule::lunar::DayInfoProvider`]），
//! 对齐中文用户的日程习惯（避开节假日安排会议等）。

use chrono::{Duration, NaiveDateTime};

use super::lunar::DayInfoProvider;
use super::rules::{find_conflicts, parse_local_time};
use super::ScheduleEvent;

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
            color: String::new(),
            desc: String::new(),
            cron: String::new(),
            notify: true,
            created_at: String::new(),
            updated_at: String::new(),
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
}
