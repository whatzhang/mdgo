use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

use rusqlite::Connection;
use uuid::Uuid;

use mdgo_core::{ChatMessage, ChatMessageSource, ChatSession, ChatSessionSearchResult};

/// 每类对话（regular / rag）最多保留的非收藏会话数量
const MAX_SESSIONS_PER_TYPE: i64 = 100;

// ─── 存储服务 ───

pub struct ChatStore {
    conn: Mutex<Connection>,
}

impl ChatStore {
    /// 创建新的 ChatStore，自动创建数据库目录和表
    pub fn new(db_dir_path: &str) -> Result<Self, String> {
        let db_path = Self::get_db_path(db_dir_path);

        // 创建数据库目录
        if let Some(parent) = Path::new(&db_path).parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建数据库目录失败: {}", e))?;
        }

        let conn = Connection::open(&db_path).map_err(|e| format!("打开数据库失败: {}", e))?;
        // 启用外键约束（默认关闭，必须手动开启）
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| format!("启用外键约束失败: {}", e))?;
        // 启用 WAL 模式，支持多连接并发读写（与 AiHistoryStore 共享同一文件）
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| format!("启用 WAL 模式失败: {}", e))?;
        Self::init_tables(&conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn get_db_path(db_dir_path: &str) -> String {
        Path::new(db_dir_path)
            .join("mdgo.db")
            .to_string_lossy()
            .to_string()
    }

    fn init_tables(conn: &Connection) -> Result<(), String> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS chat_sessions (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                favorite INTEGER NOT NULL DEFAULT 0,
                message_count INTEGER NOT NULL DEFAULT 0,
                token_usage INTEGER NOT NULL DEFAULT 0,
                type TEXT NOT NULL DEFAULT 'regular'
            );

            CREATE TABLE IF NOT EXISTS chat_messages (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                token_count INTEGER DEFAULT 0,
                created_at INTEGER NOT NULL,
                FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS chat_message_sources (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                doc_name TEXT NOT NULL,
                score REAL NOT NULL DEFAULT 0,
                snippet TEXT NOT NULL DEFAULT '',
                FOREIGN KEY (message_id) REFERENCES chat_messages(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS chat_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            ",
        )
        .map_err(|e| format!("建表失败: {}", e))?;

        // 迁移旧数据：将秒级时间戳转换为毫秒级（一次性）
        conn.execute_batch(
            "
            UPDATE chat_sessions SET created_at = created_at * 1000 WHERE created_at > 0 AND created_at < 100000000000;
            UPDATE chat_sessions SET updated_at = updated_at * 1000 WHERE updated_at > 0 AND updated_at < 100000000000;
            UPDATE chat_messages SET created_at = created_at * 1000 WHERE created_at > 0 AND created_at < 100000000000;
            ",
        )
        .map_err(|e| format!("时间戳迁移失败: {}", e))?;

        Ok(())
    }

    /// 创建新会话。
    ///
    /// 每类对话（regular / rag）最多保留 100 条非收藏会话。
    /// 超出时自动删除最早 updated_at 的非收藏会话（及其消息，CASCADE）。
    /// 返回 (新会话, 被删除的旧会话 ID 列表)。
    pub fn create_session(&self, title: &str, session_type: &str) -> Result<(ChatSession, Vec<String>), String> {
        let now = unix_timestamp_now();
        let id = Uuid::new_v4().to_string();
        let month_group = unix_timestamp_to_year_month(now);

        let conn = self.conn.lock().map_err(|e| e.to_string())?;

        conn.execute_batch("BEGIN TRANSACTION")
            .map_err(|e| format!("开启事务失败: {}", e))?;

        let insert_result = conn.execute(
            "INSERT INTO chat_sessions (id, title, created_at, updated_at, favorite, message_count, token_usage, type) VALUES (?1, ?2, ?3, ?4, 0, 0, 0, ?5)",
            rusqlite::params![id, title, now, now, session_type],
        );

        if let Err(e) = insert_result {
            conn.execute_batch("ROLLBACK").ok();
            return Err(format!("创建会话失败: {}", e));
        }

        // 统计同类型非收藏会话数量
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chat_sessions WHERE type = ?1 AND favorite = 0",
                rusqlite::params![session_type],
                |row| row.get(0),
            )
            .map_err(|e| format!("统计会话数失败: {}", e))?;

        // 超出上限时删除最旧的（按 updated_at ASC 排序）
            let mut deleted_ids: Vec<String> = Vec::new();
            if count > MAX_SESSIONS_PER_TYPE {
                let excess = count - MAX_SESSIONS_PER_TYPE;

                let mut stmt = conn
                .prepare("SELECT id FROM chat_sessions WHERE type = ?1 AND favorite = 0 ORDER BY updated_at ASC, id ASC LIMIT ?2")
                .map_err(|e| format!("查询待删除会话失败: {}", e))?;
            let ids: Vec<String> = stmt
                .query_map(rusqlite::params![session_type, excess], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(|e| format!("查询待删除会话失败: {}", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("查询待删除会话失败: {}", e))?;

            if !ids.is_empty() {
                // 再执行 DELETE
                let placeholders: Vec<String> = ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 1)).collect();
                let sql = format!("DELETE FROM chat_sessions WHERE id IN ({})", placeholders.join(","));
                let params: Vec<&dyn rusqlite::types::ToSql> = ids.iter().map(|s| s as &dyn rusqlite::types::ToSql).collect();
                if let Err(e) = conn.execute(&sql, params.as_slice()) {
                    conn.execute_batch("ROLLBACK").ok();
                    return Err(format!("清理旧会话失败: {}", e));
                }
                deleted_ids = ids;
            }
        }

        conn.execute_batch("COMMIT")
            .map_err(|e| format!("提交事务失败: {}", e))?;

        Ok((ChatSession {
            id,
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            favorite: false,
            message_count: 0,
            token_usage: 0,
            month_group,
            r#type: session_type.to_string(),
        }, deleted_ids))
    }

    /// 删除会话及其所有消息（CASCADE），同时清理 chat_config 中相关条目
    pub fn delete_session(&self, id: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;

        conn.execute_batch("BEGIN TRANSACTION")
            .map_err(|e| format!("开启事务失败: {}", e))?;

        let affected = conn
            .execute("DELETE FROM chat_sessions WHERE id = ?1", rusqlite::params![id])
            .map_err(|e| format!("删除会话失败: {}", e))?;
        if affected == 0 {
            conn.execute_batch("ROLLBACK").ok();
            return Err("会话不存在".to_string());
        }

        // 清理 chat_config 中该会话的索引计数条目
        let indexed_key = format!("indexed_msg_count_{}", id);
        conn.execute("DELETE FROM chat_config WHERE key = ?1", rusqlite::params![indexed_key])
            .map_err(|e| format!("清理索引入数失败: {}", e))?;

        // 如果被删除的会话是 last_session，也一并清理
        // 先读出当前 last_session 值
        let last_val: Result<String, _> = conn.query_row(
            "SELECT value FROM chat_config WHERE key = 'last_session'",
            [],
            |row| row.get(0),
        );
        if let Ok(val) = last_val {
            if val.contains(id) {
                conn.execute("DELETE FROM chat_config WHERE key = 'last_session'", [])
                    .ok();
            }
        }

        conn.execute_batch("COMMIT")
            .map_err(|e| format!("提交事务失败: {}", e))?;

        Ok(())
    }

    /// 重命名会话
    pub fn rename_session(&self, id: &str, title: &str) -> Result<(), String> {
        let now = unix_timestamp_now();
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let affected = conn
            .execute(
                "UPDATE chat_sessions SET title = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![title, now, id],
            )
            .map_err(|e| format!("重命名会话失败: {}", e))?;
        if affected == 0 {
            return Err("会话不存在".to_string());
        }
        Ok(())
    }

    /// 切换收藏状态，返回新状态
    pub fn toggle_favorite(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let affected = conn
            .execute(
                "UPDATE chat_sessions SET favorite = CASE WHEN favorite = 0 THEN 1 ELSE 0 END WHERE id = ?1",
                rusqlite::params![id],
            )
            .map_err(|e| format!("切换收藏失败: {}", e))?;
        if affected == 0 {
            return Err("会话不存在".to_string());
        }

        // 查询切换后的收藏状态
        let new_fav: i32 = conn
            .query_row(
                "SELECT favorite FROM chat_sessions WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .map_err(|e| format!("查询收藏状态失败: {}", e))?;
        Ok(new_fav != 0)
    }

    /// 获取所有会话的总数
    pub fn get_session_count(&self) -> u32 {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        conn.query_row("SELECT COUNT(*) FROM chat_sessions", [], |row| row.get(0))
            .unwrap_or(0)
    }

    /// 获取指定类型的会话总数
    pub fn get_session_count_by_type(&self, session_type: &str) -> u32 {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        conn.query_row(
            "SELECT COUNT(*) FROM chat_sessions WHERE type = ?1",
            rusqlite::params![session_type],
            |row| row.get(0),
        )
        .unwrap_or(0)
    }

    /// 获取所有会话的消息总数
    pub fn get_message_count(&self) -> u32 {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        conn.query_row(
            "SELECT COALESCE(SUM(message_count), 0) FROM chat_sessions",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0)
    }

    /// 获取指定类型的会话消息总数
    pub fn get_message_count_by_type(&self, session_type: &str) -> u32 {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        conn.query_row(
            "SELECT COALESCE(SUM(message_count), 0) FROM chat_sessions WHERE type = ?1",
            rusqlite::params![session_type],
            |row| row.get(0),
        )
        .unwrap_or(0)
    }

    /// 按 updated_at DESC 排序返回所有会话
    pub fn list_sessions(&self) -> Result<Vec<ChatSession>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT id, title, created_at, updated_at, favorite, message_count, token_usage, type FROM chat_sessions ORDER BY updated_at DESC",
            )
            .map_err(|e| format!("查询会话列表失败: {}", e))?;

        let sessions = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let title: String = row.get(1)?;
                let created_at: u64 = row.get(2)?;
                let updated_at: u64 = row.get(3)?;
                let favorite: i32 = row.get(4)?;
                let message_count: u32 = row.get(5)?;
                let token_usage: u32 = row.get(6)?;
                let r#type: String = row.get(7)?;

                Ok(ChatSession {
                    id,
                    title,
                    created_at,
                    updated_at,
                    favorite: favorite != 0,
                    message_count,
                    token_usage,
                    month_group: unix_timestamp_to_year_month(created_at),
                    r#type,
                })
            })
            .map_err(|e| format!("查询会话列表失败: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取会话列表失败: {}", e))?;

        Ok(sessions)
    }

    /// 按 created_at ASC 返回会话消息
    pub fn get_session_messages(&self, session_id: &str) -> Result<Vec<ChatMessage>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, role, content, token_count, created_at FROM chat_messages WHERE session_id = ?1 ORDER BY created_at ASC",
            )
            .map_err(|e| format!("查询消息失败: {}", e))?;

        let messages = stmt
            .query_map(rusqlite::params![session_id], |row| {
                let id: String = row.get(0)?;
                let session_id: String = row.get(1)?;
                let role: String = row.get(2)?;
                let content: String = row.get(3)?;
                let token_count: i32 = row.get(4)?;
                let created_at: u64 = row.get(5)?;

                Ok(ChatMessage {
                    id,
                    session_id,
                    role,
                    content,
                    token_count,
                    created_at,
                })
            })
            .map_err(|e| format!("查询消息失败: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取消息失败: {}", e))?;

        Ok(messages)
    }

    /// 保存消息，同时更新会话的 message_count、token_usage、updated_at
    pub fn save_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        token_count: i32,
    ) -> Result<ChatMessage, String> {
        let now = unix_timestamp_now();
        let id = Uuid::new_v4().to_string();

        let conn = self.conn.lock().map_err(|e| e.to_string())?;

        conn.execute_batch("BEGIN TRANSACTION")
            .map_err(|e| format!("开启事务失败: {}", e))?;

        // 验证会话存在
        let session_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM chat_sessions WHERE id = ?1",
                rusqlite::params![session_id],
                |row| row.get::<_, i32>(0),
            )
            .map(|count| count > 0)
            .map_err(|e| format!("验证会话失败: {}", e))?;

        if !session_exists {
            conn.execute_batch("ROLLBACK").ok();
            return Err("会话不存在".to_string());
        }

        // 插入消息
        if let Err(e) = conn.execute(
            "INSERT INTO chat_messages (id, session_id, role, content, token_count, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, session_id, role, content, token_count, now],
        ) {
            conn.execute_batch("ROLLBACK").ok();
            return Err(format!("保存消息失败: {}", e));
        }

        // 更新会话统计
        if let Err(e) = conn.execute(
            "UPDATE chat_sessions SET message_count = message_count + 1, token_usage = token_usage + ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![token_count.max(0) as u32, now, session_id],
        ) {
            conn.execute_batch("ROLLBACK").ok();
            return Err(format!("更新会话统计失败: {}", e));
        }

        conn.execute_batch("COMMIT")
            .map_err(|e| format!("提交事务失败: {}", e))?;

        Ok(ChatMessage {
            id,
            session_id: session_id.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            token_count,
            created_at: now,
        })
    }

    /// 清空会话的所有消息，重置 message_count 和 token_usage
    pub fn clear_session_messages(&self, session_id: &str) -> Result<(), String> {
        let now = unix_timestamp_now();
        let conn = self.conn.lock().map_err(|e| e.to_string())?;

        conn.execute_batch("BEGIN TRANSACTION")
            .map_err(|e| format!("开启事务失败: {}", e))?;

        if let Err(e) = conn.execute(
            "DELETE FROM chat_messages WHERE session_id = ?1",
            rusqlite::params![session_id],
        ) {
            conn.execute_batch("ROLLBACK").ok();
            return Err(format!("清空消息失败: {}", e));
        }

        if let Err(e) = conn.execute(
            "UPDATE chat_sessions SET message_count = 0, token_usage = 0, updated_at = ?1 WHERE id = ?2",
            rusqlite::params![now, session_id],
        ) {
            conn.execute_batch("ROLLBACK").ok();
            return Err(format!("重置会话统计失败: {}", e));
        }

        // 同步重置已索引入数，避免 chat_session_index_current 去重判断永久不匹配
        let indexed_key = format!("indexed_msg_count_{}", session_id);
        if let Err(e) = conn.execute(
            "DELETE FROM chat_config WHERE key = ?1",
            rusqlite::params![indexed_key],
        ) {
            conn.execute_batch("ROLLBACK").ok();
            return Err(format!("重置已索引入数失败: {}", e));
        }

        conn.execute_batch("COMMIT")
            .map_err(|e| format!("提交事务失败: {}", e))?;

        log::info!("[chat_store] 清空会话 {} 的消息", session_id);
        Ok(())
    }

    /// 保存消息的引用来源（RAG 模式）
    ///
    /// 先删除该 message_id 下已有的 sources，再插入新的，保证幂等。
    pub fn save_message_sources(
        &self,
        message_id: &str,
        sources: &[ChatMessageSource],
    ) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;

        conn.execute_batch("BEGIN TRANSACTION")
            .map_err(|e| format!("开启事务失败: {}", e))?;

        // 先删除已有 sources（保证幂等，避免重复调用产生脏数据）
        if let Err(e) = conn.execute(
            "DELETE FROM chat_message_sources WHERE message_id = ?1",
            rusqlite::params![message_id],
        ) {
            conn.execute_batch("ROLLBACK").ok();
            return Err(format!("清理旧引用来源失败: {}", e));
        }

        for src in sources {
            if let Err(e) = conn.execute(
                "INSERT INTO chat_message_sources (id, message_id, doc_name, score, snippet) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![src.id, message_id, src.doc_name, src.score, src.snippet],
            ) {
                conn.execute_batch("ROLLBACK").ok();
                return Err(format!("保存引用来源失败: {}", e));
            }
        }

        conn.execute_batch("COMMIT")
            .map_err(|e| format!("提交事务失败: {}", e))?;

        Ok(())
    }

    /// 获取指定消息的所有引用来源
    pub fn get_message_sources(&self, message_id: &str) -> Result<Vec<ChatMessageSource>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT id, message_id, doc_name, score, snippet FROM chat_message_sources WHERE message_id = ?1 ORDER BY score DESC",
            )
            .map_err(|e| format!("查询引用来源失败: {}", e))?;

        let sources = stmt
            .query_map(rusqlite::params![message_id], |row| {
                let id: String = row.get(0)?;
                let message_id: String = row.get(1)?;
                let doc_name: String = row.get(2)?;
                let score: f32 = row.get(3)?;
                let snippet: String = row.get(4)?;

                Ok(ChatMessageSource {
                    id,
                    message_id,
                    doc_name,
                    score,
                    snippet,
                })
            })
            .map_err(|e| format!("查询引用来源失败: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取引用来源失败: {}", e))?;

        Ok(sources)
    }

    /// 批量获取多条消息的引用来源，按 message_id 分组
    pub fn get_messages_sources(&self, message_ids: &[String]) -> Result<std::collections::HashMap<String, Vec<ChatMessageSource>>, String> {
        if message_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // 构建占位符
        let placeholders: Vec<String> = message_ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 1)).collect();
        let sql = format!(
            "SELECT id, message_id, doc_name, score, snippet FROM chat_message_sources WHERE message_id IN ({}) ORDER BY score DESC",
            placeholders.join(",")
        );

        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn.prepare(&sql).map_err(|e| format!("查询引用来源失败: {}", e))?;

        let params: Vec<&dyn rusqlite::types::ToSql> = message_ids.iter().map(|s| s as &dyn rusqlite::types::ToSql).collect();

        let mut result: std::collections::HashMap<String, Vec<ChatMessageSource>> = std::collections::HashMap::new();

        let rows = stmt
            .query_map(params.as_slice(), |row| {
                let id: String = row.get(0)?;
                let message_id: String = row.get(1)?;
                let doc_name: String = row.get(2)?;
                let score: f32 = row.get(3)?;
                let snippet: String = row.get(4)?;
                Ok(ChatMessageSource {
                    id,
                    message_id,
                    doc_name,
                    score,
                    snippet,
                })
            })
            .map_err(|e| format!("查询引用来源失败: {}", e))?;

        let sources = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取引用来源失败: {}", e))?;

        for src in sources {
            result.entry(src.message_id.clone()).or_default().push(src);
        }

        Ok(result)
    }

    /// 混合搜索会话。
    ///
    /// **新架构**：向量检索和 BM25 检索由 `Indexer::search_chat_sessions` 完成
    /// （查询预索引的 `chat_vectors` / `chat_bm25`，只需 1 次 query embedding）。
    /// 本方法负责 SQL LIKE 文本匹配 + 根据 Indexer 返回的 session_id 组装最终结果。
    ///
    /// - `indexer_hits`: Indexer 混合检索返回的 `(session_id, score, matched_text)` 列表
    pub fn search_sessions(
        &self,
        query_text: &str,
        indexer_hits: &[(String, f32, String)],
    ) -> Result<Vec<ChatSessionSearchResult>, String> {
        if query_text.trim().is_empty() && indexer_hits.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.conn.lock().map_err(|e| e.to_string())?;

        // 1. SQL LIKE 模糊查询（标题 + 消息内容），获取文本匹配的 session_id 集合
        let mut like_session_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut like_snippets: std::collections::HashMap<String, String> = std::collections::HashMap::new();

        if !query_text.trim().is_empty() {
            // 标题匹配
            let like_pattern = format!("%{}%", query_text.replace('%', "\\%").replace('_', "\\_"));
            let mut title_stmt = conn
                .prepare("SELECT id, title FROM chat_sessions WHERE LOWER(title) LIKE LOWER(?1) ESCAPE '\\'")
                .map_err(|e| format!("查询会话标题失败: {}", e))?;
            let title_rows = title_stmt
                .query_map(rusqlite::params![like_pattern], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| format!("查询会话标题失败: {}", e))?;
            for row in title_rows {
                if let Ok((id, title)) = row {
                    like_session_ids.insert(id.clone());
                    like_snippets.insert(id, format!("[标题] {}", title));
                }
            }

            // 消息内容匹配
            let mut msg_stmt = conn
                .prepare("SELECT DISTINCT session_id, content FROM chat_messages WHERE LOWER(content) LIKE LOWER(?1) ESCAPE '\\'")
                .map_err(|e| format!("查询消息失败: {}", e))?;
            let msg_rows = msg_stmt
                .query_map(rusqlite::params![like_pattern], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| format!("查询消息失败: {}", e))?;
            for row in msg_rows {
                if let Ok((sid, content)) = row {
                    like_session_ids.insert(sid.clone());
                    let snippet = if content.len() > 100 {
                        let char_end = content.char_indices().take(100).last()
                            .map(|(i, c)| i + c.len_utf8())
                            .unwrap_or(content.len());
                        format!("{}...", &content[..char_end])
                    } else {
                        content
                    };
                    like_snippets.entry(sid).or_insert(snippet);
                }
            }
        }

        // 2. 合并候选 session_id（LIKE 命中 + Indexer 命中）
        let mut candidate_ids: std::collections::HashSet<String> = like_session_ids.clone();
        for (sid, _, _) in indexer_hits {
            candidate_ids.insert(sid.clone());
        }

        if candidate_ids.is_empty() {
            return Ok(Vec::new());
        }

        // 3. 批量查询候选会话元信息
        let candidate_vec: Vec<String> = candidate_ids.into_iter().collect();
        let placeholders: Vec<String> = candidate_vec
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect();
        let sql = format!(
            "SELECT id, title, created_at, updated_at, favorite, message_count, token_usage, type FROM chat_sessions WHERE id IN ({})",
            placeholders.join(",")
        );
        let params: Vec<&dyn rusqlite::types::ToSql> = candidate_vec
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();

        let mut stmt = conn.prepare(&sql).map_err(|e| format!("查询会话失败: {}", e))?;
        let sessions: Vec<ChatSession> = stmt
            .query_map(params.as_slice(), |row| {
                let id: String = row.get(0)?;
                let title: String = row.get(1)?;
                let created_at: u64 = row.get(2)?;
                let updated_at: u64 = row.get(3)?;
                let favorite: i32 = row.get(4)?;
                let message_count: u32 = row.get(5)?;
                let token_usage: u32 = row.get(6)?;
                let r#type: String = row.get(7)?;
                Ok(ChatSession {
                    id,
                    title,
                    created_at,
                    updated_at,
                    favorite: favorite != 0,
                    message_count,
                    token_usage,
                    month_group: unix_timestamp_to_year_month(created_at),
                    r#type,
                })
            })
            .map_err(|e| format!("查询会话失败: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取会话失败: {}", e))?;

        let session_map: std::collections::HashMap<String, ChatSession> = sessions
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect();

        // 4. 组装结果：LIKE 命中给基础分 0.05，Indexer 命中保留其 RRF score，取 max
        let indexer_score_map: std::collections::HashMap<String, (f32, String)> = indexer_hits
            .iter()
            .cloned()
            .map(|(sid, score, text)| (sid, (score, text)))
            .collect();

        let mut results: Vec<ChatSessionSearchResult> = Vec::new();
        for (sid, session) in &session_map {
            let like_hit = like_session_ids.contains(sid);
            let indexer_hit = indexer_score_map.get(sid);

            if !like_hit && indexer_hit.is_none() {
                continue;
            }

            let (score, matched_content) = if let Some((idx_score, idx_text)) = indexer_hit {
                // Indexer 命中：使用 RRF score，matched_text 用索引返回的内容
                let idx_score = *idx_score;
                let final_score = if like_hit { idx_score.max(0.05) } else { idx_score };
                let content = like_snippets.get(sid).cloned().unwrap_or_else(|| idx_text.clone());
                (final_score, content)
            } else {
                // 仅 LIKE 命中：给基础分 0.05
                let content = like_snippets.get(sid).cloned().unwrap_or_default();
                (0.05, content)
            };

            results.push(ChatSessionSearchResult {
                session: session.clone(),
                score,
                matched_content,
            });
        }

        // 按 score 降序排列（f32 无法直接 Ord，通过 partial_cmp 降序）
        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        Ok(results)
    }

    /// 根据 ID 获取单个会话
    pub fn get_session(&self, session_id: &str) -> Result<Option<ChatSession>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let result = conn.query_row(
            "SELECT id, title, created_at, updated_at, favorite, message_count, token_usage, type FROM chat_sessions WHERE id = ?1",
            rusqlite::params![session_id],
            |row| {
                let id: String = row.get(0)?;
                let title: String = row.get(1)?;
                let created_at: u64 = row.get(2)?;
                let updated_at: u64 = row.get(3)?;
                let favorite: i32 = row.get(4)?;
                let message_count: u32 = row.get(5)?;
                let token_usage: u32 = row.get(6)?;
                let r#type: String = row.get(7)?;
                Ok(ChatSession {
                    id,
                    title,
                    created_at,
                    updated_at,
                    favorite: favorite != 0,
                    message_count,
                    token_usage,
                    month_group: unix_timestamp_to_year_month(created_at),
                    r#type,
                })
            },
        );
        match result {
            Ok(session) => Ok(Some(session)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(format!("查询会话失败: {}", e)),
        }
    }

    /// 获取指定类型最近更新的会话（按 updated_at DESC），用于新建会话时索引上一个会话
    pub fn get_last_session_by_type(&self, session_type: &str) -> Result<Option<ChatSession>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let result = conn.query_row(
            "SELECT id, title, created_at, updated_at, favorite, message_count, token_usage, type FROM chat_sessions WHERE type = ?1 ORDER BY updated_at DESC LIMIT 1",
            rusqlite::params![session_type],
            |row| {
                let id: String = row.get(0)?;
                let title: String = row.get(1)?;
                let created_at: u64 = row.get(2)?;
                let updated_at: u64 = row.get(3)?;
                let favorite: i32 = row.get(4)?;
                let message_count: u32 = row.get(5)?;
                let token_usage: u32 = row.get(6)?;
                let r#type: String = row.get(7)?;
                Ok(ChatSession {
                    id,
                    title,
                    created_at,
                    updated_at,
                    favorite: favorite != 0,
                    message_count,
                    token_usage,
                    month_group: unix_timestamp_to_year_month(created_at),
                    r#type,
                })
            },
        );
        match result {
            Ok(session) => Ok(Some(session)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(format!("查询会话失败: {}", e)),
        }
    }

    /// 更新会话标题
    pub fn update_session_title(&self, id: &str, title: &str) -> Result<(), String> {
        // update_session_title 与 rename_session 语义相同
        self.rename_session(id, title)
    }

    /// 记录最后打开的会话（覆盖写入）
    pub fn set_last_session(&self, session_id: &str, mode: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT OR REPLACE INTO chat_config (key, value) VALUES ('last_session', ?1)",
            rusqlite::params![serde_json::json!({"sessionId": session_id, "mode": mode}).to_string()],
        )
        .map_err(|e| format!("记录最后会话失败: {}", e))?;
        Ok(())
    }

    /// 获取最后打开的会话，返回 (session_id, mode)
    pub fn get_last_session(&self) -> Result<Option<(String, String)>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let result: Result<String, _> = conn.query_row(
            "SELECT value FROM chat_config WHERE key = 'last_session'",
            [],
            |row| row.get(0),
        );
        match result {
            Ok(json_str) => {
                let val: serde_json::Value = serde_json::from_str(&json_str)
                    .map_err(|e| format!("解析最后会话数据失败: {}", e))?;
                let session_id = val["sessionId"].as_str().unwrap_or("").to_string();
                let mode = val["mode"].as_str().unwrap_or("normal").to_string();
                if session_id.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some((session_id, mode)))
                }
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(format!("查询最后会话失败: {}", e)),
        }
    }

    /// 获取该会话已索引的消息条数（从 chat_config 读取，用于去重）
    pub fn get_indexed_message_count(&self, session_id: &str) -> Result<u32, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let key = format!("indexed_msg_count_{}", session_id);
        let result: Result<String, _> = conn.query_row(
            "SELECT value FROM chat_config WHERE key = ?1",
            rusqlite::params![key],
            |row| row.get(0),
        );
        match result {
            Ok(v) => v.parse::<u32>().map_err(|e| format!("解析已索引消息数失败: {}", e)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
            Err(e) => Err(format!("查询已索引消息数失败: {}", e)),
        }
    }

    /// 记录该会话已索引的消息条数（用于去重）
    pub fn set_indexed_message_count(&self, session_id: &str, count: u32) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let key = format!("indexed_msg_count_{}", session_id);
        conn.execute(
            "INSERT OR REPLACE INTO chat_config (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, count.to_string()],
        )
        .map_err(|e| format!("记录已索引消息数失败: {}", e))?;
        Ok(())
    }
}

// ─── 工具函数 ───

/// 获取当前 Unix 时间戳（毫秒）
fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 将 Unix 时间戳（秒）转换为 "YYYY-MM" 格式的月份分组字符串
fn unix_timestamp_to_year_month(ts: u64) -> String {
    // 兼容毫秒和秒级时间戳
    let ts_i: i64 = if ts > 100_000_000_000 { (ts / 1000) as i64 } else { ts as i64 };
    let mut days = ts_i / 86400;
    let mut year = 1970i32;

    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
        // 安全保护：防止无限循环
        if year > 3000 {
            break;
        }
    }

    let month_days = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1u32;
    for &md in &month_days {
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }

    format!("{:04}-{:02}", year, month)
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}
