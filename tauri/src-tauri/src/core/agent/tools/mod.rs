//! Agent 内置工具集：文件读取/编辑/删除、目录列举、Git 状态查询、技能参考读取。
//!
//! 读写协议对齐 Codex / Claude Code：`read` 只读（含技能参考文档），
//! `edit` / `delete` 写操作被严格限制在打开目录内（并排除 `.mdgo` 内部数据）。
//!
//! 所有工具调用都会实时写入 [`ToolCallBus`]，由 commands 层转发为
//! `agent:tool_call` / `agent:tool_result` 事件，前端据此渲染调用轨迹卡片。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use rig_agent::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use serde::Serialize;

use crate::core::agent::KbSearchConfig;
use crate::core::db::utils::IgnoreMatcher;
use crate::core::skill::activation::ActiveSkillState;
use crate::core::skill::SkillRegistry;

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
    /// 触发该工具调用的技能 ID（格式：scope:skill_id），用于前端显示技能来源
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_id: Option<String>,
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

    fn record_call(&self, request_id: &str, tool: &str, args_preview: &str, skill_id: Option<&str>) {
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
                skill_id: skill_id.map(|s| s.to_string()),
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
            // 从配对的 call 事件中继承 skill_id
            let skill_id = events
                .iter()
                .find(|e| e.seq == call_seq && e.kind == "call")
                .and_then(|e| e.skill_id.clone());
            events.push(ToolCallEvent {
                seq,
                kind: "result".into(),
                tool: tool.into(),
                args_preview: String::new(),
                ok,
                summary: summary.into(),
                call_seq,
                skill_id,
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
///
/// 技能来源 `skill_id` 动态解析：优先取「已激活技能中声明了该工具」的技能，
/// 同时覆盖预激活与 LLM 激活两条路径；无声明时回退到预激活主技能（`cfg.skill_id`）。
pub fn record_tool_call(cfg: &KbSearchConfig, tool: &str, args_preview: &str) {
    let skill_id = cfg
        .skill_state
        .activated()
        .iter()
        .find(|s| s.tools.iter().any(|t| t == tool))
        .map(|s| format!("{}:{}", s.scope.as_str(), s.id))
        .or_else(|| cfg.skill_id.clone());
    tool_call_bus().record_call(&cfg.request_id, tool, args_preview, skill_id.as_deref());
}

/// 记录工具调用结果（供命令层转发 `agent:tool_result`）。
pub fn record_tool_result(cfg: &KbSearchConfig, tool: &str, ok: bool, summary: &str) {
    tool_call_bus().record_result(&cfg.request_id, tool, ok, summary);
}

// ─────────────────────────── 文件读取工具 ───────────────────────────

/// 单次读取上限（避免大文件撑爆模型上下文）
const MAX_FILE_READ_CHARS: usize = 8192;
/// 目录列举上限
const MAX_LIST_ITEMS: usize = 60;

// ─────────────────────────── 重复调用熔断（Loop Guard） ───────────────────────────

/// 单个 run 的 (工具名, 规范化参数) 连续调用记录，按 run_id 隔离。
struct LoopGuardEntry {
    calls: Vec<(String, String)>,
    updated_at: Instant,
}

static LOOP_GUARD: OnceLock<Mutex<HashMap<String, LoopGuardEntry>>> = OnceLock::new();

fn loop_guard() -> &'static Mutex<HashMap<String, LoopGuardEntry>> {
    LOOP_GUARD.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 将工具参数 JSON 规范化（对象键排序后扁平拼接），
/// 避免模型调整键顺序导致的误判。
fn canonical_args(args: &str) -> String {
    fn canon(v: &serde_json::Value) -> String {
        match v {
            serde_json::Value::Object(map) => {
                let mut entries: Vec<(String, String)> = map
                    .iter()
                    .map(|(k, val)| (k.clone(), canon(val)))
                    .collect();
                entries.sort();
                entries
                    .iter()
                    .map(|(k, val)| format!("{k}={val}"))
                    .collect::<Vec<_>>()
                    .join("&")
            }
            _ => v.to_string(),
        }
    }
    match serde_json::from_str(args) {
        Ok(v) => canon(&v),
        Err(_) => args.to_string(),
    }
}

/// run 内防重复调用（Hook 层在工具执行前调用）。
///
/// 检测「连续相同 (工具, 规范化参数)」的已执行次数：同一调用连续出现 ≥2 次后，
/// 第 3 次起返回熔断提示（`None` = 放行）。只统计连续重复，因此
/// `read(A) → edit(A) → read(A)` 这类「改后再读」不会被误判。
pub fn guard_duplicate_call(run_id: &str, tool: &str, args: &str) -> Option<String> {
    let key = canonical_args(args);
    let mut map = loop_guard().lock().unwrap_or_else(|e| e.into_inner());
    // 定期清理过期 run 记录，防止长时间运行内存膨胀
    if map.len() > 64 {
        let cutoff = Instant::now() - Duration::from_secs(60);
        map.retain(|_, e| e.updated_at >= cutoff);
    }
    let entry = map.entry(run_id.to_string()).or_insert_with(|| LoopGuardEntry {
        calls: Vec::new(),
        updated_at: Instant::now(),
    });
    entry.updated_at = Instant::now();

    let mut streak = 0;
    for (t, k) in entry.calls.iter().rev() {
        if t == tool && k == &key {
            streak += 1;
        } else {
            break;
        }
    }
    if streak >= 2 {
        return Some(format!(
            "防重复调用：'{}' 已使用相同参数（{}）连续调用过 {} 次，重复执行只会得到相同结果。请更换参数或改用其他策略（如 read 可指定 offset 分页续读），或基于已有信息直接给出答案，不要再次重复相同调用。",
            tool, key, streak
        ));
    }
    entry.calls.push((tool.to_string(), key));
    None
}

// ─────────────────────────── 文件列表缓存 ───────────────────────────

/// 缓存的文件列表快照
struct FileListSnapshot {
    /// (relative_path, file_size)，已按路径排序，已通过 ignore 过滤
    entries: Vec<(String, u64)>,
    updated_at: Instant,
}

/// 全局文件列表缓存，按目录路径索引
static FILE_LIST_CACHE: OnceLock<RwLock<HashMap<String, FileListSnapshot>>> = OnceLock::new();

/// 缓存 TTL：30 分钟后自动刷新（watcher 会在文件变更时主动失效，TTL 仅为兜底）
const CACHE_TTL: Duration = Duration::from_secs(30 * 60);
/// 目录遍历深度上限（防止深层嵌套导致栈溢出）
const WALK_MAX_DEPTH: usize = 10;

/// 获取缓存管理器
fn file_list_cache() -> &'static RwLock<HashMap<String, FileListSnapshot>> {
    FILE_LIST_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 失效指定目录的缓存（供文件监视器在文件变更时调用）
///
/// 内部会 canonicalize 路径，与缓存 key 保持一致。
pub fn invalidate_file_list_cache(dir_path: &str) {
    let canonical = std::fs::canonicalize(dir_path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| dir_path.to_string());
    if let Some(cache) = FILE_LIST_CACHE.get() {
        if let Ok(mut map) = cache.write() {
            map.remove(&canonical);
        }
    }
}

/// 获取或刷新文件列表缓存（读锁检查 → 写锁 double-check → 刷新）
///
/// 缓存 key 使用 canonicalized 路径，确保符号链接、大小写变体共享同一份缓存。
fn get_or_refresh_cache(
    dir_path: &str,
    dir_blacklist: &[String],
    file_blacklist: &[String],
) -> Result<Vec<(String, u64)>, String> {
    let base = std::fs::canonicalize(dir_path)
        .map_err(|e| format!("无法访问知识库目录: {}", e))?;
    let cache_key = base.to_string_lossy().to_string();

    let cache = file_list_cache();

    // 快速路径：读锁检查缓存是否有效
    {
        let map = cache.read().unwrap_or_else(|e| e.into_inner());
        if let Some(snapshot) = map.get(&cache_key) {
            if snapshot.updated_at.elapsed() < CACHE_TTL {
                return Ok(snapshot.entries.clone());
            }
        }
    }

    // 慢路径：获取写锁后 double-check（避免多线程重复刷新）
    let mut map = cache.write().unwrap_or_else(|e| e.into_inner());
    if let Some(snapshot) = map.get(&cache_key) {
        if snapshot.updated_at.elapsed() < CACHE_TTL {
            return Ok(snapshot.entries.clone());
        }
    }

    let ignore = IgnoreMatcher::new(dir_blacklist, file_blacklist);

    let mut entries: Vec<(String, u64)> = Vec::new();
    walk_dir_all(&base, &base, &ignore, 0, &mut entries);
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    map.insert(cache_key, FileListSnapshot {
        entries: entries.clone(),
        updated_at: Instant::now(),
    });

    Ok(entries)
}

/// 遍历目录收集所有文件（无数量限制，已通过 ignore 过滤，深度上限 WALK_MAX_DEPTH）
fn walk_dir_all(
    base: &PathBuf,
    dir: &PathBuf,
    ignore: &IgnoreMatcher,
    depth: usize,
    out: &mut Vec<(String, u64)>,
) {
    if depth > WALK_MAX_DEPTH {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if path.is_dir() {
            if !ignore.is_kb_dir_allowed(&name, &rel) {
                continue;
            }
            out.push((format!("{rel}/"), 0));
            walk_dir_all(base, &path, ignore, depth + 1, out);
        } else {
            if !ignore.is_kb_file_allowed(&name, &rel) {
                continue;
            }
            if let Ok(meta) = path.metadata() {
                out.push((rel, meta.len()));
            }
        }
    }
}

// ─────────────────────────── 文件列表工具 ───────────────────────────

/// 将相对路径安全解析到指定根目录内（防路径穿越）。
fn safe_resolve_in(base_dir: &Path, rel: &str) -> Result<PathBuf, String> {
    let base = std::fs::canonicalize(base_dir)
        .map_err(|e| format!("无法访问目录: {}", e))?;
    let full = std::fs::canonicalize(base.join(rel))
        .map_err(|e| format!("文件不存在: {}", e))?;
    if !full.starts_with(&base) {
        return Err("路径越界：仅允许访问限定目录内的文件".into());
    }
    Ok(full)
}

/// 将相对路径安全解析到知识库根目录内（防路径穿越）。
fn safe_resolve(dir_path: &str, rel: &str) -> Result<PathBuf, String> {
    safe_resolve_in(Path::new(dir_path), rel)
}

/// 读取已解析路径的文本内容（目录拒绝、超长分页）。
///
/// `offset` 为字符偏移（从 0 开始）：返回 `[offset, offset + MAX_FILE_READ_CHARS)` 区间的内容。
/// 文件仍有后续内容时，截断提示会给出总字符数与下一次读取的 offset，供模型分页续读，
/// 避免模型为读取长文件剩余部分而反复从头重读（浪费多轮工具调用）。
fn read_text(full: &Path, display: &str, offset: usize) -> Result<String, String> {
    let meta = std::fs::metadata(full).map_err(|e| format!("读取文件信息失败: {}", e))?;
    if meta.is_dir() {
        return Err(format!("{} 是目录，请改用 list_files 查看目录内容", display));
    }
    let data = std::fs::read(full).map_err(|e| format!("读取文件失败: {}", e))?;
    let text = String::from_utf8_lossy(&data).into_owned();
    let total = text.chars().count();
    if offset >= total {
        return Ok(format!("[已达文件末尾（共 {total} 字符），offset={offset} 超出范围]"));
    }
    let chunk: String = text.chars().skip(offset).take(MAX_FILE_READ_CHARS).collect();
    if offset + MAX_FILE_READ_CHARS >= total {
        Ok(chunk)
    } else {
        Ok(format!(
            "{chunk}\n\n[内容过长：已显示第 {}~{} 字符（共 {total} 字符）。可再次调用 read 并指定 offset={} 读取后续内容]",
            offset + 1,
            offset + chunk.chars().count(),
            offset + MAX_FILE_READ_CHARS
        ))
    }
}

/// 读取知识库（当前打开目录）内文件或当前激活技能的参考文档（渐进式披露 L3）。
///
/// `offset` 为字符偏移（从 0 开始，长文件分页续读用，见 [`read_text`]）。
///
/// 解析顺序：
/// 1. 知识库目录内的相对路径（如 `docs/note.md`）
/// 2. 当前激活技能目录下的相对路径（如 `references/flowchart.md`），
///    按激活技能逐一尝试；技能基础目录由 `cfg.skill_bases` 提供，仅限已激活技能
pub async fn read(cfg: &KbSearchConfig, rel_path: &str, offset: usize) -> Result<String, String> {
    match safe_resolve(&cfg.dir_path, rel_path) {
        Ok(full) => return read_text(&full, rel_path, offset),
        Err(e) if cfg.skill_state.activated().is_empty() => {
            // 无任何激活技能：若目标是技能参考路径，明确指出需先激活技能，
            // 避免模型误以为文件不存在而反复尝试（浪费多轮工具调用）
            return Err(skill_ref_hint(rel_path, e));
        }
        Err(_) => {}
    }
    let mut last_err = "文件不存在（知识库内与已激活技能的参考目录均未找到）".to_string();
    for skill in cfg.skill_state.activated() {
        for (scope, base) in &cfg.skill_bases {
            if scope != skill.scope.as_str() {
                continue;
            }
            let dir = Path::new(base).join(&skill.id);
            match safe_resolve_in(&dir, rel_path) {
                Ok(full) => return read_text(&full, rel_path, offset),
                Err(e) => last_err = e,
            }
        }
    }
    Err(skill_ref_hint(rel_path, last_err))
}

/// 当读取路径指向技能参考文档（`references/` 开头）而解析失败时，
/// 在原错误信息后追加「需先 activate_skill 激活对应技能」的引导。
fn skill_ref_hint(rel_path: &str, base_err: String) -> String {
    if rel_path.starts_with("references/") {
        format!(
            "{}。提示：references/ 开头的路径是技能参考文档，需先调用 activate_skill 激活对应技能后才能读取。",
            base_err
        )
    } else {
        base_err
    }
}

pub async fn list_files(cfg: &KbSearchConfig, pattern: &str, max_items: u32) -> Result<String, String> {
    let entries = get_or_refresh_cache(&cfg.dir_path, &cfg.dir_blacklist, &cfg.file_blacklist)?;
    let pattern = pattern.trim().to_lowercase();
    let max = (max_items as usize).clamp(1, MAX_LIST_ITEMS);

    let matched: Vec<&(String, u64)> = if pattern.is_empty() {
        entries.iter().take(max).collect()
    } else {
        entries
            .iter()
            .filter(|(rel, _)| rel.to_lowercase().contains(&pattern))
            .take(max)
            .collect()
    };

    if matched.is_empty() {
        return Ok(format!(
            "目录中未找到匹配的文件（模式：{}）",
            if pattern.is_empty() { "全部" } else { &pattern }
        ));
    }
    let lines: Vec<String> = matched
        .iter()
        .map(|(rel, size)| format!("{rel}  ({} 字节)", size))
        .collect();
    Ok(format!("共 {} 项：\n{}", lines.len(), lines.join("\n")))
}

// ─────────────────────────── 内容搜索工具（grep） ───────────────────────────

/// 单次搜索最多返回的命中文件数
const MAX_GREP_FILES: usize = 20;
/// 单文件最多返回的匹配行数
const MAX_GREP_LINES_PER_FILE: usize = 10;
/// 匹配行最大显示长度（超长截断）
const MAX_GREP_LINE_CHARS: usize = 200;
/// 参与搜索的文件大小上限（跳过超大文件，避免拖慢工具调用）
const MAX_GREP_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// 在知识库目录内所有文本文件中搜索关键词（大小写不敏感子串匹配），
/// 返回 `文件路径:行号:匹配行`，供模型先定位再精读（配合 read + offset）。
pub async fn grep_files(cfg: &KbSearchConfig, pattern: &str, max_files: u32) -> Result<String, String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Err("搜索关键词为空，请提供 pattern 参数".to_string());
    }
    if pattern.chars().count() < 2 {
        return Err("搜索关键词过短（至少 2 个字符），请提供更具体的关键词".to_string());
    }
    let needle = pattern.to_lowercase();
    let entries = get_or_refresh_cache(&cfg.dir_path, &cfg.dir_blacklist, &cfg.file_blacklist)?;
    let limit = (max_files as usize).clamp(1, MAX_GREP_FILES);

    let mut hits: Vec<(String, Vec<(usize, String)>)> = Vec::new();
    for (rel, size) in entries {
        if hits.len() >= limit {
            break;
        }
        // 跳过超大文件（文本文件通常远小于此阈值）
        if size > MAX_GREP_FILE_BYTES {
            continue;
        }
        let full = match safe_resolve(&cfg.dir_path, &rel) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let Ok(data) = std::fs::read(&full) else { continue };
        // 含 NUL 字节视为二进制文件，跳过
        if data.contains(&0) {
            continue;
        }
        let text = String::from_utf8_lossy(&data);
        let mut matches: Vec<(usize, String)> = Vec::new();
        for (idx, line) in text.lines().enumerate() {
            if line.to_lowercase().contains(&needle) {
                let display: String = line.chars().take(MAX_GREP_LINE_CHARS).collect();
                matches.push((idx + 1, display));
                if matches.len() >= MAX_GREP_LINES_PER_FILE {
                    break;
                }
            }
        }
        if !matches.is_empty() {
            hits.push((rel, matches));
        }
    }

    if hits.is_empty() {
        return Ok(format!("未找到包含“{}”的文件。", pattern));
    }
    let mut out = format!("搜索“{}”命中 {} 个文件：\n", pattern, hits.len());
    for (rel, matches) in hits {
        out.push_str(&format!("\n{rel}\n"));
        for (no, line) in matches {
            out.push_str(&format!("  {no}: {line}\n"));
        }
    }
    Ok(out)
}

/// 构建 grep 工具：在知识库内搜索文件内容。
pub fn build_grep_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "grep",
        "在知识库目录内的所有文本文件中搜索指定关键词（大小写不敏感子串匹配），返回\"文件路径:行号:匹配行\"。当需要定位某个术语/函数/配置出现在哪些文件、或不想整读大文件时调用；典型用法是先用 grep 定位，再用 read 工具精读相关行（read 支持 offset 分页）。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "要搜索的关键词或短语（至少 2 个字符），大小写不敏感"
                },
                "max_files": {
                    "type": "integer",
                    "description": "最多返回命中文件数，默认 10，最大 20"
                }
            },
            "required": ["pattern"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let pattern = args
                    .get("pattern")
                    .and_then(|p| p.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if pattern.is_empty() {
                    return Err(tool_error("grep", "搜索关键词为空，请提供 pattern 参数"));
                }
                let max_files = args
                    .get("max_files")
                    .and_then(|m| m.as_u64())
                    .map(|v| v as u32)
                    .unwrap_or(10);
                record_tool_call(&cfg, "grep", &pattern);
                match grep_files(&cfg, &pattern, max_files).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "grep", true, &format!("{} 字符", text.chars().count()));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "grep", false, &e);
                        Err(tool_error("grep", &e))
                    }
                }
            })
        },
    )
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

