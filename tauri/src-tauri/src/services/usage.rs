//! 使用行为统计（「文档库分析」页的数据底座）。
//!
//! 三张表，全部位于 `{知识库}/.mdgo/mdgo.db`（知识库级，随库目录同步）：
//!
//! | 表 | 粒度 | 用途 |
//! |---|---|---|
//! | `usage_daily` | 天 | KPI 汇总、趋势曲线、连续活跃天数、环比 |
//! | `usage_hourly` | 天 × 小时 × 事件类型 | 24 小时活跃时段堆叠图 |
//! | `usage_file_daily` | 天 × 文件 | 文件热度 TopN、目录树图、浏览/编辑散点、冷热分布 |
//!
//! # 为什么把「天」交给前端传
//!
//! 日期口径必须与用户看到的本地日历一致（`new Date()` 的本地日）。后端若自行按 UTC 取日，
//! 东八区凌晨 0–8 点的行为会被记到前一天。因此事件里带 `date`（`YYYY-MM-DD`）与 `hour`。
//!
//! # 幂等
//!
//! 全部写入走 `ON CONFLICT ... DO UPDATE SET x = x + excluded.x`：
//! 重复投递（前端重试、flush 重入）只会多算同一笔增量，不会覆盖历史累计值。

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::core::db::pool::DbPool;

/// 单次批量上限（防御异常调用）
const MAX_EVENTS: usize = 5000;
/// 单条事件数值上限（毫秒/次数）
const MAX_VALUE: i64 = 1_000_000_000;

// ─── 数据模型 ───

