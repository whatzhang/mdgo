//! 技能指标聚合模块
//!
//! 提供技能调度和执行的监控指标，包括命中率、成功率、耗时分布、错误码等。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use super::activation::ActivationSource;

/// 单次执行记录
#[derive(Debug, Clone)]
pub struct ExecutionRecord {
    pub skill_id: String,
    pub scope: String,
    pub timestamp: u64, // 毫秒时间戳
    pub duration_ms: u64,
    pub success: bool,
    pub error_code: Option<String>,
    pub source: ActivationSource,
    pub match_score: f32,
}

/// 技能指标摘要
#[derive(Debug, Clone, Serialize)]
pub struct SkillMetricsSummary {
    /// 技能 ID
    pub skill_id: String,
    /// 作用域
    pub scope: String,
    /// 总调用次数
    pub total_calls: u64,
    /// 成功次数
    pub success_count: u64,
    /// 失败次数
    pub failure_count: u64,
    /// 成功率 (0.0 - 1.0)
    pub success_rate: f32,
    /// 平均耗时 (ms)
    pub avg_duration_ms: f64,
    /// P50 耗时 (ms)
    pub p50_duration_ms: f64,
    /// P95 耗时 (ms)
    pub p95_duration_ms: f64,
    /// 错误码分布
    pub error_codes: HashMap<String, u64>,
    /// 激活来源分布（attached / manual / llm）
    pub activation_sources: HashMap<String, u64>,
    /// 平均匹配分数
    pub avg_match_score: f32,
    /// 最后调用时间 (毫秒时间戳)
    pub last_call_at: u64,
}

/// 全局指标聚合
#[derive(Debug, Clone, Serialize)]
pub struct GlobalMetricsSummary {
    /// 总调度次数
    pub total_dispatches: u64,
    /// 命中技能的调度次数
    pub matched_dispatches: u64,
    /// 调度命中率 (0.0 - 1.0)
    pub dispatch_hit_rate: f32,
    /// 总执行次数
    pub total_executions: u64,
    /// 总成功次数
    pub total_successes: u64,
    /// 总失败次数
    pub total_failures: u64,
    /// 全局成功率 (0.0 - 1.0)
    pub global_success_rate: f32,
    /// 全局平均耗时 (ms)
    pub global_avg_duration_ms: f64,
    /// 各技能指标
    pub skills: Vec<SkillMetricsSummary>,
    /// 统计时间范围起始 (毫秒时间戳)
    pub since: u64,
    /// 统计时间范围结束 (毫秒时间戳)
    pub until: u64,
}

/// 技能指标收集器
pub struct SkillMetrics {
    /// 执行记录环形缓冲（保留最近 10000 条）
    records: RwLock<Vec<ExecutionRecord>>,
    /// 全局调度统计
    total_dispatches: AtomicU64,
    matched_dispatches: AtomicU64,
}

impl SkillMetrics {
    pub fn new() -> Self {
        Self {
            records: RwLock::new(Vec::new()),
            total_dispatches: AtomicU64::new(0),
            matched_dispatches: AtomicU64::new(0),
        }
    }

