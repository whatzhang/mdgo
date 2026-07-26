use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ─── 常量 ───

/// 非收藏记录保留上限
const MAX_RECORDS: i64 = 1000;
/// 收藏记录保留上限
const MAX_FAVORITE_RECORDS: i64 = 200;

// ─── 数据模型 ───

#[derive(Debug, Serialize, Clone)]
pub struct AiHistoryItem {
    pub id: String,
    pub r#type: String,
    pub label: String,
    pub prompt: String,
    pub result: String,
    pub file_name: String,
    pub file_path: String,
    pub created_at: u64,
    pub last_access_at: u64,
    pub favorite: bool,
    pub token_count: i32,
    pub prompt_length: i32,
    pub result_length: i32,
}

#[derive(Debug, Deserialize)]
pub struct AddAiHistoryRequest {
    pub r#type: String,
    pub label: String,
    pub prompt: String,
    pub result: String,
    pub file_name: String,
    pub file_path: String,
    pub token_count: i32,
}

/// AI 操作统计（供知识库面板使用）
#[derive(Debug, Serialize)]
pub struct AiHistoryStats {
    pub total_count: u32,
    pub favorite_count: u32,
    pub count_by_type: Vec<TypeCount>,
    pub daily_trend: Vec<DailyCount>,
    pub top_files: Vec<FileCount>,
    pub total_token_usage: u64,
}

#[derive(Debug, Serialize)]
pub struct TypeCount {
    pub r#type: String,
    pub count: u32,
}

#[derive(Debug, Serialize)]
pub struct DailyCount {
    pub date: String,
    pub count: u32,
}

#[derive(Debug, Serialize)]
pub struct FileCount {
    pub file_name: String,
    pub file_path: String,
    pub count: u32,
}

// ─── 存储服务 ───

pub struct AiHistoryStore {
    /// 使用 Mutex 保证线程安全。
    /// 注意：rusqlite::Connection 是 !Sync，无法使用 RwLock，
    /// 但 SQLite 的 WAL 模式可支持多读一写。数据量上限 ~1200 条，单个 Mutex 足够。
    conn: Mutex<Connection>,
}

impl AiHistoryStore {
    /// 创建新的 AiHistoryStore，自动创建数据库目录和表
    pub fn new(db_dir_path: &str) -> Result<Self, String> {
        let db_path = Path::new(db_dir_path)
            .join("mdgo.db")
            .to_string_lossy()
            .to_string();

        if let Some(parent) = Path::new(&db_path).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建数据库目录失败: {}", e))?;
        }

