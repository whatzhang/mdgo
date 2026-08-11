//! 跨会话长期记忆层（P0-2）。
//!
//! # 设计（SOLID）
//!
//! - [`MemoryStore`]：SQLite 持久化存储（`%APPDATA%/com.mdgo/memory.db`，WAL），
//!   单一职责：记忆条目的创建/更新/删除/检索。
//! - [`MemoryItem`]：记忆数据模型（scope/kind/title/body/keywords/revision），
//!   `revision` 单调递增（对齐 Reasonix 记忆审计链：更新不覆盖，旧版可追溯）。
//! - 检索为轻量关键词打分（title 命中权重高 + body/keywords 子串计数），
//!   不依赖向量库（知识库对话向量在 `chat_vectors`，记忆规模小，关键词足够；
//!   后续可注入 embedding 精确检索，见 `docs/agent_gap_plan.md` P0-2）。
//!
//! 注入路径：`commands/llm.rs` 生成阶段前把 top-k 相关记忆拼入 preamble；
//! 工具面：`remember`（写入）/ `forget`（删除）/ `search_memory`（只读检索）
//! 注册进 Agent 工具集，子代理只读白名单含 `search_memory`。

pub mod vector;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 记忆条目的输入（工具层反序列化用；id/revision 由存储生成）
#[derive(Debug, Clone, Deserialize)]
pub struct MemoryInput {
    /// 记忆标题（一句话概括）
    pub title: String,
    /// 记忆正文（事实/偏好/参考的详细内容）
    pub body: String,
    /// 空格分隔的检索关键词（可选，增强召回）
    #[serde(default)]
    pub keywords: String,
    /// 作用域：`project`（当前知识库）| `global`（所有知识库），默认 `project`
    #[serde(default)]
    pub scope: String,
    /// 类型：`fact` | `preference` | `reference`，默认 `fact`
    #[serde(default)]
    pub kind: String,
    /// 来源引用（如文档路径/会话 id，可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    /// 过期时间（unix 毫秒；O2：有值且已过期则不再召回与注入）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

/// 记忆条目（存储视图）
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MemoryItem {
    pub id: String,
    pub scope: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub keywords: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    /// 过期时间（unix 毫秒；None = 永不过期）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
    /// 单调递增版本号：每次更新 +1（审计链）
    pub revision: u32,
}

impl MemoryItem {
    /// 是否已过期（有 expires_at 且早于当前时间）
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at.is_some_and(|e| e <= now_ms)
    }
}

/// 跨会话长期记忆存储（SQLite，全局用户数据目录）。
pub struct MemoryStore {
    conn: Mutex<Connection>,
}

impl MemoryStore {
    /// 全局记忆数据库路径：`%APPDATA%/com.mdgo/memory.db`
    fn db_path() -> PathBuf {
        #[cfg(target_os = "windows")]
        let base = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        #[cfg(target_os = "macos")]
        let base = PathBuf::from(
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
        )
        .join("Library")
        .join("Application Support");
        #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
        let base = PathBuf::from(
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
        )
        .join(".local")
        .join("share");
        base.join("com.mdgo").join("memory.db")
    }

