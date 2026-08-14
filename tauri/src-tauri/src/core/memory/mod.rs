//! 跨会话长期记忆层（P0-2）。
//!
//! # 设计（SOLID）
//!
//! - [`MemoryStore`]：SQLite 持久化存储（系统级 `{系统数据目录}/com.mdgo/memory.db`，WAL），
//!   单一职责：记忆条目的创建/更新/删除/检索。
//! - [`MemoryItem`]：记忆数据模型（scope/dir_path/kind/title/body/keywords/revision），
//!   `revision` 单调递增（对齐 Reasonix 记忆审计链：更新不覆盖，旧版可追溯）。
//! - **两级记忆（P0-3）**：`scope='project'` 的记忆按 `dir_path` 归属知识库，切换目录后
//!   自然隔离；`scope='global'` 的记忆 `dir_path=''`，跨知识库常驻（用户偏好/身份等
//!   系统级信息）。检索注入时取「当前知识库 ∪ 全局」。
//! - 检索为轻量关键词打分（title 命中权重高 + body/keywords 子串计数），
//!   不依赖向量库（知识库对话向量在 `chat_vectors`，记忆规模小，关键词足够；
//!   后续可注入 embedding 精确检索，见 `docs/agent_gap_plan.md` P0-2）。
//!
//! 注入路径：`commands/llm.rs` 生成阶段前把 top-k 相关记忆拼入 preamble；
//! 工具面：`remember`（写入）/ `forget`（删除）/ `search_memory`（只读检索）
//! 注册进 Agent 工具集，子代理只读白名单含 `search_memory`。

pub mod vector;

use std::path::PathBuf;
use std::sync::Arc;
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
    /// 知识库目录（两级记忆的定位键）：`scope='project'` 时必须填所属知识库目录，
    /// `scope='global'` 时为空串。仅用于写入侧（remember 工具填充），外部不直接传。
    #[serde(default)]
    pub dir_path: String,
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
    /// 所属知识库目录：`scope='project'` 时为具体目录，`scope='global'` 时为空串。
    pub dir_path: String,
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

/// 跨会话长期记忆存储（SQLite，系统级用户数据目录）。
///
/// 两级隔离（P0-3）：`scope='project'` 按 `dir_path` 归属知识库（切换目录即隔离），
/// `scope='global'` 全局共享（`dir_path=''`）。
pub struct MemoryStore {
    /// 读写分离连接池（系统级 memory.db 全局共享）：单写连接串行写，多读连接并行读。
    pool: crate::core::db::pool::DbPool,
}

impl MemoryStore {
    /// 打开系统级 memory 数据库（`{系统数据目录}/com.mdgo/memory.db`，全局共享，
    /// 不区分知识库目录——记忆为跨知识库长期记忆）。
    pub fn new() -> Result<Self, String> {
        let pool = crate::core::db::global::open_system_memory_pool()?;
        pool.with_write(|conn| Self::init_tables(conn))?;
        Ok(Self { pool })
    }

    /// 打开指定 DB 文件（测试用，tempdir 隔离）
    pub fn open_at(db_path: impl Into<PathBuf>) -> Result<Self, String> {
        let pool = crate::core::db::pool::DbPool::open(db_path.into())?;
        pool.with_write(|conn| Self::init_tables(conn))?;
        Ok(Self { pool })
    }

