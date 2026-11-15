//! 「文档库分析」结果的知识库级持久化存储。
//!
//! # 为什么放数据库而不是前端 localStorage
//!
//! 旧实现把整份分析结果塞进 `localStorage`（Tauri 下落到
//! `%APPDATA%/com.mdgo/app-settings.json`），带来三个问题：
//! 1. **不按库隔离**：key 是全局固定名，同一天切换知识库会看到上一个库的结果；
//! 2. **跨设备不可用**：数据在应用级 Store，换设备/重装即丢；
//! 3. **不可靠**：异步 flush + 跨天失效逻辑，会出现"进入页面什么都不显示"。
//!
//! 现方案：落在 `{知识库}/.mdgo/mdgo.db` 的 `ai_summary` 表 ——
//! 数据随知识库目录走（库同步即跨设备），一库一份天然隔离。
//!
//! # 语义
//!
//! **单槽位覆盖写入**（`slot = 'doc_analysis'`）：只有「重新分析」成功后整份替换，
//! 进入页面只读不写、不触发模型调用。因此不存在"点一次失败就把旧结果冲掉"的路径。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::core::db::pool::DbPool;

/// 「文档库分析」面板的槽位名（后续若增加其它分析面板，加槽位即可，不需要改表结构）
pub const SLOT_DOC_ANALYSIS: &str = "doc_analysis";

/// payload 体积上限：正常一份（11+3 段 markdown 结果）约 10~60KB；
/// 4MB 足够覆盖超长输出，同时挡住异常写入把库撑坏。
const MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

// ─── 数据模型 ───

/// 一条持久化记录（前端直接作为渲染输入）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiSummaryRecord {
    pub slot: String,
    /// 前端契约版本（`payload.v`，渲染结构变化时前端自增）
    pub version: i64,
    /// 完整 JSON：`{ v, generatedAt, behavior:[{key,label,ok,result}], knowledge:[...] }`
    pub payload: String,
    /// 成功段数（`ok=true` 的条数）：读取侧据此判断"是否是可展示的结果"
    pub ok_count: i64,
    /// 总段数
    pub total_count: i64,
    /// 写入时间（毫秒时间戳）
    pub updated_at: u64,
}

/// 写入请求（前端 → 命令层）
#[derive(Debug, Deserialize)]
pub struct AiSummarySaveRequest {
    pub payload: String,
    pub version: i64,
    pub ok_count: i64,
    pub total_count: i64,
    pub slot: Option<String>,
}

// ─── 存储服务 ───

pub struct AiSummaryStore {
    /// 与 chat / ai_history / prompts 共享同一个 `mdgo.db` 文件（不同表）
    pool: DbPool,
}

impl AiSummaryStore {
    /// 打开（或创建）某知识库的存储：参数是该库的 `.mdgo` 目录。
    pub fn new(db_dir_path: &str) -> Result<Self, String> {
        let db_path = PathBuf::from(db_dir_path).join("mdgo.db");
        let pool = DbPool::open(db_path)?;
        pool.with_write(Self::init_tables)?;
        Ok(Self { pool })
    }

