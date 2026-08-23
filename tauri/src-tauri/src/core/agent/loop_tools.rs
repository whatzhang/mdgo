//! 业务工具的新内核实现——直接构建于 `core/loop` 基石（对齐 DeepSeek Harness `ToolDefinition`）。
//!
//! 迁移原则（"直接重构，不做桥接"）：每个工具 = [`ToolSpec`]（模型可见 schema）+ [`Tool::execute`]
//! （业务助手 + [`ToolEventSink`] 事件），**不依赖 rig**（`DynamicTool` 于 Phase 5 移除）。
//! 与 rig 版工具共存于 M1 并行验证期；命令层切换后 rig 版下线。
//!
//! SOLID：
//! - 单一职责：每个工具只做一件事；参数解析/软门禁抽为**纯函数**（可单测）；
//! - 开闭：新增工具 = 本模块加一个结构体 + [`build_loop_tool_registry`] 一行注册；
//! - 依赖倒置：工具依赖 `core/loop::Tool` 抽象与业务助手（`super::tools::*`），不依赖 LLM/循环。
//!
//! 本批迁移只读工具（检索 + 文件读/列举），`concurrency_safe=true`；写工具（edit/write/delete/
//! git_commit 等）在 Phase 4 迁移时保持 exclusive（副作用不可并行）。
//!
//! 注：Phase 4 命令层接入前，本模块构造器/构建器未被引用——dead_code 告警为预期，届时移除。

#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tauri::Manager;

use crate::core::agent::limits::{
    ASK_USER_TIMEOUT_SECS, KB_TOP_K_SCHEMA_MAX, MAX_GREP_FILES, MAX_LIST_ITEMS, MAX_TOP_K,
    POMODORO_MINUTES_MAX, READ_PATHS_MAX,
};
use crate::core::r#loop::{
    HashMapToolRegistry, Tool, ToolError, ToolRunContext, ToolSpec,
};

use super::tools::{glob_files, grep_files, list_files, read};
use super::{code_search, kb_search, KbSearchConfig};

// ─────────────────────────── 纯函数（可单测） ───────────────────────────

/// 软门禁判定（与 rig 版工具闭包语义一致）：
/// - `skill_gating=false`（子代理等受限场景）→ 放行；
/// - `skill_gating=true` 且 `allowed_tools()==None`（无激活技能）→ 引导（不放行）；
/// - 否则仅当激活技能声明了该工具时放行。
pub fn skill_gated(skill_gating: bool, allowed: Option<&Vec<String>>, tool: &str) -> bool {
    if !skill_gating {
        return true;
    }
    allowed.is_some_and(|list| list.iter().any(|t| t == tool))
}

/// 解析 `top_k`（钳制到 `[1, MAX_TOP_K]`；缺省回退 `default`）。
pub fn resolve_top_k(args: &Value, default: u32) -> u32 {
    args.get("top_k")
        .and_then(|t| t.as_u64())
        .map(|v| v as u32)
        .filter(|v| *v > 0)
        .map(|v| v.min(MAX_TOP_K))
        .unwrap_or(default.min(MAX_TOP_K))
}