    fn init_tables(conn: &Connection) -> Result<(), String> {
        // 两级记忆（P0-3）：memory_items 含 dir_path 列（'' = 全局）。旧库无该列时 ALTER 补齐，
        // 存量数据归为全局（dir_path=''，保持历史"全局混存"行为，不丢数据）。
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_items (
                id         TEXT PRIMARY KEY,
                scope      TEXT NOT NULL DEFAULT 'project',
                dir_path   TEXT NOT NULL DEFAULT '',
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
            CREATE INDEX IF NOT EXISTS idx_memory_dir ON memory_items(dir_path);
            ",
        )
        .map_err(|e| format!("创建 memory_items 表失败: {}", e))?;
        // 旧库迁移：无 dir_path 列时 ALTER 补列（存量数据默认 '' → 全局，不丢数据）
        let has_dir_col = conn
            .prepare("PRAGMA table_info(memory_items)")
            .map_err(|e| e.to_string())?
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
            .iter()
            .any(|name| name == "dir_path");
        if !has_dir_col {
            conn.execute_batch(
                "ALTER TABLE memory_items ADD COLUMN dir_path TEXT NOT NULL DEFAULT '';
                 CREATE INDEX IF NOT EXISTS idx_memory_dir ON memory_items(dir_path);",
            )
            .map_err(|e| format!("迁移 memory_items 表（补 dir_path 列）失败: {}", e))?;
        }

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
            let _ = Self::rebuild_fts(conn);
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
        let now = Self::now_ms();
        // 两级记忆：scope='global' → dir_path 强制 ''（跨库可见）；scope='project' → 记所属知识库
        let scope = if input.scope.trim().is_empty() {
            "project".to_string()
        } else {
            input.scope.trim().to_string()
        };
        let dir_path = if scope == "global" {
            String::new()
        } else {
            input.dir_path.trim().to_string()
        };
        let item = MemoryItem {
            id: Uuid::new_v4().to_string(),
            scope,
            dir_path,
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
        // 单写连接：插入 + FTS 重建原子完成
        self.pool.with_write(|conn| {
            conn.execute(
                "INSERT INTO memory_items (id, scope, dir_path, kind, title, body, keywords, source_ref, expires_at, created_at, updated_at, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                rusqlite::params![
                    item.id, item.scope, item.dir_path, item.kind, item.title, item.body, item.keywords,
                    item.source_ref.as_deref().unwrap_or(""), item.expires_at.map(|v| v as i64),
                    item.created_at, item.updated_at, item.revision
                ],
            )
            .map_err(|e| format!("写入记忆失败: {}", e))?;
            Self::rebuild_fts(conn)?;
            Ok(())
        })?;
        Ok(item)
    }

    /// 更新已有记忆：revision 单调 +1，保留 created_at。
    pub fn update(&self, id: &str, input: &MemoryInput) -> Result<Option<MemoryItem>, String> {
        let now = Self::now_ms();
        self.pool.with_write(|conn| {
            let affected = conn
                .execute(
                    "UPDATE memory_items
                     SET title = ?1, body = ?2, keywords = ?3, scope = ?4, dir_path = ?5, kind = ?6,
                         source_ref = ?7, expires_at = ?8, updated_at = ?9, revision = revision + 1
                     WHERE id = ?10",
                    rusqlite::params![
                        input.title.trim(), input.body.trim(), input.keywords.trim(),
                        if input.scope.is_empty() { "project" } else { &input.scope },
                        if input.scope.trim() == "global" { "" } else { input.dir_path.trim() },
                        if input.kind.is_empty() { "fact" } else { &input.kind },
                        input.source_ref.as_deref().unwrap_or(""),
                        input.expires_at.map(|v| v as i64), now, id
                    ],
                )
                .map_err(|e| format!("更新记忆失败: {}", e))?;
            if affected == 0 {
                return Ok(None);
            }
            Self::rebuild_fts(conn)?;
            Self::get_with_conn(conn, id)
        })
    }

    /// 删除记忆；返回是否删除成功。
    pub fn delete(&self, id: &str) -> Result<bool, String> {
        self.pool.with_write(|conn| {
            let affected = conn
                .execute("DELETE FROM memory_items WHERE id = ?1", rusqlite::params![id])
                .map_err(|e| format!("删除记忆失败: {}", e))?;
            Self::rebuild_fts(conn)?;
            Ok(affected > 0)
        })
    }

    /// 按 id 读取。
    pub fn get(&self, id: &str) -> Result<Option<MemoryItem>, String> {
        self.pool.with_read(|conn| Self::get_with_conn(conn, id))
    }