/// 一条待累计事件
#[derive(Debug, Clone, Deserialize)]
pub struct UsageEvent {
    /// `open` | `view_ms` | `edit` | `create` | `delete` | `ai`
    pub kind: String,
    /// 本地日期 `YYYY-MM-DD`
    pub date: String,
    /// 本地小时 0..=23
    #[serde(default)]
    pub hour: i64,
    /// 关联文件相对路径（仅 open/view_ms/edit 记到文件维度）
    #[serde(default)]
    pub path: Option<String>,
    /// 计数（次数）或毫秒（`view_ms`）
    pub n: i64,
    /// 事件时间戳（毫秒），用于「最近打开时间」
    #[serde(default)]
    pub ts: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageTotals {
    pub view_ms: i64,
    pub opens: i64,
    pub edits: i64,
    pub creates: i64,
    pub deletes: i64,
    pub ai_calls: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageDaily {
    pub date: String,
    #[serde(flatten)]
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageHour {
    pub hour: i64,
    pub kind: String,
    pub n: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageSummary {
    pub daily: Vec<UsageDaily>,
    pub hourly: Vec<UsageHour>,
    pub totals: UsageTotals,
    /// 上一等长周期总计（用于 ▲/▼ 环比）
    pub previous: UsageTotals,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageFileRow {
    pub path: String,
    pub opens: i64,
    pub view_ms: i64,
    pub edits: i64,
    pub last_open_ts: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageFileList {
    /// 按期区间内浏览时长倒序的前 N 个文件
    pub rows: Vec<UsageFileRow>,
    /// 区间内被打开过的**不重复文件数**（用于覆盖率，不受 limit 影响）
    pub opened_total: i64,
}

/// 写入结果
#[derive(Debug, Clone, Serialize)]
pub struct UsageRecordResult {
    pub applied: u32,
    pub skipped: u32,
}

// ─── 存储 ───

pub struct UsageStore {
    pool: DbPool,
}

impl UsageStore {
    pub fn new(db_dir_path: &str) -> Result<Self, String> {
        let db_path = std::path::PathBuf::from(db_dir_path).join("mdgo.db");
        let pool = DbPool::open(db_path)?;
        pool.with_write(Self::init_tables)?;
        Ok(Self { pool })
    }

    fn init_tables(conn: &mut Connection) -> Result<(), String> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage_daily (
                date       TEXT PRIMARY KEY,
                view_ms    INTEGER NOT NULL DEFAULT 0,
                opens      INTEGER NOT NULL DEFAULT 0,
                edits      INTEGER NOT NULL DEFAULT 0,
                creates    INTEGER NOT NULL DEFAULT 0,
                deletes    INTEGER NOT NULL DEFAULT 0,
                ai_calls   INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS usage_hourly (
                date TEXT NOT NULL,
                hour INTEGER NOT NULL,
                kind TEXT NOT NULL,
                n    INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (date, hour, kind)
            );
            CREATE TABLE IF NOT EXISTS usage_file_daily (
                date         TEXT NOT NULL,
                path         TEXT NOT NULL,
                opens        INTEGER NOT NULL DEFAULT 0,
                view_ms      INTEGER NOT NULL DEFAULT 0,
                edits        INTEGER NOT NULL DEFAULT 0,
                last_open_ts INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (date, path)
            );
            CREATE INDEX IF NOT EXISTS idx_usage_file_daily_path ON usage_file_daily(path);",
        )
        .map_err(|e| format!("初始化使用统计表失败: {}", e))
    }

    /// 批量累计事件。非法事件跳过而不是整批失败（避免一条脏数据让整个统计停摆）。
    pub fn record(
        &self,
        events: &[UsageEvent],
        prune_before: Option<&str>,
    ) -> Result<UsageRecordResult, String> {
        if events.is_empty() {
            return Ok(UsageRecordResult { applied: 0, skipped: 0 });
        }
        let events = if events.len() > MAX_EVENTS {
            &events[..MAX_EVENTS]
        } else {
            events
        };
        let now = unix_millis_now() as i64;
        let mut applied = 0u32;
        let mut skipped = 0u32;

        self.pool.with_write_txn(|conn| {
            for ev in events {
                let Some(kind) = parse_kind(&ev.kind) else {
                    skipped += 1;
                    continue;
                };
                let date = ev.date.trim();
                if !is_valid_date(date) {
                    skipped += 1;
                    continue;
                }
                let hour = ev.hour.clamp(0, 23);
                let n = ev.n.clamp(1, MAX_VALUE);
                let col = kind.daily_column();

                let daily_sql = format!(
                    "INSERT INTO usage_daily (date, {col}, updated_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT(date) DO UPDATE SET {col} = {col} + excluded.{col},
                                                    updated_at = excluded.updated_at"
                );
                conn.execute(&daily_sql, params![date, n, now])
                    .map_err(|e| format!("累计日统计失败: {}", e))?;

                // 时长不需要小时分布（时段图按「次数」看更直观）
                if kind != UsageKind::ViewMs {
                    conn.execute(
                        "INSERT INTO usage_hourly (date, hour, kind, n) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(date, hour, kind) DO UPDATE SET n = n + excluded.n",
                        params![date, hour, kind.as_str(), n],
                    )
                    .map_err(|e| format!("累计小时统计失败: {}", e))?;
                }

                if let Some(path) = ev
                    .path
                    .as_deref()
                    .map(str::trim)
                    .filter(|p| !p.is_empty() && p.len() <= 1024)
                {
                    if let Some(fcol) = kind.file_column() {
                        let ts = ev.ts.unwrap_or(now as u64).min(i64::MAX as u64) as i64;
                        let file_sql = format!(
                            "INSERT INTO usage_file_daily (date, path, {fcol}, last_open_ts)
                             VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT(date, path) DO UPDATE SET
                                {fcol} = {fcol} + excluded.{fcol},
                                last_open_ts = MAX(last_open_ts, excluded.last_open_ts)"
                        );
                        conn.execute(&file_sql, params![date, path, n, ts])
                            .map_err(|e| format!("累计文件统计失败: {}", e))?;
                    }
                }
                applied += 1;
            }

            // 保留期裁剪：只做一次 DELETE，走主键索引
            if let Some(before) = prune_before.map(str::trim).filter(|d| is_valid_date(d)) {
                let _ = conn.execute("DELETE FROM usage_daily WHERE date < ?1", params![before]);
                let _ = conn.execute("DELETE FROM usage_hourly WHERE date < ?1", params![before]);
                let _ = conn.execute("DELETE FROM usage_file_daily WHERE date < ?1", params![before]);
            }
            Ok(())
        })?;

        Ok(UsageRecordResult { applied, skipped })
    }

    /// 区间汇总（含上一等长周期，供环比）
    pub fn summary(
        &self,
        from: &str,
        to: &str,
        prev_from: &str,
        prev_to: &str,
    ) -> Result<UsageSummary, String> {
        let daily = self.pool.with_read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT date, view_ms, opens, edits, creates, deletes, ai_calls
                     FROM usage_daily WHERE date >= ?1 AND date <= ?2 ORDER BY date ASC",
                )
                .map_err(|e| format!("准备日统计查询失败: {}", e))?;
            let rows = stmt
                .query_map(params![from.trim(), to.trim()], |row| {
                    Ok(UsageDaily {
                        date: row.get(0)?,
                        totals: UsageTotals {
                            view_ms: row.get(1)?,
                            opens: row.get(2)?,
                            edits: row.get(3)?,
                            creates: row.get(4)?,
                            deletes: row.get(5)?,
                            ai_calls: row.get(6)?,
                        },
                    })
                })
                .map_err(|e| format!("查询日统计失败: {}", e))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| format!("读取日统计失败: {}", e))?);
            }
            Ok(out)
        })?;