    fn init_tables(conn: &mut Connection) -> Result<(), String> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ai_summary (
                slot        TEXT PRIMARY KEY,
                version     INTEGER NOT NULL,
                payload     TEXT NOT NULL,
                ok_count    INTEGER NOT NULL DEFAULT 0,
                total_count INTEGER NOT NULL DEFAULT 0,
                updated_at  INTEGER NOT NULL
            );",
        )
        .map_err(|e| format!("初始化 ai_summary 表失败: {}", e))
    }

    /// 读取某槽位（不存在返回 `Ok(None)`）
    pub fn load(&self, slot: &str) -> Result<Option<AiSummaryRecord>, String> {
        self.pool.with_read(|conn| {
            conn.query_row(
                "SELECT slot, version, payload, ok_count, total_count, updated_at
                 FROM ai_summary WHERE slot = ?1",
                params![slot],
                |row| {
                    Ok(AiSummaryRecord {
                        slot: row.get(0)?,
                        version: row.get(1)?,
                        payload: row.get(2)?,
                        ok_count: row.get(3)?,
                        total_count: row.get(4)?,
                        updated_at: row.get::<_, i64>(5)? as u64,
                    })
                },
            )
            .optional()
            .map_err(|e| format!("读取分析结果失败: {}", e))
        })
    }

    /// 覆盖写入（原子 upsert）。失败时旧记录保持不变——调用方据此"失败不动旧结果"。
    pub fn save(&self, req: &AiSummarySaveRequest, slot: &str) -> Result<AiSummaryRecord, String> {
        let payload = req.payload.trim();
        if payload.is_empty() {
            return Err("分析结果为空，已跳过写入".to_string());
        }
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(format!(
                "分析结果过大（{} 字节，上限 {}），已跳过写入",
                payload.len(),
                MAX_PAYLOAD_BYTES
            ));
        }
        // 结构校验：必须是合法 JSON 对象，且至少含一个数组（behavior/knowledge），
        // 避免把半截字符串或多重序列化写进库、下次读取直接渲染失败。
        let value: serde_json::Value = serde_json::from_str(payload)
            .map_err(|e| format!("分析结果不是合法 JSON: {}", e))?;
        if !value.is_object() {
            return Err("分析结果必须是 JSON 对象".to_string());
        }
        if !value.get("behavior").map(|v| v.is_array()).unwrap_or(false)
            && !value.get("knowledge").map(|v| v.is_array()).unwrap_or(false)
        {
            return Err("分析结果缺少 behavior/knowledge 数组".to_string());
        }

        let now = unix_millis_now();
        let payload_owned = payload.to_string();
        let slot_owned = slot.to_string();
        let version = req.version;
        let ok_count = req.ok_count.max(0);
        let total_count = req.total_count.max(0);

        self.pool.with_write(|conn| {
            conn.execute(
                "INSERT INTO ai_summary (slot, version, payload, ok_count, total_count, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(slot) DO UPDATE SET
                    version = excluded.version,
                    payload = excluded.payload,
                    ok_count = excluded.ok_count,
                    total_count = excluded.total_count,
                    updated_at = excluded.updated_at",
                params![slot_owned, version, payload_owned, ok_count, total_count, now as i64],
            )
            .map_err(|e| format!("写入分析结果失败: {}", e))?;
            Ok(())
        })?;

        Ok(AiSummaryRecord {
            slot: slot.to_string(),
            version: req.version,
            payload: payload.to_string(),
            ok_count: req.ok_count.max(0),
            total_count: req.total_count.max(0),
            updated_at: now,
        })
    }

    /// 删除某槽位（用户主动清空时使用；当前 UI 未暴露）
    pub fn clear(&self, slot: &str) -> Result<bool, String> {
        self.pool.with_write(|conn| {
            let n = conn
                .execute("DELETE FROM ai_summary WHERE slot = ?1", params![slot])
                .map_err(|e| format!("清除分析结果失败: {}", e))?;
            Ok(n > 0)
        })
    }
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ─── 测试 ───

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> (tempfile::TempDir, AiSummaryStore) {
        let dir = tempfile::tempdir().expect("创建临时目录");
        let store = AiSummaryStore::new(&dir.path().to_string_lossy()).expect("打开存储");
        (dir, store)
    }

    fn req(payload: &str, ok: i64, total: i64) -> AiSummarySaveRequest {
        AiSummarySaveRequest {
            payload: payload.to_string(),
            version: 3,
            ok_count: ok,
            total_count: total,
            slot: None,
        }
    }

    #[test]
    fn save_then_load_round_trip() {
        let (_dir, store) = tmp_store();
        let payload = r#"{"v":3,"behavior":[{"key":"operation","label":"操作记录","ok":true,"result":"OK"}],"knowledge":[]}"#;
        let saved = store.save(&req(payload, 1, 1), SLOT_DOC_ANALYSIS).expect("保存");
        assert_eq!(saved.ok_count, 1);
        assert!(saved.updated_at > 0);

        let loaded = store.load(SLOT_DOC_ANALYSIS).expect("读取").expect("应有记录");
        assert_eq!(loaded.payload, payload);
        assert_eq!(loaded.version, 3);
        assert_eq!(loaded.total_count, 1);
    }

    #[test]
    fn overwrite_replaces_previous_record() {
        let (_dir, store) = tmp_store();
        let first = r#"{"v":3,"behavior":[{"key":"a","label":"A","ok":true,"result":"旧"}],"knowledge":[]}"#;
        let second = r#"{"v":3,"behavior":[{"key":"a","label":"A","ok":true,"result":"新"}],"knowledge":[]}"#;
        store.save(&req(first, 1, 1), SLOT_DOC_ANALYSIS).expect("首次写入");
        let saved = store.save(&req(second, 1, 1), SLOT_DOC_ANALYSIS).expect("覆盖写入");
        let loaded = store.load(SLOT_DOC_ANALYSIS).expect("读取").expect("应有记录");
        assert_eq!(loaded.payload, second);
        assert_eq!(loaded.updated_at, saved.updated_at);
    }

    #[test]
    fn invalid_payload_is_rejected_and_old_record_kept() {
        let (_dir, store) = tmp_store();
        let good = r#"{"v":3,"behavior":[{"key":"a","label":"A","ok":true,"result":"保留我"}],"knowledge":[]}"#;
        store.save(&req(good, 1, 1), SLOT_DOC_ANALYSIS).expect("首次写入");

        // 非 JSON
        assert!(store.save(&req("not-json", 1, 1), SLOT_DOC_ANALYSIS).is_err());
        // 合法 JSON 但结构不对
        assert!(store.save(&req(r#"{"foo":1}"#, 1, 1), SLOT_DOC_ANALYSIS).is_err());
        // 空串
        assert!(store.save(&req("   ", 1, 1), SLOT_DOC_ANALYSIS).is_err());

        let loaded = store.load(SLOT_DOC_ANALYSIS).expect("读取").expect("旧记录应仍在");
        assert!(loaded.payload.contains("保留我"));
    }

    #[test]
    fn clear_removes_slot_only() {
        let (_dir, store) = tmp_store();
        let payload = r#"{"v":3,"behavior":[],"knowledge":[]}"#;
        store.save(&req(payload, 0, 0), SLOT_DOC_ANALYSIS).expect("写入");
        store.save(&req(payload, 0, 0), "other_slot").expect("写入其它槽位");
        assert!(store.clear(SLOT_DOC_ANALYSIS).expect("清除"));
        assert!(store.load(SLOT_DOC_ANALYSIS).expect("读取").is_none());
        assert!(store.load("other_slot").expect("读取").is_some());
    }

    #[test]
    fn per_database_isolation() {
        let dir_a = tempfile::tempdir().expect("临时目录 A");
        let dir_b = tempfile::tempdir().expect("临时目录 B");
        let store_a = AiSummaryStore::new(&dir_a.path().to_string_lossy()).expect("库 A");
        let store_b = AiSummaryStore::new(&dir_b.path().to_string_lossy()).expect("库 B");
        let payload = r#"{"v":3,"behavior":[{"key":"a","label":"A","ok":true,"result":"A 库"}],"knowledge":[]}"#;
        store_a.save(&req(payload, 1, 1), SLOT_DOC_ANALYSIS).expect("写入 A");
        assert!(store_a.load(SLOT_DOC_ANALYSIS).expect("读 A").is_some());
        assert!(store_b.load(SLOT_DOC_ANALYSIS).expect("读 B").is_none());
    }
}
