use std::path::PathBuf;
use std::time::SystemTime;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::Manager;
use uuid::Uuid;

use crate::core::db::pool::DbPool;

/// Prompt 模板数据模型（三层：system / global / project）
#[derive(Debug, Serialize, Clone)]
pub struct PromptItem {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub scope: String,
    /// 是否对前端可见：false = 系统内置、用户无感知（如内部自动化 prompt）
    pub display: bool,
    /// 触发关键词（多个以逗号分隔）：用于将系统 prompt 与前端动作（如 AI_SELECTION_ACTIONS）
    /// 通过关键词匹配关联；也供未来意图匹配使用。
    #[serde(default)]
    pub keywords: String,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Deserialize)]
pub struct UpsertPromptRequest {
    pub name: String,
    pub prompt: String,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ════════════════════════════════════════════════════════════════════════════
// 系统内置 Prompt 种子（resources/prompt/seed.sql，写入全局 DB）
// ════════════════════════════════════════════════════════════════════════════

/// 系统内置 Prompt 资源目录（开发/测试兜底：源码 `resources/prompt`；运行时由
/// [`resolve_prompt_resource_dir`] 注入打包资源目录后覆盖）。
fn prompt_resource_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("prompt")
}

/// 解析系统 Prompt 资源目录：运行时资源目录优先（打包后），源码资源目录回退（开发期）。
fn resolve_prompt_resource_dir(app: &tauri::AppHandle) -> PathBuf {
    app.path()
        .resource_dir()
        .ok()
        .map(|r| r.join("prompt"))
        .filter(|p| p.is_dir())
        .unwrap_or_else(prompt_resource_dir)
}

/// 读取系统内置 Prompt 种子 SQL（`seed.sql`）。
///
/// 维护约定：开发人员只维护 seed.sql，系统 prompt 全部经此脚本以 `INSERT OR IGNORE`
/// 幂等写入全局 DB，不再扫描 .md 文件。
pub fn read_seed_sql(app: &tauri::AppHandle) -> Result<String, String> {
    let dir = resolve_prompt_resource_dir(app);
    let path = dir.join("seed.sql");
    std::fs::read_to_string(&path)
        .map_err(|e| format!("读取系统 Prompt 种子脚本失败 ({}): {}", path.display(), e))
}

// ════════════════════════════════════════════════════════════════════════════
// 全局 Prompt（用户数据目录，跨项目共享；含系统内置种子）
// ════════════════════════════════════════════════════════════════════════════

/// 全局 Prompt 存储（用户数据目录 `{APPDATA}/com.mdgo/prompts.db`，与 skill 全局目录同构）。
///
/// 表结构含 `scope`（system/global）与 `display`（0=前端隐藏 / 1=前端可见）列：
/// - system 行：由 `seed.sql` 初始化写入（`INSERT OR IGNORE`，用户改过不覆盖）；
/// - global 行：前端创建，display 恒为 1。
pub struct GlobalPromptStore {
    pool: DbPool,
}

impl GlobalPromptStore {
    /// 打开全局 Prompt 数据库（用户数据目录），建表 + 迁移 + 灌入系统种子。
    pub fn new(app: &tauri::AppHandle) -> Result<Self, String> {
        let dir = global_prompt_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建全局 Prompt 目录失败: {}", e))?;
        let db_path = dir.join("prompts.db");
        let pool = DbPool::open(db_path)?;
        pool.with_write(|conn| {
            Self::init_tables(conn)?;
            Self::migrate_columns(conn)?;
            Self::seed_system_prompts(conn, app)
        })?;
        Ok(Self { pool })
    }

