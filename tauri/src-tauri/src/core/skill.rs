//! Skill 核心模块：模型、SKILL.md 解析与校验、文件存储、内存注册表、DB 缓存同步。
//!
//! 职责边界（单一职责原则）：
//! - `SkillStore`：目录解析 + SKILL.md 文件读写 + 三目录扫描（不含监控）
//! - `SkillRegistry`：内存注册表（RwLock 读写分离）+ 全量重建 + DB 缓存同步
//! - `SkillDb`：按目录打开 `.mdgo/mdgo.db` 并保证表结构（DDL 在 `core/db/schema.rs`）
//! - 文件变更监控已合并到 `core/watcher.rs` 的 `WatcherService`，不混入本模块
//! - `matcher`：分层意图匹配算法（已废弃，决策移交 LLM，模块已删除）
//! - `activation`：技能激活状态（L2 加载核心，LLM 驱动 activate_skill/deactivate_skill）
//! - `context`：技能预激活上下文解析（手动触发 / 会话挂载）

pub mod activation;
pub mod context;
pub mod metrics;
pub mod policy;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use rusqlite::{Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::core::db::schema;

/// 允许 Skill 声明的内置工具白名单（与 Rig Agent 注册的内置工具一致）。
/// 白名单仅为声明约束：技能声明了系统外的工具名时直接忽略，不做强类型校验。
/// 这是工具清单的「单一来源」：前端 Tauri 模式下经 `skill_allowed_tools` 下发，
/// 本地打开 index.html（无 Tauri）时使用前端内置 fallback 副本。
pub const ALLOWED_TOOLS: &[&str] = &[
    "kb_search", "code_lookup", "read", "edit", "multi_edit", "delete", "ls", "glob", "grep", "write", "git_status", "git_diff", "git_commit", "git_checkout", "webfetch",
    "activate_skill", "deactivate_skill", "pomodoro", "raw-parse", "schedule",
];

/// 工具展示名（与前端 fallback 一致；Tauri 模式下前端以此清单为准）
fn tool_label(key: &str) -> String {
    match key {
        "kb_search" => "知识库搜索".into(),
        "code_lookup" => "代码查找".into(),
        "read" => "读取文件".into(),
        "write" => "写入文件".into(),
        "glob" => "文件匹配".into(),
        "edit" => "编辑文件".into(),
        "multi_edit" => "批量编辑".into(),
        "delete" => "删除文件".into(),
        "grep" => "全局搜索".into(),
        "ls" => "列出文件".into(),
        "git_status" => "获取 Git 状态".into(),
        "git_diff" => "查看 Git 差异".into(),
        "git_commit" => "Git 提交".into(),
        "git_checkout" => "Git 恢复文件".into(),
        "webfetch" => "网页抓取".into(),
        "pomodoro" => "番茄钟".into(),
        "raw-parse" => "RAW/ARW 照片解析".into(),
        "schedule" => "日程管理".into(),
        "activate_skill" => "激活技能".into(),
        "deactivate_skill" => "停用技能".into(),
        _ => key.to_string(),
    }
}

/// 工具清单条目（key + 展示名），供前端技能表单/详情渲染。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AllowedToolInfo {
    pub key: String,
    pub label: String,
}

/// 返回工具白名单及展示名（单一来源；前端 Tauri 模式下经 command 获取）。
pub fn allowed_tools_info() -> Vec<AllowedToolInfo> {
    ALLOWED_TOOLS
        .iter()
        .map(|k| AllowedToolInfo {
            key: (*k).to_string(),
            label: tool_label(k),
        })
        .collect()
}

/// Skill 作用域（三层体系）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillScope {
    /// 系统内置（随安装包分发，只读）
    System,
    /// 用户全局（应用数据目录，跨项目共享）
    Global,
    /// 用户项目（{打开目录}/.mdgo/skills，随目录走）
    Project,
}