// ─────────────────────────── 文件编辑/删除工具（限打开目录） ───────────────────────────

/// 判断相对路径是否指向 `.mdgo` 内部数据（配置/技能/索引，禁止编辑/删除）。
fn is_mdgo_internal(rel: &str) -> bool {
    let norm = rel.trim_start_matches(['/', '\\']);
    norm.eq_ignore_ascii_case(".mdgo")
        || norm.starts_with(".mdgo/")
        || norm.starts_with(".mdgo\\")
}

/// 编辑知识库（当前打开目录）内文本文件：将唯一匹配的 old_string 精确替换为 new_string。
///
/// 安全边界：路径经 `safe_resolve` 限制在打开目录内，且拒绝 `.mdgo` 内部数据。
pub async fn edit_file(
    cfg: &KbSearchConfig,
    rel_path: &str,
    old_string: &str,
    new_string: &str,
) -> Result<String, String> {
    if old_string.is_empty() {
        return Err("old_string 不能为空".into());
    }
    if is_mdgo_internal(rel_path) {
        return Err(".mdgo 为应用内部数据目录（配置/技能/索引），不允许编辑".into());
    }
    let full = safe_resolve(&cfg.dir_path, rel_path)?;
    let meta = std::fs::metadata(&full).map_err(|e| format!("读取文件信息失败: {}", e))?;
    if meta.is_dir() {
        return Err(format!("{} 是目录，仅支持编辑文本文件", rel_path));
    }
    if meta.len() > 1024 * 1024 {
        return Err(format!("{} 超过 1MB，请改用其他方式编辑", rel_path));
    }
    let data = std::fs::read(&full).map_err(|e| format!("读取文件失败: {}", e))?;
    let content = String::from_utf8_lossy(&data).into_owned();
    let occurrences: Vec<usize> = content.match_indices(old_string).map(|(i, _)| i).collect();
    match occurrences.len() {
        0 => Err("未在文件中找到与 old_string 完全匹配的内容，请先使用 read 读取文件确认原文（注意换行符、空格、大小写需完全一致）".into()),
        1 => {
            let start = occurrences[0];
            let mut new_content = String::with_capacity(content.len() + new_string.len());
            new_content.push_str(&content[..start]);
            new_content.push_str(new_string);
            new_content.push_str(&content[start + old_string.len()..]);
            std::fs::write(&full, new_content.as_bytes())
                .map_err(|e| format!("写入文件失败: {}", e))?;
            Ok(format!(
                "已更新 {}：替换 1 处（{} 字符 → {} 字符）",
                rel_path,
                old_string.chars().count(),
                new_string.chars().count()
            ))
        }
        n => Err(format!(
            "old_string 在文件中出现 {} 次，请提供更长的上下文使其唯一匹配",
            n
        )),
    }
}

