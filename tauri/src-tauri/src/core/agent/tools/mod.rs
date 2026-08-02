//! Agent 内置工具集：文件只读、目录列举、Git 状态查询（全部只读，无写操作）。
//!
//! 所有工具调用都会实时写入 [`ToolCallBus`]，由 commands 层转发为
//! `agent:tool_call` / `agent:tool_result` 事件，前端据此渲染调用轨迹卡片。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use rig_agent::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use serde::Serialize;

use crate::core::agent::KbSearchConfig;
use crate::core::db::utils::IgnoreMatcher;

/// 单条工具调用事件（`kind = "call"` 或 `kind = "result"`）
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallEvent {
    pub seq: u64,
    pub kind: String,
    pub tool: String,
    /// 调用时的参数摘要（模型视角）
    pub args_preview: String,
    /// 结果是否成功
    pub ok: bool,
    /// 结果摘要（成功为返回规模，失败为错误信息）
    pub summary: String,
    /// 关联的 call 事件 seq（result 事件用它找到对应卡片）
    pub call_seq: u64,
}

/// 按 `request_id` 记录工具调用轨迹的全局总线。
///
/// 工具闭包在 Rig 流式内部执行，无法直接访问 Tauri 事件发射器，
/// 因此先写入本总线，由 `commands/llm.rs` 的流式循环按请求 drain 并转发。
pub struct ToolCallBus {
    seq: AtomicU64,
    map: Mutex<HashMap<String, Vec<ToolCallEvent>>>,
}

impl ToolCallBus {
    fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            map: Mutex::new(HashMap::new()),
        }
    }

    fn record_call(&self, request_id: &str, tool: &str, args_preview: &str) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut map) = self.map.lock() {
            map.entry(request_id.to_string()).or_default().push(ToolCallEvent {
                seq,
                kind: "call".into(),
                tool: tool.into(),
                args_preview: args_preview.into(),
                ok: false,
                summary: String::new(),
                call_seq: 0,
            });
        }
    }

    fn record_result(&self, request_id: &str, tool: &str, ok: bool, summary: &str) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut map) = self.map.lock() {
            let events = map.entry(request_id.to_string()).or_default();
            // 配对到该工具第一个"尚无对应 result"的 call（并行同名调用时也能正确配对）
            let mut referenced: std::collections::HashSet<u64> = events
                .iter()
                .filter(|e| e.kind == "result")
                .map(|e| e.call_seq)
                .collect();
            let call_seq = events
                .iter()
                .find(|e| e.kind == "call" && e.tool == tool && referenced.insert(e.seq))
                .map(|e| e.seq)
                .unwrap_or(seq);
            events.push(ToolCallEvent {
                seq,
                kind: "result".into(),
                tool: tool.into(),
                args_preview: String::new(),
                ok,
                summary: summary.into(),
                call_seq,
            });
        }
    }

    /// 消费式取出该请求尚未转发的事件，并清理空桶。
    pub fn drain(&self, request_id: &str) -> Vec<ToolCallEvent> {
        let mut out = Vec::new();
        if let Ok(mut map) = self.map.lock() {
            if let Some(events) = map.get_mut(request_id) {
                out = std::mem::take(events);
            }
            if map.get(request_id).map_or(true, |v| v.is_empty()) {
                map.remove(request_id);
            }
        }
        out
    }

    /// 请求结束时清理，防止长会话内存泄漏。
    pub fn clear(&self, request_id: &str) {
        if let Ok(mut map) = self.map.lock() {
            map.remove(request_id);
        }
    }
}

static TOOL_CALL_BUS: OnceLock<ToolCallBus> = OnceLock::new();

pub fn tool_call_bus() -> &'static ToolCallBus {
    TOOL_CALL_BUS.get_or_init(ToolCallBus::new)
}

/// 记录工具调用开始（供命令层转发 `agent:tool_call`）。
pub fn record_tool_call(cfg: &KbSearchConfig, tool: &str, args_preview: &str) {
    tool_call_bus().record_call(&cfg.request_id, tool, args_preview);
}