impl SkillScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkillScope::System => "system",
            SkillScope::Global => "global",
            SkillScope::Project => "project",
        }
    }

    pub fn from_str(s: &str) -> Option<SkillScope> {
        match s {
            "system" => Some(SkillScope::System),
            "global" => Some(SkillScope::Global),
            "project" => Some(SkillScope::Project),
            _ => None,
        }
    }

    /// 可写性：系统内置只读
    pub fn is_writable(&self) -> bool {
        !matches!(self, SkillScope::System)
    }
}

/// 作用域覆盖优先级（同名技能：项目 > 全局 > 系统）
fn scope_rank(scope: SkillScope) -> u8 {
    match scope {
        SkillScope::System => 0,
        SkillScope::Global => 1,
        SkillScope::Project => 2,
    }
}

/// SKILL.md YAML frontmatter（字段顺序即写入顺序，与 PRD Schema 契约一致）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
struct SkillFrontmatter {
    id: String,
    #[serde(default)]
    scope: Option<String>,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default = "default_priority")]
    priority: u32,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    triggers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min_score: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_docs: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_chunks_per_doc: Option<usize>,
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    created_at: u64,
    #[serde(default)]
    updated_at: u64,
}

fn default_priority() -> u32 {
    50
}
fn default_enabled() -> bool {
    true
}
fn default_version() -> u32 {
    1
}

/// Skill 完整模型（注册表 / DB / 前端返回统一使用）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: String,
    pub scope: SkillScope,
    pub name: String,
    pub description: String,
    pub priority: u32,
    pub tools: Vec<String>,
    /// 触发关键词（意图自动匹配激活用）：用户消息命中任一关键词即自动激活
    /// 本技能（请求级 Turn 生命周期，正文注入 + 工具解锁），作为 LLM 自主
    /// activate_skill 决策的可靠兜底。空列表 = 不参与自动匹配。
    pub triggers: Vec<String>,
    pub top_k: Option<u32>,
    pub min_score: Option<f32>,
    pub max_docs: Option<usize>,
    pub max_chunks_per_doc: Option<usize>,
    pub enabled: bool,
    pub version: u32,
    /// Markdown 指令正文
    pub body: String,
    /// 事实来源文件路径（系统内置为空字符串）
    pub file_path: String,
    pub created_at: u64,
    pub updated_at: u64,
}

/// 前端创建/更新 Skill 的入参（版本/时间戳由服务端计算）
#[derive(Debug, Clone, Deserialize)]
pub struct SkillInput {
    pub id: Option<String>,
    pub name: String,
    pub description: String,
    pub priority: Option<u32>,
    pub tools: Option<Vec<String>>,
    pub triggers: Option<Vec<String>>,
    pub top_k: Option<u32>,
    pub min_score: Option<f32>,
    pub max_docs: Option<usize>,
    pub max_chunks_per_doc: Option<usize>,
    pub enabled: Option<bool>,
    pub body: String,
}

impl SkillInput {
    /// 合并到基础 Skill（用于更新：None 字段保留原值）
    pub fn merge_into(&self, base: &Skill) -> Skill {
        let mut skill = base.clone();
        skill.name = self.name.clone();
        skill.description = self.description.clone();
        if let Some(v) = self.priority { skill.priority = v; }
        if let Some(v) = &self.tools {
            skill.tools = v
                .iter()
                .filter(|t| ALLOWED_TOOLS.contains(&t.as_str()))
                .cloned()
                .collect();
        }
        if let Some(v) = &self.triggers {
            skill.triggers = v
                .iter()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
        }
        if let Some(v) = self.top_k { skill.top_k = Some(v); }
        if let Some(v) = self.min_score { skill.min_score = Some(v); }
        if let Some(v) = self.max_docs { skill.max_docs = Some(v); }
        if let Some(v) = self.max_chunks_per_doc { skill.max_chunks_per_doc = Some(v); }
        if let Some(v) = self.enabled { skill.enabled = v; }
        skill.body = self.body.clone();
        skill
    }