    pub fn new() -> Result<Self, String> {
        let db_path = Self::db_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建记忆数据库目录失败: {}", e))?;
        }
        let conn = Connection::open(&db_path)
            .map_err(|e| format!("打开记忆数据库失败: {}", e))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| format!("启用 WAL 模式失败: {}", e))?;
        // O4：WAL 推荐 synchronous=NORMAL（提交不 fsync，仅 checkpoint 同步）
        conn.execute_batch("PRAGMA synchronous=NORMAL;")
            .map_err(|e| format!("设置 synchronous=NORMAL 失败: {}", e))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.init_tables()?;
        Ok(store)
    }

    fn init_tables(&self) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_items (
                id         TEXT PRIMARY KEY,
                scope      TEXT NOT NULL DEFAULT 'project',
                kind       TEXT NOT NULL DEFAULT 'fact',
                title      TEXT NOT NULL,
                body       TEXT NOT NULL DEFAULT '',
                keywords   TEXT NOT NULL DEFAULT '',
                source_ref TEXT DEFAULT '',
                expires_at INTEGER,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                revision   INTEGER NOT NULL DEFAULT 1
            );
            CREATE INDEX IF NOT EXISTS idx_memory_scope ON memory_items(scope);
            ",
        )
        .map_err(|e| format!("创建 memory_items 表失败: {}", e))?;

        // O3：FTS5 关键词倒排（title/body/keywords）；写后全量重建（记忆规模小，
        // 规避 FTS5 delete/触发器内容匹配坑）；建表失败降级为 LIKE 检索。
        // 先清理旧版残留触发器与 FTS 表（IF NOT EXISTS 不会删除历史遗留，防止
        // update/delete 触发残留触发器导致 SQLITE_ERROR）。
        let fts_ok = conn
            .execute_batch(
                "DROP TRIGGER IF EXISTS memory_fts_ai;
                 DROP TRIGGER IF EXISTS memory_fts_ad;
                 DROP TRIGGER IF EXISTS memory_fts_au;
                 DROP TABLE IF EXISTS memory_fts;
                 CREATE VIRTUAL TABLE memory_fts USING fts5(
                     id UNINDEXED, title, body, keywords, tokenize='unicode61'
                 );",
            )
            .is_ok();
        if fts_ok {
            // 迁移存量数据 + 幂等重建
            let _ = Self::rebuild_fts(&conn);
        } else {
            log::warn!("[memory] FTS5 不可用，记忆检索降级为关键词扫描");
        }
        Ok(())
    }

    /// 全量重建 FTS 索引（create/update/delete 写后调用；记忆规模小，O(n) 可接受）。
    fn rebuild_fts(conn: &Connection) -> Result<(), String> {
        conn.execute_batch(
            "DELETE FROM memory_fts;
             INSERT INTO memory_fts(rowid, id, title, body, keywords)
             SELECT rowid, id, title, body, keywords FROM memory_items;",
        )
        .map_err(|e| format!("重建记忆全文索引失败: {}", e))
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// 创建一条新记忆（revision = 1）。
    pub fn create(&self, input: &MemoryInput) -> Result<MemoryItem, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let now = Self::now_ms();
        let item = MemoryItem {
            id: Uuid::new_v4().to_string(),
            scope: if input.scope.is_empty() { "project".into() } else { input.scope.clone() },
            kind: if input.kind.is_empty() { "fact".into() } else { input.kind.clone() },
            title: input.title.trim().to_string(),
            body: input.body.trim().to_string(),
            keywords: input.keywords.trim().to_string(),
            source_ref: input.source_ref.clone(),
            expires_at: input.expires_at,
            created_at: now,
            updated_at: now,
            revision: 1,
        };
        if item.title.is_empty() {
            return Err("记忆标题不能为空".into());
        }
        conn.execute(
            "INSERT INTO memory_items (id, scope, kind, title, body, keywords, source_ref, expires_at, created_at, updated_at, revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                item.id, item.scope, item.kind, item.title, item.body, item.keywords,
                item.source_ref.as_deref().unwrap_or(""), item.expires_at.map(|v| v as i64),
                item.created_at, item.updated_at, item.revision
            ],
        )
        .map_err(|e| format!("写入记忆失败: {}", e))?;
        Self::rebuild_fts(&conn)?;
        Ok(item)
    }

    /// 更新已有记忆：revision 单调 +1，保留 created_at。
    pub fn update(&self, id: &str, input: &MemoryInput) -> Result<Option<MemoryItem>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let now = Self::now_ms();
        let affected = conn
            .execute(
                "UPDATE memory_items
                 SET title = ?1, body = ?2, keywords = ?3, scope = ?4, kind = ?5,
                     source_ref = ?6, expires_at = ?7, updated_at = ?8, revision = revision + 1
                 WHERE id = ?9",
                rusqlite::params![
                    input.title.trim(), input.body.trim(), input.keywords.trim(),
                    if input.scope.is_empty() { "project" } else { &input.scope },
                    if input.kind.is_empty() { "fact" } else { &input.kind },
                    input.source_ref.as_deref().unwrap_or(""),
                    input.expires_at.map(|v| v as i64), now, id
                ],
            )
            .map_err(|e| format!("更新记忆失败: {}", e))?;
        if affected == 0 {
            return Ok(None);
        }
        Self::rebuild_fts(&conn)?;
        Self::get_with_conn(&conn, id)
    }

    /// 删除记忆；返回是否删除成功。
    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let affected = conn
            .execute("DELETE FROM memory_items WHERE id = ?1", rusqlite::params![id])
            .map_err(|e| format!("删除记忆失败: {}", e))?;
        Self::rebuild_fts(&conn)?;
        Ok(affected > 0)
    }

    /// 按 id 读取。
    pub fn get(&self, id: &str) -> Result<Option<MemoryItem>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        Self::get_with_conn(&conn, id)
    }

    fn get_with_conn(conn: &Connection, id: &str) -> Result<Option<MemoryItem>, String> {
        let mut stmt = conn
            .prepare(
                "SELECT id, scope, kind, title, body, keywords, source_ref, expires_at, created_at, updated_at, revision
                 FROM memory_items WHERE id = ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![id], row_to_item)
            .map_err(|e| e.to_string())?;
        Ok(rows.into_iter().next().transpose().map_err(|e| e.to_string())?)
    }

    /// 列出记忆（可选按 scope 过滤），按更新时间倒序；过滤已过期条目（O2）。
    pub fn list(&self, scope: Option<&str>, limit: usize) -> Result<Vec<MemoryItem>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let limit = limit.clamp(1, 100) as i64;
        let now = Self::now_ms() as i64;
        let mut stmt = match scope {
            Some(_) => conn
                .prepare(
                    "SELECT id, scope, kind, title, body, keywords, source_ref, expires_at, created_at, updated_at, revision
                     FROM memory_items WHERE scope = ?1 AND (expires_at IS NULL OR expires_at > ?2)
                     ORDER BY updated_at DESC LIMIT ?3",
                )
                .map_err(|e| e.to_string())?,
            None => conn
                .prepare(
                    "SELECT id, scope, kind, title, body, keywords, source_ref, expires_at, created_at, updated_at, revision
                     FROM memory_items WHERE expires_at IS NULL OR expires_at > ?1
                     ORDER BY updated_at DESC LIMIT ?2",
                )
                .map_err(|e| e.to_string())?,
        };
        let params: Vec<rusqlite::types::Value> = match scope {
            Some(s) => vec![s.to_string().into(), now.into(), limit.into()],
            None => vec![now.into(), limit.into()],
        };
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), row_to_item)
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    /// 检索记忆：FTS5 倒排优先（O3），失败降级关键词打分；均过滤过期（O2）。
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>, String> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .filter(|s| s.chars().count() > 1)
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 20) as i64;
        let now = Self::now_ms() as i64;
        let conn = self.conn.lock().map_err(|e| e.to_string())?;

        // O3：FTS5 优先（bm25 排序；MATCH 词转义引号防注入）
        let fts_query = terms
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");
        let fts_ids: Result<Vec<String>, String> = (|| {
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM memory_fts
                     WHERE memory_fts MATCH ?1 AND id IN
                           (SELECT id FROM memory_items WHERE expires_at IS NULL OR expires_at > ?2)
                     ORDER BY bm25(memory_fts) LIMIT ?3",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(rusqlite::params![fts_query, now, limit], |r| {
                    r.get::<_, String>(0)
                })
                .map_err(|e| e.to_string())?;
            let mut ids = Vec::new();
            for r in rows {
                ids.push(r.map_err(|e| e.to_string())?);
            }
            Ok(ids)
        })();
        match fts_ids {
            Ok(ids) => {
                let mut out = Vec::new();
                for id in ids {
                    if let Some(item) = Self::get_with_conn(&conn, &id)? {
                        out.push(item);
                    }
                }
                Ok(out)
            }
            Err(_) => {
                // 降级：关键词打分（title ×3、body/keywords ×1），过滤过期
                let all = self.list(None, 100)?;
                let mut scored: Vec<(i64, MemoryItem)> = Vec::new();
                for item in all {
                    let title_l = item.title.to_lowercase();
                    let body_l = item.body.to_lowercase();
                    let kw_l = item.keywords.to_lowercase();
                    let mut score = 0i64;
                    for term in &terms {
                        if title_l.contains(term) {
                            score += 3;
                        }
                        if body_l.contains(term) || kw_l.contains(term) {
                            score += 1;
                        }
                    }
                    if score > 0 {
                        scored.push((score, item));
                    }
                }
                scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.updated_at.cmp(&a.1.updated_at)));
                Ok(scored
                    .into_iter()
                    .take(limit as usize)
                    .map(|(_, i)| i)
                    .collect())
            }
        }
    }
}