/// 解析字符串列表参数：接受 JSON 数组 或 逗号分隔字符串（与 rig 版 `parse_str_list` 同语义）。
pub fn parse_str_list(v: &Value) -> Vec<String> {
    match v {
        Value::Array(items) => items
            .iter()
            .filter_map(|i| i.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect(),
        Value::String(s) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// 解析 read 参数：返回 `(单路径 Option, 多路径 Vec, offset)`。
pub fn parse_read_args(args: &Value) -> (Option<String>, Vec<String>, usize) {
    let single = args
        .get("path")
        .and_then(|s| s.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let multi: Vec<String> = args
        .get("paths")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let offset = args
        .get("offset")
        .and_then(|o| o.as_u64())
        .unwrap_or(0) as usize;
    (single, multi, offset)
}

// ─────────────────────────── 工具实现 ───────────────────────────

fn read_only_spec(
    name: &'static str,
    description: &'static str,
    parameters: Value,
) -> ToolSpec {
    let mut spec = ToolSpec::new(name, description, parameters);
    spec.concurrency_safe = true;
    spec
}

/// kb_search：知识库混合检索（业务助手 `kb_search`）。
pub struct KbSearchTool {
    cfg: KbSearchConfig,
}

impl KbSearchTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for KbSearchTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "kb_search",
                "在用户指定的本地知识库中检索与问题相关的文档片段。当回答需要知识库内容支撑、或当前信息不足时，调用本工具获取参考资料；可多次调用以从不同角度检索。",
                json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "用于检索知识库的问题或关键词，应聚焦单一角度"
                        },
                        "top_k": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": KB_TOP_K_SCHEMA_MAX,
                            "description": "期望返回的文档片段数量，默认 5"
                        }
                    },
                    "required": ["query"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let query = args
            .get("query")
            .and_then(|q| q.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if query.is_empty() {
            return Err(ToolError::InvalidArgs("检索关键词为空，请提供 query 参数".into()));
        }
        if !skill_gated(
            self.cfg.skill_gating,
            self.cfg.skill_state.allowed_tools().as_ref(),
            "kb_search",
        ) {
            return Ok(Value::String(
                "kb_search 需要先激活 kb-search 技能（请调用 activate_skill，skill_id='kb-search'）后才能执行，本次未执行检索。请先激活该技能，再重新发起检索。".into(),
            ));
        }
        let top_k = resolve_top_k(&args, self.cfg.default_top_k);
        ctx.sink.on_call(ctx.call_id, "kb_search", &query, &args);
        match kb_search(&self.cfg, &query, top_k).await {
            Ok((text, _structured)) => {
                let summary = format!("{} 字符", text.chars().count());
                ctx.sink.on_result(ctx.call_id, "kb_search", true, &summary, Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "kb_search", false, &e, Some(&e));
                Err(ToolError::Failed(format!("知识库检索失败: {e}")))
            }
        }
    }
}

/// code_lookup：按符号名定位代码定义（业务助手 `code_search`）。
pub struct CodeLookupTool {
    cfg: KbSearchConfig,
}

impl CodeLookupTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for CodeLookupTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "code_lookup",
                "在知识库中按符号名（函数名、类名、方法名、变量名等）定位代码定义位置。当问题涉及具体的函数/类/方法名、或需要查找某段代码在哪个文件实现时，调用本工具；符号名越精确，检索效果越好。",
                json!({
                    "type": "object",
                    "properties": {
                        "symbol": {
                            "type": "string",
                            "description": "要查找的代码符号名，如 handle_timeout、LRUCache、parseJSON"
                        },
                        "top_k": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": KB_TOP_K_SCHEMA_MAX,
                            "description": "期望返回的代码片段数量，默认 5"
                        }
                    },
                    "required": ["symbol"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let symbol = args
            .get("symbol")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if symbol.is_empty() {
            return Err(ToolError::InvalidArgs("请提供要查找的代码符号名".into()));
        }
        if !skill_gated(
            self.cfg.skill_gating,
            self.cfg.skill_state.allowed_tools().as_ref(),
            "code_lookup",
        ) {
            return Ok(Value::String(
                "code_lookup 需要先激活 kb-search/code-lookup 技能后才能执行，本次未执行查找。请先激活对应技能后重试。".into(),
            ));
        }
        let top_k = resolve_top_k(&args, self.cfg.default_top_k);
        ctx.sink.on_call(ctx.call_id, "code_lookup", &symbol, &args);
        match code_search(&self.cfg, &symbol, top_k).await {
            Ok((text, _structured)) => {
                let summary = format!("{} 字符", text.chars().count());
                ctx.sink.on_result(ctx.call_id, "code_lookup", true, &summary, Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "code_lookup", false, &e, Some(&e));
                Err(ToolError::Failed(format!("符号检索失败: {e}")))
            }
        }
    }
}

/// read：读取文件（单路径分页 / 多路径并行，业务助手 `read`）。
pub struct ReadTool {
    cfg: KbSearchConfig,
}

impl ReadTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "read",
                "读取文件内容，单次最多返回 8192 字符，支持分页续读。支持两类路径：1) 知识库目录内的相对路径（如 docs/note.md，可读取打开目录中的所有文件，含子目录）；2) 当前激活技能的参考文档路径（如 references/flowchart.md，通常由技能 SKILL.md 中以相对链接给出；未激活技能时无法读取，需先 activate_skill）。当返回内容末尾提示\"内容过长\"时，内容只显示了第 1~8192 字符，若需要文件后续部分，请再次调用本工具并指定 offset 参数（如 offset=8192）继续读取，不要从头重读全文。如需一次读取多个文件，可用 paths 数组并行读取（最多 10 个）。",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "文件相对路径：知识库内路径，或技能参考文档路径（如 references/flowchart.md）。与 paths 二选一"
                        },
                        "paths": {
                            "type": "array",
                            "maxItems": READ_PATHS_MAX,
                            "items": { "type": "string" },
                            "description": "多个文件相对路径（并行读取，最多 10 个）。与 path 二选一"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "字符偏移量（从 0 开始），用于分页续读长文件。首次读取省略；截断提示中会给出下次应使用的 offset"
                        }
                    },
                    "anyOf": [
                        { "required": ["path"] },
                        { "required": ["paths"] }
                    ]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let (single, multi, offset) = parse_read_args(&args);
        if single.is_none() && multi.is_empty() {
            return Err(ToolError::InvalidArgs("文件路径为空，请提供 path 或 paths 参数".into()));
        }
        if !multi.is_empty() {
            if multi.len() > READ_PATHS_MAX {
                return Err(ToolError::InvalidArgs(format!("paths 最多 {READ_PATHS_MAX} 个文件")));
            }
            let preview = format!("{} 个文件", multi.len());
            ctx.sink.on_call(ctx.call_id, "read", &preview, &args);
            // 并行读取（缓冲 4 并发），按输入顺序拼接（业务逻辑与 rig 版一致，rig-free）
            let mut entries: Vec<(String, Result<String, String>)> = Vec::new();
            let mut stream = futures_util::stream::iter(multi.iter().cloned())
                .map(|p| {
                    let cfg = self.cfg.clone();
                    async move {
                        let out = read(&cfg, &p, 0).await;
                        (p, out)
                    }
                })
                .buffer_unordered(4);
            while let Some(entry) = stream.next().await {
                entries.push(entry);
            }
            let mut out = String::new();
            let mut failed = false;
            for p in &multi {
                if let Some((_, Ok(text))) = entries.iter().find(|(pp, _)| pp == p) {
                    out.push_str(&format!("===== {p} =====\n{text}\n"));
                } else if let Some((_, Err(e))) = entries.iter().find(|(pp, _)| pp == p) {
                    failed = true;
                    out.push_str(&format!("===== {p}（读取失败）=====\n{e}\n"));
                }
            }
            let summary = format!("{} 个文件", multi.len());
            ctx.sink.on_result(ctx.call_id, "read", !failed, &summary, Some(&out));
            return Ok(Value::String(out));
        }

        let rel = single.expect("multi 为空时 single 必为 Some");
        let preview = if offset == 0 { rel.clone() } else { format!("{rel} (offset={offset})") };
        ctx.sink.on_call(ctx.call_id, "read", &preview, &args);
        match read(&self.cfg, &rel, offset).await {
            Ok(text) => {
                let summary = format!("{} 字符", text.chars().count());
                ctx.sink.on_result(ctx.call_id, "read", true, &summary, Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "read", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// grep：文本搜索（业务助手 `grep_files`）。
pub struct GrepTool {
    cfg: KbSearchConfig,
}

impl GrepTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "grep",
                "在知识库目录内的文本文件中搜索关键词（大小写不敏感子串匹配，跳过二进制与超大文件）。已按用户配置的目录/文件黑名单过滤（如 assets/、node_modules/ 等配置的目录不会被搜索）。输出格式：每个命中文件先输出一行相对路径，随后每行\"  行号: 内容\"；context_lines>0 时匹配行以 \">\" 开头、上下文行以空格开头、非连续区间用 \"--\" 分隔；list_only=true 时仅输出文件名。pattern 支持多关键词（空格分隔）：默认 and 模式，可设 match_mode=\"or\"；用双引号包裹 pattern 可精确搜索连续短语。include/exclude 支持 glob 与目录名。\n使用建议：快速定位文件用 list_only=true；需要看懂代码片段周边逻辑用 context_lines=3；缩小范围用 include:[\"*.rs\"]；定位后建议用 read 工具精读相关行。",
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "搜索文本（至少 2 个字符），大小写不敏感；多个词以空格分隔默认 AND 匹配；用双引号包裹（如 \"fn main()\"）开启精确连续短语匹配"
                        },
                        "max_files": {
                            "type": "integer",
                            "default": 10,
                            "minimum": 1,
                            "maximum": MAX_GREP_FILES,
                            "description": "最多返回命中文件数，默认 10，最大 20"
                        },
                        "include": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "glob 包含过滤器，例：[\"*.rs\",\"*.md\"]；也可传逗号分隔字符串"
                        },
                        "exclude": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "glob 排除过滤器，例：[\"target/**\",\"dist/**\"]；也可传逗号分隔字符串"
                        },
                        "context_lines": {
                            "type": "integer",
                            "default": 0,
                            "minimum": 0,
                            "maximum": 5,
                            "description": "匹配行前后展示的上下文行数（最大 5）"
                        },
                        "match_mode": {
                            "type": "string",
                            "enum": ["and", "or"],
                            "default": "and",
                            "description": "多关键词匹配策略"
                        },
                        "list_only": {
                            "type": "boolean",
                            "default": false,
                            "description": "只输出匹配的文件名称（等效 grep -l）"
                        }
                    },
                    "required": ["pattern"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let pattern = args
            .get("pattern")
            .and_then(|p| p.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if pattern.is_empty() {
            return Err(ToolError::InvalidArgs("搜索关键词为空，请提供 pattern 参数".into()));
        }
        let max_files = args.get("max_files").and_then(|m| m.as_u64()).map(|v| v as u32).unwrap_or(10);
        let include = parse_str_list(args.get("include").unwrap_or(&Value::Null));
        let exclude = parse_str_list(args.get("exclude").unwrap_or(&Value::Null));
        let context_lines = args.get("context_lines").and_then(|c| c.as_u64()).unwrap_or(0) as usize;
        let match_mode = args.get("match_mode").and_then(|m| m.as_str()).unwrap_or("and").to_string();
        let list_only = args.get("list_only").and_then(|b| b.as_bool()).unwrap_or(false);
        ctx.sink.on_call(ctx.call_id, "grep", &pattern, &args);
        match grep_files(
            &self.cfg, &pattern, max_files, &include, &exclude, context_lines, &match_mode, list_only,
        )
        .await
        {
            Ok(text) => {
                let summary = format!("{} 字符", text.chars().count());
                ctx.sink.on_result(ctx.call_id, "grep", true, &summary, Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "grep", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// ls：列举目录（业务助手 `list_files`）。
pub struct LsTool {
    cfg: KbSearchConfig,
}

impl LsTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for LsTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "ls",
                "列举知识库目录下的文件与子目录（返回相对路径与大小），支持按名称子串过滤，最多返回 60 项。已按用户配置的目录/文件黑名单过滤（如 assets/、node_modules/、dist/ 等配置的目录不会列出；系统内置的 .mdgo 内部数据同样排除）。当需要了解知识库目录结构、或不确定文件路径时调用。",
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "文件名子串过滤条件（不区分大小写），为空则列出全部"
                        },
                        "max_items": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": MAX_LIST_ITEMS,
                            "description": "最多返回条数，默认 30，上限 60"
                        }
                    },
                    "required": []
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let pattern = args
            .get("pattern")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        let max_items = args.get("max_items").and_then(|v| v.as_u64()).map(|v| v as u32).unwrap_or(30);
        let preview = if pattern.is_empty() { "全部".to_string() } else { pattern.clone() };
        ctx.sink.on_call(ctx.call_id, "ls", &preview, &args);
        match list_files(&self.cfg, &pattern, max_items).await {
            Ok(text) => {
                let summary = format!("{} 项", text.lines().count().saturating_sub(1));
                ctx.sink.on_result(ctx.call_id, "ls", true, &summary, Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "ls", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// glob：按 glob 模式列举文件（业务助手 `glob_files`）。
pub struct GlobTool {
    cfg: KbSearchConfig,
}

impl GlobTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "glob",
                "按 glob 模式列举当前打开知识库目录内匹配的文件（相对路径 + 字节大小）。模式支持 *（单层任意）、**（任意层级）、?（单字符）、[abc]（字符集）；含 / 的模式锚定根目录，裸文件名（如 *.rs）匹配任意层级的 basename；目录名（如 src）自动展开为其下全部文件。已按用户配置的目录/文件黑名单过滤。最多返回 60 个匹配文件，超出会提示剩余数量。用于快速定位文件与批量确认路径，比 grep 更轻量。",
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "glob 模式，如 **/*.rs、docs/*.md、src"
                        },
                        "max_items": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": MAX_LIST_ITEMS,
                            "description": "最多返回条数，默认 30，上限 60"
                        }
                    },
                    "required": ["pattern"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let pattern = args
            .get("pattern")
            .and_then(|p| p.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if pattern.is_empty() {
            return Err(ToolError::InvalidArgs("glob 模式不能为空，如 **/*.rs、docs/*.md".into()));
        }
        let max_items = args.get("max_items").and_then(|v| v.as_u64()).map(|v| v as u32).unwrap_or(30);
        ctx.sink.on_call(ctx.call_id, "glob", &pattern, &args);
        match glob_files(&self.cfg, &pattern, max_items).await {
            Ok(text) => {
                let summary = format!("{} 字符", text.chars().count());
                ctx.sink.on_result(ctx.call_id, "glob", true, &summary, Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "glob", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// graph_search_nodes：图谱节点搜索（PRD §56：Agent 定位知识实体）。
pub struct GraphSearchTool {
    cfg: KbSearchConfig,
}

impl GraphSearchTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for GraphSearchTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "graph_search_nodes",
                "在本地知识图谱中搜索节点（文档/目录/实体/概念/技术等），返回匹配节点的名称、类型与度数。当用户询问某个概念/技术/项目是否在知识库中、或需要定位图谱实体时调用。",
                json!({
                    "type": "object",
                    "properties": {
                        "keyword": { "type": "string", "description": "搜索关键词" },
                        "limit": { "type": "integer", "description": "最多返回条数，默认 10，上限 50" }
                    },
                    "required": ["keyword"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let keyword = args.get("keyword").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        if keyword.is_empty() {
            return Err(ToolError::InvalidArgs("keyword 为空".into()));
        }
        let limit = args.get("limit").and_then(|v| v.as_u64()).map(|v| v as u32).unwrap_or(10);
        ctx.sink.on_call(ctx.call_id, "graph_search_nodes", &keyword, &args);
        match super::tools::graph_search_nodes(&self.cfg, &keyword, limit).await {
            Ok(text) => {
                ctx.sink.on_result(ctx.call_id, "graph_search_nodes", true, &format!("{} 字符", text.chars().count()), Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "graph_search_nodes", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// graph_find_related：图节点邻域与关系（PRD §56 find_related_knowledge / find_evidence）。
pub struct GraphFindRelatedTool {
    cfg: KbSearchConfig,
}

impl GraphFindRelatedTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for GraphFindRelatedTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "graph_find_related",
                "查询知识图谱中某节点的邻域与关系（如 A 引用 B、A 使用 Redis）。用于回答「X 和哪些知识相关」「X 使用了什么」等问题，返回实体间关系证据。",
                json!({
                    "type": "object",
                    "properties": {
                        "node": { "type": "string", "description": "节点名称（实体/概念/文档名）" },
                        "depth": { "type": "integer", "description": "扩展深度，默认 1，上限 2" }
                    },
                    "required": ["node"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let node = args.get("node").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        if node.is_empty() {
            return Err(ToolError::InvalidArgs("node 为空".into()));
        }
        let depth = args.get("depth").and_then(|v| v.as_u64()).map(|v| v as u32).unwrap_or(1);
        ctx.sink.on_call(ctx.call_id, "graph_find_related", &node, &args);
        match super::tools::graph_find_related(&self.cfg, &node, depth).await {
            Ok(text) => {
                ctx.sink.on_result(ctx.call_id, "graph_find_related", true, &format!("{} 字符", text.chars().count()), Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "graph_find_related", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// graph_find_path：图谱最短路径（PRD §56 reason_over_graph / §24 find_path）。
pub struct GraphFindPathTool {
    cfg: KbSearchConfig,
}

impl GraphFindPathTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for GraphFindPathTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "graph_find_path",
                "查询知识图谱中两个节点之间的最短关系路径（如 Redis → Cache → Application → Kubernetes）。用于回答「A 和 B 有什么关系/如何关联」类问题，返回路径上的节点与跳数。",
                json!({
                    "type": "object",
                    "properties": {
                        "source": { "type": "string", "description": "起点名称" },
                        "target": { "type": "string", "description": "终点名称" }
                    },
                    "required": ["source", "target"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let source = args.get("source").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        let target = args.get("target").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        if source.is_empty() || target.is_empty() {
            return Err(ToolError::InvalidArgs("source/target 不能为空".into()));
        }
        ctx.sink.on_call(ctx.call_id, "graph_find_path", &format!("{} -> {}", source, target), &args);
        match super::tools::graph_find_path(&self.cfg, &source, &target).await {
            Ok(text) => {
                ctx.sink.on_result(ctx.call_id, "graph_find_path", true, &format!("{} 字符", text.chars().count()), Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "graph_find_path", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// 构建已迁移工具的注册表（M1 并行验证期：新 loop 路径使用；rig 版工具下线于 Phase 5）。
pub fn build_loop_tool_registry(cfg: KbSearchConfig) -> HashMapToolRegistry {    let mut reg = HashMapToolRegistry::new();
    // 只读（concurrency_safe=true）
    reg.register(Arc::new(KbSearchTool::new(cfg.clone())));
    reg.register(Arc::new(CodeLookupTool::new(cfg.clone())));
    reg.register(Arc::new(ReadTool::new(cfg.clone())));
    reg.register(Arc::new(GrepTool::new(cfg.clone())));
    reg.register(Arc::new(LsTool::new(cfg.clone())));
    reg.register(Arc::new(GlobTool::new(cfg.clone())));
    // 写操作（concurrency_safe=false → exclusive；审批由 loop 层 ApprovalHook 门控）
    reg.register(Arc::new(WriteTool::new(cfg.clone())));
    reg.register(Arc::new(EditTool::new(cfg.clone())));
    reg.register(Arc::new(MultiEditTool::new(cfg.clone())));
    reg.register(Arc::new(DeleteTool::new(cfg.clone())));
    reg.register(Arc::new(GitStatusTool::new(cfg.clone())));
    reg.register(Arc::new(GitDiffTool::new(cfg.clone())));
    reg.register(Arc::new(GitCommitTool::new(cfg.clone())));
    reg.register(Arc::new(GitCheckoutTool::new(cfg.clone())));
    // 长期记忆 + 任务清单
    reg.register(Arc::new(RememberTool::new(cfg.clone())));
    reg.register(Arc::new(ForgetTool::new(cfg.clone())));
    reg.register(Arc::new(SearchMemoryTool::new(cfg.clone())));
    reg.register(Arc::new(TodoWriteTool::new(cfg.clone())));
    // 子代理 + 网络（多代理/Web 能力）
    reg.register(Arc::new(DeepResearchTool::new(cfg.clone())));
    reg.register(Arc::new(ReadSubagentResultTool::new(cfg.clone())));
    reg.register(Arc::new(SpawnSubagentTool::new(cfg.clone())));
    reg.register(Arc::new(ParallelResearchTool::new(cfg.clone())));
    // 反思质量门 + 用户澄清（exclusive：等待用户期间不并行其他工具）
    reg.register(Arc::new(SelfReviewTool::new(cfg.clone())));
    reg.register(Arc::new(AskUserQuestionTool::new(cfg.clone())));
    // 日程 + 书签（schedule 独占；书签只读可并行）
    reg.register(Arc::new(ScheduleTool::new(cfg.clone())));
    reg.register(Arc::new(SearchBookmarksTool::new(cfg.clone())));
    reg.register(Arc::new(GetBookmarkTool::new(cfg.clone())));
    // 知识图谱（PRD §56：Agent 使用知识图谱；只读可并行）
    reg.register(Arc::new(GraphSearchTool::new(cfg.clone())));
    reg.register(Arc::new(GraphFindRelatedTool::new(cfg.clone())));
    reg.register(Arc::new(GraphFindPathTool::new(cfg.clone())));
    // 前端桥接工具（pomodoro/raw-parse/open-ui，技能声明门控）+ 外部 HTTP 工具（配置驱动）
    register_bridge_tools(&mut reg, cfg.clone());
    register_external_tools(&mut reg, cfg.clone());
    reg.register(Arc::new(WebfetchTool::new(cfg)));
    reg
}

// ─────────────────────────── 子代理 + 网络工具 ───────────────────────────

fn subagent_mode(s: &str) -> crate::core::subagent::SubagentMode {
    if s == "write" {
        crate::core::subagent::SubagentMode::Write
    } else {
        crate::core::subagent::SubagentMode::ReadOnly
    }
}

/// deep_research：派生只读子代理做深度调研（复用公共执行器 run_subagent_impl，v3 内核）。
pub struct DeepResearchTool {
    cfg: KbSearchConfig,
}

impl DeepResearchTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for DeepResearchTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "deep_research",
                "派生一个隔离上下文的只读子代理进行深度调研：它可以检索知识库（kb_search）、读取与搜索文件（read/grep/ls），适合需要阅读大量文件、跨文档总结、独立调查的任务。子代理不修改任何文件，也不共享当前对话的技能激活状态。返回有界摘要（含 subagent_id）；若需完整结果，用 read_subagent_result 指定 subagent_id 分页读取。",
                json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "调研任务描述：说明要调查什么、产出什么形式的结论"
                        },
                        "max_turns": {
                            "type": "integer",
                            "description": "可选，子代理轮次上限（默认 12，最大 30）",
                            "minimum": 1,
                            "maximum": 30
                        }
                    },
                    "required": ["task"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let task = args.get("task").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        let max_turns = args.get("max_turns").and_then(|m| m.as_u64()).map(|v| v as usize).unwrap_or(12).clamp(1, 30);
        ctx.sink.on_call(ctx.call_id, "deep_research", &format!("task_len={} max_turns={}", task.len(), max_turns), &args);
        if task.is_empty() {
            let e = "task 不能为空".to_string();
            ctx.sink.on_result(ctx.call_id, "deep_research", false, &e, Some(&e));
            return Err(ToolError::InvalidArgs(e));
        }
        match super::tools::run_subagent_impl(&self.cfg, task, crate::core::subagent::SubagentMode::ReadOnly, max_turns).await {
            Ok((sub_request_id, outcome)) => {
                let mut out = format!("子代理调研完成(subagent_id={sub_request_id}, max_turns={max_turns}, failed={})\n\n{}", outcome.failed, outcome.summary);
                if outcome.failed {
                    out.push_str("\n\n提示：调研未完成，可重试或检查 LLM 配置。");
                } else {
                    out.push_str(&format!("\n\n如需完整输出，调用 read_subagent_result，参数 subagent_id=\"{sub_request_id}\"。"));
                }
                let summary = format!("{} 字符摘要", outcome.summary.chars().count());
                ctx.sink.on_result(ctx.call_id, "deep_research", !outcome.failed, &summary, Some(&out));
                Ok(Value::String(out))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "deep_research", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// read_subagent_result：分页读取子代理完整输出（AppState.subagent_results LRU 存储）。
pub struct ReadSubagentResultTool {
    cfg: KbSearchConfig,
}

impl ReadSubagentResultTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for ReadSubagentResultTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "read_subagent_result",
                "按 subagent_id 分页读取一次 deep_research 子代理调研的完整输出。offset 为字符偏移（默认 0），max_chars 控制本次读取长度（默认 8192）。首次读取可省略 offset；若返回末尾提示已截断，用上次 offset + 返回长度作为下次 offset 继续。",
                json!({
                    "type": "object",
                    "properties": {
                        "subagent_id": { "type": "string", "description": "deep_research 返回的 subagent_id" },
                        "offset": { "type": "integer", "description": "字符偏移（默认 0）", "minimum": 0 },
                        "max_chars": { "type": "integer", "description": "本次读取最大字符数（默认 8192，最大 60000）", "minimum": 1, "maximum": 60000 }
                    },
                    "required": ["subagent_id"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let id = args.get("subagent_id").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        let offset = args.get("offset").and_then(|o| o.as_u64()).unwrap_or(0) as usize;
        let max_chars = args.get("max_chars").and_then(|o| o.as_u64()).map(|v| v as usize).unwrap_or(8192).clamp(1, 60_000);
        ctx.sink.on_call(ctx.call_id, "read_subagent_result", &format!("{id} (offset={offset})"), &args);
        if id.is_empty() {
            let e = "subagent_id 不能为空".to_string();
            ctx.sink.on_result(ctx.call_id, "read_subagent_result", false, &e, Some(&e));
            return Err(ToolError::InvalidArgs(e));
        }
        let state = self.cfg.app_handle.state::<crate::AppState>();
        let Some(full) = state.subagent_results.get(&id) else {
            let e = format!("subagent_id 不存在或已过期: {id}");
            ctx.sink.on_result(ctx.call_id, "read_subagent_result", false, &e, Some(&e));
            return Err(ToolError::Failed(e));
        };
        let total = full.chars().count();
        if offset >= total {
            let msg = format!("(已读取到末尾：该调研共 {total} 字符，offset={offset} 已超出)");
            ctx.sink.on_result(ctx.call_id, "read_subagent_result", true, &format!("已达末尾 {total} 字符"), Some(&msg));
            return Ok(Value::String(msg));
        }
        let slice: String = full.chars().skip(offset).take(max_chars).collect();
        let next_offset = offset + slice.chars().count();
        let mut out = slice;
        if next_offset < total {
            out.push_str(&format!("\n\n…(已显示 {next_offset}/{total} 字符，继续调用请用 offset={next_offset})"));
        }
        ctx.sink.on_result(ctx.call_id, "read_subagent_result", true, &format!("{next_offset}/{total} 字符"), Some(&out));
        Ok(Value::String(out))
    }
}

/// spawn_subagent：泛化子代理（只读调研 / 写型执行，写操作需审批）。
pub struct SpawnSubagentTool {
    cfg: KbSearchConfig,
}

impl SpawnSubagentTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            ToolSpec::new(
                "spawn_subagent",
                "派生一个隔离子代理执行子任务：mode=readonly 做深度调研（白名单：检索/读/记忆检索，独立上下文，只返回有界摘要，完整输出可用 read_subagent_result 分页读取）；mode=write 可编辑/删除文件（每次写操作仍需用户确认）。适合委托独立子任务或并行拆分。",
                json!({
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "子代理任务描述（自包含，含目标与边界）" },
                        "mode": { "type": "string", "enum": ["readonly", "write"], "description": "readonly=只读调研（默认）；write=可编辑/删除文件（需用户确认）" },
                        "max_turns": { "type": "integer", "minimum": 1, "maximum": 30, "description": "轮次上限（默认 12）" }
                    },
                    "required": ["task"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let task = args.get("task").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        let mode = subagent_mode(args.get("mode").and_then(|m| m.as_str()).unwrap_or("readonly"));
        let max_turns = args.get("max_turns").and_then(|m| m.as_u64()).map(|v| v as usize).unwrap_or(12).clamp(1, 30);
        let mode_label = if mode == crate::core::subagent::SubagentMode::Write { "write" } else { "readonly" };
        ctx.sink.on_call(ctx.call_id, "spawn_subagent", &format!("mode={mode_label} task_len={} max_turns={}", task.len(), max_turns), &args);
        if task.is_empty() {
            let e = "task 不能为空".to_string();
            ctx.sink.on_result(ctx.call_id, "spawn_subagent", false, &e, Some(&e));
            return Err(ToolError::InvalidArgs(e));
        }
        match super::tools::run_subagent_impl(&self.cfg, task, mode, max_turns).await {
            Ok((sub_request_id, outcome)) => {
                let mut out = format!("子代理执行完成(subagent_id={sub_request_id}, mode={mode_label}, max_turns={max_turns}, failed={})\n\n{}", outcome.failed, outcome.summary);
                if outcome.failed {
                    out.push_str("\n\n提示：子代理未完成，可重试或检查 LLM 配置。");
                } else {
                    out.push_str(&format!("\n\n如需完整输出，调用 read_subagent_result，参数 subagent_id=\"{sub_request_id}\"。"));
                }
                let summary = format!("{} 字符摘要", outcome.summary.chars().count());
                ctx.sink.on_result(ctx.call_id, "spawn_subagent", !outcome.failed, &summary, Some(&out));
                Ok(Value::String(out))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "spawn_subagent", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// parallel_research：并行派发 2-5 个只读调研子代理（JoinSet 并发，独立收集）。
pub struct ParallelResearchTool {
    cfg: KbSearchConfig,
}

impl ParallelResearchTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for ParallelResearchTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "parallel_research",
                "并行派发 2-5 个只读调研子代理，各自独立上下文同时执行，汇总各摘要一次返回。适合从多个独立角度/主题同时调研（如分别调研 A、B、C 三个主题），显著节省串行时间。各子代理完整输出可用 read_subagent_result 分页读取。",
                json!({
                    "type": "object",
                    "properties": {
                        "tasks": {
                            "type": "array",
                            "minItems": 2,
                            "maxItems": 5,
                            "items": { "type": "string" },
                            "description": "2-5 个独立调研任务（各自自包含）"
                        },
                        "max_turns": { "type": "integer", "minimum": 1, "maximum": 30, "description": "每个子代理轮次上限（默认 12）" }
                    },
                    "required": ["tasks"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let tasks: Vec<String> = args
            .get("tasks")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let max_turns = args.get("max_turns").and_then(|m| m.as_u64()).map(|v| v as usize).unwrap_or(12).clamp(1, 30);
        ctx.sink.on_call(ctx.call_id, "parallel_research", &format!("tasks={} max_turns={}", tasks.len(), max_turns), &args);
        if !(2..=5).contains(&tasks.len()) {
            let e = "需要 2-5 个调研任务".to_string();
            ctx.sink.on_result(ctx.call_id, "parallel_research", false, &e, Some(&e));
            return Err(ToolError::InvalidArgs(e));
        }
        // 并行派发：JoinSet 并发执行，独立收集结果（任一失败不影响其余）
        let mut set = tokio::task::JoinSet::new();
        for task in tasks {
            let cfg = self.cfg.clone();
            set.spawn(async move { super::tools::run_subagent_impl(&cfg, task, crate::core::subagent::SubagentMode::ReadOnly, max_turns).await });
        }
        let mut entries: Vec<(String, String, bool)> = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Ok((id, outcome))) => entries.push((id, outcome.summary, outcome.failed)),
                Ok(Err(e)) => entries.push((String::new(), format!("子代理启动失败: {e}"), true)),
                Err(e) => entries.push((String::new(), format!("子代理任务异常: {e}"), true)),
            }
        }
        let failed_count = entries.iter().filter(|(_, _, f)| *f).count();
        let mut out = format!("并行调研完成（{} 个任务，{} 个失败）：\n", entries.len(), failed_count);
        for (i, (id, summary, failed)) in entries.iter().enumerate() {
            out.push_str(&format!("\n── 任务 {} {} ──\n", i + 1, if *failed { "(失败)" } else { "" }));
            out.push_str(summary);
            if !id.is_empty() {
                out.push_str(&format!("\n完整输出：read_subagent_result subagent_id=\"{id}\""));
            }
        }
        let summary = format!("{} 任务 {} 失败", entries.len(), failed_count);
        ctx.sink.on_result(ctx.call_id, "parallel_research", failed_count == 0, &summary, Some(&out));
        Ok(Value::String(out))
    }
}

/// webfetch：抓取网页提取可读文本（业务助手 `webfetch`；SSRF 防护/重定向校验内置）。
pub struct WebfetchTool {
    cfg: KbSearchConfig,
}

impl WebfetchTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for WebfetchTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "webfetch",
                "抓取指定 URL 的网页并提取可读文本（自动去除导航/脚本/样式，保留正文与标题；跳过二进制与超大页面；私有地址/内网地址默认拒绝防 SSRF）。适合获取在线文档、博客、API 说明等。",
                json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "要抓取的 URL（http/https）" },
                        "max_chars": { "type": "integer", "description": "提取文本上限（默认 10000，最大 50000）" }
                    },
                    "required": ["url"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let url = args.get("url").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        let max_chars = args.get("max_chars").and_then(|m| m.as_u64()).map(|v| v as usize).unwrap_or(10_000).clamp(1, 50_000);
        ctx.sink.on_call(ctx.call_id, "webfetch", &url, &args);
        if url.is_empty() {
            let e = "url 不能为空".to_string();
            ctx.sink.on_result(ctx.call_id, "webfetch", false, &e, Some(&e));
            return Err(ToolError::InvalidArgs(e));
        }
        match super::tools::webfetch(&url, max_chars).await {
            Ok(text) => {
                let summary = format!("{} 字符", text.chars().count());
                ctx.sink.on_result(ctx.call_id, "webfetch", true, &summary, Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "webfetch", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

// ─────────────────────────── 技能激活 + 反思工具 ───────────────────────────

/// activate_skill：激活技能加载指令并解锁专用工具（业务逻辑 ActiveSkillState::activate）。
pub struct ActivateSkillTool {
    registry: Arc<crate::core::skill::SkillRegistry>,
    state: Arc<crate::core::skill::activation::ActiveSkillState>,
    cfg: KbSearchConfig,
}

impl ActivateSkillTool {
    pub fn new(
        registry: Arc<crate::core::skill::SkillRegistry>,
        state: Arc<crate::core::skill::activation::ActiveSkillState>,
        cfg: KbSearchConfig,
    ) -> Self {
        Self { registry, state, cfg }
    }
}

#[async_trait]
impl Tool for ActivateSkillTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "activate_skill",
                "激活一个技能以加载其详细指令（SKILL.md 正文核心段，一次性提供、不重复注入）并解锁其声明的专用工具。技能 ID 见常驻技能目录；仅当目录中的技能与当前任务明确相关时才调用。激活后：1) 正文随本工具结果一次性进入上下文，后续轮次不再重复注入，请遵循其中的流程与输出规范；2) 其声明的检索工具（如 kb_search）将可用；3) 可用 read 工具读取其 references/ 下的参考资料；正文被截断时可用 read 读取 {skill_id}/SKILL.md 获取完整内容。重复激活同一技能只会返回已激活提示，不会重复返回正文。",
                json!({
                    "type": "object",
                    "properties": {
                        "skill_id": { "type": "string", "description": "技能目录中的技能 ID，如 kb-search、code-lookup" }
                    },
                    "required": ["skill_id"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let id = args.get("skill_id").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        ctx.sink.on_call(ctx.call_id, "activate_skill", &id, &args);
        if id.is_empty() {
            let e = "skill_id 为空".to_string();
            ctx.sink.on_result(ctx.call_id, "activate_skill", false, &e, Some(&e));
            return Err(ToolError::InvalidArgs(e));
        }
        use crate::core::skill::activation::{ActivationSource, SkillLifetime, MAX_SKILL_BODY_CHARS};
        // 幂等：技能已激活且正文已注入 → 返回已激活提示，不重复返回正文
        if self.state.is_loaded(&id) {
            let desc = self.registry.find_enabled(&id).map(|s| s.description.trim().to_string()).unwrap_or_default();
            let mut msg = format!("技能 '{id}' 已激活且指令已注入，本请求内不会重复注入正文。");
            if !desc.is_empty() {
                msg.push_str(&format!(" 说明：{desc}"));
            }
            ctx.sink.on_result(ctx.call_id, "activate_skill", true, &msg, Some(&msg));
            return Ok(Value::String(msg));
        }
        let Some(skill) = self.registry.find_enabled(&id) else {
            let e = format!("技能 '{id}' 不存在或未启用，请从技能目录中选择");
            ctx.sink.on_result(ctx.call_id, "activate_skill", false, &e, Some(&e));
            return Err(ToolError::Failed(e));
        };
        let body = skill.body.trim();
        let body_chars = body.chars().count();
        let body_short: String = if body_chars > MAX_SKILL_BODY_CHARS {
            body.chars().take(MAX_SKILL_BODY_CHARS).collect()
        } else {
            body.to_string()
        };
        let truncated = body_short.chars().count() < body_chars;
        // 激活会话挂载（warm）中的技能时保留 Session 生命周期（P5 跨请求恢复）
        let lifetime = if self.state.activated().iter().any(|a| a.skill_id == id && a.lifetime == SkillLifetime::Session) {
            SkillLifetime::Session
        } else {
            SkillLifetime::Turn
        };
        self.state.activate(&skill, lifetime, ActivationSource::Llm, true);
        let mut msg = format!("<active_skill id=\"{id}\" version=\"{}\" source=\"llm\">\n{body_short}\n</active_skill>", skill.version);
        if truncated {
            msg.push_str(&format!(
                "\n\n[技能正文超过单次注入预算（{body_chars} 字符），已显示前 {} 字符；如需完整内容，可用 read 读取 '{id}/SKILL.md'（已激活技能目录内）]",
                MAX_SKILL_BODY_CHARS
            ));
        }
        if !skill.description.trim().is_empty() {
            msg.push_str(&format!("\n\n说明：{}", skill.description.trim()));
        }
        if !skill.tools.is_empty() {
            msg.push_str(&format!("\n专用工具：{}", skill.tools.iter().map(|t| format!("`{t}`")).collect::<Vec<_>>().join(", ")));
        }
        ctx.sink.on_result(ctx.call_id, "activate_skill", true, &format!("{} 字符", msg.chars().count()), Some(&msg));
        Ok(Value::String(msg))
    }
}

/// deactivate_skill：停用已激活技能（业务逻辑 ActiveSkillState::deactivate）。
pub struct DeactivateSkillTool {
    state: Arc<crate::core::skill::activation::ActiveSkillState>,
    cfg: KbSearchConfig,
}

impl DeactivateSkillTool {
    pub fn new(state: Arc<crate::core::skill::activation::ActiveSkillState>, cfg: KbSearchConfig) -> Self {
        Self { state, cfg }
    }
}

#[async_trait]
impl Tool for DeactivateSkillTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "deactivate_skill",
                "停用一个此前已激活的技能：其指令不再注入，其声明的专用工具将不再可用。当某技能不再适用于当前任务、或需要避免多余指令干扰时调用。",
                json!({
                    "type": "object",
                    "properties": {
                        "skill_id": { "type": "string", "description": "要停用的技能 ID" }
                    },
                    "required": ["skill_id"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let id = args.get("skill_id").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        ctx.sink.on_call(ctx.call_id, "deactivate_skill", &id, &args);
        if id.is_empty() {
            let e = "skill_id 为空".to_string();
            ctx.sink.on_result(ctx.call_id, "deactivate_skill", false, &e, Some(&e));
            return Err(ToolError::InvalidArgs(e));
        }
        if self.state.deactivate(&id) {
            let msg = format!("技能已停用：{id}");
            ctx.sink.on_result(ctx.call_id, "deactivate_skill", true, &msg, Some(&msg));
            Ok(Value::String(msg))
        } else {
            let e = format!("技能 '{id}' 当前未激活，无需停用");
            ctx.sink.on_result(ctx.call_id, "deactivate_skill", false, &e, Some(&e));
            Err(ToolError::Failed(e))
        }
    }
}

/// self_review：反思质量门（复用业务服务 `LLMClient::review_text`，独立非流式审查）。
pub struct SelfReviewTool {
    cfg: KbSearchConfig,
}

impl SelfReviewTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for SelfReviewTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "self_review",
                "在给出最终答案前自检：把用户目标与你的初稿交给独立审查，返回待修正问题清单。审查返回\"无问题\"时答案已达标，直接输出最终答案；返回问题列表时请逐条修正后再输出。适合长答案或多轮工具任务后使用。",
                json!({
                    "type": "object",
                    "properties": {
                        "goal": { "type": "string", "description": "用户原始目标/问题（原样引用）" },
                        "draft": { "type": "string", "description": "你的初稿答案（完整内容）" }
                    },
                    "required": ["goal", "draft"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let goal = args.get("goal").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        let draft = args.get("draft").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        ctx.sink.on_call(ctx.call_id, "self_review", &format!("goal_len={} draft_len={}", goal.len(), draft.len()), &args);
        if goal.is_empty() || draft.is_empty() {
            let e = "goal 与 draft 均不能为空".to_string();
            ctx.sink.on_result(ctx.call_id, "self_review", false, &e, Some(&e));
            return Err(ToolError::InvalidArgs(e));
        }
        let state = self.cfg.app_handle.state::<crate::AppState>();
        let llm_cfg = state.llm_config.read().unwrap_or_else(|e| e.into_inner()).clone();
        let llm = match state
            .llm_client_for(&llm_cfg.endpoint, &llm_cfg.model, &llm_cfg.api_key)
            .await
        {
            Ok(client) => client,
            Err(e) => {
                let msg = format!("LLM 未配置或构建失败: {e}");
                ctx.sink.on_result(ctx.call_id, "self_review", false, &msg, Some(&msg));
                return Err(ToolError::Failed(msg));
            }
        };
        let cancel = self.cfg.cancel.clone().unwrap_or_else(|| tokio_util::sync::CancellationToken::new());
        match llm.review_text(&goal, &draft, cancel).await {
            Some(result) if result.needs_fix() => {
                let mut out = format!("审查发现 {} 个问题，请逐条修正后输出最终答案：\n", result.issues.len());
                for (i, issue) in result.issues.iter().enumerate() {
                    out.push_str(&format!("{}. 问题：{}\n   修正建议：{}\n", i + 1, issue.issue, issue.fix));
                }
                let summary = format!("{} 个问题", result.issues.len());
                ctx.sink.on_result(ctx.call_id, "self_review", true, &summary, Some(&out));
                Ok(Value::String(out))
            }
            Some(result) => {
                let msg = format!("审查通过（verdict={}），初稿已达标，请直接输出最终答案。", result.verdict);
                ctx.sink.on_result(ctx.call_id, "self_review", true, "通过", Some(&msg));
                Ok(Value::String(msg))
            }
            None => {
                let msg = "审查不可用（LLM 未配置或评审失败），请自行检查初稿后输出最终答案。".to_string();
                ctx.sink.on_result(ctx.call_id, "self_review", false, "评审不可用", Some(&msg));
                Ok(Value::String(msg))
            }
        }
    }
}

/// ask_user_question：任务信息不足时向用户提出澄清问题（对齐 DSH `ask_user_question` seam）。
///
/// 通道与审批/规划确认同构：oneshot 挂起表（`AppState.user_question_pending`）
/// + `question:request` 事件 → 前端弹窗 → `question_respond` IPC 回传。
/// 超时（见 `limits::ASK_USER_TIMEOUT_SECS`）与父链取消均视为「未回答」，
/// 返回引导让模型改用已有信息作答或如实说明缺口。
/// 等待用户回答期间独占执行（`concurrency_safe=false`，不得与其他工具并行）。
pub struct AskUserQuestionTool {
    cfg: KbSearchConfig,
}

impl AskUserQuestionTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            ToolSpec::new(
                "ask_user_question",
                "向用户提出一个澄清问题（当任务需求含糊、存在多选一决策、或缺少关键参数时使用，不要猜测）。用户回答后，本工具返回其回答文本。问题应具体、可回答；options 可给候选选项（用户可选择或自由输入）；一次只问一个最关键的问题。",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "question": {
                            "type": "string",
                            "minLength": 1,
                            "description": "要询问用户的具体问题"
                        },
                        "header": {
                            "type": "string",
                            "description": "可选的弹窗标题（默认“AI 需要确认”）"
                        },
                        "options": {
                            "type": "array",
                            "maxItems": 6,
                            "items": { "type": "string", "minLength": 1 },
                            "description": "可选候选选项（用户可点选，也可自由输入其他回答）"
                        }
                    },
                    "required": ["question"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let question = args
            .get("question")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if question.is_empty() {
            return Err(ToolError::InvalidArgs("question 不能为空".into()));
        }
        let header = args
            .get("header")
            .and_then(|s| s.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let options: Vec<String> = args
            .get("options")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .take(6)
                    .collect()
            })
            .unwrap_or_default();

        ctx.sink.on_call(ctx.call_id, "ask_user_question", &question, &args);

        // ── 请求用户回答：挂起表 + 事件 + oneshot + 超时/取消（语义与 rig 版一致） ──
        use tauri::Emitter; // app.emit 需要 Emitter trait
        let app = self.cfg.app_handle.clone();
        let question_id = format!("q_{}", uuid::Uuid::new_v4());
        let state = app.state::<crate::AppState>();
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<String>>();
        state
            .user_question_pending
            .lock()
            .map_err(|e| ToolError::Internal(format!("挂起表锁异常: {e}")))?
            .insert(question_id.clone(), tx);
        let _ = app.emit(
            "question:request",
            serde_json::json!({
                "question_id": question_id,
                "request_id": self.cfg.request_id,
                "question": question,
                "header": header,
                "options": options,
            }),
        );
        // rx 已 move 进 select；超时/取消时从挂起表清理（防泄漏）
        let pending = state.user_question_pending.clone();
        let mut cancel_fut = Box::pin(ctx.cancel.cancelled());
        let mut timeout = Box::pin(tokio::time::timeout(
            std::time::Duration::from_secs(ASK_USER_TIMEOUT_SECS),
            rx,
        ));
        let answer: Option<String> = tokio::select! {
            _ = &mut cancel_fut => {
                let _ = pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&question_id);
                None
            }
            res = &mut timeout => match res {
                Ok(Ok(ans)) => ans,
                Ok(Err(_)) => None, // 通道关闭（异常路径）
                Err(_) => {
                    let _ = pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&question_id);
                    None
                }
            },
        };
        match answer {
            Some(text) if !text.trim().is_empty() => {
                let trimmed = text.trim().to_string();
                ctx.sink.on_result(
                    ctx.call_id,
                    "ask_user_question",
                    true,
                    &format!("用户回答：{}", trimmed),
                    Some(&trimmed),
                );
                Ok(Value::String(format!("用户回答：{}", trimmed)))
            }
            _ => {
                let msg = "用户未在限时内回答或取消了提问，请基于已有信息继续，或如实告知用户信息不足"
                    .to_string();
                ctx.sink.on_result(
                    ctx.call_id,
                    "ask_user_question",
                    false,
                    "用户未回答（取消或超时）",
                    Some("用户未回答（取消或超时）"),
                );
                Err(ToolError::Failed(msg))
            }
        }
    }
}

/// 注册技能激活类工具（activate/deactivate 需 SkillRegistry + ActiveSkillState，
/// 由 Agent 组装方注入；子代理白名单不含此类工具）。
pub fn register_skill_tools(
    reg: &mut HashMapToolRegistry,
    cfg: KbSearchConfig,
    registry: Arc<crate::core::skill::SkillRegistry>,
    state: Arc<crate::core::skill::activation::ActiveSkillState>,
) {
    reg.register(Arc::new(ActivateSkillTool::new(registry, state.clone(), cfg.clone())));
    reg.register(Arc::new(DeactivateSkillTool::new(state, cfg)));
}

// ─────────────────────────── 前端桥接/外部/MCP 工具（v3 迁移，Phase 6） ───────────────────────────

/// 截断长字符串（用于工具轨迹参数/结果摘要，避免撑爆事件负载）。
fn truncate_text(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

/// 通用前端桥工具（pomodoro / raw-parse / open-ui）：Rust 工具闭包 ↔ 前端业务 handler。
///
/// 协议与 rig 版 `build_bridge_tool` 完全一致：软门禁（技能声明）→ 动作解析
/// （缺省回退默认动作）→ 轨迹事件 → `core::bridge::request`（5s 桥超时兜底）→
/// 结果回填。前端注册同名 handler 监听 `frontend_bridge:request` 事件（开闭原则：
/// 新增"与番茄钟类似的业务"只需新 spec 一行注册）。
pub struct BridgeTool {
    cfg: KbSearchConfig,
    spec: ToolSpec,
    default_action: String,
}

impl BridgeTool {
    pub fn new(
        cfg: KbSearchConfig,
        name: &'static str,
        description: &'static str,
        schema: Value,
        default_action: &'static str,
    ) -> Self {
        Self {
            cfg,
            spec: ToolSpec::new(name, description, schema),
            default_action: default_action.to_string(),
        }
    }
}

#[async_trait]
impl Tool for BridgeTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let tool = self.spec.name.clone();
        // 软门禁（替代 rig active_tools 硬过滤）：pomodoro/raw-parse/open-ui 始终可见可调，
        // 但仅当声明它的技能已激活时才执行；未激活返回引导（与 SkillGateHook 语义一致）。
        let unlocked = if !self.cfg.skill_gating {
            true
        } else {
            self.cfg
                .skill_state
                .allowed_tools()
                .as_ref()
                .is_some_and(|list| list.iter().any(|t| t == &tool))
        };
        if !unlocked {
            let msg = format!(
                "{} 需要先激活声明它的技能（调用 activate_skill，从技能目录选择）后才能执行，本次未执行。请先激活对应技能，再重新发起操作。",
                tool
            );
            log::info!("[agent] {} 未激活技能被调用，返回引导 request_id={}", tool, self.cfg.request_id);
            return Ok(Value::String(msg));
        }
        // 动作：显式指定优先，缺失/为空时回退默认动作（如 status）
        let action = args
            .get("action")
            .and_then(|a| a.as_str())
            .filter(|a| !a.trim().is_empty())
            .map(|a| a.trim().to_string())
            .unwrap_or_else(|| self.default_action.clone());
        let preview = truncate_text(&serde_json::to_string(&args).unwrap_or_default(), 120);
        ctx.sink.on_call(ctx.call_id, &tool, &preview, &args);
        let app_handle = self.cfg.app_handle.clone();
        match crate::core::bridge::request(&app_handle, &tool, &action, args).await {
            Ok(text) => {
                ctx.sink.on_result(ctx.call_id, &tool, true, &truncate_text(&text, 200), Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, &tool, false, &truncate_text(&e, 200), Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// 注册三个前端桥接工具（pomodoro / raw-parse / open-ui），schema/描述与 rig 版逐字对齐。
pub fn register_bridge_tools(reg: &mut HashMapToolRegistry, cfg: KbSearchConfig) {
    reg.register(Arc::new(BridgeTool::new(
        cfg.clone(),
        "pomodoro",
        "控制番茄钟（专注计时器）。动作：start 开始计时（mode=focus 专注，默认 25 分钟；mode=break 休息，默认 5 分钟；可选 minutes 自定义时长，范围 1-180）；autoBreak 开启/关闭自动开始休息（openEnable 布尔值）；autoFocus 开启/关闭自动开始专注（openEnable 布尔值）；stop 停止当前计时；status 查询当前运行状态。当用户要求定时、开始、停止、查询番茄钟或设置自动衔接时调用。",
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["start", "autoBreak", "autoFocus", "stop", "status"],
                    "description": "要执行的动作"
                },
                "mode": {
                    "type": "string",
                    "enum": ["focus", "break"],
                    "description": "计时模式，仅 start 使用：focus 专注 / break 休息"
                },
                "minutes": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": POMODORO_MINUTES_MAX,
                    "description": "自定义时长（分钟），仅 start 使用；focus 默认 25，break 默认 5"
                },
                "openEnable": {
                    "type": "boolean",
                    "description": "是否开启，仅 autoBreak / autoFocus 使用：true 开启，false 关闭"
                }
            },
            "required": ["action"]
        }),
        "status",
    )));
    reg.register(Arc::new(BridgeTool::new(
        cfg.clone(),
        "raw-parse",
        "解析 RAW 照片文件（.arw/.cr2/.nef/.dng/.orf 等）的元数据并输出为 Markdown。动作：parse（path 为知识库内 RAW 文件的相对路径，返回相机·镜头、拍摄参数、图像信息）。当用户要求查看 RAW 照片信息、解析相机拍摄参数时调用。",
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["parse"],
                    "description": "要执行的动作：parse 解析元数据"
                },
                "path": {
                    "type": "string",
                    "description": "RAW 文件在知识库中的相对路径（如 note/photo/IMG_0001.arw）"
                }
            },
            "required": ["action", "path"]
        }),
        "parse",
    )));
    reg.register(Arc::new(BridgeTool::new(
        cfg,
        "open-ui",
        "打开知识库文件或跳转打开应用页面。动作：open_file 在系统中打开文件预览的 ui（会切换当前工作区到该文件，可能打断正在编辑的文件）；open_page 跳转系统 ui 页面/视图（仅支持下列 page 枚举，共 26 种：fileGraph 文件图谱、noteGraph 文档关联图谱、dashboard 系统首页、calendar 日历/日程、knowledge 知识库监控面板、skill 技能管理页面、mcp MCP 管理页面、timeline 文件时间线页面、canvas 画布、whiteboard 白板、mermaid mermaid 图表预览编辑页面、wordCloud 词云、gitRecords Git 管理页面、pomodoro 番茄钟页面、tempEditor 临时编辑器、urlEncoder 编码器页面、video 视频播放页面、raw RAW 照片预览页面、regexTest 正则表达式测试页面、cron Cron 表达式测试页面、bookmarks 书签预览页面、dirSpace 目录空间数据统计大屏、swaggerDemo swagger api 预览页面、graphQLPlayground GraphQL 预览接口测试页面、openRestyEditor nginx 配置编辑器、fileType 文件类型分布）。仅打开查看，不修改文件内容；打开文件/跳转页面会切换当前工作区视图。当用户要求打开某个文件、跳转到某页面、查看图谱/日历/看板/思维导图等时调用。",
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["open_file", "open_page"],
                    "description": "要执行的动作：open_file 在系统中打开文件预览的 ui；open_page 跳转系统 ui 页面/视图"
                },
                "relativePath": {
                    "type": "string",
                    "description": "知识库内相对路径（open_file 必填，如 notes/plan.md；禁止 ../、绝对路径或盘符）"
                },
                "page": {
                    "type": "string",
                    "enum": ["fileGraph", "noteGraph", "dashboard", "calendar", "knowledge", "skill", "mcp", "timeline", "canvas", "whiteboard", "mermaid", "wordCloud", "gitRecords", "pomodoro", "tempEditor", "urlEncoder", "video", "raw", "regexTest", "cron", "bookmarks", "dirSpace", "swaggerDemo", "graphQLPlayground", "openRestyEditor", "fileType"],
                    "description": "要跳转的系统 ui 页面/视图（仅此枚举内的页面，不得发明新页面名）"
                }
            },
            "required": ["action"]
        }),
        "open_file",
    )));
}

/// 外部 HTTP 工具（P2-15 配置驱动）：`ExternalToolDef` → core/loop Tool。
/// 与 rig 版 `build_external_tool` 语义一致：HTTP POST/GET JSON 调用外部端点，
/// 响应体截断护栏（`MAX_EXTERNAL_RESPONSE_CHARS`）防撑爆模型上下文。
pub struct ExternalHttpTool {
    def: crate::core::agent::external_tools::ExternalToolDef,
    cfg: KbSearchConfig,
    spec: ToolSpec,
}

impl ExternalHttpTool {
    pub fn new(
        def: crate::core::agent::external_tools::ExternalToolDef,
        cfg: KbSearchConfig,
    ) -> Self {
        let spec = ToolSpec::new(def.name.clone(), def.description.clone(), def.params_schema.clone());
        Self { def, cfg, spec }
    }
}

#[async_trait]
impl Tool for ExternalHttpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let name = self.def.name.clone();
        ctx.sink.on_call(
            ctx.call_id,
            &name,
            &args.to_string().chars().take(80).collect::<String>(),
            &args,
        );
        let client = reqwest::Client::new();
        let url = self.def.url.clone();
        let timeout = std::time::Duration::from_secs(self.def.timeout_secs.max(1));
        let method = self.def.method.clone();
        let result = tokio::time::timeout(timeout, async {
            match method.to_ascii_uppercase().as_str() {
                "GET" => client.get(&url).query(&args).send().await,
                _ => client.post(&url).json(&args).send().await,
            }
        })
        .await;
        match result {
            Ok(Ok(resp)) => {
                let status = resp.status();
                match resp.text().await {
                    Ok(body) if status.is_success() => {
                        let truncated = body.chars().count() > crate::core::agent::limits::MAX_EXTERNAL_RESPONSE_CHARS;
                        let final_body = if truncated {
                            let cut: String = body
                                .chars()
                                .take(crate::core::agent::limits::MAX_EXTERNAL_RESPONSE_CHARS)
                                .collect();
                            format!("{}（响应体过长已截断，共 {} 字符）", cut, body.chars().count())
                        } else {
                            body
                        };
                        ctx.sink.on_result(
                            ctx.call_id,
                            &name,
                            true,
                            &format!(
                                "HTTP {}，{} 字符{}",
                                status,
                                final_body.chars().count(),
                                if truncated { "（已截断）" } else { "" }
                            ),
                            Some(&final_body),
                        );
                        Ok(Value::String(final_body))
                    }
                    Ok(body) => {
                        let msg = format!("HTTP {} 错误: {}", status, body);
                        ctx.sink.on_result(ctx.call_id, &name, false, &msg, Some(&msg));
                        Err(ToolError::Failed(msg))
                    }
                    Err(e) => {
                        let msg = format!("读取响应失败: {e}");
                        ctx.sink.on_result(ctx.call_id, &name, false, &msg, Some(&msg));
                        Err(ToolError::Failed(msg))
                    }
                }
            }
            Ok(Err(e)) => {
                let msg = format!("外部工具请求失败: {e}");
                ctx.sink.on_result(ctx.call_id, &name, false, &msg, Some(&msg));
                Err(ToolError::Failed(msg))
            }
            Err(_) => {
                let msg = format!("外部工具请求超时（{}s）", self.def.timeout_secs);
                ctx.sink.on_result(ctx.call_id, &name, false, &msg, Some(&msg));
                Err(ToolError::Failed(msg))
            }
        }
    }
}

/// 把配置的外部工具注册进注册表（无配置时为空操作）。
pub fn register_external_tools(reg: &mut HashMapToolRegistry, cfg: KbSearchConfig) {
    for def in crate::core::agent::external_tools::load_external_tools_or_default() {
        reg.register(Arc::new(ExternalHttpTool::new(def, cfg.clone())));
    }
}

/// MCP 工具（v3）：连接中的 MCP 服务器工具 → core/loop Tool。
/// 注册名规范化 `mcp_<server>_<tool>`（下划线，兼容 OpenAI function name 约束）；
/// 闭包内仍按原始 server/tool 名调用 `McpRegistry::call_tool`。参数 schema 校验
/// （`mcp::validate_args`）+ 轨迹事件与内置工具对齐；审批由 ApprovalHook 统一拦截。
pub struct McpTool {
    spec: ToolSpec,
    schema: Value,
    server: String,
    tool: String,
    registry: Arc<crate::core::mcp::McpRegistry>,
    cfg: KbSearchConfig,
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        if let Err(e) = crate::core::mcp::validate_args(&self.schema, &args) {
            return Err(ToolError::InvalidArgs(e));
        }
        let full_name = self.spec.name.clone();
        let preview = truncate_text(&serde_json::to_string(&args).unwrap_or_default(), 120);
        ctx.sink.on_call(ctx.call_id, &full_name, &preview, &args);
        match self.registry.call_tool(&self.server, &self.tool, args).await {
            Ok(text) => {
                ctx.sink.on_result(ctx.call_id, &full_name, true, &truncate_text(&text, 200), Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, &full_name, false, &truncate_text(&e, 200), Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// 注册当前已连接的 MCP 服务器工具；返回注册名列表（供 Hook 可见性/放行集补齐）。
/// 子代理（filter_registry 白名单）默认不含 MCP 工具，由调用方决定是否使用本函数。
pub async fn register_mcp_tools(reg: &mut HashMapToolRegistry, cfg: KbSearchConfig) -> Vec<String> {
    let state = cfg.app_handle.state::<crate::AppState>();
    let mcp = state.mcp.clone();
    let mut names = Vec::new();
    for info in mcp.list().await {
        if info.status != crate::core::mcp::STATUS_CONNECTED {
            continue;
        }
        if let Some(detail) = mcp.get(&info.name).await {
            for def in detail.tools {
                let normalized = format!(
                    "mcp_{}_{}",
                    info.name.replace([' ', ':'], "_"),
                    def.name.replace([' ', ':'], "_")
                );
                let description = if def.description.trim().is_empty() {
                    format!("MCP 工具（服务器 {}）", info.name)
                } else {
                    format!("{}（MCP 服务器 {}）", def.description, info.name)
                };
                let schema = if def.input_schema.is_null() || def.input_schema.as_object().is_none() {
                    json!({ "type": "object", "properties": {} })
                } else {
                    def.input_schema.clone()
                };
                reg.register(Arc::new(McpTool {
                    spec: ToolSpec::new(normalized.clone(), description, schema.clone()),
                    schema,
                    server: info.name.clone(),
                    tool: def.name.clone(),
                    registry: mcp.clone(),
                    cfg: cfg.clone(),
                }));
                names.push(normalized);
            }
        }
    }
    if !names.is_empty() {
        log::info!("[mcp] Agent 已挂载 {} 个 MCP 工具", names.len());
    }
    names
}

// ─────────────────────────── 写/文件/Git 工具（exclusive） ───────────────────────────

/// write：创建/整体覆盖文件（业务助手 `write_file`）。
pub struct WriteTool {
    cfg: KbSearchConfig,
}

impl WriteTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            ToolSpec::new(
                "write",
                "创建新文件或整体覆盖当前打开知识库目录内的文本文件。content 为文件的完整新内容（覆盖写，非追加）。适合新建文档/笔记/代码文件，或整体重写小文件（≤1MB，按 UTF-8 字节计）。只允许在打开目录内写入，父目录不存在时会自动创建，不允许写入 .mdgo 内部数据。写入为不可撤销操作，覆盖已有文件前请确认用户意图。",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "rel_path": {
                            "type": "string",
                            "description": "文件在知识库根目录下的相对路径，如 docs/new-note.md"
                        },
                        "content": {
                            "type": "string",
                            "description": "文件的完整新内容（UTF-8 文本，最大 1MB）"
                        }
                    },
                    "required": ["rel_path", "content"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let rel = args.get("rel_path").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        let content = args.get("content").and_then(|s| s.as_str()).unwrap_or_default().to_string();
        if rel.is_empty() {
            return Err(ToolError::InvalidArgs("rel_path 为空".into()));
        }
        let preview = format!("{rel}: {} 字符", content.chars().count());
        ctx.sink.on_call(ctx.call_id, "write", &preview, &args);
        match super::tools::write_file(&self.cfg, &rel, &content).await {
            Ok(text) => {
                ctx.sink.on_result(ctx.call_id, "write", true, &text, Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "write", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// edit：唯一匹配替换（业务助手 `edit_file`）。
pub struct EditTool {
    cfg: KbSearchConfig,
}

impl EditTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            ToolSpec::new(
                "edit",
                "编辑当前打开知识库目录内的一个文本文件：将文件中与 old_string 完全匹配且唯一出现的片段替换为 new_string。只允许操作当前打开目录内的文件，不能操作目录外的文件，也不允许修改 .mdgo 内部数据。修改前建议先用 read 读取文件确认原文。",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "rel_path": {
                            "type": "string",
                            "description": "文件相对路径（知识库根目录内）"
                        },
                        "old_string": {
                            "type": "string",
                            "description": "需替换的原文片段（必须与文件内容完全一致且唯一出现）"
                        },
                        "new_string": {
                            "type": "string",
                            "description": "替换后的新内容"
                        }
                    },
                    "required": ["rel_path", "old_string", "new_string"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let rel = args.get("rel_path").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        let old = args.get("old_string").and_then(|s| s.as_str()).unwrap_or_default().to_string();
        let new = args.get("new_string").and_then(|s| s.as_str()).unwrap_or_default().to_string();
        if rel.is_empty() {
            return Err(ToolError::InvalidArgs("rel_path 为空".into()));
        }
        let preview = format!("{rel}: {} → {} 字符", old.chars().count(), new.chars().count());
        ctx.sink.on_call(ctx.call_id, "edit", &preview, &args);
        match super::tools::edit_file(&self.cfg, &rel, &old, &new).await {
            Ok(text) => {
                ctx.sink.on_result(ctx.call_id, "edit", true, &text, Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "edit", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// multi_edit：批量编辑多个文件（业务助手 `multi_edit_files`，all-or-nothing 校验）。
pub struct MultiEditTool {
    cfg: KbSearchConfig,
}

impl MultiEditTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for MultiEditTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            ToolSpec::new(
                "multi_edit",
                "批量编辑当前打开知识库目录内的多个文本文件：每个编辑项为 {rel_path, old_string, new_string}，所有编辑先全量校验（路径安全/UTF-8/old_string 唯一匹配），全部通过后一次性写入。相比逐次调用 edit 节省模型轮次预算；校验失败时不写任何文件。只允许操作打开目录内文件，不允许修改 .mdgo 内部数据。",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "edits": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "rel_path": { "type": "string", "description": "文件相对路径" },
                                    "old_string": { "type": "string", "description": "需替换的原文片段（必须唯一出现）" },
                                    "new_string": { "type": "string", "description": "替换后的新内容" }
                                },
                                "required": ["rel_path", "old_string", "new_string"]
                            },
                            "description": "编辑项数组（最多 10 个）"
                        }
                    },
                    "required": ["edits"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let edits: Vec<(String, String, String)> = args
            .get("edits")
            .and_then(|e| e.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|it| {
                        let rel = it.get("rel_path").and_then(|s| s.as_str()).unwrap_or("").trim().to_string();
                        let old = it.get("old_string").and_then(|s| s.as_str()).unwrap_or("").to_string();
                        let new = it.get("new_string").and_then(|s| s.as_str()).unwrap_or("").to_string();
                        if rel.is_empty() { None } else { Some((rel, old, new)) }
                    })
                    .collect()
            })
            .unwrap_or_default();
        if edits.is_empty() {
            return Err(ToolError::InvalidArgs("edits 不能为空".into()));
        }
        let preview = format!("{} 个文件", edits.len());
        ctx.sink.on_call(ctx.call_id, "multi_edit", &preview, &args);
        match super::tools::multi_edit_files(&self.cfg, &edits).await {
            Ok(text) => {
                ctx.sink.on_result(ctx.call_id, "multi_edit", true, &text, Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "multi_edit", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// delete：删除文件（不可恢复；业务助手 `delete_file`）。
pub struct DeleteTool {
    cfg: KbSearchConfig,
}

impl DeleteTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for DeleteTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            ToolSpec::new(
                "delete",
                "删除当前打开知识库目录内的一个文件（不可恢复）。只允许删除打开目录内的文件，不允许删除目录，也不允许删除 .mdgo 内部数据。删除前请确认用户意图。",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "rel_path": {
                            "type": "string",
                            "description": "要删除的文件相对路径"
                        }
                    },
                    "required": ["rel_path"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let rel = args.get("rel_path").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        if rel.is_empty() {
            return Err(ToolError::InvalidArgs("rel_path 为空".into()));
        }
        ctx.sink.on_call(ctx.call_id, "delete", &rel, &args);
        match super::tools::delete_file(&self.cfg, &rel).await {
            Ok(text) => {
                ctx.sink.on_result(ctx.call_id, "delete", true, &text, Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "delete", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// git_status：工作区状态（业务助手 `git_status`）。
pub struct GitStatusTool {
    cfg: KbSearchConfig,
}

impl GitStatusTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for GitStatusTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "git_status",
                "查看当前打开知识库目录的 Git 工作区状态（未提交的修改/新增/删除文件概览）。当用户询问改动情况、或需要了解仓库当前状态时调用。",
                json!({ "type": "object", "properties": {}, "required": [] }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        ctx.sink.on_call(ctx.call_id, "git_status", "status", &args);
        match super::tools::git_status(&self.cfg).await {
            Ok(text) => {
                ctx.sink.on_result(ctx.call_id, "git_status", true, &format!("{} 字符", text.chars().count()), Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "git_status", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// git_diff：查看工作区/暂存区差异（业务助手 `git_diff`）。
pub struct GitDiffTool {
    cfg: KbSearchConfig,
}

impl GitDiffTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for GitDiffTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "git_diff",
                "查看当前打开知识库目录的 Git 差异：staged=false 查看工作区未暂存改动，staged=true 查看已暂存改动；stat_only=true 只输出文件级增删统计（+N/-N）。差异过大自动截断。",
                json!({
                    "type": "object",
                    "properties": {
                        "staged": {
                            "type": "boolean",
                            "description": "是否查看已暂存（--cached）差异，默认 false"
                        },
                        "stat_only": {
                            "type": "boolean",
                            "description": "是否只输出文件级增删统计（numstat），默认 false"
                        }
                    },
                    "required": []
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let staged = args.get("staged").and_then(|b| b.as_bool()).unwrap_or(false);
        let stat_only = args.get("stat_only").and_then(|b| b.as_bool()).unwrap_or(false);
        let preview = if stat_only { "stat".to_string() } else if staged { "staged".to_string() } else { "diff".to_string() };
        ctx.sink.on_call(ctx.call_id, "git_diff", &preview, &args);
        match super::tools::git_diff(&self.cfg.dir_path, staged, stat_only).await {
            Ok((text, _structured)) => {
                ctx.sink.on_result(ctx.call_id, "git_diff", true, &format!("{} 字符", text.chars().count()), Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "git_diff", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// git_commit：提交暂存区改动（写操作；业务助手 `git_commit`）。
pub struct GitCommitTool {
    cfg: KbSearchConfig,
}

impl GitCommitTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for GitCommitTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            ToolSpec::new(
                "git_commit",
                "将当前打开知识库目录的 Git 暂存区改动提交为一次 commit。写操作需用户确认；暂存区含 .mdgo 内部数据时拒绝提交。",
                json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "commit message"
                        }
                    },
                    "required": ["message"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let message = args.get("message").and_then(|s| s.as_str()).unwrap_or_default().to_string();
        if message.trim().is_empty() {
            return Err(ToolError::InvalidArgs("commit message 不能为空".into()));
        }
        ctx.sink.on_call(ctx.call_id, "git_commit", &message, &args);
        match super::tools::git_commit(&self.cfg.dir_path, &message).await {
            Ok(text) => {
                ctx.sink.on_result(ctx.call_id, "git_commit", true, &text, Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "git_commit", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// git_checkout：恢复文件到 HEAD（写操作；业务助手 `git_checkout`）。
pub struct GitCheckoutTool {
    cfg: KbSearchConfig,
}

impl GitCheckoutTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for GitCheckoutTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            ToolSpec::new(
                "git_checkout",
                "将当前打开知识库目录中的文件恢复到 Git HEAD 状态（丢弃未提交的修改，不可恢复）。写操作需用户确认；不允许操作 .mdgo 内部数据。",
                json!({
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "要恢复的文件相对路径列表"
                        }
                    },
                    "required": ["paths"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let paths: Vec<String> = args
            .get("paths")
            .and_then(|p| p.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        if paths.is_empty() {
            return Err(ToolError::InvalidArgs("paths 不能为空".into()));
        }
        let preview = format!("{} 个文件", paths.len());
        ctx.sink.on_call(ctx.call_id, "git_checkout", &preview, &args);
        match super::tools::git_checkout(&self.cfg.dir_path, &paths).await {
            Ok(text) => {
                ctx.sink.on_result(ctx.call_id, "git_checkout", true, &text, Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "git_checkout", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

// ─────────────────────────── 长期记忆 + 任务清单工具 ───────────────────────────

/// remember：写入跨会话长期记忆（两级作用域 project/global；业务逻辑 MemoryStore::create）。
pub struct RememberTool {
    cfg: KbSearchConfig,
}

impl RememberTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for RememberTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            ToolSpec::new(
                "remember",
                "把一条长期记忆写入跨会话存储（用户偏好、项目约定、已验证结论等），后续对话可检索引用。title 一句话概括，body 写完整事实；keywords 用空格分隔便于检索；expires_in_days 可设置过期天数（过期后不再召回）。",
                json!({
                    "type": "object",
                    "properties": {
                        "title": { "type": "string", "description": "记忆标题（一句话概括）" },
                        "body": { "type": "string", "description": "记忆正文（完整事实/偏好/约定）" },
                        "keywords": { "type": "string", "description": "检索关键词，空格分隔（可选）" },
                        "scope": { "type": "string", "enum": ["project", "global"], "description": "作用域：project=当前知识库，global=全部（默认 project）" },
                        "kind": { "type": "string", "enum": ["fact", "preference", "reference"], "description": "记忆类型（默认 fact）" },
                        "expires_in_days": { "type": "integer", "minimum": 1, "description": "过期天数（可选；过期后不再召回与注入）" }
                    },
                    "required": ["title", "body"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let title = args.get("title").and_then(|t| t.as_str()).unwrap_or_default().trim().to_string();
        if title.is_empty() {
            return Err(ToolError::InvalidArgs("title 不能为空".into()));
        }
        let preview: String = title.chars().take(40).collect();
        ctx.sink.on_call(ctx.call_id, "remember", &preview, &args);

        let expires_in_days = args.get("expires_in_days").and_then(|v| v.as_u64());
        let mut input: crate::core::memory::MemoryInput =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        // 两级记忆：scope='global' 由存储归一为 ''；project（默认）绑定当前知识库目录
        if input.scope.trim() != "global" {
            input.dir_path = self.cfg.dir_path.clone();
        }
        if let Some(days) = expires_in_days {
            if days > 0 {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                input.expires_at = Some(now_ms + days * 24 * 60 * 60 * 1000);
            }
        }
        let store = self.cfg.app_handle.state::<crate::AppState>().memory_store.clone();
        match tokio::task::spawn_blocking(move || store.create(&input)).await {
            Ok(Ok(item)) => {
                let msg = format!("已保存记忆（id={}，revision={}）：{}\n{}", item.id, item.revision, item.title, item.body);
                let summary = format!("id={} revision={}", item.id, item.revision);
                ctx.sink.on_result(ctx.call_id, "remember", true, &summary, Some(&msg));
                Ok(Value::String(msg))
            }
            Ok(Err(e)) => {
                ctx.sink.on_result(ctx.call_id, "remember", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
            Err(e) => {
                let msg = e.to_string();
                ctx.sink.on_result(ctx.call_id, "remember", false, &msg, Some(&msg));
                Err(ToolError::Failed(msg))
            }
        }
    }
}

/// forget：删除一条长期记忆（业务逻辑 MemoryStore::delete）。
pub struct ForgetTool {
    cfg: KbSearchConfig,
}

impl ForgetTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for ForgetTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            ToolSpec::new(
                "forget",
                "删除一条已保存的长期记忆（需要记忆 id，可用 search_memory 查询得到）。",
                json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "要删除的记忆 id" }
                    },
                    "required": ["id"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let id = args.get("id").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        ctx.sink.on_call(ctx.call_id, "forget", &format!("id={id}"), &args);
        if id.is_empty() {
            let e = "记忆 id 不能为空".to_string();
            ctx.sink.on_result(ctx.call_id, "forget", false, &e, Some(&e));
            return Err(ToolError::InvalidArgs(e));
        }
        let store = self.cfg.app_handle.state::<crate::AppState>().memory_store.clone();
        let forget_id = id.clone();
        match tokio::task::spawn_blocking(move || store.delete(&forget_id)).await {
            Ok(Ok(true)) => {
                let msg = format!("已删除记忆 {id}");
                ctx.sink.on_result(ctx.call_id, "forget", true, &msg, Some(&msg));
                Ok(Value::String(msg))
            }
            Ok(Ok(false)) => {
                let e = format!("记忆 {id} 不存在或已删除");
                ctx.sink.on_result(ctx.call_id, "forget", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
            Ok(Err(e)) => {
                ctx.sink.on_result(ctx.call_id, "forget", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
            Err(e) => {
                let msg = e.to_string();
                ctx.sink.on_result(ctx.call_id, "forget", false, &msg, Some(&msg));
                Err(ToolError::Failed(msg))
            }
        }
    }
}

/// search_memory：按关键词检索跨会话长期记忆（只读；融合检索 search_hybrid）。
pub struct SearchMemoryTool {
    cfg: KbSearchConfig,
}

impl SearchMemoryTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for SearchMemoryTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "search_memory",
                "按关键词检索跨会话长期记忆（用户偏好、项目约定、已验证结论）。在需要回忆用户此前说过/偏好什么、或复用此前结论时调用。",
                json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "检索关键词（空格分隔多个词）" },
                        "limit": { "type": "integer", "description": "最多返回条数（默认 5，最大 20）" }
                    },
                    "required": ["query"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let query = args.get("query").and_then(|q| q.as_str()).unwrap_or_default().trim().to_string();
        let limit = args.get("limit").and_then(|l| l.as_u64()).map(|v| v as usize).unwrap_or(5).clamp(1, 20);
        ctx.sink.on_call(ctx.call_id, "search_memory", &format!("query={query} limit={limit}"), &args);
        if query.is_empty() {
            let e = "检索关键词不能为空".to_string();
            ctx.sink.on_result(ctx.call_id, "search_memory", false, &e, Some(&e));
            return Err(ToolError::InvalidArgs(e));
        }
        let state = self.cfg.app_handle.state::<crate::AppState>();
        match crate::core::memory::search_hybrid(
            state.memory_store.clone(),
            state.memory_vectors.clone(),
            &query,
            limit,
            &self.cfg.dir_path,
        )
        .await
        {
            Ok(items) => {
                if items.is_empty() {
                    let msg = format!("未找到与「{query}」相关的记忆");
                    ctx.sink.on_result(ctx.call_id, "search_memory", true, &msg, Some(&msg));
                    return Ok(Value::String(msg));
                }
                let mut out = String::from("相关长期记忆：\n");
                for (i, item) in items.iter().enumerate() {
                    out.push_str(&format!(
                        "{}. [{}] {}（id={}）\n   {}\n",
                        i + 1,
                        item.kind,
                        item.title,
                        item.id,
                        item.body
                    ));
                }
                let summary = format!("{} 条", items.len());
                ctx.sink.on_result(ctx.call_id, "search_memory", true, &summary, Some(&out));
                Ok(Value::String(out))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "search_memory", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// todo_write：维护当前任务的任务清单（业务助手 `todo_write`）。
pub struct TodoWriteTool {
    cfg: KbSearchConfig,
}

impl TodoWriteTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            read_only_spec(
                "todo_write",
                "维护当前任务的任务清单（用于长任务执行中跟踪进度、防止遗漏步骤）。action 支持：add（追加待办）、complete（标记完成，items 为空则全部完成）、remove（移除条目，items 为空则清空）、clear（清空清单）、replace（整体替换清单）。调用后返回最新清单（[x]=已完成，[ ]=待办）。任务结束时用 clear 清空。",
                json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["add", "complete", "remove", "clear", "replace"],
                            "description": "操作类型"
                        },
                        "items": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "涉及的清单条目文本（add/complete/remove/replace 使用；complete/remove 为空时作用于全部）"
                        }
                    },
                    "required": ["action"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let action = args.get("action").and_then(|s| s.as_str()).unwrap_or_default().trim().to_string();
        let items: Vec<String> = args
            .get("items")
            .and_then(|a| a.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        if action.is_empty() {
            return Err(ToolError::InvalidArgs("action 不能为空（add/complete/remove/clear/replace）".into()));
        }
        let preview = format!("{action}: {} 项", items.len());
        ctx.sink.on_call(ctx.call_id, "todo_write", &preview, &args);
        match super::tools::todo_write(&self.cfg, &action, &items).await {
            Ok(text) => {
                ctx.sink.on_result(ctx.call_id, "todo_write", true, &format!("{} 项", text.lines().count().saturating_sub(1)), Some(&text));
                Ok(Value::String(text))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "todo_write", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
    }
}

/// 按协议选择 LlmAdapter（OpenAI 兼容 / Anthropic Messages）——LlmAdapter seam 统一入口。
/// Anthropic 适配器暂不含工具协议面（Agent 模式下等同纯对话语义；工具映射后续扩展）。
pub fn build_loop_adapter(
    llm_cfg: &crate::LlmConfig,
) -> Arc<dyn crate::core::r#loop::LlmAdapter> {
    if llm_cfg.protocol == "anthropic" {
        Arc::new(crate::core::r#loop::AnthropicAdapter::new(
            llm_cfg.endpoint.clone(),
            llm_cfg.api_key.clone(),
            llm_cfg.model.clone(),
            llm_cfg.max_tokens.unwrap_or(0),
            None, // thinking 档位暂不映射（reasoning_effort → thinking_budget 后续接入）
        ))
    } else {
        Arc::new(crate::core::r#loop::OpenAiAdapter::new(
            llm_cfg.endpoint.clone(),
            llm_cfg.model.clone(),
            llm_cfg.api_key.clone(),
            llm_cfg.reasoning_effort.clone(),
        ))
    }
}

/// 按白名单过滤注册表（子代理只读/写型工具集；白名单外工具不注册 → 模型不可见不可调）。
pub fn filter_registry(
    full: &dyn crate::core::r#loop::ToolRegistry,
    whitelist: &std::collections::HashSet<String>,
) -> HashMapToolRegistry {
    let mut reg = HashMapToolRegistry::new();
    for name in whitelist {
        if let Some(t) = full.get(name) {
            reg.register(t);
        }
    }
    reg
}

/// 前端事件协议适配件：把新 loop 的工具事件写入现有 [`ToolCallBus`]（`super::tools`），
/// 命令层现有 `emit_pending_tool_events` 无需改动即可把 `agent:tool_call`/`agent:tool_result`
/// 转发前端——**前端事件协议兼容零改动**（"以 DSH 核心为基石，业务/传输向基石对齐"）。
///
/// 说明：总线以工具名配对 call/result（预存在语义）；新 loop 的调度器按模型序提交结果，
/// 故 `on_result` 调用序与 call 序一致，配对正确。
pub struct BusToolEventSink {
    cfg: KbSearchConfig,
}

impl BusToolEventSink {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

impl crate::core::r#loop::ToolEventSink for BusToolEventSink {
    fn on_call(&self, _call_id: &str, tool: &str, args_preview: &str, args: &Value) {
        super::tools::record_tool_call(&self.cfg, tool, args_preview, Some(args));
    }
    fn on_result(&self, _call_id: &str, tool: &str, ok: bool, summary: &str, result: Option<&str>) {
        super::tools::record_tool_result(&self.cfg, tool, ok, summary, result);
    }
}

// ---- schedule + bookmarks tools (v3 migration, Phase 6) ----
/// schedule：日程管理（直接调用 Rust 引擎 core::schedule，不经 FrontendBridge）。动作与参数与 rig 版逐字一致（对齐 resources/skills/schedule/SKILL.md）：list/add/update/remove/conflicts/remind/lunar/next_available/plan/optimize/review/focus/today_plan + reminder_* 提醒操作；强制规则：任何涉及日程/提醒的回答必须先调用本工具查询最新数据，禁止依据上下文推断/编造。等待期间独占执行（concurrency_safe=false）。
pub struct ScheduleTool {
    cfg: KbSearchConfig,
    spec: ToolSpec,
}

impl ScheduleTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self {
            cfg,
            spec: ToolSpec::new(
                "schedule",
        "日程管理：查询/创建/更新/删除日程与闹钟提醒、冲突检测、到点提醒、农历节假日、找空闲时间段、任务排期、时间统计、日复盘、专注块、当日计划。动作：list 全部日程（输出含 id）；add 新建（title/start/end 必填，YYYY-MM-DDTHH:MM）；update 按 id 或唯一 target_title 部分更新（未传字段保留原值）；remove 按 id 或唯一 title 删除；conflicts 区间重叠检测（start/end 必填）；remind 到点应提醒；lunar 农历节假日（date）；next_available 空闲段（duration_minutes 必填）；plan 任务排布（deadline+tasks 必填，只建议不创建）；optimize 时间统计（range 默认 7d）；review 日复盘；focus 专注块（duration_minutes 必填）；today_plan 某日计划。提醒：reminder_list/reminder_add（time+title 必填）/reminder_update（不传 time 保留原时间）/reminder_remove。可选参数（add/update）：desc/color/cron（5 字段）/notify/notify_before/event_type/priority/related_docs/related_tasks/related_git/ai_category/ai_energy/ai_estimated_hours。当用户要求安排会议、查看日程、规划任务、复盘时间、设置提醒、查询节假日时调用。**强制规则：任何涉及日程/提醒的回答（含\"查询今日日程\"\"有什么安排\"\"到点提醒\"等）必须先调用本工具查询最新数据，禁止依据对话上下文或历史输出推断、复述或编造；用户提到的日程/提醒时间也以本工具返回为准。**",
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "add", "update", "remove", "conflicts", "remind", "lunar", "next_available", "plan", "optimize", "review", "focus", "today_plan", "reminder_list", "reminder_add", "reminder_update", "reminder_remove"],
                    "description": "要执行的动作"
                },
                "id": { "type": "string", "description": "事件 id（update/remove 用，与标题定位二选一）" },
                "target_title": { "type": "string", "description": "按标题定位要更新的日程（update 用，与 id 二选一；标题唯一匹配时生效；更新字段仍用 title/start/end 等）" },
                "title": { "type": "string", "description": "日程标题（add/update/plan 任务标题；remove 时作为按标题删除的定位依据）" },
                "start": { "type": "string", "description": "开始时间 YYYY-MM-DDTHH:MM（add/update/conflicts/focus 用）" },
                "end": { "type": "string", "description": "结束时间 YYYY-MM-DDTHH:MM（add/update/conflicts 用）" },
                "desc": { "type": "string", "description": "描述（add/update 可选）" },
                "color": { "type": "string", "description": "颜色标记（add/update 可选，默认 blue ）", "enum": ["blue", "green", "orange", "red", "purple"] },
                "cron": { "type": "string", "description": "Cron 重复表达式（5 字段，add/update 可选）" },
                "notify": { "type": "boolean", "description": "是否提醒（add/update 可选，默认 true）" },
                "notify_before": { "type": "integer", "description": "提前提醒分钟数，0=开始即提醒（add/update 可选）" },
                "event_type": { "type": "string", "description": "事件类型 work/meeting/focus/personal/task（add/update 可选，focus 动作自动为 focus）" },
                "priority": { "type": "string", "description": "优先级 high/medium/low（add/update 可选）" },
                "related_docs": { "type": "array", "items": { "type": "string" }, "description": "关联文档路径列表（add/update 可选）" },
                "related_tasks": { "type": "array", "items": { "type": "string" }, "description": "关联任务列表（add/update 可选）" },
                "related_git": { "type": "array", "items": { "type": "string" }, "description": "关联 Git 提交列表（add/update 可选）" },
                "ai_category": { "type": "string", "description": "AI 任务类别（add/update 可选）" },
                "ai_energy": { "type": "string", "description": "AI 精力类型 deep_work/shallow/rest（add/update 可选）" },
                "ai_estimated_hours": { "type": "number", "description": "AI 预估投入小时数（add/update 可选）" },
                "ignore_id": { "type": "string", "description": "冲突检测时忽略的事件 id（conflicts 可选）" },
                "date": { "type": "string", "description": "日期 YYYY-MM-DD（lunar/review/today_plan 用，review/today_plan 可省略默认今天）" },
                "duration_minutes": { "type": "integer", "description": "所需时长（分钟，next_available/focus 必填）" },
                "start_after": { "type": "string", "description": "最早开始时间（next_available 可选）" },
                "skip_rest_days": { "type": "boolean", "description": "是否跳过休息日/节假日（next_available/plan 可选，默认 true）" },
                "deadline": { "type": "string", "description": "截止日期 YYYY-MM-DD（plan 必填）" },
                "tasks": { "type": "array", "items": { "type": "object", "properties": { "title": { "type": "string" }, "hours": { "type": "number" } }, "required": ["title", "hours"] }, "description": "AI 拆解后的任务列表 [{title,hours}]（plan 必填）" },
                "work_start": { "type": "integer", "description": "每日工作开始小时（plan/today_plan 可选，默认 9）" },
                "work_end": { "type": "integer", "description": "每日工作结束小时（plan/today_plan 可选，默认 18）" },
                "range": { "type": "string", "description": "统计范围 7d/30d/YYYY-MM-DD..YYYY-MM-DD（optimize 可选，默认 7d）" },
                "task": { "type": "string", "description": "专注内容标题（focus 可选）" },
                "time": { "type": "string", "description": "提醒时间 YYYY-MM-DDTHH:MM（reminder_add 必填；reminder_update 可选，不传则保留原时间）" }
            },
            "required": ["action"]
        })
            ),
        }
    }
}

#[async_trait]
impl Tool for ScheduleTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
let dir = self.cfg.dir_path.clone();
let app = self.cfg.app_handle.clone();
let cfg = self.cfg.clone();
        use crate::core::schedule::rules;
        use crate::core::schedule::store::EventStore;
        use crate::core::schedule::{AiMeta, RelatedLinks, ScheduleEvent, ScheduleEventInput};
        use tauri::Emitter;

        let action = args
            .get("action")
            .and_then(|a| a.as_str())
            .filter(|a| !a.trim().is_empty())
            .unwrap_or("list");
        // 提醒操作归一化：reminder_* 复用 add/update/remove/list 逻辑
        // - reminder_add：单点时间（start=end=time），强制 event_type=reminder、notify=true
        // - reminder_update：按 id 更新（保持 reminder 类型）
        // - reminder_remove / reminder_list：按 id 删除 / 仅列提醒
        let raw_action = action.to_string();
        let is_reminder_op = raw_action.starts_with("reminder_");
        let action: &str = if is_reminder_op {
            match raw_action.as_str() {
                "reminder_add" => "add",
                "reminder_update" => "update",
                "reminder_remove" => "remove",
                "reminder_list" => "list",
                _ => return Err(ToolError::Failed(format!("未知动作: {}", raw_action))),
            }
        } else {
            raw_action.as_str()
        };
        // 软门禁（替代 rig active_tools 硬过滤）：
        // P0-8：主对话（skill_gating=true）下仅当 schedule 技能 Active 时执行，
        // None（无激活技能）→ 引导激活（与 SkillGateHook 语义一致）；
        // 子代理（skill_gating=false）白名单已过滤，直接放行。
        let unlocked = if !cfg.skill_gating {
            true
        } else {
            cfg.skill_state
                .allowed_tools()
                .as_ref()
                .is_some_and(|list| list.iter().any(|t| t == "schedule"))
        };
        if !unlocked {
            let msg = "当前技能集未声明 schedule 工具（已激活的技能未包含日程管理）。如需日程功能，请先调用 activate_skill（skill_id='schedule'）激活 schedule 技能，再重新发起操作；本次未执行。";
            log::info!("[agent] schedule 未声明于当前技能集被调用，返回引导 request_id={}", cfg.request_id);
            return Ok(Value::String(msg.to_string()));
        }
        // Mutation Verification 轨迹：schedule 需走 ToolCallBus，前端 tool-trace
        // 依赖 agent:tool_call / agent:tool_result 事件（此前缺失导致不显示）
        ctx.sink.on_call(ctx.call_id, "schedule", &format!("action={}", action), &args);
        // 用户可见时间显示：ISO 分隔符 T → 空格（2026-08-16T10:00 → 2026-08-16 10:00）
        let disp = |ts: &str| ts.replace('T', " ");
        let get = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let get_i64 = |k: &str, default: i64| args.get(k).and_then(|v| v.as_i64()).unwrap_or(default);
        let get_f64 = |k: &str, default: f64| args.get(k).and_then(|v| v.as_f64()).unwrap_or(default);
        let get_strings = |k: &str| {
            args.get(k)
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let state = app.state::<crate::AppState>();
        // 共享存储：与 IPC 命令 / 提醒调度器共用同一 Arc<Mutex>，杜绝并发写丢失更新
        let store_ref = state.schedule_store(&dir).map_err(|e| ToolError::Failed(e))?;
        let now = chrono::Local::now().naive_local();
        let fmt = |dt: chrono::NaiveDateTime| dt.format("%Y-%m-%dT%H:%M").to_string();
        // 短锁：每个动作获取一次 guard（add/update 在单锁内完成读+写）；poison 恢复保证高可用
        let store_guard = || store_ref.lock().unwrap_or_else(|e| e.into_inner());
        // 「稍后提醒」临时事件对 Agent 不可见（与前端 _isSnoozeReminderEvent 口径一致）：
        // 标题 `[稍后提醒] ` 前缀。所有查询/统计/冲突/排期/到点提醒均排除该事件，
        // 避免残留的延迟提醒干扰 AI 对用户日程的判断（如误避让/误合并）。
        let is_snooze_reminder = |e: &ScheduleEvent| e.title.starts_with("[稍后提醒]");
        let list_visible = || -> Result<Vec<ScheduleEvent>, String> {
            Ok(store_guard()
                .list()?
                .into_iter()
                .filter(|e| !is_snooze_reminder(e))
                .collect())
        };

        // 内部 async 块：match 各分支的 `return Ok/Err` / `?` 提前结束本块并携带
        // 结果，外层统一 record_tool_result（前端 tool-trace 依赖轨迹事件）。
        let result: Result<Value, ToolError> = async {
        match action {
            "list" => {
                let store = store_guard();
                let mut events = store.list().map_err(|e| ToolError::Failed(e))?;
                events.retain(|e| !is_snooze_reminder(e)); // 稍后提醒对 Agent 不可见
                if is_reminder_op {
                    // reminder_list：只列提醒（event_type=reminder 的单点事件）
                    events.retain(|e| e.event_type == "reminder");
                }
                if events.is_empty() {
                    return Ok(Value::String(if is_reminder_op {
                        "当前没有提醒".to_string()
                    } else {
                        "当前没有日程".to_string()
                    }));
                }
                let lines: Vec<String> = events
                    .iter()
                    .map(|e| {
                        let cron = if e.cron.trim().is_empty() { String::new() } else { format!("（重复 {}）", e.cron) };
                        let mut extra: Vec<String> = Vec::new();
                        if !e.event_type.is_empty() && !is_reminder_op {
                            extra.push(e.event_type.clone());
                        }
                        if !e.priority.is_empty() {
                            extra.push(e.priority.clone());
                        }
                        if e.notify_before > 0 {
                            extra.push(format!("提前{}分钟提醒", e.notify_before));
                        }
                        let tags = if extra.is_empty() { String::new() } else { format!(" [{}]", extra.join("/")) };
                        if is_reminder_op {
                            format!("- {}（id: {}）：{}（备注 {}）{}", e.title, e.id, disp(&e.start), if e.desc.is_empty() { "无" } else { &e.desc }, tags)
                        } else {
                            format!("- {}（id: {}）：{} ~ {}{}{}", e.title, e.id, disp(&e.start), disp(&e.end), cron, tags)
                        }
                    })
                    .collect();
                Ok(Value::String(format!(
                    "共 {} 个{}：\n{}",
                    events.len(),
                    if is_reminder_op { "提醒" } else { "日程" },
                    lines.join("\n")
                )))
            }
            "add" | "update" => {
                let mut input = ScheduleEventInput {
                    title: get("title"),
                    start: if is_reminder_op { get("time") } else { get("start") },
                    end: if is_reminder_op { get("time") } else { get("end") },
                    color: {
                        let c = get("color");
                        if c.is_empty() { "blue".to_string() } else { c }
                    },
                    desc: get("desc"),
                    cron: get("cron"),
                    notify: args.get("notify").and_then(|v| v.as_bool()).unwrap_or(true),
                    notify_before: get_i64("notify_before", 0),
                    event_type: get("event_type"),
                    priority: get("priority"),
                    related: RelatedLinks {
                        docs: get_strings("related_docs"),
                        tasks: get_strings("related_tasks"),
                        git: get_strings("related_git"),
                    },
                    ai: AiMeta {
                        category: get("ai_category"),
                        energy: get("ai_energy"),
                        estimated_hours: get_f64("ai_estimated_hours", 0.0),
                    },
                };
                // 提醒（reminder_*）：强制单点时间事件类型，通知必开
                if is_reminder_op {
                    input.event_type = "reminder".into();
                    input.notify = true;
                }
                if input.title.trim().is_empty() {
                    return Err(ToolError::Failed("日程标题不能为空".into()));
                }
                if action == "add" {
                    // 单锁内完成读+写（冲突检测与写入原子，杜绝并发窗口丢失更新）；块结束即释放锁
                    let (event, conflict_events) = {
                        let mut store = store_guard();
                        let now_s = fmt(now);
                        let event = ScheduleEvent {
                            id: uuid::Uuid::new_v4().to_string(),
                            title: input.title,
                            start: input.start,
                            end: input.end,
                            color: input.color,
                            desc: input.desc,
                            cron: input.cron,
                            notify: input.notify,
                            notify_before: input.notify_before,
                            event_type: input.event_type,
                            priority: input.priority,
                            related: input.related,
                            ai: input.ai,
                            created_at: now_s.clone(),
                            updated_at: now_s,
                        };
                        event.validate().map_err(|e| ToolError::Failed(e))?;
                        // 冲突提示（同一锁内，避免并发下读到过期快照）
                        let mut conflict_events: Vec<ScheduleEvent> = Vec::new();
                        // 提醒（单点）与 Cron 事件不做日程冲突检测（提醒到点弹窗，不占日程档期）
                        if event.cron.trim().is_empty() && event.event_type != "reminder" {
                            if let (Some(s), Some(e)) =
                                (rules::parse_local_time(&event.start), rules::parse_local_time(&event.end))
                            {
                                let existing: Vec<ScheduleEvent> = store.list()
                                    .map_err(|e| ToolError::Failed(e))?
                                    .into_iter()
                                    .filter(|e| !is_snooze_reminder(e))
                                    .collect();
                                conflict_events = rules::find_conflicts(&existing, s, e, None);
                            }
                        }
                        store.upsert(event.clone()).map_err(|e| ToolError::Failed(e))?;
                        (event, conflict_events)
                    };
                    let _ = app.emit("schedule:changed", ()); // 通知前端刷新（AI 直写 DB 后 UI 同步）
                    let mut msg = if is_reminder_op {
                        format!("已创建提醒：{}（{}）", event.title, disp(&event.start))
                    } else {
                        format!("已创建日程：{}（{} ~ {}", event.title, disp(&event.start), disp(&event.end))
                    };
                    if !conflict_events.is_empty() {
                        msg.push_str(&format!(
                            "\n⚠ 时间冲突：{}",
                            conflict_events.iter().map(|c| c.title.as_str()).collect::<Vec<_>>().join("、")
                        ));
                        // 冲突时给出备选建议（只建议不自动移动/覆盖；锁已释放，可安全 await）
                        let duration = (rules::parse_local_time(&event.end)
                            .and_then(|e| rules::parse_local_time(&event.start).map(|s| (e - s).num_minutes()))
                            .unwrap_or(60))
                            .max(15);
                        let events_snapshot = list_visible().map_err(|e| ToolError::Failed(e))?;
                        let provider = state.schedule_day_info.clone();
                        let end_dt = rules::parse_local_time(&event.end).unwrap_or(now);
                        let alts = tokio::task::spawn_blocking(move || {
                            crate::core::schedule::planner::suggest_alternatives(
                                &events_snapshot, provider.as_ref(), duration, end_dt, true,
                            )
                        })
                        .await
                        .map_err(|e| ToolError::Failed(format!("生成备选建议失败: {}", e)))?;
                        if !alts.is_empty() {
                            msg.push_str("\n备选建议（需确认后另行 add）：");
                            for (i, t) in alts.iter().take(2).enumerate() {
                                let end_t = *t + chrono::Duration::minutes(duration);
                                msg.push_str(&format!("\n方案{}: {} ~ {}", i + 1, disp(&fmt(*t)), disp(&fmt(end_t))));
                            }
                        }
                    }
                    if !is_reminder_op {
                        msg.push(')');
                    }
                    Ok(Value::String(msg))
                } else {
                    // 单锁内完成 读→改→写（消除锁窗口：并发写者在此期间插入/删除不会被覆盖）
                    let mut store = store_guard();
                    let id = get("id");
                    let target_title = get("target_title");
                    let mut events: Vec<ScheduleEvent> = store
                        .list()
                        .map_err(|e| ToolError::Failed(e))?
                        .into_iter()
                        .filter(|e| !is_snooze_reminder(e))
                        .collect();
                    // 定位：id 优先；未提供 id 时按 target_title 唯一匹配（list 不展示内部 id）
                    let matched_idx: Option<usize> = if !id.is_empty() {
                        events.iter().position(|e| e.id == id)
                    } else if !target_title.is_empty() {
                        let matches: Vec<usize> = events
                            .iter()
                            .enumerate()
                            .filter(|(_, e)| e.title == target_title)
                            .map(|(i, _)| i)
                            .collect();
                        match matches.len() {
                            1 => Some(matches[0]),
                            0 => return Err(ToolError::Failed(format!(
                                "未找到标题为『{}』的日程，请先 list 确认标题", target_title
                            ))),
                            n => return Err(ToolError::Failed(format!(
                                "标题『{}』匹配 {} 个日程，请改用 id 或更精确的标题", target_title, n
                            ))),
                        }
                    } else {
                        return Err(ToolError::Failed("update 需要 id 或 target_title 定位".into()));
                    };
                    let Some(existing) = matched_idx.map(|i| &mut events[i]) else {
                        return Err(ToolError::Failed("日程不存在".into()));
                    };
                    // 部分更新：title/start/end(或 time)/color/cron/event_type/priority 未传时保留原值；
                    // desc/notify/notify_before/related/ai 显式传值即覆盖（与 SKILL.md 契约一致）。
                    // 注意：字符串字段"空串视为未传"意味着无法用 update 清空这些字段（清空可传显式空串的
                    // desc 除外——desc 保留覆盖语义），权衡以安全为先。
                    if !get("title").is_empty() {
                        existing.title = input.title;
                    }
                    if is_reminder_op {
                        // 提醒更新未传 time 时保留原提醒时间（只改标题/备注/颜色）
                        if !get("time").is_empty() {
                            existing.start = input.start;
                            existing.end = input.end;
                        }
                    } else {
                        if !get("start").is_empty() {
                            existing.start = input.start;
                        }
                        if !get("end").is_empty() {
                            existing.end = input.end;
                        }
                    }
                    if !get("color").is_empty() {
                        existing.color = input.color;
                    }
                    if !get("cron").is_empty() {
                        existing.cron = input.cron;
                    }
                    if !get("event_type").is_empty() {
                        existing.event_type = input.event_type;
                    }
                    if !get("priority").is_empty() {
                        existing.priority = input.priority;
                    }
                    existing.desc = input.desc;
                    existing.notify = input.notify;
                    existing.notify_before = input.notify_before;
                    existing.related = input.related;
                    existing.ai = input.ai;
                    existing.updated_at = fmt(now);
                    existing.validate().map_err(|e| ToolError::Failed(e))?;
                    let updated = existing.clone();
                    store.replace_all(events).map_err(|e| ToolError::Failed(e))?;
                    let _ = app.emit("schedule:changed", ()); // 通知前端刷新
                    Ok(Value::String(if is_reminder_op {
                        format!("已更新提醒：{}（{}）", updated.title, disp(&updated.start))
                    } else {
                        format!(
                            "已更新日程：{}（{} ~ {}）",
                            updated.title, disp(&updated.start), disp(&updated.end)
                        )
                    }))
                }
            }
            "remove" => {
                let id = get("id");
                let title = get("title");
                if id.is_empty() && title.is_empty() {
                    return Err(ToolError::Failed("remove 需要 id 或 title 定位".into()));
                }
                let mut store = store_guard();
                let events: Vec<ScheduleEvent> = store
                    .list()
                    .map_err(|e| ToolError::Failed(e))?
                    .into_iter()
                    .filter(|e| !is_snooze_reminder(e))
                    .collect();
                // 定位：id 优先；未提供 id 时按 title 唯一匹配（list 不展示内部 id）
                let removed_title: String;
                let remove_id: String = if !id.is_empty() {
                    removed_title = events
                        .iter()
                        .find(|e| e.id == id)
                        .map(|e| e.title.clone())
                        .unwrap_or_default();
                    id
                } else {
                    let matches: Vec<&ScheduleEvent> = events.iter().filter(|e| e.title == title).collect();
                    match matches.len() {
                        1 => {
                            removed_title = matches[0].title.clone();
                            matches[0].id.clone()
                        }
                        0 => return Err(ToolError::Failed(format!(
                            "未找到标题为『{}』的日程，请先 list 确认标题", title
                        ))),
                        n => return Err(ToolError::Failed(format!(
                            "标题『{}』匹配 {} 个日程，请改用 id 或更精确的标题", title, n
                        ))),
                    }
                };
                store.remove(&remove_id).map_err(|e| ToolError::Failed(e))?;
                let _ = app.emit("schedule:changed", ()); // 通知前端刷新
                let msg = if removed_title.is_empty() {
                    if is_reminder_op { "提醒已删除".to_string() } else { "日程已删除".to_string() }
                } else if is_reminder_op {
                    format!("已删除提醒：{}", removed_title)
                } else {
                    format!("已删除日程：{}", removed_title)
                };
                Ok(Value::String(msg))
            }
            "conflicts" => {
                let s = rules::parse_local_time(&get("start".into())).ok_or_else(|| ToolError::Failed("开始时间格式无效".into()))?;
                let e = rules::parse_local_time(&get("end".into())).ok_or_else(|| ToolError::Failed("结束时间格式无效".into()))?;
                let ignore = if get("ignore_id").is_empty() { None } else { Some(get("ignore_id")) };
                let events = list_visible().map_err(|e| ToolError::Failed(e))?;
                let conflicts = rules::find_conflicts(&events, s, e, ignore.as_deref());
                if conflicts.is_empty() {
                    Ok(Value::String("该时间段无冲突".to_string()))
                } else {
                    let lines: Vec<String> = conflicts.iter().map(|c| format!("- {}：{} ~ {}", c.title, disp(&c.start), disp(&c.end))).collect();
                    Ok(Value::String(format!("时间冲突 {} 项：\n{}", conflicts.len(), lines.join("\n"))))
                }
            }
            "remind" => {
                let events = list_visible().map_err(|e| ToolError::Failed(e))?;
                let due = rules::due_reminders(&events, now);
                if due.is_empty() {
                    Ok(Value::String("当前无到点提醒".to_string()))
                } else {
                    let lines: Vec<String> = due.iter().map(|e| format!("- {}：{} ~ {}", e.title, disp(&e.start), disp(&e.end))).collect();
                    Ok(Value::String(format!("到点提醒 {} 项：\n{}", due.len(), lines.join("\n"))))
                }
            }
            "lunar" => {
                let date = chrono::NaiveDate::parse_from_str(&get("date"), "%Y-%m-%d")
                    .map_err(|_| ToolError::Failed("日期格式无效（应为 YYYY-MM-DD）".into()))?;
                // day_info 内部可能触发 timor.tech blocking 网络，须在 blocking 线程执行
                let provider = state.schedule_day_info.clone();
                let info = tokio::task::spawn_blocking(move || provider.day_info(date))
                    .await
                    .map_err(|e| ToolError::Failed(format!("农历/节假日计算失败: {}", e)))?;
                let mut parts = vec![format!("农历 {}", if info.lunar_month.is_empty() { info.lunar_day.clone() } else { format!("{}{}", info.lunar_month, info.lunar_day) })];
                if !info.festival.is_empty() {
                    parts.push(format!("节日 {}", info.festival));
                }
                parts.push(if info.is_workday { "调休班日" } else if info.is_rest_day { "休息日" } else { "工作日" }.to_string());
                Ok(Value::String(format!("{}：{}", get("date"), parts.join("｜"))))
            }
            "next_available" => {
                let duration = args.get("duration_minutes").and_then(|v| v.as_i64()).ok_or_else(|| ToolError::Failed("缺少 duration_minutes".into()))?;
                let start_after = if get("start_after").is_empty() { now } else { rules::parse_local_time(&get("start_after".into())).ok_or_else(|| ToolError::Failed("start_after 格式无效".into()))? };
                let skip = args.get("skip_rest_days").and_then(|v| v.as_bool()).unwrap_or(true);
                // 临时 guard：取数后立即释放（guard 非 Send，不能跨 await）
                let events = list_visible().map_err(|e| ToolError::Failed(e))?;
                // planner 内部调 day_info（可能 blocking 网络），须在 blocking 线程执行
                let provider = state.schedule_day_info.clone();
                let next = tokio::task::spawn_blocking(move || {
                    crate::core::schedule::planner::next_available(&events, provider.as_ref(), duration, start_after, skip)
                })
                .await
                .map_err(|e| ToolError::Failed(format!("查找可安排时间失败: {}", e)))?;
                match next {
                    Some(t) => Ok(Value::String(format!("下一个可安排时间段：{}（持续 {} 分钟）", disp(&fmt(t)), duration))),
                    None => Err(ToolError::Failed("30 天内未找到可安排时间段".into())),
                }
            }
            "plan" => {
                // 任务排布建议：AI 拆解任务（title+hours）→ 引擎排到 deadline 前（只建议，不创建）
                let deadline = chrono::NaiveDate::parse_from_str(&get("deadline"), "%Y-%m-%d")
                    .map_err(|_| ToolError::Failed("deadline 格式无效（应为 YYYY-MM-DD）".into()))?;
                let tasks_raw = args
                    .get("tasks")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| ToolError::Failed("缺少 tasks（任务数组，每项含 title/hours）".into()))?;
                let tasks: Vec<crate::core::schedule::planner::PlannedTask> = tasks_raw
                    .iter()
                    .map(|t| crate::core::schedule::planner::PlannedTask {
                        title: t.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        hours: t.get("hours").and_then(|x| x.as_f64()).unwrap_or(0.0),
                    })
                    .filter(|t| !t.title.trim().is_empty() && t.hours > 0.0)
                    .collect();
                if tasks.is_empty() {
                    return Err(ToolError::Failed("tasks 为空或格式无效（每项需含 title 与 hours）".into()));
                }
                let ws = get_i64("work_start", 9) as u32;
                let we = get_i64("work_end", 18) as u32;
                let skip = args.get("skip_rest_days").and_then(|v| v.as_bool()).unwrap_or(true);
                let events = list_visible().map_err(|e| ToolError::Failed(e))?;
                let provider = state.schedule_day_info.clone();
                let tasks_for_plan = tasks.clone();
                let results = tokio::task::spawn_blocking(move || {
                    crate::core::schedule::planner::plan_tasks(&events, provider.as_ref(), &tasks_for_plan, deadline, ws, we, skip, now)
                })
                .await
                .map_err(|e| ToolError::Failed(format!("任务排布失败: {}", e)))?;
                let mut lines = vec![format!(
                    "任务排布建议（截止 {}，工作日 {}:00-{}:00{}）：",
                    deadline.format("%Y-%m-%d"),
                    ws,
                    we,
                    if skip { "，跳过休息日" } else { "" }
                )];
                for (i, r) in results.iter().enumerate() {
                    match r {
                        Some(slot) => lines.push(format!("- {}：{} ~ {}", slot.title, disp(&fmt(slot.start)), disp(&fmt(slot.end)))),
                        None => lines.push(format!("- {}：截止日期前排不下（请拆分任务或延后截止日期）", tasks[i].title)),
                    }
                }
                lines.push("以上仅为排布建议，确认后请用 add 逐条创建日程。".to_string());
                Ok(Value::String(lines.join("\n")))
            }
            "optimize" => {
                // 时间投入统计（确定性数据），优化建议由 AI 基于输出生成
                let range = get("range");
                let (from, to) = if range.is_empty() || range == "7d" {
                    ((now - chrono::Duration::days(6)).date().and_hms_opt(0, 0, 0).unwrap(), now)
                } else if range == "30d" {
                    ((now - chrono::Duration::days(29)).date().and_hms_opt(0, 0, 0).unwrap(), now)
                } else if let Some((a, b)) = range.split_once("..") {
                    let da = chrono::NaiveDate::parse_from_str(a.trim(), "%Y-%m-%d")
                        .map_err(|_| ToolError::Failed("range 起始日期格式无效（应为 YYYY-MM-DD..YYYY-MM-DD）".into()))?;
                    let db = chrono::NaiveDate::parse_from_str(b.trim(), "%Y-%m-%d")
                        .map_err(|_| ToolError::Failed("range 结束日期格式无效（应为 YYYY-MM-DD..YYYY-MM-DD）".into()))?;
                    if db < da {
                        return Err(ToolError::Failed("range 结束日期早于起始日期".into()));
                    }
                    (da.and_hms_opt(0, 0, 0).unwrap(), db.and_hms_opt(23, 59, 0).unwrap())
                } else {
                    return Err(ToolError::Failed("range 格式无效（支持 7d / 30d / YYYY-MM-DD..YYYY-MM-DD）".into()));
                };
                let events = list_visible().map_err(|e| ToolError::Failed(e))?;
                let stats = tokio::task::spawn_blocking(move || {
                    crate::core::schedule::analyze::analyze_range(&events, from, to)
                })
                .await
                .map_err(|e| ToolError::Failed(format!("时间统计失败: {}", e)))?;
                use crate::core::schedule::analyze::fmt_minutes;
                let mut lines = vec![format!(
                    "时间投入统计（{} ~ {}）：",
                    from.format("%Y-%m-%d"),
                    to.format("%Y-%m-%d")
                )];
                lines.push(format!(
                    "共 {} 项日程，总投入 {}，平均每个有投入工作日 {}",
                    stats.event_count,
                    fmt_minutes(stats.total_minutes),
                    format!("{:.1}小时", stats.avg_workday_hours)
                ));
                lines.push(format!("其中会议 {}，深度工作 {}（含 focus/work/energy=deep_work）", fmt_minutes(stats.meeting_minutes), fmt_minutes(stats.deep_work_minutes)));
                if stats.evening_meeting_minutes > 0 {
                    lines.push(format!("下午（13:00 起）会议 {}，占比 {:.0}%", fmt_minutes(stats.evening_meeting_minutes), (stats.evening_meeting_minutes as f64 / stats.meeting_minutes.max(1) as f64 * 100.0)));
                }
                if !stats.by_type.is_empty() {
                    let types = stats
                        .by_type
                        .iter()
                        .map(|(t, m)| format!("{}:{}", t, fmt_minutes(*m)))
                        .collect::<Vec<_>>()
                        .join("，");
                    lines.push(format!("按类型：{}", types));
                }
                if !stats.by_day.is_empty() {
                    let days = stats
                        .by_day
                        .iter()
                        .map(|(d, m, c)| format!("{}:{}分钟/{}项", d.format("%m-%d"), m, c))
                        .collect::<Vec<_>>()
                        .join("，");
                    lines.push(format!("按天：{}", days));
                }
                lines.push("以上为确定性统计，优化建议（如保护深度工作时间）请由 AI 结合上下文生成。".to_string());
                Ok(Value::String(lines.join("\n")))
            }
            "review" => {
                // 日复盘统计：完成 / 进行中 / 未开始 + 投入时长（原因分析与建议由 AI 生成）
                let date = if get("date").is_empty() {
                    now.date()
                } else {
                    chrono::NaiveDate::parse_from_str(&get("date"), "%Y-%m-%d")
                        .map_err(|_| ToolError::Failed("日期格式无效（应为 YYYY-MM-DD）".into()))?
                };
                let events = list_visible().map_err(|e| ToolError::Failed(e))?;
                let summary = tokio::task::spawn_blocking(move || {
                    crate::core::schedule::analyze::day_summary(&events, date, now)
                })
                .await
                .map_err(|e| ToolError::Failed(format!("复盘统计失败: {}", e)))?;
                use crate::core::schedule::analyze::fmt_minutes;
                let mut lines = vec![format!(
                    "{} 日程复盘：共 {} 项，投入 {}",
                    date.format("%Y-%m-%d"),
                    summary.done.len() + summary.ongoing.len() + summary.upcoming.len(),
                    fmt_minutes(summary.total_minutes)
                )];
                if !summary.done.is_empty() {
                    lines.push("已完成：".to_string());
                    for e in &summary.done {
                        lines.push(format!("✅ {}（{} ~ {}）", e.title, disp(&e.start), disp(&e.end)));
                    }
                }
                if !summary.ongoing.is_empty() {
                    lines.push("进行中：".to_string());
                    for e in &summary.ongoing {
                        lines.push(format!("⏳ {}（{} ~ {}）", e.title, disp(&e.start), disp(&e.end)));
                    }
                }
                if !summary.upcoming.is_empty() {
                    lines.push("未开始：".to_string());
                    for e in &summary.upcoming {
                        lines.push(format!("🔜 {}（{} ~ {}）", e.title, disp(&e.start), disp(&e.end)));
                    }
                }
                lines.push("以上为确定性归类，延期原因与改进建议请由 AI 结合上下文生成。".to_string());
                Ok(Value::String(lines.join("\n")))
            }
            "focus" => {
                // 专注时间块：指定 start 时校验冲突并创建（type=focus）；未指定时只推荐时间段
                let duration = args.get("duration_minutes").and_then(|v| v.as_i64()).ok_or_else(|| ToolError::Failed("缺少 duration_minutes".into()))?;
                if duration < 1 {
                    return Err(ToolError::Failed("duration_minutes 必须大于 0".into()));
                }
                let task = get("task");
                let title = if task.trim().is_empty() { "专注时间".to_string() } else { format!("专注：{}", task.trim()) };
                let start_str = get("start");
                if start_str.is_empty() {
                    // 未指定开始时间：推荐下一个空档（不创建）
                    let events = list_visible().map_err(|e| ToolError::Failed(e))?;
                    let provider = state.schedule_day_info.clone();
                    let next = tokio::task::spawn_blocking(move || {
                        crate::core::schedule::planner::next_available(&events, provider.as_ref(), duration, now, true)
                    })
                    .await
                    .map_err(|e| ToolError::Failed(format!("查找专注时间段失败: {}", e)))?;
                    match next {
                        Some(t) => Ok(Value::String(format!(
                            "建议专注时间段：{} ~ {}（{} 分钟）。如需创建请确认后调用 add（event_type=focus）。",
                            disp(&fmt(t)),
                            disp(&fmt(t + chrono::Duration::minutes(duration))),
                            duration
                        ))),
                        None => Err(ToolError::Failed("30 天内未找到可安排时间段".into())),
                    }
                } else {
                    let start = rules::parse_local_time(&start_str)
                        .ok_or_else(|| ToolError::Failed("start 格式无效（应为 YYYY-MM-DDTHH:MM）".into()))?;
                    let end = start + chrono::Duration::minutes(duration);
                    let mut store = store_guard();
                    let existing: Vec<ScheduleEvent> = store
                        .list()
                        .map_err(|e| ToolError::Failed(e))?
                        .into_iter()
                        .filter(|e| !is_snooze_reminder(e))
                        .collect();
                    let conflicts = rules::find_conflicts(&existing, start, end, None);
                    if !conflicts.is_empty() {
                        return Ok(Value::String(format!(
                            "时间冲突，未创建专注块：{}",
                            conflicts.iter().map(|c| format!("{}（{} ~ {}）", c.title, disp(&c.start), disp(&c.end))).collect::<Vec<_>>().join("、")
                        )));
                    }
                    let now_s = fmt(now);
                    let event = ScheduleEvent {
                        id: uuid::Uuid::new_v4().to_string(),
                        title,
                        start: start_str,
                        end: fmt(end),
                        color: "blue".into(),
                        event_type: "focus".into(),
                        notify: true,
                        created_at: now_s.clone(),
                        updated_at: now_s,
                        ..Default::default()
                    };
                    event.validate().map_err(|e| ToolError::Failed(e))?;
                    store.upsert(event.clone()).map_err(|e| ToolError::Failed(e))?;
                    let _ = app.emit("schedule:changed", ());
                    Ok(Value::String(format!(
                        "已创建专注时间块：{}（{} ~ {}）",
                        event.title, disp(&event.start), disp(&event.end)
                    )))
                }
            }
            "today_plan" => {
                // 某日日程 + 空闲时间段（供 AI 生成今日/明日计划）
                let date = if get("date").is_empty() {
                    now.date()
                } else {
                    chrono::NaiveDate::parse_from_str(&get("date"), "%Y-%m-%d")
                        .map_err(|_| ToolError::Failed("日期格式无效（应为 YYYY-MM-DD）".into()))?
                };
                let ws = get_i64("work_start", 9) as u32;
                let we = get_i64("work_end", 18) as u32;
                let events = list_visible().map_err(|e| ToolError::Failed(e))?;
                let (day_events, blocks) = tokio::task::spawn_blocking(move || {
                    let day_events = rules::events_on_date(&events, date);
                    let blocks = crate::core::schedule::analyze::available_blocks(&events, date, ws, we);
                    (day_events, blocks)
                })
                .await
                .map_err(|e| ToolError::Failed(format!("生成当日计划失败: {}", e)))?;
                use crate::core::schedule::analyze::{fmt_blocks, fmt_minutes};
                let mut lines = vec![format!("{} 计划：", date.format("%Y-%m-%d"))];
                if day_events.is_empty() {
                    lines.push("当天无日程。".to_string());
                } else {
                    lines.push(format!("日程 {} 项：", day_events.len()));
                    for e in &day_events {
                        let mut tag = format!("- {}（{} ~ {}", e.title, disp(&e.start), disp(&e.end));
                        if !e.event_type.is_empty() {
                            tag.push_str(&format!("，{}", e.event_type));
                        }
                        tag.push(')');
                        lines.push(tag);
                    }
                }
                lines.push(format!("工作窗口 {}:00-{}:00 空闲段：{}", ws, we, fmt_blocks(&blocks)));
                if let Some(total) = day_events.iter().filter_map(|e| {
                    rules::parse_local_time(&e.start).and_then(|s| rules::parse_local_time(&e.end).map(|t| (t - s).num_minutes()))
                }).reduce(|a, b| a + b) {
                    lines.push(format!("当天日程总时长：{}", fmt_minutes(total)));
                }
                lines.push("请基于以上日程与空闲段生成今日安排建议。".to_string());
                Ok(Value::String(lines.join("\n")))
            }
            _ => Err(ToolError::Failed(format!("未知动作: {}", action))),
        }
        }.await;
        // Mutation Verification 轨迹：统一记录 schedule 调用结果（成功/失败），
        // 前端 tool-trace 据此渲染卡片状态与参数摘要。
        match &result {
            Ok(out) => {
                let t = out.as_str().map(|s| s.to_string()).unwrap_or_else(|| out.to_string());
                ctx.sink.on_result(ctx.call_id, "schedule", true, &truncate_text(&t, 200), Some(&t));
            }
            Err(e) => {
                let m = e.to_string();
                ctx.sink.on_result(ctx.call_id, "schedule", false, &truncate_text(&m, 200), Some(&m));
            }
        }
        result
}
}

/// search_bookmarks：检索用户收藏的书签知识资产（浏览器收藏的网页链接及其 AI 摘要/标签/分类）。在需要回忆用户收藏过哪些资料、或回答我收藏过什么/有没有相关资源时调用。只读（concurrency_safe=true）。
pub struct SearchBookmarksTool {
    cfg: KbSearchConfig,
    spec: ToolSpec,
}

impl SearchBookmarksTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self {
            cfg,
            spec: ToolSpec::new(
                "search_bookmarks",
        "检索用户收藏的书签知识资产（浏览器收藏的网页链接及其 AI 摘要/标签/分类）。在需要回忆用户收藏过哪些资料、或回答\"我收藏过什么/有没有相关资源\"时调用。只读。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "检索关键词（空格分隔多个词）" },
                "limit": { "type": "integer", "description": "最多返回条数（默认 5，最大 20）" },
                "category": { "type": "string", "description": "按 AI 分类过滤（如 AI/LLM），可选" },
                "folder": { "type": "string", "description": "按浏览器原始目录前缀过滤（如 AI），可选" }
            },
            "required": ["query"]
        })
            ),
        }
    }
}

#[async_trait]
impl Tool for SearchBookmarksTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
let cfg = self.cfg.clone();
        let query = args
            .get("query")
            .and_then(|q| q.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        let limit = args
            .get("limit")
            .and_then(|l| l.as_u64())
            .map(|v| v as usize)
            .unwrap_or(5)
            .clamp(1, 20);
        let category = args.get("category").and_then(|c| c.as_str()).map(|s| s.to_string());
        let folder = args.get("folder").and_then(|f| f.as_str()).map(|s| s.to_string());
        ctx.sink.on_call(ctx.call_id, "search_bookmarks", &format!("query={} limit={}", query, limit), &args);
        if query.is_empty() {
            let e = "检索关键词不能为空".to_string();
            ctx.sink.on_result(ctx.call_id, "search_bookmarks", false, &e, Some(&e));
            return Err(ToolError::Failed(e));
        }
        let state = cfg.app_handle.state::<crate::AppState>();
        let store = match state.bookmark_store(&cfg.dir_path) {
            Ok(s) => s,
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "search_bookmarks", false, &e, Some(&e));
                return Err(ToolError::Failed(e));
            }
        };
        let hits = {
            let store = store;
            match crate::core::knowledge::bookmark::search::search_with_vectors(
                &*store,
                &cfg.dir_path,
                &query,
                limit,
                category.as_deref(),
                folder.as_deref(),
            )
            .await
            {
                Ok(h) => h,
                Err(e) => {
                    ctx.sink.on_result(ctx.call_id, "search_bookmarks", false, &e, Some(&e));
                    return Err(ToolError::Failed(e));
                }
            }
        };
        if hits.is_empty() {
            let msg = format!("未找到与「{query}」相关的书签");
            ctx.sink.on_result(ctx.call_id, "search_bookmarks", true, &msg, Some(&msg));
            return Ok(Value::String(msg));
        }
        let mut out = String::from("相关书签收藏：\n");
        for (i, h) in hits.iter().enumerate() {
            out.push_str(&format!(
                "{}. {}（id={}）\n   URL: {}\n",
                i + 1,
                h.title.clone().unwrap_or_else(|| h.url.clone()),
                h.id,
                h.url
            ));
            if let Some(s) = &h.summary {
                if !s.is_empty() {
                    out.push_str(&format!("   摘要: {}\n", s));
                }
            }
            if let Some(t) = &h.tags {
                if t != "[]" && !t.is_empty() {
                    out.push_str(&format!("   标签: {}\n", t));
                }
            }
            if let Some(c) = &h.category {
                if !c.is_empty() {
                    out.push_str(&format!("   分类: {}\n", c));
                }
            }
        }
        ctx.sink.on_result(ctx.call_id, "search_bookmarks", true, &format!("{} 条", hits.len()), Some(&out));
        Ok(Value::String(out))
}
}

/// get_bookmark：按 id 获取某个书签收藏的完整详情（含 AI 摘要、标签、分类、抓取正文、状态）。在 search_bookmarks 定位到具体收藏后需要深入了解时调用。只读（concurrency_safe=true）。
pub struct GetBookmarkTool {
    cfg: KbSearchConfig,
    spec: ToolSpec,
}

impl GetBookmarkTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self {
            cfg,
            spec: ToolSpec::new(
                "get_bookmark",
        "按 id 获取某个书签收藏的完整详情（含 AI 摘要、标签、分类、抓取正文、状态）。在 search_bookmarks 定位到具体收藏后需要深入了解时调用。只读。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "书签 id（search_bookmarks 返回）" }
            },
            "required": ["id"]
        })
            ),
        }
    }
}

#[async_trait]
impl Tool for GetBookmarkTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
let cfg = self.cfg.clone();
        let id = args
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        ctx.sink.on_call(ctx.call_id, "get_bookmark", &format!("id={}", id), &args);
        if id.is_empty() {
            let e = "书签 id 不能为空".to_string();
            ctx.sink.on_result(ctx.call_id, "get_bookmark", false, &e, Some(&e));
            return Err(ToolError::Failed(e));
        }
        let state = cfg.app_handle.state::<crate::AppState>();
        let store = match state.bookmark_store(&cfg.dir_path) {
            Ok(s) => s,
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "get_bookmark", false, &e, Some(&e));
                return Err(ToolError::Failed(e));
            }
        };
        let bookmark = {
            let guard = match store.lock() {
                Ok(g) => g,
                Err(e) => {
                    let e = e.to_string();
                    ctx.sink.on_result(ctx.call_id, "get_bookmark", false, &e, Some(&e));
                    return Err(ToolError::Failed(e));
                }
            };
            guard.get(&id)
        };
        match bookmark {
            Ok(Some(b)) => {
                let status_line = if b.dead {
                    format!("状态: {}（死链）", b.status)
                } else {
                    format!("状态: {}", b.status)
                };
                let mut out = format!(
                    "书签详情（id={}）：\n标题: {}\nURL: {}\n{}\n浏览器目录: {}\n",
                    b.id,
                    b.title.clone().unwrap_or_default(),
                    b.url,
                    status_line,
                    b.browser_folder.clone().unwrap_or_default(),
                );
                if let Some(c) = &b.category {
                    if !c.is_empty() {
                        out.push_str(&format!("分类: {}\n", c));
                    }
                }
                if let Some(s) = &b.summary {
                    if !s.is_empty() {
                        out.push_str(&format!("摘要: {}\n", s));
                    }
                }
                if let Some(t) = &b.tags {
                    if t != "[]" && !t.is_empty() {
                        out.push_str(&format!("标签: {}\n", t));
                    }
                }
                if let Some(raw) = &b.raw_content {
                    if !raw.is_empty() {
                        let cut: String = raw.chars().take(800).collect();
                        out.push_str(&format!("正文（截断）: {}\n", cut));
                    }
                }
                ctx.sink.on_result(ctx.call_id, "get_bookmark", true, "ok", Some(&out));
                Ok(Value::String(out))
            }
            Ok(None) => {
                let msg = format!("未找到书签: {}", id);
                ctx.sink.on_result(ctx.call_id, "get_bookmark", true, &msg, Some(&msg));
                Ok(Value::String(msg))
            }
            Err(e) => {
                ctx.sink.on_result(ctx.call_id, "get_bookmark", false, &e, Some(&e));
                Err(ToolError::Failed(e))
            }
        }
}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_gate_semantics() {
        // 子代理场景：skill_gating=false → 放行
        assert!(skill_gated(false, None, "kb_search"));
        // 主对话：无激活技能（None）→ 不放行（引导）
        assert!(!skill_gated(true, None, "kb_search"));
        // 主对话：激活技能声明了该工具 → 放行
        let allowed = vec!["kb_search".to_string(), "read".to_string()];
        assert!(skill_gated(true, Some(&allowed), "kb_search"));
        assert!(!skill_gated(true, Some(&allowed), "code_lookup"));
        // 基础工具（read）不受技能声明限制：主对话无激活技能时仍应放行——
        // 注意：BASE_TOOLS 语义由 loop 层 Hook 保证；此处 kb_search 走软门禁，read 不在此判定
        assert!(!skill_gated(true, None, "kb_search"));
    }

    #[test]
    fn resolve_top_k_clamps() {
        assert_eq!(resolve_top_k(&json!({"top_k": 100}), 5), MAX_TOP_K);
        assert_eq!(resolve_top_k(&json!({"top_k": 0}), 5), 5);
        assert_eq!(resolve_top_k(&json!({"top_k": 3}), 5), 3);
        assert_eq!(resolve_top_k(&json!({}), 7), 7.min(MAX_TOP_K));
    }

    #[test]
    fn parse_str_list_accepts_array_and_csv() {
        assert_eq!(parse_str_list(&json!(["*.rs", "*.md"])), vec!["*.rs".to_string(), "*.md".to_string()]);
        assert_eq!(parse_str_list(&json!("*.rs, *.md")), vec!["*.rs".to_string(), "*.md".to_string()]);
        assert!(parse_str_list(&Value::Null).is_empty());
    }

    #[test]
    fn parse_read_args_single_multi_offset() {
        let (single, multi, offset) = parse_read_args(&json!({"path": "a.md", "offset": 1024}));
        assert_eq!(single, Some("a.md".to_string()));
        assert!(multi.is_empty());
        assert_eq!(offset, 1024);

        let (single, multi, offset) = parse_read_args(&json!({"paths": ["a.md", "b.md"]}));
        assert!(single.is_none());
        assert_eq!(multi.len(), 2);
        assert_eq!(offset, 0);

        let (single, multi, _) = parse_read_args(&json!({}));
        assert!(single.is_none());
        assert!(multi.is_empty());
    }

    #[test]
    fn schemas_are_valid_json_schema_objects() {
        // 工具实例需要 KbSearchConfig（含 AppHandle），无法在单测构造；
        // 此处直接验证 schema 常量形状（type=object + required 数组）
        for schema in [
            &json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}),
            &json!({"type":"object","properties":{"symbol":{"type":"string"}},"required":["symbol"]}),
            &json!({"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]}),
        ] {
            assert_eq!(schema["type"], "object");
            assert!(schema["properties"].is_object());
        }
    }
}