    /// 由空输入构造新 Skill（用于创建）
    pub fn to_new_skill(&self, scope: SkillScope, id: &str) -> Skill {
        let now = unix_timestamp_now();
        Skill {
            id: id.to_string(),
            scope,
            name: self.name.clone(),
            description: self.description.clone(),
            priority: self.priority.unwrap_or(50),
            tools: self
                .tools
                .clone()
                .unwrap_or_default()
                .into_iter()
                .filter(|t| ALLOWED_TOOLS.contains(&t.as_str()))
                .collect(),
            triggers: self.triggers.clone().unwrap_or_default(),
            top_k: self.top_k,
            min_score: self.min_score,
            max_docs: self.max_docs,
            max_chunks_per_doc: self.max_chunks_per_doc,
            enabled: self.enabled.unwrap_or(true),
            version: 1,
            body: self.body.clone(),
            file_path: String::new(),
            created_at: now,
            updated_at: now,
        }
    }
}

/// 字段级校验错误（前端定位到具体字段）
#[derive(Debug, Clone, Serialize)]
pub struct SkillFieldError {
    pub field: String,
    pub message: String,
}

// ─── 时间戳工具 ───

fn unix_timestamp_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ─── SKILL.md 解析与序列化 ───

/// 解析 SKILL.md 全文（`---\nYAML\n---\n正文`），返回 Skill 或字段级错误列表。
pub fn parse_skill_md(
    content: &str,
    default_scope: SkillScope,
    file_path: String,
) -> Result<Skill, Vec<SkillFieldError>> {
    let (yaml_text, body) = split_frontmatter(content).ok_or_else(|| {
        vec![SkillFieldError {
            field: "frontmatter".into(),
            message: "缺少合法的 YAML frontmatter（需以 --- 开头并以 --- 结束）".into(),
        }]
    })?;

    let fm: SkillFrontmatter = serde_yaml::from_str(yaml_text).map_err(|e| {
        vec![SkillFieldError {
            field: "frontmatter".into(),
            message: format!("YAML 解析失败: {}", e),
        }]
    })?;

    let scope = match &fm.scope {
        Some(s) => match SkillScope::from_str(s) {
            Some(sc) => sc,
            None => {
                return Err(vec![SkillFieldError {
                    field: "scope".into(),
                    message: format!("scope 非法: {}（应为 system/global/project）", s),
                }]);
            }
        },
        None => default_scope,
    };

    let mut skill = Skill {
        id: fm.id,
        scope,
        name: fm.name,
        description: fm.description,
        priority: fm.priority,
        tools: fm.tools,
        triggers: fm.triggers,
        top_k: fm.top_k,
        min_score: fm.min_score,
        max_docs: fm.max_docs,
        max_chunks_per_doc: fm.max_chunks_per_doc,
        enabled: fm.enabled,
        version: fm.version,
        body: body.to_string(),
        file_path,
        created_at: fm.created_at,
        updated_at: fm.updated_at,
    };

    // 白名单仅为声明：不在系统白名单中的工具名直接忽略，不视为加载失败
    skill
        .tools
        .retain(|t| ALLOWED_TOOLS.contains(&t.as_str()));

    let errors = validate_skill(&skill);
    if errors.is_empty() {
        Ok(skill)
    } else {
        Err(errors)
    }
}

/// 从 YAML 片段中分离 frontmatter 与正文
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let content = content.trim_start_matches('\u{feff}');
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    // 允许 `---` 后紧跟换行
    let rest = rest.strip_prefix('\n')?;
    let end = rest.find("\n---")?;
    let yaml_text = &rest[..end];
    let after = &rest[end + 4..];
    let body = after.strip_prefix('\n').unwrap_or(after);
    Some((yaml_text, body))
}

/// Schema 校验（字段类型/枚举/白名单）
pub fn validate_skill(skill: &Skill) -> Vec<SkillFieldError> {
    let mut errors = Vec::new();

    if skill.id.trim().is_empty() {
        errors.push(field_err("id", "id 不能为空"));
    } else if !is_valid_skill_id(&skill.id) {
        errors.push(field_err(
            "id",
            "id 含非法字符（不允许路径分隔符 / \\、引号、控制字符与首尾空白，长度 ≤ 128）",
        ));
    }

    if skill.name.trim().is_empty() {
        errors.push(field_err("name", "name 不能为空"));
    }

    if skill.priority > 100 {
        errors.push(field_err("priority", "priority 必须在 0~100 之间"));
    }

    // 工具白名单仅为声明：不做强类型校验，系统外的工具名在加载时被忽略（见 parse_skill_md）

    errors
}

