//! 农历 / 节假日 / 调休信息服务（单一职责 + 依赖倒置）。
//!
//! - [`DayInfo`]：某日期的展示信息（农历日/月、节日、是否休息日/调休班日）。
//! - [`DayInfoProvider`]：抽象接口，命令层与规划器只依赖此 trait。
//! - [`HolidayService`]：实现——农历用 `chinese-lunisolar-calendar`（现成框架，离线纯计算），
//!   节假日/调休用 timor.tech API（现成服务）+ 本地文件缓存。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{Datelike, NaiveDate, Weekday};

/// 某日期的展示信息
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DayInfo {
    /// 农历日（"初一"、"十三" 等）
    pub lunar_day: String,
    /// 农历月（"正月"、"六月" 等；非初一为空）
    pub lunar_month: String,
    /// 节日名（如 "春节"、"中秋节"；无则为空）
    pub festival: String,
    /// 是否法定节假日（timor.tech holiday === true）
    pub is_holiday: bool,
    /// 是否休息日（法定节假日 或 普通周末）
    pub is_rest_day: bool,
    /// 是否调休工作日（周末补班，timor.tech holiday === false）
    pub is_workday: bool,
}

/// 日期信息提供者抽象
pub trait DayInfoProvider: Send + Sync {
    fn day_info(&self, date: NaiveDate) -> DayInfo;
}

/// 节假日服务：农历（chinese-lunisolar-calendar）+ timor.tech API + 文件缓存
pub struct HolidayService {
    client: reqwest::blocking::Client,
    cache_dir: PathBuf,
    /// 年 → { "MM-DD" → (holiday, name) }；holiday=false 表示调休班日
    year_cache: Mutex<HashMap<i32, HashMap<String, (bool, String)>>>,
}

impl HolidayService {
    /// 创建节假日服务；调休缓存存放于全局用户数据目录（`%APPDATA%/com.mdgo/schedule_cache`），与知识库无关
    pub fn new() -> Self {
        let cache_dir = dirs::data_dir()
            .map(|p| p.join("com.mdgo").join("schedule_cache"))
            .unwrap_or_else(|| PathBuf::from("com.mdgo").join("schedule_cache"));
        Self {
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            cache_dir,
            year_cache: Mutex::new(HashMap::new()),
        }
    }

    /// 测试用：指定缓存目录
    #[allow(dead_code)]
    pub fn with_cache_dir(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            cache_dir: cache_dir.into(),
            year_cache: Mutex::new(HashMap::new()),
        }
    }

    /// 获取某年调休数据；内存缓存 → 文件缓存 → 网络（失败降级为空映射）
    fn year_holidays(&self, year: i32) -> HashMap<String, (bool, String)> {
        if let Some(map) = self.year_cache.lock().unwrap().get(&year) {
            return map.clone();
        }
        // 文件缓存
        let cache_path = self.cache_dir.join(format!("holiday_cache_{}.json", year));
        if let Ok(raw) = std::fs::read_to_string(&cache_path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, (bool, String)>>(&raw) {
                self.year_cache.lock().unwrap().insert(year, map.clone());
                return map;
            }
        }
        // 网络拉取 timor.tech
        let url = format!("https://timor.tech/api/holiday/year/{}", year);
        let map = self
            .client
            .get(&url)
            .send()
            .ok()
            .and_then(|r| r.json::<serde_json::Value>().ok())
            .and_then(|v| v.get("holiday").cloned())
            .and_then(|h| {
                let mut m = HashMap::new();
                if let Some(obj) = h.as_object() {
                    for (date, item) in obj {
                        // date 形如 "2026-01-01"
                        if let Some(mmdd) = date.strip_prefix(&format!("{}-", year)) {
                            let holiday = item
                                .get("holiday")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let name = item
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            m.insert(mmdd.to_string(), (holiday, name));
                        }
                    }
                }
                Some(m)
            })
            .unwrap_or_default();
        // 写文件缓存
        if !map.is_empty() {
            if let Ok(json) = serde_json::to_string(&map) {
                let _ = std::fs::create_dir_all(&self.cache_dir);
                let _ = std::fs::write(&cache_path, json);
            }
        }
        self.year_cache.lock().unwrap().insert(year, map.clone());
        map
    }
}

impl DayInfoProvider for HolidayService {
    fn day_info(&self, date: NaiveDate) -> DayInfo {
        let mut info = DayInfo::default();
        // 农历（chinese-lunisolar-calendar，离线纯计算）
        if let Ok(lunisolar) = chinese_lunisolar_calendar::LunisolarDate::from_date(date) {
            let day_str = lunisolar.to_lunar_day().as_ref().to_string();
            info.lunar_day = day_str;
            if info.lunar_day == "初一" {
                info.lunar_month = format!("{:#}", lunisolar.to_lunar_month());
            }
        }
        // 节假日 / 调休
        let mmdd = format!("{:02}-{:02}", date.month(), date.day());
        if let Some((holiday, name)) = self.year_holidays(date.year()).get(&mmdd) {
            info.is_holiday = *holiday;
            if !name.is_empty() {
                info.festival = name.clone();
            }
            info.is_rest_day = *holiday;
            info.is_workday = !*holiday; // 调休班日
        }
        // 普通周末（未命中节假日数据时）
        let weekend = matches!(date.weekday(), Weekday::Sat | Weekday::Sun);
        if weekend && !info.is_workday {
            info.is_rest_day = true;
        }
        if !weekend && !info.is_holiday && !info.is_workday {
            info.is_rest_day = false;
        }
        info
    }
}

/// 无节假日数据的简化实现（测试 / 离线降级用）
#[allow(dead_code)]
pub struct SimpleDayInfo;

impl DayInfoProvider for SimpleDayInfo {
    fn day_info(&self, date: NaiveDate) -> DayInfo {
        let mut info = DayInfo::default();
        let weekend = matches!(date.weekday(), Weekday::Sat | Weekday::Sun);
        info.is_rest_day = weekend;
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_provider_marks_weekend() {
        let p = SimpleDayInfo;
        let sat = NaiveDate::from_ymd_opt(2026, 8, 15).unwrap(); // 周六
        let mon = NaiveDate::from_ymd_opt(2026, 8, 17).unwrap(); // 周一
        assert!(p.day_info(sat).is_rest_day);
        assert!(!p.day_info(mon).is_rest_day);
    }

    #[test]
    fn lunar_calculation_works() {
        // 直接验证农历计算本身（2026-08-13，仅验证输出非空）
        let lunisolar =
            chinese_lunisolar_calendar::LunisolarDate::from_date(NaiveDate::from_ymd_opt(2026, 8, 13).unwrap())
                .unwrap();
        assert!(!lunisolar.to_lunar_day().as_ref().is_empty());
    }

    #[test]
    fn holiday_service_reads_year_cache() {
        let dir = tempfile::tempdir().unwrap();
        let svc = HolidayService::with_cache_dir(dir.path().to_str().unwrap());
        // 未命中节假日/周末：8/15 周六为休息日
        let sat = svc.day_info(NaiveDate::from_ymd_opt(2026, 8, 15).unwrap());
        assert!(sat.is_rest_day);
    }
}
