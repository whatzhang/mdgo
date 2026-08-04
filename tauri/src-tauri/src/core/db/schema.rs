//! 全项目唯一建表 DDL 与种子数据文件（SOLID 单一职责原则）。
//!
//! 约定：此后所有新表一律在此文件定义；各 Store 只负责读写逻辑，不再内嵌 DDL。
//! 提供：
//! - `init_all(conn)`：依次执行全部 `CREATE TABLE IF NOT EXISTS` + 列迁移
//! - `seed_system_data(conn, system_skills)`：写入系统内置 Skill 种子数据
//! - `SYSTEM_SKILL_MD`：系统内置 Skill 的 SKILL.md 原文（编译期嵌入，随安装包分发）

use rusqlite::Connection;

use crate::core::skill::{Skill, SkillScope};

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
        "note-writing",
        include_str!("../../../resources/skills/note-writing/SKILL.md"),
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
            trigger_rules      TEXT NOT NULL DEFAULT '{}',-- JSON：type/keywords/similarity_threshold
            mutex              TEXT NOT NULL DEFAULT '[]',-- JSON 数组：互斥 skill id
            token_budget       INTEGER NOT NULL DEFAULT 0,-- 预留扩展
            input_schema       TEXT NOT NULL DEFAULT '[]',-- JSON 数组：入参 schema
            output_format      TEXT NOT NULL DEFAULT 'text',-- 预留扩展
            timeout_ms         INTEGER NOT NULL DEFAULT 30000,
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

        -- 会话挂载快照（含 version，恢复时校验版本漂移）
        CREATE TABLE IF NOT EXISTS chat_session_skills (
            session_id TEXT NOT NULL,
            scope      TEXT NOT NULL,
            skill_id   TEXT NOT NULL,
            version    INTEGER NOT NULL,
            PRIMARY KEY (session_id, scope, skill_id),
            FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE
        );

        -- 指标聚合（预留，为其他业务前置准备；M3 启用）
        CREATE TABLE IF NOT EXISTS skill_exec_metrics (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            request_id   TEXT NOT NULL,
            scope        TEXT NOT NULL,
            skill_id     TEXT NOT NULL,
            match_level  TEXT NOT NULL,      -- L1/L2/L3/attached/manual
            score        REAL,
            state        TEXT NOT NULL,      -- pending/running/success/failed/degraded
            duration_ms  INTEGER,
            tokens_in    INTEGER,
            tokens_out   INTEGER,
            error_code   TEXT,
            created_at   INTEGER NOT NULL
        );
        ",
    )
    .map_err(|e| format!("[schema] 建表失败: {}", e))?;
    Ok(())
}

/// 插入/更新单条技能记录（以 `(scope, id)` 为主键）。
pub fn upsert_skill_row(conn: &Connection, skill: &Skill) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO skills (
            id, scope, name, description, priority, trigger_rules, mutex, token_budget,
            input_schema, output_format, timeout_ms, tools, top_k, min_score,
            max_docs, max_chunks_per_doc, enabled, version, file_path, body, created_at, updated_at
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
            ?17, ?18, ?19, ?20, ?21, ?22
        )",
        rusqlite::params![
            skill.id,
            skill.scope.as_str(),
            skill.name,
            skill.description,
            skill.priority as i64,
            serde_json::to_string(&skill.trigger_rules).unwrap_or_else(|_| "{}".into()),
            serde_json::to_string(&skill.mutex).unwrap_or_else(|_| "[]".into()),
            skill.token_budget as i64,
            serde_json::to_string(&skill.input_schema).unwrap_or_else(|_| "[]".into()),
            skill.output_format,
            skill.timeout_ms as i64,
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

/// 从 DB 读取技能记录（M2 匹配调度 / 会话恢复用）。
#[allow(dead_code)]
pub fn load_skills_from_db(conn: &Connection) -> Result<Vec<Skill>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, scope, name, description, priority, trigger_rules, mutex, token_budget,
                    input_schema, output_format, timeout_ms, tools, top_k, min_score,
                    max_docs, max_chunks_per_doc, enabled, version, file_path, body, created_at, updated_at
             FROM skills",
        )
        .map_err(|e| format!("[schema] 查询技能失败: {}", e))?;

    let rows = stmt
        .query_map([], |row| {
            let scope_str: String = row.get(1)?;
            let trigger_rules_json: String = row.get(5)?;
            let mutex_json: String = row.get(6)?;
            let input_schema_json: String = row.get(8)?;
            let tools_json: String = row.get(11)?;

            Ok(Skill {
                id: row.get(0)?,
                scope: SkillScope::from_str(&scope_str).unwrap_or(SkillScope::Project),
                name: row.get(2)?,
                description: row.get(3)?,
                priority: row.get::<_, i64>(4)? as u32,
                trigger_rules: serde_json::from_str(&trigger_rules_json).unwrap_or_default(),
                mutex: serde_json::from_str(&mutex_json).unwrap_or_default(),
                token_budget: row.get::<_, i64>(7)? as u32,
                input_schema: serde_json::from_str(&input_schema_json).unwrap_or_default(),
                output_format: row.get(9)?,
                timeout_ms: row.get::<_, i64>(10)? as u64,
                tools: serde_json::from_str(&tools_json).unwrap_or_default(),
                top_k: row.get::<_, Option<i64>>(12)?.map(|v| v as u32),
                min_score: row.get(13)?,
                max_docs: row.get::<_, Option<i64>>(14)?.map(|v| v as usize),
                max_chunks_per_doc: row.get::<_, Option<i64>>(15)?.map(|v| v as usize),
                enabled: row.get::<_, i64>(16)? != 0,
                version: row.get::<_, i64>(17)? as u32,
                file_path: row.get(18)?,
                body: row.get(19)?,
                created_at: row.get::<_, i64>(20)? as u64,
                updated_at: row.get::<_, i64>(21)? as u64,
            })
        })
        .map_err(|e| format!("[schema] 读取技能失败: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("[schema] 读取技能失败: {}", e))?;

    Ok(rows)
}