fn field_err(field: &str, message: impl Into<String>) -> SkillFieldError {
    SkillFieldError {
        field: field.into(),
        message: message.into(),
    }
}

/// 将字段级错误列表格式化为可读错误串
fn format_skill_field_errors(errors: &[SkillFieldError]) -> String {
    errors
        .iter()
        .map(|e| format!("{}: {}", e.field, e.message))
        .collect::<Vec<_>>()
        .join("; ")
}

/// 兼容主流通用 skill 命名（允许大小写、Unicode、空格、点、下划线等），
/// 仅做安全底线校验：非空、长度限制、禁止控制字符、路径分隔符与引号（防路径穿越及前端 DOM/JS 注入）。
fn is_valid_skill_id(id: &str) -> bool {
    if id.trim().is_empty() {
        return false;
    }
    if id != id.trim() {
        return false; // 首尾不允许空白，避免目录名歧义
    }
    if id.len() > 128 {
        return false;
    }
    if id == "." || id == ".." {
        return false; // 防路径穿越
    }
    // 引号（' " `）会被前端拼入内联事件与 HTML 属性，一并禁止
    !id.chars()
        .any(|c| c.is_control() || c == '/' || c == '\\' || c == '\'' || c == '"' || c == '`')
}

/// 将 Skill 序列化为完整 SKILL.md 全文（frontmatter + 正文）
pub fn to_skill_md(skill: &Skill) -> Result<String, String> {
    let fm = SkillFrontmatter {
        id: skill.id.clone(),
        scope: Some(skill.scope.as_str().into()),
        name: skill.name.clone(),
        description: skill.description.clone(),
        priority: skill.priority,
        tools: skill.tools.clone(),
        triggers: skill.triggers.clone(),
        top_k: skill.top_k,
        min_score: skill.min_score,
        max_docs: skill.max_docs,
        max_chunks_per_doc: skill.max_chunks_per_doc,
        enabled: skill.enabled,
        version: skill.version,
        created_at: skill.created_at,
        updated_at: skill.updated_at,
    };
    let yaml_text = serde_yaml::to_string(&fm)
        .map_err(|e| format!("序列化 frontmatter 失败: {}", e))?;
    Ok(format!("---\n{}---\n{}\n", yaml_text, skill.body))
}

// ─── 目录解析与文件存储 ───

/// Skill 文件存储与目录扫描
pub struct SkillStore;

impl SkillStore {
    /// 用户全局 Skill 目录（始终使用应用数据目录，避免安装目录不可写）
    pub fn global_skills_dir() -> PathBuf {
        #[cfg(target_os = "windows")]
        {
            std::env::var("APPDATA")
                .map(|p| PathBuf::from(p).join("com.mdgo").join("skills"))
                .unwrap_or_else(|_| PathBuf::from("com.mdgo").join("skills"))
        }
        #[cfg(target_os = "macos")]
        {
            std::env::var("HOME")
                .map(|h| {
                    PathBuf::from(h)
                        .join("Library")
                        .join("Application Support")
                        .join("com.mdgo")
                        .join("skills")
                })
                .unwrap_or_else(|_| PathBuf::from("com.mdgo").join("skills"))
        }
        #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
        {
            std::env::var("XDG_DATA_HOME")
                .map(|p| PathBuf::from(p).join("com.mdgo").join("skills"))
                .or_else(|_| {
                    std::env::var("HOME")
                        .map(|h| {
                            PathBuf::from(h)
                                .join(".local")
                                .join("share")
                                .join("com.mdgo")
                                .join("skills")
                        })
                })
                .unwrap_or_else(|_| PathBuf::from("com.mdgo").join("skills"))
        }
    }