        let hourly = self.pool.with_read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT hour, kind, SUM(n) FROM usage_hourly
                     WHERE date >= ?1 AND date <= ?2 GROUP BY hour, kind ORDER BY hour ASC",
                )
                .map_err(|e| format!("准备小时统计查询失败: {}", e))?;
            let rows = stmt
                .query_map(params![from.trim(), to.trim()], |row| {
                    Ok(UsageHour {
                        hour: row.get(0)?,
                        kind: row.get(1)?,
                        n: row.get(2)?,
                    })
                })
                .map_err(|e| format!("查询小时统计失败: {}", e))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| format!("读取小时统计失败: {}", e))?);
            }
            Ok(out)
        })?;

        let mut totals = UsageTotals::default();
        for d in &daily {
            totals.view_ms += d.totals.view_ms;
            totals.opens += d.totals.opens;
            totals.edits += d.totals.edits;
            totals.creates += d.totals.creates;
            totals.deletes += d.totals.deletes;
            totals.ai_calls += d.totals.ai_calls;
        }

        let previous = self.pool.with_read(|conn| {
            conn.query_row(
                "SELECT COALESCE(SUM(view_ms),0), COALESCE(SUM(opens),0), COALESCE(SUM(edits),0),
                        COALESCE(SUM(creates),0), COALESCE(SUM(deletes),0), COALESCE(SUM(ai_calls),0)
                 FROM usage_daily WHERE date >= ?1 AND date <= ?2",
                params![prev_from.trim(), prev_to.trim()],
                |row| {
                    Ok(UsageTotals {
                        view_ms: row.get(0)?,
                        opens: row.get(1)?,
                        edits: row.get(2)?,
                        creates: row.get(3)?,
                        deletes: row.get(4)?,
                        ai_calls: row.get(5)?,
                    })
                },
            )
            .map_err(|e| format!("查询上期统计失败: {}", e))
        })?;

        Ok(UsageSummary {
            daily,
            totals,
            previous,
            // 小时数据在 range 内已聚合
            hourly,
        })
    }

    /// 区间内按文件聚合（按时长倒序），并返回不重复打开文件数
    pub fn files(&self, from: &str, to: &str, limit: u32) -> Result<UsageFileList, String> {
        let limit = limit.clamp(1, 2000) as i64;
        let rows = self.pool.with_read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT path, SUM(opens), SUM(view_ms), SUM(edits), MAX(last_open_ts)
                     FROM usage_file_daily
                     WHERE date >= ?1 AND date <= ?2
                     GROUP BY path
                     ORDER BY SUM(view_ms) DESC, SUM(opens) DESC
                     LIMIT ?3",
                )
                .map_err(|e| format!("准备文件统计查询失败: {}", e))?;
            let mapped = stmt
                .query_map(params![from.trim(), to.trim(), limit], |row| {
                    Ok(UsageFileRow {
                        path: row.get(0)?,
                        opens: row.get(1)?,
                        view_ms: row.get(2)?,
                        edits: row.get(3)?,
                        last_open_ts: row.get(4)?,
                    })
                })
                .map_err(|e| format!("查询文件统计失败: {}", e))?;
            let mut out = Vec::new();
            for r in mapped {
                out.push(r.map_err(|e| format!("读取文件统计失败: {}", e))?);
            }
            Ok(out)
        })?;

        let opened_total: i64 = self.pool.with_read(|conn| {
            conn.query_row(
                "SELECT COUNT(DISTINCT path) FROM usage_file_daily WHERE date >= ?1 AND date <= ?2",
                params![from.trim(), to.trim()],
                |row| row.get(0),
            )
            .map_err(|e| format!("查询覆盖率失败: {}", e))
        })?;

        Ok(UsageFileList {
            rows,
            opened_total,
        })
    }
}

