//! 技能指标聚合模块
//!
//! 提供技能调度和执行的监控指标，包括命中率、成功率、耗时分布、错误码等。
//!
//! 设计：**SQLite 为最终事实源**（按目录写入各自 `.mdgo/mdgo.db`）。
//! - 调度计数（`record_dispatch` / `record_dispatch_matched`）：先累加内存，
//!   每次请求结束统一批量落库（与执行明细同批），并设阈值兜底，
//!   将单请求 2~3 次写合并为 1 次，降低与 ChatStore / AiHistoryStore 的写锁竞争
//! - 执行明细：`record_execution_batch` 单事务批量落库
//! - 读取：`get_summary` 读取前先冲刷待落库计数，保证口径一致
//! - 明细仅含执行元数据（耗时 / 结果 / 来源 / 错误码），不记录入参出参
//! - 连接按目录缓存复用（`conns`），避免每次调用重开连接与全量 DDL

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, TransactionBehavior};
use serde::Serialize;

use super::activation::ActivationSource;

/// 单次执行记录（聚合中间态，由 DB 行读出）
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

/// 一次批量执行输入（由命令层收集后经 spawn_blocking 落库）
pub struct ExecInput {
    pub skill_id: String,
    pub scope: String,
    pub source: ActivationSource,
    pub match_score: f32,
    /// 该技能自激活到请求结束的耗时（按技能独立计时）
    pub duration_ms: u64,
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

/// 待落库的调度计数（内存累积态，批量 flush，降低写锁竞争）
#[derive(Default, Clone, Copy)]
struct PendingDispatch {
    total: u64,
    matched: u64,
}

/// 技能指标收集器（全局单例；调度计数内存累积 + 批量落库，执行明细直写）
pub struct SkillMetrics {
    /// 按目录复用的 SQLite 连接（每目录一把锁，不同目录读写互不阻塞；首次打开时建表）
    conns: Mutex<HashMap<String, Arc<Mutex<Connection>>>>,
    /// 各目录最近一次过期明细清理时刻（毫秒），用于节流 90 天过期清理
    last_cleanup: Mutex<HashMap<String, i64>>,
    /// 各目录待落库的调度计数（record_dispatch 高频调用先累加内存，请求结束或达阈值时统一落库）
    pending: Mutex<HashMap<String, PendingDispatch>>,
}

/// 调度计数批量落库阈值：内存累积达到该值即触发一次写，避免异常中断路径长期滞留
const DISPATCH_FLUSH_THRESHOLD: u64 = 16;

impl SkillMetrics {
    pub fn new() -> Self {
        Self {
            conns: Mutex::new(HashMap::new()),
            last_cleanup: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// 在指定目录连接上执行闭包（首次打开并初始化表结构，之后复用缓存连接）。
    ///
    /// 采用「外层 map 锁仅做连接查找/插入，释放后取每目录连接锁执行 SQL」的两级锁，
    /// 避免不同目录的读写被同一把全局锁串行化。
    fn with_conn<T>(
        &self,
        dir_path: &str,
        f: impl FnOnce(&mut Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        let conn = {
            let mut guard = self
                .conns
                .lock()
                .map_err(|e| format!("技能指标连接锁失效: {}", e))?;
            match guard.get(dir_path) {
                Some(c) => Arc::clone(c),
                None => {
                    let db_dir = Path::new(dir_path).join(".mdgo");
                    std::fs::create_dir_all(&db_dir)
                        .map_err(|e| format!("创建数据库目录失败: {}", e))?;
                    let conn = Connection::open(db_dir.join("mdgo.db"))
                        .map_err(|e| format!("打开技能指标数据库失败: {}", e))?;
                    conn.execute_batch("PRAGMA journal_mode=WAL;")
                        .map_err(|e| format!("启用 WAL 失败: {}", e))?;
                    // ChatStore / AiHistoryStore / Indexer 均以独立连接打开同一 mdgo.db，
                    // WAL 下写写互斥；必须设置忙等待，否则并发写直接 SQLITE_BUSY 丢指标
                    conn.execute_batch("PRAGMA busy_timeout=5000;")
                        .map_err(|e| format!("设置 busy_timeout 失败: {}", e))?;
                    crate::core::db::schema::init_all(&conn)?;
                    let shared = Arc::new(Mutex::new(conn));
                    guard.insert(dir_path.to_string(), Arc::clone(&shared));
                    shared
                }
            }
        };
        let mut guard = conn.lock().map_err(|e| format!("技能指标连接锁失效: {}", e))?;
        f(&mut guard)
    }

    /// 记录一次调度（matched = 是否命中技能）。
    ///
    /// 调度计数采用「起始计总数 + 结束补命中」两段式：
    /// - 请求起始调用本方法（matched 传 false，仅 total 自增，不阻塞请求主链路）；
    /// - 请求结束时若有技能实际激活，由 `record_dispatch_matched` 补记命中。
    /// 命中判定因此覆盖「预激活 ∪ LLM 动态激活」，避免 LLM 激活被计为 miss。
    ///
    /// 高频路径先累加内存（`pending`），请求结束或达阈值时统一落库，
    /// 将单请求 2~3 次写合并为 1 次，降低与 ChatStore / AiHistoryStore 的写锁竞争。
    pub fn record_dispatch(&self, dir_path: &str, matched: bool) {
        {
            let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let p = guard.entry(dir_path.to_string()).or_default();
            p.total += 1;
            if matched {
                p.matched += 1;
            }
        }
        if let Some(pending) = self.take_pending_dispatch(dir_path, DISPATCH_FLUSH_THRESHOLD) {
            self.flush_dispatch_pending(dir_path, pending);
        }
    }

    /// 请求结束时补记一次命中（matched_dispatches + 1），修正命中率口径。
    pub fn record_dispatch_matched(&self, dir_path: &str) {
        {
            let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            guard.entry(dir_path.to_string()).or_default().matched += 1;
        }
        if let Some(pending) = self.take_pending_dispatch(dir_path, DISPATCH_FLUSH_THRESHOLD) {
            self.flush_dispatch_pending(dir_path, pending);
        }
    }

    /// 取出指定目录达到阈值的待落库计数（未达阈值返回 None；取出后内存归零）。
    fn take_pending_dispatch(&self, dir_path: &str, threshold: u64) -> Option<PendingDispatch> {
        let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let p = guard.entry(dir_path.to_string()).or_default();
        if p.total >= threshold {
            let taken = *p;
            *p = PendingDispatch::default();
            Some(taken)
        } else {
            None
        }
    }

    /// 将指定目录的待落库调度计数写入 DB（单事务幂等累加）。
    fn flush_dispatch_pending(&self, dir_path: &str, pending: PendingDispatch) {
        if pending.total == 0 {
            return;
        }
        let r = self.with_conn(dir_path, |conn| -> Result<(), String> {
            // 写事务 IMMEDIATE：WAL 下避免 DEFERRED 读快照升级失败的 SQLITE_BUSY_SNAPSHOT
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT OR IGNORE INTO skill_dispatch_stats (id, total_dispatches, matched_dispatches) VALUES (1, 0, 0)",
                [],
            )
            .map_err(|e| e.to_string())?;
            tx.execute(
                "UPDATE skill_dispatch_stats SET
                     total_dispatches = total_dispatches + ?1,
                     matched_dispatches = matched_dispatches + ?2
                 WHERE id = 1",
                params![pending.total as i64, pending.matched as i64],
            )
            .map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())
        });
        if let Err(e) = r {
            log::error!("[skill-metrics] 调度统计落盘失败: {}", e);
        }
    }

    /// 批量记录一次请求的执行明细（单事务；每技能一行），并节流清理 90 天前过期明细。
    pub fn record_execution_batch(
        &self,
        dir_path: &str,
        inputs: Vec<ExecInput>,
        success: bool,
        error_code: Option<&str>,
    ) {
        // 每次请求结束统一冲刷该目录待落库调度计数（无论是否有技能激活），
        // 将本请求生命周期内的 2~3 次调度写合并为 1 次落库
        if let Some(pending) = self.take_pending_dispatch(dir_path, 1) {
            self.flush_dispatch_pending(dir_path, pending);
        }
        if inputs.is_empty() {
            return;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        // 过期清理节流：每目录每小时至多执行一次，避免每次写入都附带范围 DELETE
        let need_cleanup = {
            let mut guard = self.last_cleanup.lock().unwrap_or_else(|e| e.into_inner());
            let last = guard.get(dir_path).copied().unwrap_or(0);
            if now - last >= 3600_000 {
                guard.insert(dir_path.to_string(), now);
                true
            } else {
                false
            }
        };
        let r = self.with_conn(dir_path, |conn| -> Result<(), String> {
            // 写事务 IMMEDIATE：WAL 下避免 DEFERRED 读快照升级失败的 SQLITE_BUSY_SNAPSHOT
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|e| e.to_string())?;
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO skill_exec_metrics
                            (request_id, scope, skill_id, match_level, score, state, duration_ms, error_code, created_at)
                         VALUES ('', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    )
                    .map_err(|e| e.to_string())?;
                for inp in inputs {
                    stmt.execute(params![
                        inp.scope,
                        inp.skill_id,
                        inp.source.as_str(),
                        inp.match_score,
                        if success { "success" } else { "failed" },
                        inp.duration_ms as i64,
                        error_code,
                        now,
                    ])
                    .map_err(|e| e.to_string())?;
                }
            }
            if need_cleanup {
                tx.execute(
                    "DELETE FROM skill_exec_metrics WHERE created_at < ?1",
                    params![now - 90 * 24 * 3600 * 1000_i64],
                )
                .map_err(|e| e.to_string())?;
            }
            tx.commit().map_err(|e| e.to_string())
        });
        if let Err(e) = r {
            log::error!("[skill-metrics] 执行明细落盘失败: {}", e);
        }
    }