    /// 用户项目 Skill 目录：{打开目录}/.mdgo/skills
    pub fn project_skills_dir(dir_path: &str) -> PathBuf {
        Path::new(dir_path).join(".mdgo").join("skills")
    }

    /// 单个 Skill 的目录：{scope_dir}/{skill_id}
    fn skill_dir(scope_dir: &Path, skill_id: &str) -> PathBuf {
        scope_dir.join(skill_id)
    }

    /// 解析系统内置技能（编译期嵌入常量，scope=system）
    pub fn scan_system() -> Vec<Skill> {
        schema::SYSTEM_SKILL_MD
            .iter()
            .filter_map(|(id, content)| {
                match parse_skill_md(content, SkillScope::System, String::new()) {
                    Ok(mut skill) => {
                        skill.id = id.to_string();
                        skill.scope = SkillScope::System;
                        Some(skill)
                    }
                    Err(e) => {
                        log::warn!("[skill] 系统内置技能解析失败 ({}): {:?}", id, e);
                        None
                    }
                }
            })
            .collect()
    }

    /// 扫描目录下全部 Skill，并收集解析失败项（供前端消息提醒）。
    ///
    /// 失败项格式：`{文件路径}: {字段}: {原因}`，多个字段错误以 `; ` 分隔。
    fn scan_dir_with_errors(
        scope_dir: &Path,
        scope: SkillScope,
        errors: &mut Vec<String>,
    ) -> Vec<Skill> {
        if !scope_dir.exists() {
            return Vec::new();
        }
        let mut skills = Vec::new();
        let entries = match std::fs::read_dir(scope_dir) {
            Ok(e) => e,
            Err(e) => {
                log::warn!("[skill] 扫描目录失败 ({}): {}", scope_dir.display(), e);
                return skills;
            }
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let skill_md = entry.path().join("SKILL.md");
            if !skill_md.exists() {
                continue;
            }
            match std::fs::read_to_string(&skill_md) {
                Ok(content) => {
                    match parse_skill_md(
                        &content,
                        scope,
                        skill_md.to_string_lossy().to_string(),
                    ) {
                        Ok(skill) => skills.push(skill),
                        Err(field_errors) => {
                            let detail = format_skill_field_errors(&field_errors);
                            log::warn!("[skill] 解析失败 ({}): {}", skill_md.display(), detail);
                            errors.push(format!("{}: {}", skill_md.display(), detail));
                        }
                    }
                }
                Err(e) => {
                    log::warn!("[skill] 读取失败 ({}): {}", skill_md.display(), e);
                    errors.push(format!("{}: 读取失败: {}", skill_md.display(), e));
                }
            }
        }
        skills
    }

    /// 写入（新建/更新）用户级 Skill 文件；系统内置拒绝写入
    pub fn save_skill(dir_path: &str, skill: &Skill) -> Result<(), String> {
        if !skill.scope.is_writable() {
            return Err("系统内置技能不可修改".into());
        }
        let scope_dir = match skill.scope {
            SkillScope::Global => Self::global_skills_dir(),
            SkillScope::Project => Self::project_skills_dir(dir_path),
            SkillScope::System => return Err("系统内置技能不可修改".into()),
        };
        let skill_md_path = Self::skill_dir(&scope_dir, &skill.id).join("SKILL.md");
        if let Some(parent) = skill_md_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建技能目录失败 ({}): {}", parent.display(), e))?;
        }
        let content = to_skill_md(skill)?;
        std::fs::write(&skill_md_path, &content)
            .map_err(|e| format!("写入技能文件失败 ({}): {}", skill_md_path.display(), e))?;
        log::info!(
            "[skill] 已保存技能 {}:{} -> {}",
            skill.scope.as_str(),
            skill.id,
            skill_md_path.display()
        );
        Ok(())
    }

    /// 删除用户级 Skill（整目录）
    pub fn delete_skill(dir_path: &str, scope: SkillScope, id: &str) -> Result<(), String> {
        if !scope.is_writable() {
            return Err("系统内置技能不可删除".into());
        }
        let scope_dir = match scope {
            SkillScope::Global => Self::global_skills_dir(),
            SkillScope::Project => Self::project_skills_dir(dir_path),
            SkillScope::System => return Err("系统内置技能不可删除".into()),
        };
        let dir = Self::skill_dir(&scope_dir, id);
        if !dir.exists() {
            return Err(format!("技能不存在: {}:{}", scope.as_str(), id));
        }
        std::fs::remove_dir_all(&dir)
            .map_err(|e| format!("删除技能目录失败 ({}): {}", dir.display(), e))?;
        log::info!("[skill] 已删除技能 {}:{}", scope.as_str(), id);
        Ok(())
    }
}