/// 删除知识库（当前打开目录）内的一个文件（不可恢复）。
///
/// 安全边界：路径经 `safe_resolve` 限制在打开目录内，且拒绝 `.mdgo` 内部数据。
pub async fn delete_file(cfg: &KbSearchConfig, rel_path: &str) -> Result<String, String> {
    if is_mdgo_internal(rel_path) {
        return Err(".mdgo 为应用内部数据目录（配置/技能/索引），不允许删除".into());
    }
    let full = safe_resolve(&cfg.dir_path, rel_path)?;
    let meta = std::fs::metadata(&full).map_err(|e| format!("读取文件信息失败: {}", e))?;
    if meta.is_dir() {
        return Err(format!("{} 是目录，delete 仅支持删除文件，不支持目录", rel_path));
    }
    std::fs::remove_file(&full).map_err(|e| format!("删除文件失败: {}", e))?;
    Ok(format!("已删除文件 {}", rel_path))
}

// ─────────────────────────── 工具构建 ───────────────────────────

fn tool_error(tool: &str, msg: &str) -> ToolExecutionError {
    ToolExecutionError::other(format!("{tool} 执行失败: {msg}"))
        .with_model_output(ToolOutput::text(msg.to_string()))
}

/// 截断长字符串（用于工具轨迹参数摘要，避免撑爆事件负载）
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