    fn init_tables(conn: &Connection) -> Result<(), String> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS prompts (
                id         TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                prompt     TEXT NOT NULL DEFAULT '',
                scope      TEXT NOT NULL DEFAULT 'global',
                display    INTEGER NOT NULL DEFAULT 1,
                keywords   TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );",
        )
        .map_err(|e| format!("创建全局 prompts 表失败: {}", e))
    }

    /// 兼容旧表：补 scope / display / keywords 列（旧表无这些列）
    fn migrate_columns(conn: &Connection) -> Result<(), String> {
        // SQLite 无 IF NOT EXISTS 的 ADD COLUMN，逐列检测
        let has_scope: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('prompts') WHERE name = 'scope')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        let has_display: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('prompts') WHERE name = 'display')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        let has_keywords: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('prompts') WHERE name = 'keywords')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if !has_scope {
            conn.execute_batch("ALTER TABLE prompts ADD COLUMN scope TEXT NOT NULL DEFAULT 'global';")
                .map_err(|e| format!("迁移 prompts.scope 列失败: {}", e))?;
        }
        if !has_display {
            conn.execute_batch("ALTER TABLE prompts ADD COLUMN display INTEGER NOT NULL DEFAULT 1;")
                .map_err(|e| format!("迁移 prompts.display 列失败: {}", e))?;
        }
        if !has_keywords {
            conn.execute_batch("ALTER TABLE prompts ADD COLUMN keywords TEXT NOT NULL DEFAULT '';")
                .map_err(|e| format!("迁移 prompts.keywords 列失败: {}", e))?;
        }
        Ok(())
    }

    /// 执行 seed.sql 灌入系统内置 Prompt（INSERT OR IGNORE，幂等）。
    /// 种子脚本缺失（开发期尚未创建）时不视为失败，仅记录日志。
    fn seed_system_prompts(conn: &Connection, app: &tauri::AppHandle) -> Result<(), String> {
        match read_seed_sql(app) {
            Ok(sql) => {
                conn.execute_batch(&sql)
                    .map_err(|e| format!("执行系统 Prompt 种子脚本失败: {}", e))?;
                log::info!("[prompt] 系统内置 Prompt 种子已灌入全局 DB");
            }
            Err(e) => {
                log::warn!("[prompt] 跳过系统 Prompt 种子（{}）", e);
            }
        }
        Ok(())
    }

    /// 全部列表（含 display=0 的隐藏项）
    pub fn list_all(&self) -> Result<Vec<PromptItem>, String> {
        self.pool.with_read(|conn| {
            let mut stmt = conn
                .prepare_cached("SELECT id, name, prompt, scope, display, keywords, created_at, updated_at FROM prompts ORDER BY scope DESC, updated_at DESC")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |row| map_prompt_row(row))
                .map_err(|e| e.to_string())?;
            let mut items = Vec::new();
            for row in rows {
                items.push(row.map_err(|e| e.to_string())?);
            }
            Ok(items)
        })
    }

    /// 前端可见列表（display=1）
    pub fn list_visible(&self) -> Result<Vec<PromptItem>, String> {
        self.pool.with_read(|conn| {
            let mut stmt = conn
                .prepare_cached("SELECT id, name, prompt, scope, display, keywords, created_at, updated_at FROM prompts WHERE display = 1 ORDER BY scope DESC, updated_at DESC")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |row| map_prompt_row(row))
                .map_err(|e| e.to_string())?;
            let mut items = Vec::new();
            for row in rows {
                items.push(row.map_err(|e| e.to_string())?);
            }
            Ok(items)
        })
    }

    /// 创建全局 prompt（display 恒为 true，keywords 为空）
    pub fn create(&self, req: &UpsertPromptRequest) -> Result<PromptItem, String> {
        let id = Uuid::new_v4().to_string();
        let now = now_ms();
        self.pool.with_write(|conn| {
            conn.execute(
                "INSERT INTO prompts (id, name, prompt, scope, display, keywords, created_at, updated_at) VALUES (?1, ?2, ?3, 'global', 1, '', ?4, ?5)",
                rusqlite::params![id, req.name, req.prompt, now, now],
            )
            .map_err(|e| format!("创建全局 prompt 失败: {}", e))?;
            Ok(())
        })?;
        Ok(PromptItem {
            id,
            name: req.name.clone(),
            prompt: req.prompt.clone(),
            scope: "global".to_string(),
            display: true,
            keywords: String::new(),
            created_at: now,
            updated_at: now,
        })
    }

    /// 更新全局 prompt（display 保留原值）
    pub fn update(&self, id: &str, req: &UpsertPromptRequest) -> Result<PromptItem, String> {
        let now = now_ms();
        self.pool.with_write(|conn| {
            let affected = conn
                .execute(
                    "UPDATE prompts SET name = ?1, prompt = ?2, updated_at = ?3 WHERE id = ?4 AND scope = 'global'",
                    rusqlite::params![req.name, req.prompt, now, id],
                )
                .map_err(|e| format!("更新全局 prompt 失败: {}", e))?;
            if affected == 0 {
                return Err("prompt 不存在（或为系统内置，不可修改）".to_string());
            }
            let item = conn
                .query_row(
                    "SELECT id, name, prompt, scope, display, keywords, created_at, updated_at FROM prompts WHERE id = ?1",
                    rusqlite::params![id],
                    |row| map_prompt_row(row),
                )
                .map_err(|e| e.to_string())?;
            Ok(item)
        })
    }

    /// 删除全局 prompt（仅 global；system 拒绝）
    pub fn delete(&self, id: &str) -> Result<(), String> {
        self.pool.with_write(|conn| {
            let affected = conn
                .execute("DELETE FROM prompts WHERE id = ?1 AND scope = 'global'", rusqlite::params![id])
                .map_err(|e| format!("删除全局 prompt 失败: {}", e))?;
            if affected == 0 {
                return Err("prompt 不存在（或为系统内置，不可删除）".to_string());
            }
            Ok(())
        })
    }
}