    /// 获取聚合指标：直接按目录从 DB 聚合（最近 50 条执行明细 + 调度计数）。
    ///
    /// 只统计「最近 50 条」执行明细（看板窗口需求）；since 过滤在 SQL 内完成。
    pub fn get_summary(
        &self,
        dir_path: &str,
        skill_id: Option<&str>,
        since: Option<u64>,
    ) -> GlobalMetricsSummary {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let since_ts = since.unwrap_or(0);
        let until_ts = now;

        // 执行明细：每个技能各自取「最近 50 条」（窗口按技能独立，避免技能互相挤出）。
        // 用 ROW_NUMBER 分区窗口实现，since 过滤在 SQL 内完成。
        let records: Vec<ExecutionRecord> = self
            .with_conn(dir_path, |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT scope, skill_id, match_level, score, state, duration_ms, error_code, created_at
                         FROM (
                             SELECT scope, skill_id, match_level, score, state, duration_ms, error_code, created_at,
                                    ROW_NUMBER() OVER (PARTITION BY scope, skill_id ORDER BY created_at DESC) AS rn
                             FROM skill_exec_metrics
                             WHERE created_at >= ?1 AND (?2 IS NULL OR skill_id = ?2)
                         )
                         WHERE rn <= 50
                         ORDER BY created_at DESC",
                    )
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(params![since_ts as i64, skill_id], |row| {
                        Ok(ExecutionRecord {
                            skill_id: row.get(1)?,
                            scope: row.get(0)?,
                            timestamp: row.get::<_, i64>(7)? as u64,
                            duration_ms: row.get::<_, i64>(5)?.max(0) as u64,
                            success: matches!(row.get::<_, String>(4)?.as_str(), "success"),
                            error_code: row.get(6)?,
                            source: ActivationSource::from_str(&row.get::<_, String>(2)?),
                            match_score: row.get::<_, f64>(3)? as f32,
                        })
                    })
                    .map_err(|e| e.to_string())?;
                Ok(rows.filter_map(Result::ok).collect())
            })
            .unwrap_or_else(|e| {
                log::error!("[skill-metrics] 指标读取失败: {}", e);
                Vec::new()
            });