/// 记录工具调用结果（供命令层转发 `agent:tool_result`）。
pub fn record_tool_result(cfg: &KbSearchConfig, tool: &str, ok: bool, summary: &str) {
    tool_call_bus().record_result(&cfg.request_id, tool, ok, summary);
}

// ─────────────────────────── 只读文件工具 ───────────────────────────

/// 单次读取上限（避免大文件撑爆模型上下文）
const MAX_FILE_READ_CHARS: usize = 8192;
/// 目录列举上限
const MAX_LIST_ITEMS: usize = 60;

/// 将相对路径安全解析到知识库根目录内（防路径穿越）。
fn safe_resolve(dir_path: &str, rel: &str) -> Result<PathBuf, String> {
    let base = std::fs::canonicalize(dir_path)
        .map_err(|e| format!("无法访问知识库目录: {}", e))?;
    let full = std::fs::canonicalize(base.join(rel))
        .map_err(|e| format!("文件不存在: {}", e))?;
    if !full.starts_with(&base) {
        return Err("路径越界：仅允许访问知识库目录内的文件".into());
    }
    Ok(full)
}

pub async fn read_file(cfg: &KbSearchConfig, rel_path: &str) -> Result<String, String> {
    let full = safe_resolve(&cfg.dir_path, rel_path)?;
    let meta = std::fs::metadata(&full).map_err(|e| format!("读取文件信息失败: {}", e))?;
    if meta.is_dir() {
        return Err(format!("{} 是目录，请改用 list_files 查看目录内容", rel_path));
    }
    let data = std::fs::read(&full).map_err(|e| format!("读取文件失败: {}", e))?;
    let text = String::from_utf8_lossy(&data).into_owned();
    if text.chars().count() > MAX_FILE_READ_CHARS {
        let truncated: String = text.chars().take(MAX_FILE_READ_CHARS).collect();
        return Ok(format!("{truncated}\n\n[内容过长，已截断前 {MAX_FILE_READ_CHARS} 字符]"));
    }
    Ok(text)
}

pub async fn list_files(cfg: &KbSearchConfig, pattern: &str, max_items: u32) -> Result<String, String> {
    let base = std::fs::canonicalize(&cfg.dir_path)
        .map_err(|e| format!("无法访问知识库目录: {}", e))?;
    // 与全项目一致：目录/文件黑名单（gitignore 语法），与索引、监视逻辑使用同一套过滤
    let ignore = IgnoreMatcher::new(&cfg.dir_blacklist, &cfg.file_blacklist);
    let pattern = pattern.trim().to_lowercase();
    let max = (max_items as usize).clamp(1, MAX_LIST_ITEMS);
    let mut items: Vec<(String, u64)> = Vec::new();
    walk_dir(&base, &base, &pattern, 0, max, &ignore, &mut items);
    items.sort_by(|a, b| a.0.cmp(&b.0));

    if items.is_empty() {
        return Ok(format!(
            "目录中未找到匹配的文件（模式：{}）",
            if pattern.is_empty() { "全部" } else { &pattern }
        ));
    }
    let lines: Vec<String> = items
        .iter()
        .map(|(rel, size)| format!("{rel}  ({} 字节)", size))
        .collect();
    Ok(format!("共 {} 项：\n{}", items.len(), lines.join("\n")))
}

fn walk_dir(
    base: &PathBuf,
    dir: &PathBuf,
    pattern: &str,
    depth: usize,
    max: usize,
    ignore: &IgnoreMatcher,
    out: &mut Vec<(String, u64)>,
) {
    if out.len() >= max || depth > 3 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if out.len() >= max {
            break;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if path.is_dir() {
            // 目录黑名单过滤（对齐前端 isSkipDir：gitignore 语法 + 隐藏目录）
            if !ignore.is_kb_dir_allowed(&name, &rel) {
                continue;
            }
            if pattern.is_empty() || rel.to_lowercase().contains(pattern) {
                out.push((format!("{rel}/"), 0));
            }
            walk_dir(base, &path, pattern, depth + 1, max, ignore, out);
        } else {
            // 文件黑名单过滤（对齐前端 isSkipFile：gitignore 语法 + 隐藏/临时文件）
            if !ignore.is_kb_file_allowed(&name, &rel) {
                continue;
            }
            if pattern.is_empty() || rel.to_lowercase().contains(pattern) {
                if let Ok(meta) = path.metadata() {
                    out.push((rel, meta.len()));
                }
            }
        }
    }
}