        let conn = Connection::open(&db_path)
            .map_err(|e| format!("打开数据库失败: {}", e))?;
        // 启用 WAL 模式，支持多连接并发读写（与 ChatStore 共享同一文件）
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| format!("启用 WAL 模式失败: {}", e))?;
        // 启用外键约束（每个连接独立设置，保持与 ChatStore 一致）
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| format!("启用外键约束失败: {}", e))?;
        Self::init_tables(&conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn init_tables(conn: &Connection) -> Result<(), String> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS ai_history (
                id              TEXT PRIMARY KEY,
                type            TEXT NOT NULL,
                label           TEXT NOT NULL DEFAULT '',
                prompt          TEXT NOT NULL DEFAULT '',
                result          TEXT NOT NULL DEFAULT '',
                file_name       TEXT NOT NULL DEFAULT '',
                file_path       TEXT NOT NULL DEFAULT '',
                created_at      INTEGER NOT NULL,
                last_access_at  INTEGER NOT NULL DEFAULT 0,
                favorite        INTEGER NOT NULL DEFAULT 0,
                token_count     INTEGER NOT NULL DEFAULT 0,
                prompt_length   INTEGER NOT NULL DEFAULT 0,
                result_length   INTEGER NOT NULL DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS idx_ai_history_type ON ai_history(type);
            CREATE INDEX IF NOT EXISTS idx_ai_history_favorite ON ai_history(favorite);
            CREATE INDEX IF NOT EXISTS idx_ai_history_created_at ON ai_history(created_at);
            CREATE INDEX IF NOT EXISTS idx_ai_history_file_path ON ai_history(file_path);

            -- LRU 淘汰复合索引：覆盖 favorite + last_access_at + created_at，避免 ORDER BY 临时文件排序
            CREATE INDEX IF NOT EXISTS idx_ai_history_lru_evict
                ON ai_history(favorite, last_access_at, created_at);
            ",
        )
        .map_err(|e| format!("建表失败: {}", e))?;
        Ok(())
    }

    // ─── CRUD ───

    /// 添加一条 AI 操作记录。
    ///
    /// 当非收藏记录超过上限 `MAX_RECORDS` 时，自动淘汰最早访问的记录（LRU 策略）。
    /// 收藏记录同理，使用 `MAX_FAVORITE_RECORDS` 作为上限。
    pub fn add(&self, item: &AddAiHistoryRequest) -> Result<AiHistoryItem, String> {
        let now = unix_timestamp_now();
        let id = Uuid::new_v4().to_string();
        let prompt_len = item.prompt.len() as i32;
        let result_len = item.result.len() as i32;

        let conn = self.conn.lock().map_err(|e| e.to_string())?;

        conn.execute(
            "INSERT INTO ai_history (id, type, label, prompt, result, file_name, file_path, created_at, last_access_at, favorite, token_count, prompt_length, result_length)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, ?11, ?12)",
            rusqlite::params![
                id, item.r#type, item.label, item.prompt, item.result,
                item.file_name, item.file_path, now, now,
                item.token_count, prompt_len, result_len
            ],
        )
        .map_err(|e| format!("插入记录失败: {}", e))?;

        // LRU 淘汰：非收藏记录超过上限时删除最旧的
        // 复合索引 idx_ai_history_lru_evict 使 ORDER BY 走索引，无需临时排序
        let non_fav_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ai_history WHERE favorite = 0",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if non_fav_count > MAX_RECORDS {
            let excess = non_fav_count - MAX_RECORDS;
            let _ = conn.execute(
                "DELETE FROM ai_history WHERE id IN (
                    SELECT id FROM ai_history WHERE favorite = 0 ORDER BY last_access_at ASC, created_at ASC LIMIT ?1
                )",
                rusqlite::params![excess],
            );
        }

        // 收藏记录淘汰
        let fav_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ai_history WHERE favorite = 1",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if fav_count > MAX_FAVORITE_RECORDS {
            let excess = fav_count - MAX_FAVORITE_RECORDS;
            let _ = conn.execute(
                "DELETE FROM ai_history WHERE id IN (
                    SELECT id FROM ai_history WHERE favorite = 1 ORDER BY last_access_at ASC, created_at ASC LIMIT ?1
                )",
                rusqlite::params![excess],
            );
        }

        Ok(AiHistoryItem {
            id,
            r#type: item.r#type.clone(),
            label: item.label.clone(),
            prompt: item.prompt.clone(),
            result: item.result.clone(),
            file_name: item.file_name.clone(),
            file_path: item.file_path.clone(),
            created_at: now,
            last_access_at: now,
            favorite: false,
            token_count: item.token_count,
            prompt_length: prompt_len,
            result_length: result_len,
        })
    }

    /// 列出 AI 历史记录，按创建时间降序
    pub fn list(&self, limit: u32, offset: u32) -> Result<Vec<AiHistoryItem>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT id, type, label, prompt, result, file_name, file_path, created_at, last_access_at, favorite, token_count, prompt_length, result_length
                 FROM ai_history ORDER BY created_at DESC LIMIT ?1 OFFSET ?2",
            )
            .map_err(|e| format!("查询失败: {}", e))?;

        let items = stmt
            .query_map(rusqlite::params![limit, offset], map_row)
            .map_err(|e| format!("查询失败: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取失败: {}", e))?;
        Ok(items)
    }

    /// 删除指定记录
    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let affected = conn
            .execute("DELETE FROM ai_history WHERE id = ?1", rusqlite::params![id])
            .map_err(|e| format!("删除失败: {}", e))?;
        Ok(affected > 0)
    }

    /// 切换收藏状态，返回新状态。
    ///
    /// 兼容 SQLite 3.35+（RETURNING）和更早版本（单独 SELECT）。
    pub fn toggle_favorite(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;

        // 先尝试 RETURNING 语法（SQLite 3.35+）
        let result: Result<i32, _> = conn.query_row(
            "UPDATE ai_history SET favorite = CASE WHEN favorite = 0 THEN 1 ELSE 0 END
             WHERE id = ?1 RETURNING favorite",
            rusqlite::params![id],
            |row| row.get(0),
        );

        match result {
            Ok(fav) => Ok(fav != 0),
            Err(e) => {
                // RETURNING 失败（可能是旧版 SQLite），降级为 UPDATE + SELECT
                log::warn!("[ai_history] RETURNING 语法失败，降级为两阶段更新: {}", e);
                conn.execute(
                    "UPDATE ai_history SET favorite = CASE WHEN favorite = 0 THEN 1 ELSE 0 END WHERE id = ?1",
                    rusqlite::params![id],
                )
                .map_err(|e| format!("切换收藏失败: {}", e))?;

                let new_fav: i32 = conn
                    .query_row(
                        "SELECT favorite FROM ai_history WHERE id = ?1",
                        rusqlite::params![id],
                        |row| row.get(0),
                    )
                    .map_err(|e| format!("查询收藏状态失败: {}", e))?;
                Ok(new_fav != 0)
            }
        }
    }

    /// 更新最后访问时间
    pub fn update_access_time(&self, id: &str) -> Result<(), String> {
        let now = unix_timestamp_now();
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE ai_history SET last_access_at = ?1 WHERE id = ?2",
            rusqlite::params![now, id],
        )
        .map_err(|e| format!("更新时间失败: {}", e))?;
        Ok(())
    }

    /// 文件重命名时同步更新关联的 file_name 和 file_path
    pub fn update_file_path(
        &self,
        old_file_path: &str,
        new_file_name: &str,
        new_file_path: &str,
    ) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE ai_history SET file_name = ?1, file_path = ?2 WHERE file_path = ?3",
            rusqlite::params![new_file_name, new_file_path, old_file_path],
        )
        .map_err(|e| format!("更新文件路径失败: {}", e))?;
        Ok(())
    }

    // ─── 统计查询（供知识库面板使用）───

    /// 获取 AI 操作统计摘要。
    ///
    /// 包含总数、收藏数、按类型分布、近 30 日趋势、最热文件 Top 10 和总 token 用量。
    pub fn get_stats(&self) -> Result<AiHistoryStats, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;

        let total_count: u32 = conn
            .query_row("SELECT COUNT(*) FROM ai_history", [], |row| row.get(0))
            .unwrap_or(0);

        let favorite_count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM ai_history WHERE favorite = 1",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // 按类型分布
        let mut stmt = conn
            .prepare(
                "SELECT type, COUNT(*) as cnt FROM ai_history GROUP BY type ORDER BY cnt DESC",
            )
            .map_err(|e| format!("统计查询失败: {}", e))?;
        let count_by_type = stmt
            .query_map([], |row| {
                Ok(TypeCount {
                    r#type: row.get(0)?,
                    count: row.get(1)?,
                })
            })
            .map_err(|e| format!("统计查询失败: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取统计失败: {}", e))?;

        // 近30天每日趋势
        let mut stmt = conn
            .prepare(
                "SELECT strftime('%Y-%m-%d', created_at / 1000, 'unixepoch') as day, COUNT(*) as cnt
                 FROM ai_history WHERE created_at >= ?1
                 GROUP BY day ORDER BY day ASC",
            )
            .map_err(|e| format!("趋势查询失败: {}", e))?;
        let thirty_days_ago = unix_timestamp_now() as i64 - 30 * 24 * 60 * 60 * 1000;
        let daily_trend = stmt
            .query_map(rusqlite::params![thirty_days_ago], |row| {
                Ok(DailyCount {
                    date: row.get(0)?,
                    count: row.get(1)?,
                })
            })
            .map_err(|e| format!("趋势查询失败: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取趋势失败: {}", e))?;

        // 最热文件排行 Top 10
        let mut stmt = conn
            .prepare(
                "SELECT file_name, file_path, COUNT(*) as cnt FROM ai_history
                 WHERE file_name != '' AND file_name IS NOT NULL
                 GROUP BY file_name, file_path ORDER BY cnt DESC LIMIT 10",
            )
            .map_err(|e| format!("文件排行查询失败: {}", e))?;
        let top_files = stmt
            .query_map([], |row| {
                Ok(FileCount {
                    file_name: row.get(0)?,
                    file_path: row.get(1)?,
                    count: row.get(2)?,
                })
            })
            .map_err(|e| format!("文件排行查询失败: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取文件排行失败: {}", e))?;

        let total_token_usage: u64 = conn
            .query_row(
                "SELECT COALESCE(SUM(token_count), 0) FROM ai_history",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        Ok(AiHistoryStats {
            total_count,
            favorite_count,
            count_by_type,
            daily_trend,
            top_files,
            total_token_usage,
        })
    }
}

// ─── 辅助函数 ───

fn map_row(row: &rusqlite::Row) -> rusqlite::Result<AiHistoryItem> {
    Ok(AiHistoryItem {
        id: row.get(0)?,
        r#type: row.get(1)?,
        label: row.get(2)?,
        prompt: row.get(3)?,
        result: row.get(4)?,
        file_name: row.get(5)?,
        file_path: row.get(6)?,
        created_at: row.get(7)?,
        last_access_at: row.get(8)?,
        favorite: row.get::<_, i32>(9)? != 0,
        token_count: row.get(10)?,
        prompt_length: row.get(11)?,
        result_length: row.get(12)?,
    })
}

fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