        // 全局平均耗时：按明细加权（sum/count），而非各技能均值的简单平均，避免被低频技能稀释
        let global_avg_duration = if !records.is_empty() {
            records.iter().map(|r| r.duration_ms as f64).sum::<f64>() / records.len() as f64
        } else {
            0.0
        };

        // 读取前先冲刷待落库调度计数，保证聚合口径与已落库数据一致
        if let Some(pending) = self.take_pending_dispatch(dir_path, 1) {
            self.flush_dispatch_pending(dir_path, pending);
        }

        // 调度计数（来自 DB，重启后仍准确）
        let (total_dispatches, matched_dispatches) = self
            .with_conn(dir_path, |conn| {
                conn.query_row(
                    "SELECT total_dispatches, matched_dispatches FROM skill_dispatch_stats WHERE id = 1",
                    [],
                    |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
                )
                .map_err(|e| e.to_string())
            })
            .unwrap_or_else(|e| {
                log::warn!("[skill-metrics] 调度计数读取失败: {}", e);
                (0, 0)
            });
        let dispatch_hit_rate = if total_dispatches > 0 {
            matched_dispatches as f32 / total_dispatches as f32
        } else {
            0.0
        };

        // 按技能分组
        let mut skill_groups: HashMap<String, Vec<ExecutionRecord>> = HashMap::new();
        for record in records {
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

        let total_executions: u64 = skills.iter().map(|s| s.total_calls).sum();
        let total_successes: u64 = skills.iter().map(|s| s.success_count).sum();
        let total_failures: u64 = skills.iter().map(|s| s.failure_count).sum();
        let global_success_rate = if total_executions > 0 {
            total_successes as f32 / total_executions as f32
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