/// 构建 activate_skill 工具：模型依据 L1 技能目录自主决策激活技能（渐进式披露 L2）。
///
/// 激活后：SKILL.md 正文经 [`ActiveSkillState`] 由 SkillInstructionHook 注入后续
/// 模型调用；技能声明的检索工具（kb_search / code_lookup 等）加入可见工具；
/// 技能目录加入 read 工具的 L3 参考白名单。
pub fn build_activate_skill_tool(
    registry: Arc<SkillRegistry>,
    state: Arc<ActiveSkillState>,
) -> DynamicTool {
    DynamicTool::new(
        "activate_skill",
        "激活一个技能以加载其详细指令（SKILL.md 正文）并解锁其声明的专用工具。技能 ID 见常驻技能目录；仅当目录中的技能与当前任务明确相关时才调用。激活后：1) 该技能指令将注入后续对话；2) 其声明的检索工具（如 kb_search）将可用；3) 可用 read 工具读取其 references/ 下的参考资料。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill_id": {
                    "type": "string",
                    "description": "技能目录中的技能 ID，如 kb-search、code-lookup"
                }
            },
            "required": ["skill_id"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let registry = registry.clone();
            let state = state.clone();
            Box::pin(async move {
                let id = args
                    .get("skill_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if id.is_empty() {
                    return Err(tool_error("activate_skill", "skill_id 为空"));
                }
                let skill = registry.find_enabled(&id).ok_or_else(|| {
                    tool_error(
                        "activate_skill",
                        &format!("技能 '{}' 不存在或未启用，请从技能目录中选择", id),
                    )
                })?;
                let body_len = skill.body.trim().chars().count();
                state.activate(skill.clone());
                let mut msg = format!(
                    "技能已激活：{}（{}），其指令已注入（{} 字符）。",
                    skill.name, id, body_len
                );
                if !skill.description.trim().is_empty() {
                    msg.push_str(&format!(" 说明：{}", skill.description.trim()));
                }
                if !skill.tools.is_empty() {
                    msg.push_str(&format!(
                        " 专用工具：{}",
                        skill.tools
                            .iter()
                            .map(|t| format!("`{t}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                Ok(ToolOutput::text(msg))
            })
        },
    )
}

/// 构建 deactivate_skill 工具：释放已激活技能（停止指令注入与专用工具，渐进式披露回退）。
pub fn build_deactivate_skill_tool(state: Arc<ActiveSkillState>) -> DynamicTool {
    DynamicTool::new(
        "deactivate_skill",
        "停用一个此前已激活的技能：其指令不再注入，其声明的专用工具将不再可用。当某技能不再适用于当前任务、或需要避免多余指令干扰时调用。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill_id": {
                    "type": "string",
                    "description": "要停用的技能 ID"
                }
            },
            "required": ["skill_id"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let state = state.clone();
            Box::pin(async move {
                let id = args
                    .get("skill_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if id.is_empty() {
                    return Err(tool_error("deactivate_skill", "skill_id 为空"));
                }
                if state.deactivate(&id) {
                    Ok(ToolOutput::text(format!("技能已停用：{id}")))
                } else {
                    Err(tool_error(
                        "deactivate_skill",
                        &format!("技能 '{}' 当前未激活，无需停用", id),
                    ))
                }
            })
        },
    )
}

/// 构建 read 工具：读取知识库内文件或当前激活技能的参考文档。
pub fn build_read_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "read",
        "读取文件内容，单次最多返回 8192 字符，支持分页续读。支持两类路径：1) 知识库目录内的相对路径（如 docs/note.md，可读取打开目录中的所有文件，含子目录）；2) 当前激活技能的参考文档路径（如 references/flowchart.md，通常由技能 SKILL.md 中以相对链接给出；未激活技能时无法读取，需先 activate_skill）。当返回内容末尾提示\"内容过长\"时，内容只显示了第 1~8192 字符，若需要文件后续部分，请再次调用本工具并指定 offset 参数（如 offset=8192）继续读取，不要从头重读全文。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "文件相对路径：知识库内路径，或技能参考文档路径（如 references/flowchart.md）"
                },
                "offset": {
                    "type": "integer",
                    "description": "字符偏移量（从 0 开始），用于分页续读长文件。首次读取省略；截断提示中会给出下次应使用的 offset"
                }
            },
            "required": ["path"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let rel = args
                    .get("path")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if rel.is_empty() {
                    return Err(tool_error("read", "文件路径为空，请提供 path 参数"));
                }
                let offset = args
                    .get("offset")
                    .and_then(|o| o.as_u64())
                    .unwrap_or(0) as usize;
                let args_preview = if offset == 0 {
                    rel.clone()
                } else {
                    format!("{rel} (offset={offset})")
                };
                record_tool_call(&cfg, "read", &args_preview);
                match read(&cfg, &rel, offset).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "read", true, &format!("{} 字符", text.chars().count()));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "read", false, &e);
                        Err(tool_error("read", &e))
                    }
                }
            })
        },
    )
}

