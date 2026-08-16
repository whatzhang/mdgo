//! 全项目唯一建表 DDL 与种子数据文件（SOLID 单一职责原则）。
//!
//! 约定：此后所有新表一律在此文件定义；各 Store 只负责读写逻辑，不再内嵌 DDL。
//! 提供：
//! - `init_all(conn)`：依次执行全部 `CREATE TABLE IF NOT EXISTS` + 列迁移
//! - `seed_system_data(conn, system_skills)`：写入系统内置 Skill 种子数据
//! - `SYSTEM_SKILL_MD`：系统内置 Skill 的 SKILL.md 原文（编译期嵌入，随安装包分发）

use rusqlite::Connection;

use crate::core::skill::Skill;

/// 系统内置 Skill 的 SKILL.md 原文（id → 内容）。
/// 编译期 `include_str!` 嵌入二进制，保证 dev / 打包环境一致；
/// 文件归档于 `tauri/src-tauri/resources/skills/{id}/SKILL.md`，并随 bundle 打包。
pub const SYSTEM_SKILL_MD: &[(&str, &str)] = &[
    (
        "kb-summary",
        include_str!("../../../resources/skills/kb-summary/SKILL.md"),
    ),
    (
        "repo-status",
        include_str!("../../../resources/skills/repo-status/SKILL.md"),
    ),
    (
        "code-lookup",
        include_str!("../../../resources/skills/code-lookup/SKILL.md"),
    ),
    (
        "kb-search",
        include_str!("../../../resources/skills/kb-search/SKILL.md"),
    ),
    (
        "mermaid",
        include_str!("../../../resources/skills/mermaid/SKILL.md"),
    ),
    (
        "note-writing",
        include_str!("../../../resources/skills/note-writing/SKILL.md"),
    ),
    (
        "pomodoro",
        include_str!("../../../resources/skills/pomodoro/SKILL.md"),
    ),
    (
        "raw-photography",
        include_str!("../../../resources/skills/raw-photography/SKILL.md"),
    ),
    (
        "kanban",
        include_str!("../../../resources/skills/kanban/SKILL.md"),
    ),
    (
        "schedule",
        include_str!("../../../resources/skills/schedule/SKILL.md"),
    ),
    (
        "outline-mindmap",
        include_str!("../../../resources/skills/outline-mindmap/SKILL.md"),
    ),
    (
        "open-ui",
        include_str!("../../../resources/skills/open-ui/SKILL.md"),
    ),
];

/// 执行全部建表 DDL + 列迁移（幂等，可重复调用）。
pub fn init_all(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        -- 技能注册表（与 SKILL.md frontmatter 一一对应）
        CREATE TABLE IF NOT EXISTS skills (
            id                 TEXT NOT NULL,
            scope              TEXT NOT NULL,             -- system / global / project
            name               TEXT NOT NULL DEFAULT '',
            description        TEXT NOT NULL DEFAULT '',
            priority           INTEGER NOT NULL DEFAULT 50,
            tools              TEXT NOT NULL DEFAULT '[]',-- JSON 数组：工具白名单
            top_k              INTEGER,
            min_score          REAL,
            max_docs           INTEGER,
            max_chunks_per_doc INTEGER,
            enabled            INTEGER NOT NULL DEFAULT 1,
            version            INTEGER NOT NULL DEFAULT 1,
            file_path          TEXT NOT NULL DEFAULT '',
            body               TEXT NOT NULL DEFAULT '',  -- Markdown 指令正文
            created_at         INTEGER NOT NULL,
            updated_at         INTEGER NOT NULL,
            PRIMARY KEY (scope, id)
        );

        -- 会话挂载快照（含 version，恢复时校验版本漂移；mount_mode: warm=自动准备 / active=立即生效）
        CREATE TABLE IF NOT EXISTS chat_session_skills (
            session_id TEXT NOT NULL,
            scope      TEXT NOT NULL,
            skill_id   TEXT NOT NULL,
            version    INTEGER NOT NULL,
            mount_mode TEXT NOT NULL DEFAULT 'warm',
            PRIMARY KEY (session_id, scope, skill_id),
            FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE
        );

        -- 指标聚合：单次技能执行明细（按目录写入各自 .mdgo/mdgo.db）。
        -- 仅保留执行元数据（耗时/结果/来源/错误码），不记录入参与出参。
        CREATE TABLE IF NOT EXISTS skill_exec_metrics (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            request_id   TEXT NOT NULL,
            scope        TEXT NOT NULL,
            skill_id     TEXT NOT NULL,
            match_level  TEXT NOT NULL,      -- attached/manual/llm（激活来源）
            score        REAL,
            state        TEXT NOT NULL,      -- pending/running/success/failed/degraded
            duration_ms  INTEGER,
            tokens_in    INTEGER,
            tokens_out   INTEGER,
            error_code   TEXT,
            created_at   INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_skill_exec_metrics_created
            ON skill_exec_metrics (created_at);

        -- 调度统计（每目录一行聚合，见 metrics.rs；记录本次请求是否命中技能）
        CREATE TABLE IF NOT EXISTS skill_dispatch_stats (
            id                 INTEGER PRIMARY KEY CHECK (id = 1),
            total_dispatches   INTEGER NOT NULL DEFAULT 0,
            matched_dispatches INTEGER NOT NULL DEFAULT 0
        );
        ",
    )
    .map_err(|e| format!("[schema] 建表失败: {}", e))?;

    // 旧版本数据库迁移：删除已废弃的无消费列
    migrate_drop_legacy_columns(conn)?;
    // 挂载模式列（旧库补列，幂等：已有列则跳过）
    migrate_add_mount_mode(conn)?;
    Ok(())
}