// ─── DB 缓存同步 ───

/// 技能 DB 缓存助手（无状态：连接由 [`SkillRegistry`] 按目录缓存复用）。
///
/// - `open_conn`：按目录打开 `.mdgo/mdgo.db` 并保证表结构（DDL 在 `core/db/schema.rs`）
/// - `sync_all`：全量重建 skills 表缓存（事务化，失败整体回滚，避免半同步状态）
pub struct SkillDb;

impl SkillDb {
    /// 打开目录数据库连接（创建父目录 + 建表），失败返回错误
    pub fn open_conn(dir_path: &str) -> Result<Connection, String> {
        let db_dir = Path::new(dir_path).join(".mdgo");
        std::fs::create_dir_all(&db_dir)
            .map_err(|e| format!("创建数据库目录失败: {}", e))?;
        let db_path = db_dir.join("mdgo.db");
        let conn = Connection::open(&db_path).map_err(|e| format!("打开技能数据库失败: {}", e))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| format!("启用 WAL 失败: {}", e))?;
        schema::init_all(&conn)?;
        Ok(conn)
    }

    /// 全量重建 skills 表缓存（先清空再重灌，整个操作在一个事务中）。
    ///
    /// 任一步失败即整体回滚，避免 DELETE 成功后插入中断导致的半同步状态。
    pub fn sync_all(conn: &mut Connection, skills: &[Skill]) -> Result<(), String> {
        // 写事务 IMMEDIATE：WAL 下避免 DEFERRED 读快照升级失败的 SQLITE_BUSY_SNAPSHOT
        let txn = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| format!("开启事务失败: {}", e))?;
        txn.execute("DELETE FROM skills", [])
            .map_err(|e| format!("清空技能缓存失败: {}", e))?;
        for skill in skills {
            schema::upsert_skill_row(&txn, skill)?;
        }
        txn.commit().map_err(|e| format!("提交事务失败: {}", e))?;
        Ok(())
    }
}

// ─── 内存注册表 ───

/// Skill 注册表：内存读写分离（读走 RwLock，写路径先落盘再刷新）。
///
/// 键 = `(scope, id)`；同名 id 按「系统 < 全局 < 项目」优先级覆盖。
pub struct SkillRegistry {
    inner: RwLock<HashMap<(SkillScope, String), Skill>>,
    /// 最近一次加载的目录（避免重复全量重建；写操作/watcher 会主动 reload）
    last_loaded_dir: RwLock<Option<String>>,
    /// 按目录缓存的技能 DB 连接（避免每次 reload 重开连接；Connection 是 Send）
    db_conns: std::sync::Mutex<HashMap<String, Connection>>,
    /// 最近一次 reload 的加载失败项（供命令层转发前端提醒；读走即清空）
    load_errors: std::sync::Mutex<Vec<String>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            last_loaded_dir: RwLock::new(None),
            db_conns: std::sync::Mutex::new(HashMap::new()),
            load_errors: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// 目录未加载过时执行一次全量重建（幂等）
    pub fn ensure_loaded(&self, dir_path: &str) -> Result<(), String> {
        {
            let guard = self.last_loaded_dir.read().unwrap_or_else(|e| e.into_inner());
            if guard.as_deref() == Some(dir_path) {
                return Ok(());
            }
        }
        self.reload(dir_path)?;
        Ok(())
    }