// ─── 内部工具 ───

#[derive(Clone, Copy, PartialEq, Eq)]
enum UsageKind {
    Open,
    ViewMs,
    Edit,
    Create,
    Delete,
    Ai,
}

impl UsageKind {
    fn as_str(self) -> &'static str {
        match self {
            UsageKind::Open => "open",
            UsageKind::ViewMs => "view_ms",
            UsageKind::Edit => "edit",
            UsageKind::Create => "create",
            UsageKind::Delete => "delete",
            UsageKind::Ai => "ai",
        }
    }

    /// 对应 `usage_daily` 的列名（**白名单**，不接受外部输入拼 SQL）
    fn daily_column(self) -> &'static str {
        match self {
            UsageKind::Open => "opens",
            UsageKind::ViewMs => "view_ms",
            UsageKind::Edit => "edits",
            UsageKind::Create => "creates",
            UsageKind::Delete => "deletes",
            UsageKind::Ai => "ai_calls",
        }
    }

    /// 需不需要记到文件维度
    fn file_column(self) -> Option<&'static str> {
        match self {
            UsageKind::Open => Some("opens"),
            UsageKind::ViewMs => Some("view_ms"),
            UsageKind::Edit => Some("edits"),
            _ => None,
        }
    }
}

fn parse_kind(raw: &str) -> Option<UsageKind> {
    match raw.trim() {
        "open" => Some(UsageKind::Open),
        "view_ms" => Some(UsageKind::ViewMs),
        "edit" => Some(UsageKind::Edit),
        "create" => Some(UsageKind::Create),
        "delete" => Some(UsageKind::Delete),
        "ai" => Some(UsageKind::Ai),
        _ => None,
    }
}

/// `YYYY-MM-DD`（只做形状校验；日期语义由前端保证，避免引入时区换算）
fn is_valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
}

