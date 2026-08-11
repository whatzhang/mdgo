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

use std::path::PathBuf;
use std::sync::Mutex;

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
    pub created_at: u64,
    pub updated_at: u64,
    /// 单调递增版本号：每次更新 +1（审计链）
    pub revision: u32,
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
        Ok(())
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
            created_at: now,
            updated_at: now,
            revision: 1,
        };
        if item.title.is_empty() {
            return Err("记忆标题不能为空".into());
        }
        conn.execute(
            "INSERT INTO memory_items (id, scope, kind, title, body, keywords, source_ref, created_at, updated_at, revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                item.id, item.scope, item.kind, item.title, item.body, item.keywords,
                item.source_ref.as_deref().unwrap_or(""), item.created_at, item.updated_at, item.revision
            ],
        )
        .map_err(|e| format!("写入记忆失败: {}", e))?;
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
                     source_ref = ?6, updated_at = ?7, revision = revision + 1
                 WHERE id = ?8",
                rusqlite::params![
                    input.title.trim(), input.body.trim(), input.keywords.trim(),
                    if input.scope.is_empty() { "project" } else { &input.scope },
                    if input.kind.is_empty() { "fact" } else { &input.kind },
                    input.source_ref.as_deref().unwrap_or(""), now, id
                ],
            )
            .map_err(|e| format!("更新记忆失败: {}", e))?;
        if affected == 0 {
            return Ok(None);
        }
        Self::get_with_conn(&conn, id)
    }

    /// 删除记忆；返回是否删除成功。
    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let affected = conn
            .execute("DELETE FROM memory_items WHERE id = ?1", rusqlite::params![id])
            .map_err(|e| format!("删除记忆失败: {}", e))?;
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
                "SELECT id, scope, kind, title, body, keywords, source_ref, created_at, updated_at, revision
                 FROM memory_items WHERE id = ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![id], row_to_item)
            .map_err(|e| e.to_string())?;
        Ok(rows.into_iter().next().transpose().map_err(|e| e.to_string())?)
    }

    /// 列出记忆（可选按 scope 过滤），按更新时间倒序。
    pub fn list(&self, scope: Option<&str>, limit: usize) -> Result<Vec<MemoryItem>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let limit = limit.clamp(1, 100) as i64;
        let mut stmt = match scope {
            Some(_) => conn
                .prepare(
                    "SELECT id, scope, kind, title, body, keywords, source_ref, created_at, updated_at, revision
                     FROM memory_items WHERE scope = ?1 ORDER BY updated_at DESC LIMIT ?2",
                )
                .map_err(|e| e.to_string())?,
            None => conn
                .prepare(
                    "SELECT id, scope, kind, title, body, keywords, source_ref, created_at, updated_at, revision
                     FROM memory_items ORDER BY updated_at DESC LIMIT ?1",
                )
                .map_err(|e| e.to_string())?,
        };
        let params: Vec<rusqlite::types::Value> = match scope {
            Some(s) => vec![s.to_string().into(), limit.into()],
            None => vec![limit.into()],
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

    /// 轻量关键词检索：query 按空白拆词，title 命中权重 ×3，body/keywords 子串 ×1；
    /// 返回得分前 `limit` 条（未命中任何词返回空）。
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>, String> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .filter(|s| s.chars().count() > 1)
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
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
        Ok(scored.into_iter().take(limit.clamp(1, 20)).map(|(_, i)| i).collect())
    }
}

fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryItem> {
    let source_ref: String = row.get(6)?;
    Ok(MemoryItem {
        id: row.get(0)?,
        scope: row.get(1)?,
        kind: row.get(2)?,
        title: row.get(3)?,
        body: row.get(4)?,
        keywords: row.get(5)?,
        source_ref: if source_ref.is_empty() { None } else { Some(source_ref) },
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        revision: row.get(9)?,
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
        // 标题命中的 a 应排在 body 命中的 b 之前
        assert!(hits.iter().position(|h| h.id == a.id) < hits.iter().position(|h| h.id == b.id));
        let _ = store.delete(&a.id);
        let _ = store.delete(&b.id);
    }
}