    /// 全量重建注册表（扫描系统/全局/项目三层 + 同步 DB 缓存）。
    ///
    /// 热更新入口：文件变更、增删改后均调用此方法，不重启服务。
    pub fn reload(&self, dir_path: &str) -> Result<usize, String> {
        let mut merged: HashMap<(SkillScope, String), Skill> = HashMap::new();

        // 1. 系统内置（嵌入常量）
        for skill in SkillStore::scan_system() {
            merged.insert((skill.scope, skill.id.clone()), skill);
        }
        // 2. 用户全局（覆盖系统同名） + 3. 用户项目（覆盖全局同名）
        let mut load_errors: Vec<String> = Vec::new();
        for skill in SkillStore::scan_dir_with_errors(
            &SkillStore::global_skills_dir(),
            SkillScope::Global,
            &mut load_errors,
        ) {
            merged.insert((skill.scope, skill.id.clone()), skill);
        }
        for skill in SkillStore::scan_dir_with_errors(
            &SkillStore::project_skills_dir(dir_path),
            SkillScope::Project,
            &mut load_errors,
        ) {
            merged.insert((skill.scope, skill.id.clone()), skill);
        }
        // 保存本次加载失败项（供命令层转发前端提醒）
        {
            let mut guard = self.load_errors.lock().unwrap_or_else(|e| e.into_inner());
            *guard = load_errors;
        }

        // 同步 DB 缓存（读多写少，失败不影响内存注册表）：连接按目录缓存复用，
        // 事务化重灌避免半同步状态；切换目录时释放其他目录的连接，防止长期泄漏
        {
            let mut cache = self.db_conns.lock().unwrap_or_else(|e| e.into_inner());
            cache.retain(|d, _| d == dir_path);
            if let std::collections::hash_map::Entry::Vacant(e) = cache.entry(dir_path.to_string()) {
                match SkillDb::open_conn(dir_path) {
                    Ok(conn) => {
                        e.insert(conn);
                    }
                    Err(err) => log::warn!("[skill] 打开技能数据库失败: {}", err),
                }
            }
            if let Some(conn) = cache.get_mut(dir_path) {
                let all: Vec<Skill> = merged.values().cloned().collect();
                if let Err(e) = SkillDb::sync_all(conn, &all) {
                    log::warn!("[skill] DB 缓存同步失败: {}", e);
                }
            }
        }

        {
            let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
            *guard = merged;
        }
        {
            let mut guard = self
                .last_loaded_dir
                .write()
                .unwrap_or_else(|e| e.into_inner());
            *guard = Some(dir_path.to_string());
        }

        let count = {
            let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
            guard.len()
        };
        log::info!("[skill] 注册表已重建: {} 个技能 (dir={})", count, dir_path);
        Ok(count)
    }

    /// 全部技能（按 priority 降序、创建时间升序）
    pub fn list(&self, scope: Option<SkillScope>) -> Vec<Skill> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut skills: Vec<Skill> = guard
            .values()
            .filter(|s| scope.map(|sc| s.scope == sc).unwrap_or(true))
            .cloned()
            .collect();
        skills.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.created_at.cmp(&b.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        skills
    }

    /// 单条查询
    pub fn get(&self, scope: SkillScope, id: &str) -> Option<Skill> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard.get(&(scope, id.to_string())).cloned()
    }

    /// 跨作用域查找启用技能（供 activate_skill 工具使用）。
    ///
    /// 同名多作用域时按「项目 > 全局 > 系统」覆盖优先级，取优先级最高的启用技能；
    /// 未启用或不存在返回 None。
    pub fn find_enabled(&self, id: &str) -> Option<Skill> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut best: Option<&Skill> = None;
        for skill in guard.values() {
            if skill.id != id || !skill.enabled {
                continue;
            }
            let replace = match best {
                None => true,
                Some(b) => scope_rank(skill.scope) > scope_rank(b.scope),
            };
            if replace {
                best = Some(skill);
            }
        }
        best.cloned()
    }

    /// 取走最近一次 reload 的加载失败项（消费后清空，用于转发前端提醒）
    pub fn take_load_errors(&self) -> Vec<String> {
        let mut guard = self.load_errors.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}