/// 迁移：为 `chat_session_skills` 补充 `mount_mode` 列（旧库兼容，幂等）。
///
/// 开发阶段已在新建表 DDL 中带该列；此处仅对历史库执行一次 ADD COLUMN，
/// 避免挂载/查询在新列缺失时报错（INSERT no column / SELECT no column）。
fn migrate_add_mount_mode(conn: &Connection) -> Result<(), String> {
    let existing: Vec<String> = {
        let mut stmt = conn
            .prepare("PRAGMA table_info(chat_session_skills)")
            .map_err(|e| format!("[schema] 读取 chat_session_skills 表结构失败: {}", e))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| format!("[schema] 读取 chat_session_skills 表结构失败: {}", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("[schema] 读取 chat_session_skills 表结构失败: {}", e))?
    };
    if existing.iter().any(|c| c == "mount_mode") {
        return Ok(());
    }
    conn.execute_batch(
        "ALTER TABLE chat_session_skills ADD COLUMN mount_mode TEXT NOT NULL DEFAULT 'warm'",
    )
    .map_err(|e| format!("[schema] 迁移失败（ADD COLUMN mount_mode）: {}", e))?;
    log::info!("[schema] 已为旧库 chat_session_skills 补充 mount_mode 列");
    Ok(())
}

/// 插入/更新单条技能记录（以 `(scope, id)` 为主键）。
pub fn upsert_skill_row(conn: &Connection, skill: &Skill) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO skills (
            id, scope, name, description, priority, tools, top_k, min_score,
            max_docs, max_chunks_per_doc, enabled, version, file_path, body, created_at, updated_at
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
            ?13, ?14, ?15, ?16
        )",
        rusqlite::params![
            skill.id,
            skill.scope.as_str(),
            skill.name,
            skill.description,
            skill.priority as i64,
            serde_json::to_string(&skill.tools).unwrap_or_else(|_| "[]".into()),
            skill.top_k.map(|v| v as i64),
            skill.min_score,
            skill.max_docs.map(|v| v as i64),
            skill.max_chunks_per_doc.map(|v| v as i64),
            skill.enabled as i64,
            skill.version as i64,
            skill.file_path,
            skill.body,
            skill.created_at as i64,
            skill.updated_at as i64,
        ],
    )
    .map_err(|e| format!("[schema] 写入技能记录失败: {}", e))?;
    Ok(())
}

/// 迁移：删除旧版本遗留的无消费列（trigger_rules / mutex / token_budget / input_schema / output_format / timeout_ms）。
/// SQLite 3.35+ 支持 `ALTER TABLE ... DROP COLUMN`，这些列无索引/无外键依赖，可安全删除。
fn migrate_drop_legacy_columns(conn: &Connection) -> Result<(), String> {
    let legacy_cols = [
        "trigger_rules",
        "mutex",
        "token_budget",
        "input_schema",
        "output_format",
        "timeout_ms",
    ];
    let existing: Vec<String> = {
        let mut stmt = conn
            .prepare("PRAGMA table_info(skills)")
            .map_err(|e| format!("[schema] 读取 skills 表结构失败: {}", e))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| format!("[schema] 读取 skills 表结构失败: {}", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("[schema] 读取 skills 表结构失败: {}", e))?
    };
    for col in legacy_cols {
        if existing.iter().any(|c| c == col) {
            conn.execute_batch(&format!("ALTER TABLE skills DROP COLUMN {col}"))
                .map_err(|e| format!("[schema] 迁移失败（DROP COLUMN {col}）: {}", e))?;
        }
    }
    Ok(())
}