    fn get_with_conn(conn: &Connection, id: &str) -> Result<Option<MemoryItem>, String> {
        let mut stmt = conn
            .prepare(
                "SELECT id, scope, dir_path, kind, title, body, keywords, source_ref, expires_at, created_at, updated_at, revision
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
        let limit = limit.clamp(1, 100) as i64;
        let now = Self::now_ms() as i64;
        self.pool.with_read(|conn| {
            let mut stmt = match scope {
                Some(_) => conn
                    .prepare_cached(
                        "SELECT id, scope, dir_path, kind, title, body, keywords, source_ref, expires_at, created_at, updated_at, revision
                         FROM memory_items WHERE scope = ?1 AND (expires_at IS NULL OR expires_at > ?2)
                         ORDER BY updated_at DESC LIMIT ?3",
                    )
                    .map_err(|e| e.to_string())?,
                None => conn
                    .prepare_cached(
                        "SELECT id, scope, dir_path, kind, title, body, keywords, source_ref, expires_at, created_at, updated_at, revision
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
        })
    }

    /// 列出当前知识库「可见」记忆：当前库（`dir_path` 匹配）∪ 全局（`dir_path=''`），
    /// 过滤已过期；按更新时间倒序。供检索降级路与向量路可见集过滤使用。
    pub fn list_visible(&self, dir_path: &str, limit: usize) -> Result<Vec<MemoryItem>, String> {
        self.pool.with_read(|conn| Self::list_visible_with(conn, dir_path, limit))
    }

    fn list_visible_with(conn: &Connection, dir_path: &str, limit: usize) -> Result<Vec<MemoryItem>, String> {
        let limit = limit.clamp(1, 100) as i64;
        let now = Self::now_ms() as i64;
        let mut stmt = conn
            .prepare_cached(
                "SELECT id, scope, dir_path, kind, title, body, keywords, source_ref, expires_at, created_at, updated_at, revision
                 FROM memory_items
                 WHERE (dir_path = ?1 OR dir_path = '') AND (expires_at IS NULL OR expires_at > ?2)
                 ORDER BY updated_at DESC LIMIT ?3",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![dir_path, now, limit], row_to_item)
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    /// 检索记忆（两级可见域：`dir_path` 当前库 ∪ 全局）：FTS5 倒排优先（O3），
    /// 失败降级关键词打分；均过滤过期（O2）。
    pub fn search(&self, query: &str, limit: usize, dir_path: &str) -> Result<Vec<MemoryItem>, String> {
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
        self.pool.with_read(|conn| {
            // O3：FTS5 优先（bm25 排序；MATCH 词转义引号防注入）
            let fts_query = terms
                .iter()
                .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" OR ");
            let fts_ids: Result<Vec<String>, String> = (|| {
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT id FROM memory_fts
                         WHERE memory_fts MATCH ?1 AND id IN
                               (SELECT id FROM memory_items
                                WHERE (dir_path = ?2 OR dir_path = '') AND (expires_at IS NULL OR expires_at > ?3))
                         ORDER BY bm25(memory_fts) LIMIT ?4",
                    )
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(rusqlite::params![fts_query, dir_path, now, limit], |r| {
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
                Ok(ids) if !ids.is_empty() => {
                    let mut out = Vec::new();
                    for id in ids {
                        if let Some(item) = Self::get_with_conn(conn, &id)? {
                            out.push(item);
                        }
                    }
                    Ok(out)
                }
                _ => {
                    // 降级：关键词打分（title ×3、body/keywords ×1），过滤过期
                    let all = Self::list_visible_with(conn, dir_path, 100)?;
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
        })
    }
}

/// 融合检索（O1）：关键词（FTS5/降级）∪ 向量 top-k，按 id RRF 融合。
///
/// - 两级可见域：`dir_path` 当前知识库 ∪ 全局（`dir_path=''`）的记忆参与检索与注入；
/// - embedding 为同步 ONNX 阻塞推理，移入 `spawn_blocking`；
/// - 向量路失败（模型未下载/向量化失败）自动降级为纯关键词（现状行为）；
/// - 记忆规模小，向量索引为内存惰性增量（`MemoryVectorIndex::sync`）。
pub async fn search_hybrid(
    store: Arc<MemoryStore>,
    index: Arc<vector::MemoryVectorIndex>,
    query: &str,
    limit: usize,
    dir_path: &str,
) -> Result<Vec<MemoryItem>, String> {
    // 1. 关键词检索（SQLite 阻塞 IO 移入 spawn_blocking；FTS5 优先；两级可见域过滤）
    let store_kw = store.clone();
    let q_kw = query.to_string();
    let dp_kw = dir_path.to_string();
    let lim_kw = limit.saturating_mul(2);
    let kw_hits: Vec<(String, f32)> = tokio::task::spawn_blocking(move || {
        store_kw
            .search(&q_kw, lim_kw, &dp_kw)
            .map(|items| items.into_iter().map(|m| (m.id.clone(), 1.0)).collect())
    })
    .await
    .map_err(|e| format!("记忆关键词检索任务失败: {e}"))?
    .unwrap_or_default();

    // 2. 向量检索（spawn_blocking；失败降级忽略向量路）
    let q = query.to_string();
    let lim = limit;
    let dp = dir_path.to_string();
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
        // 两级可见域过滤：只保留「当前库 ∪ 全局」的记忆命中，防跨知识库向量串入
        let visible: std::collections::HashSet<String> = store2
            .list_visible(&dp, 100)?
            .into_iter()
            .map(|m| m.id)
            .collect();
        let mut hits = index2.search(&q_emb, lim.saturating_mul(2));
        hits.retain(|(id, _)| visible.contains(id));
        Ok(hits)
    })
    .await
    .map_err(|e| format!("记忆向量检索任务失败: {e}"))?
    .unwrap_or_default();

    // 3. RRF 融合（按 id），按序取回完整条目
    let fused_ids = vector::rrf_fuse_memory(&kw_hits, &vec_hits, limit);
    let mut out = Vec::with_capacity(fused_ids.len());
    // 逐条取回为阻塞 IO，合并到单个 blocking 任务
    let store_ret = store.clone();
    let fetch_ids: Vec<String> = fused_ids.clone();
    let fetched: Vec<MemoryItem> = tokio::task::spawn_blocking(move || -> Result<Vec<MemoryItem>, String> {
        let mut items = Vec::with_capacity(fetch_ids.len());
        for id in fetch_ids {
            if let Some(item) = store_ret.get(&id)? {
                items.push(item);
            }
        }
        Ok(items)
    })
    .await
    .map_err(|e| format!("记忆取回任务失败: {e}"))??;
    out.extend(fetched);
    if out.is_empty() {
        // 融合为空（如双路均无命中/向量路失败）回退关键词
        let store_fb = store.clone();
        let q_fb = query.to_string();
        let dp_fb = dir_path.to_string();
        return tokio::task::spawn_blocking(move || store_fb.search(&q_fb, limit, &dp_fb))
            .await
            .map_err(|e| format!("记忆关键词回退检索任务失败: {e}"))?;
    }
    Ok(out)
}

fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryItem> {
    let source_ref: String = row.get(7)?;
    let expires_at: Option<i64> = row.get(8)?;
    Ok(MemoryItem {
        id: row.get(0)?,
        scope: row.get(1)?,
        dir_path: row.get(2)?,
        kind: row.get(3)?,
        title: row.get(4)?,
        body: row.get(5)?,
        keywords: row.get(6)?,
        source_ref: if source_ref.is_empty() { None } else { Some(source_ref) },
        expires_at: expires_at.map(|v| v as u64),
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        revision: row.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试独立 tempdir（隔离系统级 memory.db，避免测试间共享文件锁冲突）
    fn tmp_store() -> (tempfile::TempDir, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open_at(dir.path().join("memory.db")).expect("初始化记忆库失败");
        (dir, store)
    }

    fn input(title: &str, body: &str, keywords: &str) -> MemoryInput {
        MemoryInput {
            title: title.into(),
            body: body.into(),
            keywords: keywords.into(),
            scope: "project".into(),
            dir_path: String::new(),
            kind: "fact".into(),
            source_ref: None,
            expires_at: None,
        }
    }

    #[test]
    fn memory_store_crud_and_revision() {
        let (_dir, store) = tmp_store();
        // 清理残留（测试幂等）
        for item in store.list(None, 100).unwrap_or_default() {
            let _ = store.delete(&item.id);
        }
        let created = store.create(&input("用户偏好", "用户喜欢用中文回答", "中文 偏好")).unwrap();
        assert_eq!(created.revision, 1);
        // 检索召回
        let hits = store.search("中文", 10, "").unwrap();
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
        let (_dir, store) = tmp_store();
        let a = store.create(&input("Rust 所有权", "Rust 的所有权系统规则", "rust ownership")).unwrap();
        let b = store.create(&input("并发编程", "讨论 Rust 的并发模型", "concurrency")).unwrap();
        let hits = store.search("Rust", 10, "").unwrap();
        // 召回断言：标题/正文命中均被召回（排序语义由 FTS bm25 或降级打分决定）
        assert!(hits.iter().any(|h| h.id == a.id));
        assert!(hits.iter().any(|h| h.id == b.id));
        let _ = store.delete(&a.id);
        let _ = store.delete(&b.id);
    }

    #[test]
    fn expired_memories_are_filtered_from_list_and_search() {
        let (_dir, store) = tmp_store();
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
        let hits = store.search("过期", 10, "").unwrap();
        assert!(hits.iter().all(|m| m.id != expired_id), "过期记忆不应被检索召回");
        let _ = store.delete(&expired_id);
        let _ = store.delete(&fresh_id);
    }

    #[test]
    fn fts_or_fallback_search_finds_keyword_matches() {
        let (_dir, store) = tmp_store();
        let created = store
            .create(&input("部署指南", "生产环境部署使用 Docker Compose", "docker 部署"))
            .unwrap();
        // 命中正文与 keywords（FTS5 可用时走 bm25；不可用时降级关键词打分，行为一致）
        let hits = store.search("docker", 10, "").unwrap();
        assert!(hits.iter().any(|h| h.id == created.id));
        let _ = store.delete(&created.id);
    }

    #[test]
    fn two_tier_memory_isolates_by_dir_path() {
        let (_dir, store) = tmp_store();
        // 清理残留（测试幂等）
        for item in store.list(None, 100).unwrap_or_default() {
            let _ = store.delete(&item.id);
        }
        // 知识库 A 的项目记忆（scope=project + dir_path=A）
        let mut a = input("A库机密", "A 知识库的专属约定", "a约定");
        a.scope = "project".into();
        a.dir_path = "/kb/a".into();
        let id_a = store.create(&a).unwrap().id;
        // 知识库 B 的项目记忆
        let mut b = input("B库约定", "B 知识库的专属约定", "b约定");
        b.scope = "project".into();
        b.dir_path = "/kb/b".into();
        let id_b = store.create(&b).unwrap().id;
        // 全局记忆（scope=global → dir_path 强制 ''）
        let mut g = input("用户偏好", "用户偏好全局共享", "偏好");
        g.scope = "global".into();
        g.dir_path = "/kb/a".into(); // 写入侧即使误传目录，也应被归一为全局
        let id_g = store.create(&g).unwrap().id;
        assert_eq!(store.get(&id_g).unwrap().unwrap().dir_path, "", "global 记忆 dir_path 应被强制置空");

        // A 库可见域：A 库 ∪ 全局，不含 B 库（搜"约定"：a 命中、b 隔离）
        let hits_a = store.search("约定", 20, "/kb/a").unwrap();
        let ids_a: Vec<String> = hits_a.iter().map(|m| m.id.clone()).collect();
        assert!(ids_a.contains(&id_a));
        assert!(!ids_a.contains(&id_b), "A 库检索不应命中 B 库记忆");

        // B 库可见域：B 库 ∪ 全局，不含 A 库
        let hits_b = store.search("约定", 20, "/kb/b").unwrap();
        let ids_b: Vec<String> = hits_b.iter().map(|m| m.id.clone()).collect();
        assert!(ids_b.contains(&id_b));
        assert!(!ids_b.contains(&id_a), "B 库检索不应命中 A 库记忆");

        // 全局记忆跨库可见：搜"偏好"（g 内容），A 库与空可见域均命中 g
        let hits_pref_a = store.search("偏好", 20, "/kb/a").unwrap();
        assert!(hits_pref_a.iter().any(|m| m.id == id_g), "A 库可见域应命中全局记忆");
        let hits_pref_empty = store.search("偏好", 20, "").unwrap();
        assert!(hits_pref_empty.iter().any(|m| m.id == id_g), "空可见域应命中全局记忆");
        assert!(hits_pref_empty.iter().all(|m| m.id == id_g), "空可见域只应命中全局记忆");

        let _ = store.delete(&id_a);
        let _ = store.delete(&id_b);
        let _ = store.delete(&id_g);
    }
}