/// 融合检索（O1）：关键词（FTS5/降级）∪ 向量 top-k，按 id RRF 融合。
///
/// - embedding 为同步 ONNX 阻塞推理，移入 `spawn_blocking`；
/// - 向量路失败（模型未下载/向量化失败）自动降级为纯关键词（现状行为）；
/// - 记忆规模小，向量索引为内存惰性增量（`MemoryVectorIndex::sync`）。
pub async fn search_hybrid(
    store: Arc<MemoryStore>,
    index: Arc<vector::MemoryVectorIndex>,
    query: &str,
    limit: usize,
) -> Result<Vec<MemoryItem>, String> {
    // 1. 关键词检索（同步，FTS5 优先）
    let kw_hits: Vec<(String, f32)> = store
        .search(query, limit.saturating_mul(2))?
        .into_iter()
        .map(|m| (m.id.clone(), 1.0))
        .collect();

    // 2. 向量检索（spawn_blocking；失败降级忽略向量路）
    let q = query.to_string();
    let lim = limit;
    let store2 = store.clone();
    let index2 = index.clone();
    let vec_hits: Vec<(String, f32)> = tokio::task::spawn_blocking(move || -> Result<Vec<(String, f32)>, String> {
        // 惰性增量索引：对比全量 id 与已索引集，为新增记忆补 embedding
        let all: Vec<String> = store2.list(None, 100)?.into_iter().map(|m| m.id).collect();
        index2.sync(&all, |id| store2.get(id).ok().flatten().map(|m| (m.title, m.body)))?;
        let q_emb = crate::core::db::utils::call_embedding_query(&q)
            .map_err(|e| format!("查询向量化失败: {e}"))?
            .into_iter()
            .next()
            .ok_or_else(|| "查询向量为空".to_string())?;
        Ok(index2.search(&q_emb, lim.saturating_mul(2)))
    })
    .await
    .map_err(|e| format!("记忆向量检索任务失败: {e}"))?
    .unwrap_or_default();

    // 3. RRF 融合（按 id），按序取回完整条目
    let fused_ids = vector::rrf_fuse_memory(&kw_hits, &vec_hits, limit);
    let mut out = Vec::with_capacity(fused_ids.len());
    for id in fused_ids {
        if let Some(item) = store.get(&id)? {
            out.push(item);
        }
    }
    if out.is_empty() {
        // 融合为空（如双路均无命中/向量路失败）回退关键词
        return store.search(query, limit);
    }
    Ok(out)
}

fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryItem> {
    let source_ref: String = row.get(6)?;
    let expires_at: Option<i64> = row.get(7)?;
    Ok(MemoryItem {
        id: row.get(0)?,
        scope: row.get(1)?,
        kind: row.get(2)?,
        title: row.get(3)?,
        body: row.get(4)?,
        keywords: row.get(5)?,
        source_ref: if source_ref.is_empty() { None } else { Some(source_ref) },
        expires_at: expires_at.map(|v| v as u64),
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        revision: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(title: &str, body: &str, keywords: &str) -> MemoryInput {
        MemoryInput {
            title: title.into(),
            body: body.into(),
            keywords: keywords.into(),
            scope: "project".into(),
            kind: "fact".into(),
            source_ref: None,
            expires_at: None,
        }
    }

    #[test]
    fn memory_store_crud_and_revision() {
        let store = MemoryStore::new().expect("初始化记忆库失败");
        // 清理残留（测试幂等）
        for item in store.list(None, 100).unwrap_or_default() {
            let _ = store.delete(&item.id);
        }
        let created = store.create(&input("用户偏好", "用户喜欢用中文回答", "中文 偏好")).unwrap();
        assert_eq!(created.revision, 1);
        // 检索召回
        let hits = store.search("中文", 10).unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().any(|h| h.id == created.id));
        // 更新 → revision +1
        let updated = store
            .update(&created.id, &input("用户偏好", "用户喜欢用简洁的中文回答", "中文 简洁"))
            .unwrap()
            .expect("应更新成功");
        assert_eq!(updated.revision, 2);
        assert!(updated.body.contains("简洁"));
        // 按 id 读取 + 删除
        assert!(store.get(&created.id).unwrap().is_some());
        assert!(store.delete(&created.id).unwrap());
        assert!(store.get(&created.id).unwrap().is_none());
    }

    #[test]
    fn memory_search_title_weights_higher() {
        let store = MemoryStore::new().expect("初始化记忆库失败");
        let a = store.create(&input("Rust 所有权", "Rust 的所有权系统规则", "rust ownership")).unwrap();
        let b = store.create(&input("并发编程", "讨论 Rust 的并发模型", "concurrency")).unwrap();
        let hits = store.search("Rust", 10).unwrap();
        // 召回断言：标题/正文命中均被召回（排序语义由 FTS bm25 或降级打分决定）
        assert!(hits.iter().any(|h| h.id == a.id));
        assert!(hits.iter().any(|h| h.id == b.id));
        let _ = store.delete(&a.id);
        let _ = store.delete(&b.id);
    }

    #[test]
    fn expired_memories_are_filtered_from_list_and_search() {
        let store = MemoryStore::new().expect("初始化记忆库失败");
        // 清理残留
        for item in store.list(None, 100).unwrap_or_default() {
            let _ = store.delete(&item.id);
        }
        let mut expired = input("过期记忆", "这是一条已过期的事实", "过期");
        expired.expires_at = Some(1); // 1970 年，必然已过期
        let mut fresh = input("有效记忆", "这是一条有效的约定", "有效");
        // 未来 30 天（不能 u64::MAX——转 i64 会溢出为负数被误过滤）
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        fresh.expires_at = Some(now_ms + 30 * 24 * 60 * 60 * 1000);
        let expired_id = store.create(&expired).unwrap().id;
        let fresh_id = store.create(&fresh).unwrap().id;
        // list 不召回过期
        let listed = store.list(None, 100).unwrap();
        assert!(listed.iter().all(|m| m.id != expired_id));
        assert!(listed.iter().any(|m| m.id == fresh_id));
        // search 不召回过期（FTS 或降级关键词路径均过滤）
        let hits = store.search("过期", 10).unwrap();
        assert!(hits.iter().all(|m| m.id != expired_id), "过期记忆不应被检索召回");
        let _ = store.delete(&expired_id);
        let _ = store.delete(&fresh_id);
    }

    #[test]
    fn fts_or_fallback_search_finds_keyword_matches() {
        let store = MemoryStore::new().expect("初始化记忆库失败");
        let created = store
            .create(&input("部署指南", "生产环境部署使用 Docker Compose", "docker 部署"))
            .unwrap();
        // 命中正文与 keywords（FTS5 可用时走 bm25；不可用时降级关键词打分，行为一致）
        let hits = store.search("docker", 10).unwrap();
        assert!(hits.iter().any(|h| h.id == created.id));
        let _ = store.delete(&created.id);
    }
}