// ─────────────────────────── Git 状态工具（只读） ───────────────────────────

/// 查询知识库所在 Git 仓库的工作区状态（只读，不支持提交等写操作）。
pub async fn git_status(cfg: &KbSearchConfig) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&cfg.dir_path)
        .arg("-c")
        .arg("core.quotepath=false")
        .arg("status")
        .arg("--short")
        .output()
        .map_err(|e| format!("git 执行失败（可能未安装 git）: {}", e))?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Ok(format!("Git 状态查询失败（可能不是 Git 仓库）: {}", err));
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let total = text.lines().count();
    if total == 0 {
        return Ok("Git 工作区干净，当前无任何改动。".into());
    }
    let head: Vec<&str> = text.lines().take(200).collect();
    Ok(format!("Git 状态（共 {total} 项改动）：\n{}", head.join("\n")))
}

// ─────────────────────────── 工具构建 ───────────────────────────

fn tool_error(tool: &str, msg: &str) -> ToolExecutionError {
    ToolExecutionError::other(format!("{tool} 执行失败: {msg}"))
        .with_model_output(ToolOutput::text(msg.to_string()))
}

/// 构建 read_file 工具：读取知识库内文本文件（相对路径，最大 8K 字符）。
pub fn build_read_file_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "read_file",
        "读取知识库目录下的一个文本文件（相对路径，如 docs/note.md），最大返回前 8192 字符。当需要查看某个笔记、文档或代码文件的完整内容时调用。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "rel_path": {
                    "type": "string",
                    "description": "文件在知识库根目录下的相对路径，如 docs/note.md"
                }
            },
            "required": ["rel_path"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let rel = args
                    .get("rel_path")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if rel.is_empty() {
                    return Err(ToolExecutionError::other("文件路径为空")
                        .with_model_output(ToolOutput::text("请提供要读取的文件相对路径")));
                }
                record_tool_call(&cfg, "read_file", &rel);
                match read_file(&cfg, &rel).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "read_file", true, &format!("{} 字符", text.chars().count()));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "read_file", false, &e);
                        Err(tool_error("read_file", &e))
                    }
                }
            })
        },
    )
}

/// 构建 list_files 工具：列举知识库目录下的文件（支持子串匹配，最多 60 项）。
pub fn build_list_files_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "list_files",
        "列举知识库目录下的文件与子目录（返回相对路径与大小），支持按名称子串过滤，最多返回 60 项。当需要了解知识库目录结构、或不确定文件路径时调用。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "文件名子串过滤条件（不区分大小写），为空则列出全部"
                },
                "max_items": {
                    "type": "integer",
                    "description": "最多返回条数，默认 30，上限 60"
                }
            },
            "required": []
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let pattern = args
                    .get("pattern")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let max_items = args
                    .get("max_items")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32)
                    .unwrap_or(30);
                let preview = if pattern.is_empty() { "全部".to_string() } else { pattern.clone() };
                record_tool_call(&cfg, "list_files", &preview);
                match list_files(&cfg, &pattern, max_items).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "list_files", true, &format!("{} 项", text.lines().count().saturating_sub(1)));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "list_files", false, &e);
                        Err(tool_error("list_files", &e))
                    }
                }
            })
        },
    )
}

/// 构建 git_status 工具：查询知识库所在 Git 仓库的工作区状态（只读）。
pub fn build_git_status_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "git_status",
        "查询知识库所在 Git 仓库的工作区状态（已修改/新增/删除文件列表）。当问题涉及文件变更、最近编辑内容、或需要了解当前仓库改动时调用。只读操作，不会修改仓库。",
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
        move |_ctx: &mut ToolContext, _args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                record_tool_call(&cfg, "git_status", "");
                match git_status(&cfg).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "git_status", true, &format!("{} 行", text.lines().count()));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "git_status", false, &e);
                        Err(tool_error("git_status", &e))
                    }
                }
            })
        },
    )
}