/// 构建 edit 工具：将打开目录内文件中的唯一匹配片段替换为新内容。
pub fn build_edit_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "edit",
        "编辑当前打开知识库目录内的一个文本文件：将文件中与 old_string 完全匹配且唯一出现的片段替换为 new_string。只允许操作当前打开目录内的文件，不能操作目录外的文件，也不允许修改 .mdgo 内部数据。修改前建议先用 read 读取文件确认原文。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "rel_path": {
                    "type": "string",
                    "description": "文件在知识库根目录下的相对路径，如 docs/note.md"
                },
                "old_string": {
                    "type": "string",
                    "description": "待替换的原文片段，必须与文件内容完全一致（含换行与空格），且在文件中唯一出现"
                },
                "new_string": {
                    "type": "string",
                    "description": "替换后的新内容"
                }
            },
            "required": ["rel_path", "old_string", "new_string"]
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
                let old_string = args
                    .get("old_string")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string();
                let new_string = args
                    .get("new_string")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string();
                if rel.is_empty() {
                    return Err(tool_error("edit", "rel_path 为空"));
                }
                let preview = format!(
                    "{}: {} → {}",
                    rel,
                    truncate(&old_string, 40),
                    truncate(&new_string, 40)
                );
                record_tool_call(&cfg, "edit", &preview);
                match edit_file(&cfg, &rel, &old_string, &new_string).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "edit", true, &text);
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "edit", false, &e);
                        Err(tool_error("edit", &e))
                    }
                }
            })
        },
    )
}

/// 构建 delete 工具：删除打开目录内的一个文件。
pub fn build_delete_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "delete",
        "删除当前打开知识库目录内的一个文件（不可恢复）。只允许删除当前打开目录内的文件，不能操作目录外的文件，不允许删除目录，也不允许删除 .mdgo 内部数据。删除前请确认用户意图。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "rel_path": {
                    "type": "string",
                    "description": "文件在知识库根目录下的相对路径，如 docs/old-note.md"
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
                    return Err(tool_error("delete", "rel_path 为空"));
                }
                record_tool_call(&cfg, "delete", &rel);
                match delete_file(&cfg, &rel).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "delete", true, &text);
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "delete", false, &e);
                        Err(tool_error("delete", &e))
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

