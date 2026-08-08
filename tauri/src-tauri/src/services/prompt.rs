use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Prompt 模板数据模型
#[derive(Debug, Serialize, Clone)]
pub struct PromptItem {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Deserialize)]
pub struct UpsertPromptRequest {
    pub name: String,
    pub prompt: String,
}

/// 全局 Prompt 存储（SQLite，位于用户数据目录）
pub struct PromptStore {
    conn: Mutex<Connection>,
}

impl PromptStore {
    /// 获取全局 prompt 数据库路径：%APPDATA%/com.mdgo/prompts.db
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
        base.join("com.mdgo").join("prompts.db")
    }

    pub fn new() -> Result<Self, String> {
        let db_path = Self::db_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建 prompt 数据库目录失败: {}", e))?;
        }
        let conn = Connection::open(&db_path)
            .map_err(|e| format!("打开 prompt 数据库失败: {}", e))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| format!("启用 WAL 模式失败: {}", e))?;
        Self::init_tables(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn init_tables(conn: &Connection) -> Result<(), String> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS prompts (
                id         TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                prompt     TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );",
        )
        .map_err(|e| format!("创建 prompts 表失败: {}", e))?;
        Self::migrate_content_to_prompt(conn)?;
        Ok(())
    }

    /// 兼容旧版本：早期字段名为 content，统一重命名为 prompt（SQLite >= 3.25 支持 RENAME COLUMN）
    fn migrate_content_to_prompt(conn: &Connection) -> Result<(), String> {
        let has_content: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('prompts') WHERE name = 'content')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        let has_prompt: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('prompts') WHERE name = 'prompt')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if has_content && !has_prompt {
            conn.execute_batch("ALTER TABLE prompts RENAME COLUMN content TO prompt;")
                .map_err(|e| format!("迁移 prompts 表字段失败: {}", e))?;
        }
        Ok(())
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// 列出所有 prompt（按 updated_at 降序）
    pub fn list(&self) -> Result<Vec<PromptItem>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT id, name, prompt, created_at, updated_at FROM prompts ORDER BY updated_at DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(PromptItem {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    prompt: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })
            .map_err(|e| e.to_string())?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row.map_err(|e| e.to_string())?);
        }
        Ok(items)
    }

    /// 创建 prompt
    pub fn create(&self, req: &UpsertPromptRequest) -> Result<PromptItem, String> {
        let id = Uuid::new_v4().to_string();
        let now = Self::now_ms();
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO prompts (id, name, prompt, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, req.name, req.prompt, now, now],
        )
        .map_err(|e| format!("创建 prompt 失败: {}", e))?;
        Ok(PromptItem {
            id,
            name: req.name.clone(),
            prompt: req.prompt.clone(),
            created_at: now,
            updated_at: now,
        })
    }

    /// 更新 prompt
    pub fn update(&self, id: &str, req: &UpsertPromptRequest) -> Result<PromptItem, String> {
        let now = Self::now_ms();
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let affected = conn
            .execute(
                "UPDATE prompts SET name = ?1, prompt = ?2, updated_at = ?3 WHERE id = ?4",
                rusqlite::params![req.name, req.prompt, now, id],
            )
            .map_err(|e| format!("更新 prompt 失败: {}", e))?;
        if affected == 0 {
            return Err("prompt 不存在".to_string());
        }
        // 读取更新后的完整记录
        let item = conn
            .query_row(
                "SELECT id, name, prompt, created_at, updated_at FROM prompts WHERE id = ?1",
                rusqlite::params![id],
                |row| {
                    Ok(PromptItem {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        prompt: row.get(2)?,
                        created_at: row.get(3)?,
                        updated_at: row.get(4)?,
                    })
                },
            )
            .map_err(|e| e.to_string())?;
        Ok(item)
    }

    /// 删除 prompt
    pub fn delete(&self, id: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let affected = conn
            .execute("DELETE FROM prompts WHERE id = ?1", rusqlite::params![id])
            .map_err(|e| format!("删除 prompt 失败: {}", e))?;
        if affected == 0 {
            return Err("prompt 不存在".to_string());
        }
        Ok(())
    }
}