fn unix_millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ─── 测试 ───

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, UsageStore) {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = UsageStore::new(&dir.path().to_string_lossy()).expect("打开存储");
        (dir, store)
    }

    fn ev(kind: &str, date: &str, hour: i64, n: i64, path: Option<&str>) -> UsageEvent {
        UsageEvent {
            kind: kind.into(),
            date: date.into(),
            hour,
            path: path.map(|p| p.to_string()),
            n,
            ts: None,
        }
    }

    #[test]
    fn records_and_summarizes() {
        let (_d, s) = store();
        s.record(
            &[
                ev("view_ms", "2026-11-14", 10, 60_000, Some("a.md")),
                ev("open", "2026-11-14", 10, 1, Some("a.md")),
                ev("open", "2026-11-14", 11, 1, Some("b.md")),
                ev("edit", "2026-11-15", 9, 2, Some("a.md")),
                ev("ai", "2026-11-15", 9, 3, None),
            ],
            None,
        )
        .expect("写入");

        let sum = s
            .summary("2026-11-01", "2026-11-30", "2026-10-02", "2026-11-01")
            .expect("汇总");
        assert_eq!(sum.daily.len(), 2);
        assert_eq!(sum.totals.view_ms, 60_000);
        assert_eq!(sum.totals.opens, 2);
        assert_eq!(sum.totals.edits, 2);
        assert_eq!(sum.totals.ai_calls, 3);
        assert_eq!(sum.previous.opens, 0);

        // 小时维度：view_ms 不入小时表；其余按 (hour, kind) 聚合
        let open10 = sum
            .hourly
            .iter()
            .find(|h| h.hour == 10 && h.kind == "open")
            .expect("10 点 open");
        assert_eq!(open10.n, 1);
        assert!(sum.hourly.iter().all(|h| h.kind != "view_ms"));
    }

    #[test]
    fn accumulates_same_day_and_same_file() {
        let (_d, s) = store();
        s.record(&[ev("view_ms", "2026-11-14", 10, 1_000, Some("a.md"))], None)
            .expect("写入1");
        s.record(&[ev("view_ms", "2026-11-14", 12, 2_000, Some("a.md"))], None)
            .expect("写入2");
        s.record(&[ev("open", "2026-11-14", 12, 1, Some("a.md"))], None)
            .expect("写入3");

        let sum = s.summary("2026-11-14", "2026-11-14", "", "").expect("汇总");
        assert_eq!(sum.totals.view_ms, 3_000);
        assert_eq!(sum.daily.len(), 1, "同一天必须只有一行");

        let files = s.files("2026-11-14", "2026-11-14", 10).expect("文件统计");
        assert_eq!(files.rows.len(), 1);
        assert_eq!(files.rows[0].view_ms, 3_000);
        assert_eq!(files.rows[0].opens, 1);
        assert_eq!(files.opened_total, 1);
    }

    #[test]
    fn range_filtering_and_previous_period() {
        let (_d, s) = store();
        s.record(
            &[
                ev("open", "2026-10-01", 8, 5, Some("old.md")),
                ev("open", "2026-11-10", 8, 2, Some("new.md")),
            ],
            None,
        )
        .expect("写入");

        let sum = s
            .summary("2026-11-01", "2026-11-30", "2026-10-01", "2026-10-31")
            .expect("汇总");
        assert_eq!(sum.totals.opens, 2);
        assert_eq!(sum.previous.opens, 5);
        assert_eq!(sum.daily.len(), 1);
    }

    #[test]
    fn invalid_events_are_skipped() {
        let (_d, s) = store();
        let res = s
            .record(
                &[
                    ev("nope", "2026-11-14", 1, 1, None),
                    ev("open", "2026-11-14x", 1, 1, None),
                    ev("open", "2026-11-14", 99, 1, None), // hour 会被 clamp
                    ev("open", "2026-11-14", 1, 0, None),  // n<=0 -> clamp 到 1
                ],
                None,
            )
            .expect("写入");
        assert_eq!(res.applied, 2);
        assert_eq!(res.skipped, 2);

        let sum = s.summary("2026-11-14", "2026-11-14", "", "").expect("汇总");
        assert_eq!(sum.totals.opens, 2);
        let h23 = sum
            .hourly
            .iter()
            .find(|x| x.kind == "open" && x.hour == 23)
            .expect("hour 被 clamp 到 23");
        assert_eq!(h23.n, 1);
        let h1 = sum
            .hourly
            .iter()
            .find(|x| x.kind == "open" && x.hour == 1)
            .expect("小时 1 的 open");
        assert_eq!(h1.n, 1);
    }

    #[test]
    fn files_are_sorted_by_duration_and_limited() {
        let (_d, s) = store();
        let mut events = Vec::new();
        for i in 0..5 {
            events.push(ev(
                "view_ms",
                "2026-11-14",
                10,
                (i as i64 + 1) * 1000,
                Some(&format!("f{}.md", i)),
            ));
        }
        s.record(&events, None).expect("写入");

        let files = s.files("2026-11-14", "2026-11-14", 2).expect("文件统计");
        assert_eq!(files.rows.len(), 2);
        assert_eq!(files.rows[0].path, "f4.md");
        assert_eq!(files.rows[1].path, "f3.md");
        assert_eq!(files.opened_total, 5, "opened_total 统计区间内不重复文件数（不受 limit 影响）");
    }

    #[test]
    fn prune_before_removes_old_rows() {
        let (_d, s) = store();
        s.record(
            &[
                ev("open", "2026-01-01", 8, 1, Some("old.md")),
                ev("open", "2026-11-14", 8, 1, Some("new.md")),
            ],
            Some("2026-06-01"),
        )
        .expect("写入");
        let sum = s.summary("2026-01-01", "2026-12-31", "", "").expect("汇总");
        assert_eq!(sum.daily.len(), 1);
        assert_eq!(sum.daily[0].date, "2026-11-14");
    }
}