    /// 记录一次调度
    pub fn record_dispatch(&self, matched: bool) {
        self.total_dispatches.fetch_add(1, Ordering::Relaxed);
        if matched {
            self.matched_dispatches.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 记录一次执行
    pub fn record_execution(
        &self,
        skill_id: String,
        scope: String,
        duration_ms: u64,
        success: bool,
        error_code: Option<String>,
        source: ActivationSource,
        match_score: f32,
    ) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let record = ExecutionRecord {
            skill_id,
            scope,
            timestamp: now,
            duration_ms,
            success,
            error_code,
            source,
            match_score,
        };

        let mut records = self.records.write().unwrap_or_else(|e| e.into_inner());
        records.push(record);

        // 保留最近 10000 条
        if records.len() > 10000 {
            let drain_count = records.len() - 10000;
            records.drain(0..drain_count);
        }
    }

    /// 获取聚合指标
    pub fn get_summary(&self, skill_id: Option<&str>, since: Option<u64>) -> GlobalMetricsSummary {
        let records = self.records.read().unwrap_or_else(|e| e.into_inner());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let since_ts = since.unwrap_or(0);
        let until_ts = now;

        // 过滤时间范围内的记录
        let filtered: Vec<_> = records
            .iter()
            .filter(|r| r.timestamp >= since_ts)
            .filter(|r| skill_id.map_or(true, |id| r.skill_id == id))
            .cloned()
            .collect();

        // 按技能分组
        let mut skill_groups: HashMap<String, Vec<ExecutionRecord>> = HashMap::new();
        for record in filtered {
            let key = format!("{}:{}", record.scope, record.skill_id);
            skill_groups.entry(key).or_default().push(record);
        }

        // 计算各技能指标（聚合逻辑收敛在 summarize_skill，本方法只负责编排）
        let mut skills: Vec<SkillMetricsSummary> = skill_groups
            .into_iter()
            .map(|(key, records)| summarize_skill(key, records))
            .collect();

        // 按总调用次数降序排序
        skills.sort_by(|a, b| b.total_calls.cmp(&a.total_calls));

        // 全局统计
        let total_dispatches = self.total_dispatches.load(Ordering::Relaxed);
        let matched_dispatches = self.matched_dispatches.load(Ordering::Relaxed);
        let dispatch_hit_rate = if total_dispatches > 0 {
            matched_dispatches as f32 / total_dispatches as f32
        } else {
            0.0
        };

        let total_executions: u64 = skills.iter().map(|s| s.total_calls).sum();
        let total_successes: u64 = skills.iter().map(|s| s.success_count).sum();
        let total_failures: u64 = skills.iter().map(|s| s.failure_count).sum();
        let global_success_rate = if total_executions > 0 {
            total_successes as f32 / total_executions as f32
        } else {
            0.0
        };

        let global_avg_duration = if !skills.is_empty() {
            skills.iter().map(|s| s.avg_duration_ms).sum::<f64>() / skills.len() as f64
        } else {
            0.0
        };

        GlobalMetricsSummary {
            total_dispatches,
            matched_dispatches,
            dispatch_hit_rate,
            total_executions,
            total_successes,
            total_failures,
            global_success_rate,
            global_avg_duration_ms: global_avg_duration,
            skills,
            since: since_ts,
            until: until_ts,
        }
    }
}

/// 聚合单个 `scope:skill_id` 分组的执行记录为指标摘要。
///
/// 从 `get_summary` 抽出，职责单一（只负责一个技能组的统计计算）。
fn summarize_skill(key: String, records: Vec<ExecutionRecord>) -> SkillMetricsSummary {
    let total = records.len() as u64;
    let success_count = records.iter().filter(|r| r.success).count() as u64;
    let failure_count = total - success_count;
    let success_rate = if total > 0 {
        success_count as f32 / total as f32
    } else {
        0.0
    };

    // 耗时统计
    let mut durations: Vec<u64> = records.iter().map(|r| r.duration_ms).collect();
    durations.sort();
    let avg_duration = if !durations.is_empty() {
        durations.iter().sum::<u64>() as f64 / durations.len() as f64
    } else {
        0.0
    };
    let p50 = percentile(&durations, 50.0);
    let p95 = percentile(&durations, 95.0);

    // 错误码分布
    let mut error_codes: HashMap<String, u64> = HashMap::new();
    for record in &records {
        if let Some(ref code) = record.error_code {
            *error_codes.entry(code.clone()).or_insert(0) += 1;
        }
    }

    // 激活来源分布（复用 ActivationSource::as_str，与序列化输出大小写一致）
    let mut activation_sources: HashMap<String, u64> = HashMap::new();
    for record in &records {
        let level_str = record.source.as_str();
        *activation_sources.entry(level_str.to_string()).or_insert(0) += 1;
    }

    // 平均匹配分数
    let avg_score = if !records.is_empty() {
        records.iter().map(|r| r.match_score).sum::<f32>() / records.len() as f32
    } else {
        0.0
    };

    // 最后调用时间
    let last_call = records.iter().map(|r| r.timestamp).max().unwrap_or(0);

    // 解析 skill_id 和 scope
    let parts: Vec<_> = key.splitn(2, ':').collect();
    let (scope, skill_id) = if parts.len() == 2 {
        (parts[0].to_string(), parts[1].to_string())
    } else {
        (String::new(), key)
    };

    SkillMetricsSummary {
        skill_id,
        scope,
        total_calls: total,
        success_count,
        failure_count,
        success_rate,
        avg_duration_ms: avg_duration,
        p50_duration_ms: p50,
        p95_duration_ms: p95,
        error_codes,
        activation_sources,
        avg_match_score: avg_score,
        last_call_at: last_call,
    }
}

/// 计算百分位数
fn percentile(sorted_data: &[u64], p: f64) -> f64 {
    if sorted_data.is_empty() {
        return 0.0;
    }
    if sorted_data.len() == 1 {
        return sorted_data[0] as f64;
    }

    let rank = (p / 100.0) * (sorted_data.len() - 1) as f64;
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    let frac = rank - lower as f64;

    if upper >= sorted_data.len() {
        sorted_data[sorted_data.len() - 1] as f64
    } else {
        sorted_data[lower] as f64 * (1.0 - frac) + sorted_data[upper] as f64 * frac
    }
}

impl Default for SkillMetrics {
    fn default() -> Self {
        Self::new()
    }
}