fn map_prompt_row(row: &rusqlite::Row) -> rusqlite::Result<PromptItem> {
    Ok(PromptItem {
        id: row.get(0)?,
        name: row.get(1)?,
        prompt: row.get(2)?,
        scope: row.get(3)?,
        display: row.get::<_, i64>(4)? != 0,
        keywords: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

/// 全局 Prompt 数据目录（平台相关，与 SkillStore::global_skills_dir 同构）
pub fn global_prompt_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA")
            .map(|p| PathBuf::from(p).join("com.mdgo"))
            .unwrap_or_else(|_| PathBuf::from("com.mdgo"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var("HOME")
            .map(|h| {
                PathBuf::from(h)
                    .join("Library")
                    .join("Application Support")
                    .join("com.mdgo")
            })
            .unwrap_or_else(|_| PathBuf::from("com.mdgo"))
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        std::env::var("XDG_DATA_HOME")
            .map(|p| PathBuf::from(p).join("com.mdgo"))
            .or_else(|_| {
                std::env::var("HOME").map(|h| {
                    PathBuf::from(h)
                        .join(".local")
                        .join("share")
                        .join("com.mdgo")
                })
            })
            .unwrap_or_else(|_| PathBuf::from("com.mdgo"))
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 项目 Prompt（{dir}/.mdgo/mdgo.db 的 prompts 表，随项目走）
// ════════════════════════════════════════════════════════════════════════════

/// 项目 Prompt 存储（知识库级：`{dir}/.mdgo/mdgo.db`）
pub struct PromptStore {
    pool: DbPool,
}

impl PromptStore {
    /// 打开知识库级统一数据库（`{dir_path}/.mdgo/mdgo.db`）
    pub fn new(dir_path: &str) -> Result<Self, String> {
        let pool = DbPool::open_kb(dir_path)?;
        pool.with_write(|conn| Self::init_tables(conn))?;
        Ok(Self { pool })
    }

    fn init_tables(conn: &Connection) -> Result<(), String> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS prompts (
                id         TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                prompt     TEXT NOT NULL DEFAULT '',
                scope      TEXT NOT NULL DEFAULT 'project',
                display    INTEGER NOT NULL DEFAULT 1,
                keywords   TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );",
        )
        .map_err(|e| format!("创建 prompts 表失败: {}", e))?;
        Self::migrate_columns(conn)?;
        Self::migrate_content_to_prompt(conn)?;
        Ok(())
    }

    /// 兼容旧表：补 scope / display / keywords 列
    fn migrate_columns(conn: &Connection) -> Result<(), String> {
        let has_scope: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('prompts') WHERE name = 'scope')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        let has_display: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('prompts') WHERE name = 'display')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        let has_keywords: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('prompts') WHERE name = 'keywords')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if !has_scope {
            conn.execute_batch("ALTER TABLE prompts ADD COLUMN scope TEXT NOT NULL DEFAULT 'project';")
                .map_err(|e| format!("迁移 prompts.scope 列失败: {}", e))?;
        }
        if !has_display {
            conn.execute_batch("ALTER TABLE prompts ADD COLUMN display INTEGER NOT NULL DEFAULT 1;")
                .map_err(|e| format!("迁移 prompts.display 列失败: {}", e))?;
        }
        if !has_keywords {
            conn.execute_batch("ALTER TABLE prompts ADD COLUMN keywords TEXT NOT NULL DEFAULT '';")
                .map_err(|e| format!("迁移 prompts.keywords 列失败: {}", e))?;
        }
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

    /// 前端可见列表（display=1）
    pub fn list_visible(&self) -> Result<Vec<PromptItem>, String> {
        self.pool.with_read(|conn| {
            let mut stmt = conn
                .prepare_cached("SELECT id, name, prompt, scope, display, keywords, created_at, updated_at FROM prompts WHERE display = 1 ORDER BY updated_at DESC")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |row| map_prompt_row(row))
                .map_err(|e| e.to_string())?;
            let mut items = Vec::new();
            for row in rows {
                items.push(row.map_err(|e| e.to_string())?);
            }
            Ok(items)
        })
    }

    /// 创建项目 prompt（display 恒为 true，keywords 为空）
    pub fn create(&self, req: &UpsertPromptRequest) -> Result<PromptItem, String> {
        let id = Uuid::new_v4().to_string();
        let now = now_ms();
        self.pool.with_write(|conn| {
            conn.execute(
                "INSERT INTO prompts (id, name, prompt, scope, display, keywords, created_at, updated_at) VALUES (?1, ?2, ?3, 'project', 1, '', ?4, ?5)",
                rusqlite::params![id, req.name, req.prompt, now, now],
            )
            .map_err(|e| format!("创建 prompt 失败: {}", e))?;
            Ok(())
        })?;
        Ok(PromptItem {
            id,
            name: req.name.clone(),
            prompt: req.prompt.clone(),
            scope: "project".to_string(),
            display: true,
            keywords: String::new(),
            created_at: now,
            updated_at: now,
        })
    }

    /// 更新项目 prompt
    pub fn update(&self, id: &str, req: &UpsertPromptRequest) -> Result<PromptItem, String> {
        let now = now_ms();
        self.pool.with_write(|conn| {
            let affected = conn
                .execute(
                    "UPDATE prompts SET name = ?1, prompt = ?2, updated_at = ?3 WHERE id = ?4",
                    rusqlite::params![req.name, req.prompt, now, id],
                )
                .map_err(|e| format!("更新 prompt 失败: {}", e))?;
            if affected == 0 {
                return Err("prompt 不存在".to_string());
            }
            let item = conn
                .query_row(
                    "SELECT id, name, prompt, scope, display, keywords, created_at, updated_at FROM prompts WHERE id = ?1",
                    rusqlite::params![id],
                    |row| map_prompt_row(row),
                )
                .map_err(|e| e.to_string())?;
            Ok(item)
        })
    }

    /// 删除项目 prompt
    pub fn delete(&self, id: &str) -> Result<(), String> {
        self.pool.with_write(|conn| {
            let affected = conn
                .execute("DELETE FROM prompts WHERE id = ?1", rusqlite::params![id])
                .map_err(|e| format!("删除 prompt 失败: {}", e))?;
            if affected == 0 {
                return Err("prompt 不存在".to_string());
            }
            Ok(())
        })
    }
}
