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
use tauri::Manager;

use futures_util::StreamExt;

use crate::core::agent::KbSearchConfig;
use crate::core::db::utils::IgnoreMatcher;
use crate::core::skill::activation::ActiveSkillState;
use crate::core::skill::SkillRegistry;
use crate::core::subagent::{
    SubagentMode, SubagentRunner, SubagentSpec, SUBAGENT_MAX_TURNS, SUBAGENT_SUMMARY_CHARS,
};

/// 单条工具调用事件（`kind = "call"` 或 `kind = "result"`）
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallEvent {
    pub seq: u64,
    pub kind: String,
    pub tool: String,
    /// 工具调用 ID（`call_*`）：call 事件生成，result 事件继承，
    /// 供会话历史回放时与 tool 结果消息的 `tool_call_id` 配对
    pub call_id: String,
    /// 调用时的参数摘要（模型视角）
    pub args_preview: String,
    /// 完整参数 JSON 字符串（模型原始产出，历史回放用）
    pub arguments: String,
    /// 结果是否成功
    pub ok: bool,
    /// 结果摘要（成功为返回规模，失败为错误信息）
    pub summary: String,
    /// 完整结果文本（截断到上限，历史回放用）
    pub result: String,
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
/// 全局总线跟踪的并发请求桶上限：超过后清空最旧（工具轨迹是辅助展示，丢失无害），
/// 防止异常路径（如子代理被取消）遗留的桶永久占用内存。
const MAX_TRACKED_REQUESTS: usize = 64;

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

    fn record_call(
        &self,
        request_id: &str,
        tool: &str,
        call_id: &str,
        args_preview: &str,
        arguments: &str,
        skill_id: Option<&str>,
    ) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut map) = self.map.lock() {
            // 容量治理：超过上限清空（对齐 bridge 的 MAX_PENDING 治理思想）
            if map.len() >= MAX_TRACKED_REQUESTS {
                map.clear();
            }
            map.entry(request_id.to_string()).or_default().push(ToolCallEvent {
                seq,
                kind: "call".into(),
                tool: tool.into(),
                call_id: call_id.into(),
                args_preview: args_preview.into(),
                arguments: arguments.into(),
                ok: false,
                summary: String::new(),
                result: String::new(),
                call_seq: 0,
                skill_id: skill_id.map(|s| s.to_string()),
            });
        }
    }

    fn record_result(
        &self,
        request_id: &str,
        tool: &str,
        ok: bool,
        summary: &str,
        result: &str,
    ) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut map) = self.map.lock() {
            // 容量治理：超过上限清空（轨迹是辅助展示，丢失无害）
            if map.len() >= MAX_TRACKED_REQUESTS {
                map.clear();
            }
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
            // 从配对的 call 事件继承 skill_id 与 call_id
            let (skill_id, call_id) = events
                .iter()
                .find(|e| e.seq == call_seq && e.kind == "call")
                .map(|e| (e.skill_id.clone(), e.call_id.clone()))
                .unwrap_or((None, String::new()));
            events.push(ToolCallEvent {
                seq,
                kind: "result".into(),
                tool: tool.into(),
                call_id,
                args_preview: String::new(),
                arguments: String::new(),
                ok,
                summary: summary.into(),
                result: result.into(),
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

    /// 查看最后一个成功工具调用的结果摘要（不消费，不 drain）。
    ///
    /// 用于兜底：模型调用了工具并成功返回结果，但未生成文本回复时，
    /// 将工具结果作为最终回复内容，避免空内容报错。
    pub fn peek_last_success_summary(&self, request_id: &str) -> Option<String> {
        if let Ok(map) = self.map.lock() {
            if let Some(events) = map.get(request_id) {
                return events
                    .iter()
                    .rev()
                    .find(|e| e.kind == "result" && e.ok)
                    .map(|e| e.summary.clone());
            }
        }
        None
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
/// `args` 为模型原始参数（JSON），完整序列化用于会话历史回放；
/// 调用 ID 在本处生成（`call_{uuid}`），结果事件按 call_seq 配对继承。
pub fn record_tool_call(
    cfg: &KbSearchConfig,
    tool: &str,
    args_preview: &str,
    args: Option<&serde_json::Value>,
) {
    let skill_id = cfg
        .skill_state
        .activated()
        .iter()
        .find(|s| s.tools.iter().any(|t| t == tool))
        .map(|s| format!("{}:{}", s.scope.as_str(), s.id))
        .or_else(|| cfg.skill_id.clone());
    let call_id = format!("call_{}", uuid::Uuid::new_v4());
    let arguments = args
        .map(|a| a.to_string())
        .unwrap_or_default();
    tool_call_bus().record_call(
        &cfg.request_id,
        tool,
        &call_id,
        args_preview,
        &arguments,
        skill_id.as_deref(),
    );
}

/// 记录工具调用结果（供命令层转发 `agent:tool_result`）。
///
/// `result` 为完整结果文本（成功为工具输出，失败为错误信息），
/// 截断到上限后存入事件，供会话历史回放为 tool 结果消息。
pub fn record_tool_result(cfg: &KbSearchConfig, tool: &str, ok: bool, summary: &str, result: Option<&str>) {
    const MAX_RESULT_CHARS: usize = 12_000;
    let result = result.map(|r| truncate(r, MAX_RESULT_CHARS)).unwrap_or_default();
    tool_call_bus().record_result(&cfg.request_id, tool, ok, summary, &result);
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
    /// 构建快照时的黑白名单指纹：黑名单变更后立即失效，避免复用旧过滤结果
    blacklist_fp: u64,
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

/// 由黑白名单生成指纹：任何黑名单条目的增删改都会改变指纹，
/// 使缓存快照失效并触发重建，保证 grep/list_files 始终按最新黑名单过滤。
fn blacklist_fingerprint(dir_blacklist: &[String], file_blacklist: &[String]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    let mut dirs = dir_blacklist.to_vec();
    dirs.sort();
    let mut files = file_blacklist.to_vec();
    files.sort();
    dirs.hash(&mut h);
    files.hash(&mut h);
    h.finish()
}

/// 获取或刷新文件列表缓存（读锁检查 → 写锁 double-check → 刷新）
///
/// 缓存 key 使用 canonicalized 路径，确保符号链接、大小写变体共享同一份缓存；
/// 快照携带黑白名单指纹，黑名单变更时自动重建，避免复用旧过滤结果。
fn get_or_refresh_cache(
    dir_path: &str,
    dir_blacklist: &[String],
    file_blacklist: &[String],
) -> Result<Vec<(String, u64)>, String> {
    let base = std::fs::canonicalize(dir_path)
        .map_err(|e| format!("无法访问知识库目录: {}", e))?;
    let cache_key = base.to_string_lossy().to_string();
    let fp = blacklist_fingerprint(dir_blacklist, file_blacklist);

    let cache = file_list_cache();

    // 快速路径：读锁检查缓存是否有效（TTL 内且黑名单指纹一致）
    {
        let map = cache.read().unwrap_or_else(|e| e.into_inner());
        if let Some(snapshot) = map.get(&cache_key) {
            if snapshot.updated_at.elapsed() < CACHE_TTL && snapshot.blacklist_fp == fp {
                return Ok(snapshot.entries.clone());
            }
        }
    }

    // 慢路径：获取写锁后 double-check（避免多线程重复刷新）
    let mut map = cache.write().unwrap_or_else(|e| e.into_inner());
    if let Some(snapshot) = map.get(&cache_key) {
        if snapshot.updated_at.elapsed() < CACHE_TTL && snapshot.blacklist_fp == fp {
            return Ok(snapshot.entries.clone());
        }
    }

    let ignore = IgnoreMatcher::new(dir_blacklist, file_blacklist);

    let mut entries: Vec<(String, u64)> = Vec::new();
    walk_dir_all(&base, &base, &ignore, 0, &mut entries);
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    map.insert(cache_key, FileListSnapshot {
        entries: entries.clone(),
        blacklist_fp: fp,
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
/// 单文件最多返回的匹配行数（context=0 时即最大输出行数）
const MAX_GREP_LINES_PER_FILE: usize = 10;
/// 匹配行最大显示长度（超长截断）
const MAX_GREP_LINE_CHARS: usize = 200;
/// context>0 时单文件最多输出的行数上限（含上下文行，防止输出爆炸）
const MAX_GREP_CONTEXT_OUTPUT_LINES: usize = 40;
/// 单次搜索最终文本总字符上限（超出即截断并提示缩小范围）
const MAX_GREP_OUTPUT_CHARS: usize = 60_000;
/// 参与搜索的文件大小上限（跳过超大文件，避免拖慢工具调用）
const MAX_GREP_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// 单次搜索累计扫描字节上限（超出即停止，避免大知识库下串行读全库拖慢模型轮次）
const MAX_GREP_SCAN_BYTES: u64 = 200 * 1024 * 1024;
/// glob 模式单条长度上限（超长视为无效，防滥用）
const MAX_GLOB_PATTERN_CHARS: usize = 128;

/// 解析后的搜索模式：引号包裹 → 精确连续短语；否则多关键词。
#[derive(Debug, Clone)]
struct ParsedPattern {
    /// 精确短语模式（引号包裹）：整体作为连续子串匹配，不拆词
    exact: bool,
    /// 关键词列表（已小写化）
    terms: Vec<String>,
}

/// 解析 pattern：
/// - 首尾均为双引号（长度≥2）→ 精确短语：剥离引号后整体连续匹配（如 `"fn main()"`）
/// - 否则按空白拆分为多个关键词（同时清理残缺的引号字符，提升模型传参鲁棒性）
fn parse_pattern(pattern: &str) -> ParsedPattern {
    let pattern = pattern.trim();
    if pattern.len() >= 2 && pattern.starts_with('"') && pattern.ends_with('"') {
        let inner = pattern[1..pattern.len() - 1].trim();
        if !inner.is_empty() {
            return ParsedPattern {
                exact: true,
                terms: vec![inner.to_lowercase()],
            };
        }
    }
    ParsedPattern {
        exact: false,
        terms: pattern
            .trim_matches('"')
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .collect(),
    }
}

/// 轻量 glob 匹配器：仅服务于单次 grep 的 include/exclude 临时过滤，
/// 不污染全局文件列表缓存（全局黑白名单仍由 IgnoreMatcher 负责）。
struct GlobMatcher {
    patterns: Vec<regex::Regex>,
}

impl GlobMatcher {
    fn new(patterns: &[String]) -> Self {
        let patterns = patterns
            .iter()
            .filter_map(|p| {
                let p = p.trim();
                if p.is_empty() || p.chars().count() > MAX_GLOB_PATTERN_CHARS {
                    return None;
                }
                glob_to_regex(p).and_then(|re| regex::Regex::new(&re).ok())
            })
            .collect();
        Self { patterns }
    }

    fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    fn is_match(&self, rel: &str) -> bool {
        self.patterns.iter().any(|re| re.is_match(rel))
    }
}

/// 将 glob 模式编译为正则：
/// - `*` → `[^/]*`；`?` → `[^/]`；`**` → `.*`；`**/` → `(?:.*/)?`
/// - 含 `/` 或 `/` 开头 → 锚定根目录；不含 `/` → 匹配任意层级下的 basename
/// - 以 `/**` 结尾 → 目录全包含语义（目录本身及其下全部文件）
/// - 无通配符的裸名（如 `src`、`target/`）→ 自动展开为子树语义 `src/**`，
///   与 IgnoreMatcher 的目录规则对齐：既匹配条目本身，也匹配其下全部文件，
///   避免 `include:["src"]` 静默零命中、`exclude:["target"]` 漏排目录树
fn glob_to_regex(pat: &str) -> Option<String> {
    let mut p = pat;
    let dir_all = p.ends_with("/**");
    if dir_all {
        p = &p[..p.len() - 3];
    }
    // 以 `/` 结尾（如 "target/"）→ 目录语义
    let trailing_slash = p.ends_with('/') && p.len() > 1;
    if trailing_slash {
        p = &p[..p.len() - 1];
    }
    if p.is_empty() {
        return None;
    }
    let anchored = p.starts_with('/');
    if anchored {
        p = &p[1..];
    }
    let has_slash = p.contains('/');
    // 裸名（无通配符、无斜杠）或目录写法 → 展开为"条目本身 + 其下全部文件"
    let bare_name = !p.contains('*') && !p.contains('?') && !has_slash;
    let expand_tree = dir_all || trailing_slash || bare_name;

    let mut re = String::new();
    if anchored || has_slash {
        re.push('^');
    } else {
        re.push_str("(?:^|.*/)"); // 无斜杠 → 匹配任意层级下的同名 basename
    }
    let mut chars = p.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '{' | '}' | '[' | ']' | '|' | '^' | '$' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(c),
        }
    }
    if expand_tree {
        re.push_str("(?:/.*)?");
    }
    re.push('$');
    Some(re)
}

/// 扫描单个文件内容，返回 (文件是否命中, 输出行列表)。
///
/// - 文件级规则：AND 模式要求全部关键词出现在文件中（可不同行）；OR 模式任一关键词出现即命中；
///   精确短语模式要求短语连续出现。
/// - 行级展示：展示包含任一关键词/短语的行；`context>0` 时附带上下文行并用 `>` 标记命中行。
/// - `list_only=true` 只判定命中不生成行输出。
fn scan_content(
    text: &str,
    parsed: &ParsedPattern,
    mode_and: bool,
    context: usize,
    list_only: bool,
) -> (bool, Vec<String>) {
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    let lower_lines: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();

    // 行级命中 = 包含任一关键词（精确短语时即短语本身）
    let mut line_hit = vec![false; n];
    let mut any_hit = false;
    for (i, lower) in lower_lines.iter().enumerate() {
        let hit = parsed.terms.iter().any(|t| lower.contains(t.as_str()));
        line_hit[i] = hit;
        any_hit |= hit;
    }
    if !any_hit {
        return (false, Vec::new());
    }
    // 文件级 AND：全部关键词都出现在文件中（可在不同行）
    if !parsed.exact && mode_and {
        let all_present = parsed
            .terms
            .iter()
            .all(|t| lower_lines.iter().any(|l| l.contains(t.as_str())));
        if !all_present {
            return (false, Vec::new());
        }
    }
    if list_only {
        return (true, Vec::new());
    }

    // 命中行总数（用于超限提示）
    let total_matched = line_hit.iter().filter(|b| **b).count();
    // 命中行号（最多 MAX_GREP_LINES_PER_FILE 个，与旧行为一致）：
    // AND 多关键词模式优先展示包含"最稀有词"的行，避免稀有词所在行被前面大量
    // 命中行挤掉，导致模型误判文件未含该词（文件级命中本身是正确的）。
    let match_idxs: Vec<usize> = if !parsed.exact && mode_and && parsed.terms.len() > 1 {
        let counts: Vec<usize> = parsed
            .terms
            .iter()
            .map(|t| lower_lines.iter().filter(|l| l.contains(t.as_str())).count())
            .collect();
        let rare_term = &parsed.terms
            [counts.iter().enumerate().min_by_key(|(_, c)| **c).map(|(i, _)| i).unwrap_or(0)];
        let mut picked: Vec<usize> = Vec::new();
        let mut picked_set = vec![false; n];
        // 第一优先：包含稀有词的行（保持行序）
        for i in 0..n {
            if line_hit[i] && lower_lines[i].contains(rare_term.as_str()) {
                picked.push(i);
                picked_set[i] = true;
            }
        }
        // 补齐其余命中行（保持行序，上限 MAX_GREP_LINES_PER_FILE）
        for i in 0..n {
            if picked.len() >= MAX_GREP_LINES_PER_FILE {
                break;
            }
            if line_hit[i] && !picked_set[i] {
                picked.push(i);
                picked_set[i] = true;
            }
        }
        picked
    } else {
        (0..n)
            .filter(|&i| line_hit[i])
            .take(MAX_GREP_LINES_PER_FILE)
            .collect()
    };
    if match_idxs.is_empty() {
        return (true, Vec::new());
    }

    // 构建输出窗口（context=0 只含命中行；>0 为命中行 ±context）
    let mut include = vec![false; n];
    for &i in &match_idxs {
        if context == 0 {
            include[i] = true;
        } else {
            let lo = i.saturating_sub(context);
            let hi = (i + context + 1).min(n);
            for j in lo..hi {
                include[j] = true;
            }
        }
    }

    let mut out: Vec<String> = Vec::new();
    if context == 0 {
        // 兼容旧输出格式：`  行号: 内容`
        for i in 0..n {
            if include[i] {
                let display: String = lines[i].chars().take(MAX_GREP_LINE_CHARS).collect();
                out.push(format!("  {}: {}", i + 1, display));
            }
        }
        if total_matched > match_idxs.len() {
            out.push(format!(
                "  ... 另有 {} 个匹配行未展示",
                total_matched - match_idxs.len()
            ));
        }
        return (true, out);
    }

    // context>0：`>` 标记命中行，空格前缀为上下文行；非连续区间用 `--` 分隔
    let mut emitted = 0usize;
    let mut prev_included = false;
    for i in 0..n {
        if !include[i] {
            prev_included = false;
            continue;
        }
        if emitted >= MAX_GREP_CONTEXT_OUTPUT_LINES {
            break;
        }
        if !prev_included && !out.is_empty() {
            out.push("  --".to_string());
        }
        let display: String = lines[i].chars().take(MAX_GREP_LINE_CHARS).collect();
        let marker = if line_hit[i] { ">" } else { " " };
        out.push(format!("{} {:>3}: {}", marker, i + 1, display));
        prev_included = true;
        emitted += 1;
    }
    if total_matched > match_idxs.len() {
        out.push(format!(
            "  -- 另有 {} 个匹配行未展示",
            total_matched - match_idxs.len()
        ));
    }
    (true, out)
}

/// 在知识库目录内所有文本文件中搜索关键词（大小写不敏感子串匹配），
/// 返回 `文件路径:行号:匹配行`，供模型先定位再精读（配合 read + offset）。
///
/// 模式：
/// - `pattern` 以空白分隔多个关键词：默认 AND（文件需同时包含全部词，可不同行），
///   `match_mode="or"` 时任一关键词出现即命中
/// - `pattern` 用双引号包裹（如 `"fn main()"`）→ 精确连续短语匹配
/// - `include`/`exclude` 用 glob 限定/排除文件（仅本次搜索生效，不污染全局缓存）
/// - `context_lines` 展示命中行前后上下文；`list_only=true` 仅返回文件名
pub async fn grep_files(
    cfg: &KbSearchConfig,
    pattern: &str,
    max_files: u32,
    include: &[String],
    exclude: &[String],
    context_lines: usize,
    match_mode: &str,
    list_only: bool,
) -> Result<String, String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Err("搜索关键词为空，请提供 pattern 参数".to_string());
    }
    if pattern.chars().count() < 2 {
        return Err("搜索关键词过短（至少 2 个字符），请提供更具体的关键词".to_string());
    }
    let parsed = parse_pattern(pattern);
    if parsed.terms.is_empty() {
        return Err("搜索关键词为空，请提供 pattern 参数".to_string());
    }
    let mode_and = match_mode != "or";
    let context = context_lines.min(5);
    let limit = (max_files as usize).clamp(1, MAX_GREP_FILES);

    // 缓存读取（冷缓存时为全量目录遍历）、候选过滤与文件匹配均为 CPU/IO 密集
    // 操作，整体移到阻塞线程执行，避免阻塞 tokio 执行线程与 agent 异步循环，
    // 也避免大知识库冷缓存时首次 grep 卡死（遍历无取消机制，绝不能跑在 async 线程上）。
    let dir_path = cfg.dir_path.clone();
    let dir_blacklist = cfg.dir_blacklist.clone();
    let file_blacklist = cfg.file_blacklist.clone();
    let include_owned = include.to_vec();
    let exclude_owned = exclude.to_vec();
    let parsed_for_search = parsed.clone();
    let (hits, truncated, skipped) = tokio::task::spawn_blocking(move || {
        let entries = get_or_refresh_cache(&dir_path, &dir_blacklist, &file_blacklist)?;
        let include_matcher = GlobMatcher::new(&include_owned);
        let exclude_matcher = GlobMatcher::new(&exclude_owned);

        // 候选文件过滤：全局缓存 → 大小上限 → 本次 include/exclude glob（内存过滤，不污染缓存）
        let candidates: Vec<(String, u64)> = entries
            .iter()
            .filter(|(rel, size)| !rel.ends_with('/') && *size <= MAX_GREP_FILE_BYTES)
            .filter(|(rel, _)| include_matcher.is_empty() || include_matcher.is_match(rel))
            .filter(|(rel, _)| !exclude_matcher.is_match(rel))
            .cloned()
            .collect();

        let mut hits: Vec<(String, Vec<String>)> = Vec::new();
        let mut scanned: u64 = 0;
        let mut truncated = false;
        let mut skipped = 0u32;
        for (rel, _) in candidates {
            if hits.len() >= limit {
                break;
            }
            if scanned >= MAX_GREP_SCAN_BYTES {
                truncated = true;
                break;
            }
            // 解析/读取失败静默跳过并计数，输出时提示，避免模型把"文件不可读"误判为"未找到"
            let full = match safe_resolve(&dir_path, &rel) {
                Ok(p) => p,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let Ok(data) = std::fs::read(&full) else {
                skipped += 1;
                continue;
            };
            scanned = scanned.saturating_add(data.len() as u64);
            if scanned >= MAX_GREP_SCAN_BYTES {
                // 已到达累计扫描上限：本文件已读入，继续匹配，但后续文件不再扫描
                truncated = true;
            }
            // 含 NUL 字节视为二进制文件，跳过
            if data.contains(&0) {
                continue;
            }
            let text = String::from_utf8_lossy(&data);
            let (matched, lines) = scan_content(&text, &parsed_for_search, mode_and, context, list_only);
            if !matched {
                continue;
            }
            if list_only {
                hits.push((rel, Vec::new()));
            } else if !lines.is_empty() {
                hits.push((rel, lines));
            }
        }
        Ok::<_, String>((hits, truncated, skipped))
    })
    .await
    .map_err(|e| format!("搜索文件内容失败: {}", e))??;

    // 读取失败提示：区分"术语不存在"与"文件不可读"，避免误导模型
    let skip_note = if skipped > 0 {
        format!("\n（注：{} 个文件读取失败被跳过，结果可能不完整）", skipped)
    } else {
        String::new()
    };

    if hits.is_empty() {
        // 精确短语未命中时剥离引号，避免文案出现嵌套引号（如 未找到包含“"fn main()"”）
        let display_pattern = if parsed.exact { pattern.trim_matches('"') } else { pattern };
        let mut msg = if parsed.terms.len() == 1 {
            format!("未找到包含“{}”的文件。", display_pattern)
        } else if mode_and {
            format!("未找到同时包含“{}”的文件。", parsed.terms.join("”和“"))
        } else {
            format!("未找到包含“{}”任一关键词的文件。", parsed.terms.join("”或“"))
        };
        if truncated {
            msg.push_str(&truncate_hint());
        }
        msg.push_str(&skip_note);
        return Ok(msg);
    }

    let mut out = format!("搜索“{}”命中 {} 个文件：\n", pattern, hits.len());
    // 上限按"字符"统计（与 MAX_GREP_OUTPUT_CHARS 语义一致），避免中文内容提前截断
    let mut chars = out.chars().count();
    let mut output_truncated = false;
    for (rel, lines) in hits {
        if list_only {
            // 仅文件名：直接换行列出
            let item = format!("{rel}\n");
            let item_chars = item.chars().count();
            if chars + item_chars > MAX_GREP_OUTPUT_CHARS {
                output_truncated = true;
                break;
            }
            chars += item_chars;
            out.push_str(&item);
        } else {
            let mut block = format!("\n{rel}\n");
            for line in &lines {
                block.push_str(line);
                block.push('\n');
            }
            let block_chars = block.chars().count();
            if chars + block_chars > MAX_GREP_OUTPUT_CHARS {
                output_truncated = true;
                break;
            }
            chars += block_chars;
            out.push_str(&block);
        }
    }
    if output_truncated {
        out.push_str(&truncate_hint());
    } else if truncated {
        out.push_str(&truncate_hint());
    }
    out.push_str(&skip_note);
    Ok(out)
}

/// 扫描字节/输出字符截断时的可执行优化建议（引导模型下一步动作）。
fn truncate_hint() -> String {
    "\n⚠️ 搜索已达到单次扫描/输出上限，结果存在截断。\n建议方案：\n1. 使用 include 参数限定文件后缀缩小扫描范围（如 include:[\"*.rs\"]）\n2. 使用更精准的关键词减少匹配范围\n3. 拆分搜索，分多次查询不同目录\n".to_string()
}

/// 解析 include/exclude 参数：兼容数组与逗号分隔字符串（模型常传错类型），
/// 避免类型不符被静默忽略导致过滤失效。
fn parse_str_list(v: &serde_json::Value) -> Vec<String> {
    match v {
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect(),
        serde_json::Value::String(s) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// 构建 grep 工具：在知识库内搜索文件内容。
///
/// 参数与使用策略对齐 Claude Code / GitHub Codex 的 grep 习惯；新增参数全部带默认值，
/// 旧调用（仅 pattern/max_files）行为保持不变。
pub fn build_grep_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "grep",
        "在知识库目录内的文本文件中搜索关键词（大小写不敏感子串匹配，跳过二进制与超大文件）。输出格式：每个命中文件先输出一行相对路径，随后每行\"  行号: 内容\"；context_lines>0 时匹配行以 \">\" 开头、上下文行以空格开头、非连续区间用 \"--\" 分隔；list_only=true 时仅输出文件名。pattern 支持多关键词（空格分隔）：默认 and 模式（文件需同时包含所有词，词可出现在不同行），可设 match_mode=\"or\"（含任一词即命中）；用双引号包裹 pattern 可精确搜索连续短语（如 pattern=\"\\\"fn main()\\\"\"）。include/exclude 支持 glob 与目录名：include:[\"*.rs\",\"*.md\"] 限定文件类型，exclude:[\"target/**\",\"dist/**\"] 排除目录，目录名（如 \"src\"）自动展开为其下全部文件。\n使用建议：\n- 快速定位哪些文件包含目标文本：list_only=true（只返回文件名，省 token）\n- 需要看懂代码片段周边逻辑：context_lines=3（返回命中行前后 3 行，最大 5）\n- 缩小搜索范围减少耗时：include:[\"*.rs\"] 或 include:[\"src\"]（目录名）\n- 搜索连续代码片段：用双引号包裹 pattern，如 pattern=\"\\\"fn handle_request(\\\"\"\n- 多个术语任选其一：match_mode=\"or\"\n定位后建议用 read 工具精读相关行（read 支持 offset 分页）。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "搜索文本（至少 2 个字符），大小写不敏感；多个词以空格分隔默认 AND 匹配（文件需同时包含所有词）；用双引号包裹（如 \"fn main()\"）开启精确连续短语匹配"
                },
                "max_files": {
                    "type": "integer",
                    "default": 10,
                    "minimum": 1,
                    "maximum": 20,
                    "description": "最多返回命中文件数，默认 10，最大 20"
                },
                "include": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "glob 包含过滤器，仅扫描匹配的文件，例：[\"*.rs\",\"*.md\"]；目录名（如 \"src\"）自动展开为该目录下全部文件；也可传逗号分隔字符串"
                },
                "exclude": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "glob 排除过滤器，例：[\"target/**\",\"dist/**\"]；目录名（如 \"target\"）自动排除整个目录树；也可传逗号分隔字符串"
                },
                "context_lines": {
                    "type": "integer",
                    "default": 0,
                    "minimum": 0,
                    "maximum": 5,
                    "description": "匹配行前后展示的上下文行数（最大 5，防止超长输出）"
                },
                "match_mode": {
                    "type": "string",
                    "enum": ["and", "or"],
                    "default": "and",
                    "description": "多关键词匹配策略：and 文件必须包含所有词；or 文件包含任意一个词"
                },
                "list_only": {
                    "type": "boolean",
                    "default": false,
                    "description": "只输出匹配的文件名称，不展示匹配行（等效 grep -l）"
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
                let include: Vec<String> = parse_str_list(args.get("include").unwrap_or(&serde_json::Value::Null));
                let exclude: Vec<String> = parse_str_list(args.get("exclude").unwrap_or(&serde_json::Value::Null));
                let context_lines = args
                    .get("context_lines")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(0);
                let match_mode = args
                    .get("match_mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("and")
                    .to_string();
                let list_only = args
                    .get("list_only")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                // 工具轨迹参数预览：pattern + 非默认的关键参数（压缩后纳入日志）
                let mut preview = pattern.clone();
                if !include.is_empty() {
                    preview.push_str(&format!(" include={}", include.join(",")));
                }
                if !exclude.is_empty() {
                    preview.push_str(&format!(" exclude={}", exclude.join(",")));
                }
                if context_lines > 0 {
                    preview.push_str(&format!(" context={}", context_lines));
                }
                if match_mode == "or" {
                    preview.push_str(" mode=or");
                }
                if list_only {
                    preview.push_str(" list_only");
                }
                record_tool_call(&cfg, "grep", &preview, Some(&args));
                match grep_files(
                    &cfg,
                    &pattern,
                    max_files,
                    &include,
                    &exclude,
                    context_lines,
                    &match_mode,
                    list_only,
                )
                .await
                {
                    Ok(text) => {
                        record_tool_result(&cfg, "grep", true, &format!("{} 字符", text.chars().count()), Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "grep", false, &e, Some(&e));
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
        "读取文件内容，单次最多返回 8192 字符，支持分页续读。支持两类路径：1) 知识库目录内的相对路径（如 docs/note.md，可读取打开目录中的所有文件，含子目录）；2) 当前激活技能的参考文档路径（如 references/flowchart.md，通常由技能 SKILL.md 中以相对链接给出；未激活技能时无法读取，需先 activate_skill）。当返回内容末尾提示\"内容过长\"时，内容只显示了第 1~8192 字符，若需要文件后续部分，请再次调用本工具并指定 offset 参数（如 offset=8192）继续读取，不要从头重读全文。如需一次读取多个文件，可用 paths 数组并行读取（最多 10 个）。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "文件相对路径：知识库内路径，或技能参考文档路径（如 references/flowchart.md）。与 paths 二选一"
                },
                "paths": {
                    "type": "array",
                    "maxItems": 10,
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
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                // P1-7：paths 多文件并行读取（独立读后按原顺序拼接）
                let paths: Vec<String> = args
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
                if !paths.is_empty() {
                    if paths.len() > 10 {
                        let e = "paths 最多 10 个文件".to_string();
                        record_tool_result(&cfg, "read", false, &e, Some(&e));
                        return Err(tool_error("read", &e));
                    }
                    let args_preview = format!("{} 个文件", paths.len());
                    record_tool_call(&cfg, "read", &args_preview, Some(&args));
                    // 并行读取（缓冲 4 并发），收集后按输入顺序拼接
                    let mut entries: Vec<(String, Result<String, String>)> = Vec::new();
                    let mut stream = futures_util::stream::iter(paths.iter().cloned())
                        .map(|p| {
                            let cfg = cfg.clone();
                            async move {
                                let out = read(&cfg, &p, 0).await;
                                (p, out.map_err(|e| e))
                            }
                        })
                        .buffer_unordered(4);
                    while let Some(entry) = stream.next().await {
                        entries.push(entry);
                    }
                    let mut out = String::new();
                    let mut failed = false;
                    for p in &paths {
                        if let Some((_, Ok(text))) = entries.iter().find(|(pp, _)| pp == p) {
                            out.push_str(&format!("===== {p} =====\n{text}\n"));
                        } else if let Some((_, Err(e))) = entries.iter().find(|(pp, _)| pp == p) {
                            failed = true;
                            out.push_str(&format!("===== {p}（读取失败）=====\n{e}\n"));
                        }
                    }
                    record_tool_result(
                        &cfg,
                        "read",
                        !failed,
                        &format!("{} 个文件", paths.len()),
                        Some(&out),
                    );
                    return Ok(ToolOutput::text(out));
                }

                let rel = args
                    .get("path")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if rel.is_empty() {
                    return Err(tool_error("read", "文件路径为空，请提供 path 或 paths 参数"));
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
                record_tool_call(&cfg, "read", &args_preview, Some(&args));
                match read(&cfg, &rel, offset).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "read", true, &format!("{} 字符", text.chars().count()), Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "read", false, &e, Some(&e));
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
                record_tool_call(&cfg, "edit", &preview, Some(&args));
                match edit_file(&cfg, &rel, &old_string, &new_string).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "edit", true, &text, Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "edit", false, &e, Some(&e));
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
                record_tool_call(&cfg, "delete", &rel, Some(&args));
                match delete_file(&cfg, &rel).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "delete", true, &text, Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "delete", false, &e, Some(&e));
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
                record_tool_call(&cfg, "list_files", &preview, Some(&args));
                match list_files(&cfg, &pattern, max_items).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "list_files", true, &format!("{} 项", text.lines().count().saturating_sub(1)), Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "list_files", false, &e, Some(&e));
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
                record_tool_call(&cfg, "git_status", "", None);
                match git_status(&cfg).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "git_status", true, &format!("{} 行", text.lines().count()), Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "git_status", false, &e, Some(&e));
                        Err(tool_error("git_status", &e))
                    }
                }
            })
        },
    )
}

// ─────────────────────────── 前端通信桥工具 ───────────────────────────

/// 通用前端桥工具构建器：Rust 工具闭包 ↔ 前端业务 handler 的标准通道。
///
/// 后续新增「与番茄钟类似的业务」（纪念日、待办、定时任务等）时，只需：
/// 1. 调用本构建器生成 [`DynamicTool`]（传 name / description / schema / default_action）
/// 2. 前端注册同名 handler（监听 `frontend_bridge:request` 事件）
/// 即可复用整套请求/响应协议，无需重复实现事件发射与结果等待逻辑（开闭原则）。
///
/// 协议细节见 [`crate::core::bridge`]。单任务语义等业务规则由前端 handler
/// 内部保证，不暴露给模型。
fn build_bridge_tool(
    cfg: KbSearchConfig,
    name: &str,
    description: &str,
    schema: serde_json::Value,
    default_action: &str,
) -> DynamicTool {
    let name_for_tool = name.to_string();
    let closure_name = name.to_string();
    let default_action = default_action.to_string();
    DynamicTool::new(
        &name_for_tool,
        description,
        schema,
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            let tool = closure_name.clone();
            let default_action = default_action.clone();
            Box::pin(async move {
                // 动作：显式指定优先，缺失/为空时回退默认动作（如 status）
                let action = args
                    .get("action")
                    .and_then(|a| a.as_str())
                    .filter(|a| !a.trim().is_empty())
                    .map(|a| a.trim().to_string())
                    .unwrap_or(default_action);
                let preview = truncate(&serde_json::to_string(&args).unwrap_or_default(), 120);
                record_tool_call(&cfg, &tool, &preview, Some(&args));
                let app_handle = cfg.app_handle.clone();
                match crate::core::bridge::request(&app_handle, &tool, &action, args).await {
                    Ok(text) => {
                        record_tool_result(&cfg, &tool, true, &truncate(&text, 200), Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, &tool, false, &truncate(&e, 200), Some(&e));
                        Err(tool_error(&tool, &e))
                    }
                }
            })
        },
    )
}

/// 构建 pomodoro 工具：控制前端番茄钟业务（定时/专注/休息/自动衔接/停止/状态查询）。
///
/// 动作与参数对齐 `resources/skills/pomodoro/SKILL.md`：
/// - `start`：开始计时，`mode` 为 `focus`（专注，默认 25 分钟）或 `break`（休息，默认 5 分钟），
///   可选 `minutes` 自定义时长（1-180）
/// - `autoBreak` / `autoFocus`：开启/关闭自动衔接，`openEnable` 布尔值
/// - `stop`：停止当前计时；`status`：查询当前运行状态
///
/// 单任务语义（每次 start 前先关闭旧任务，系统同时只存在一个定时）是
/// 前端 `PomodoroService` 的内部逻辑，不向模型暴露——模型只需声明意图，
/// 唯一性保证由业务层负责。
pub fn build_pomodoro_tool(cfg: KbSearchConfig) -> DynamicTool {
    build_bridge_tool(
        cfg,
        "pomodoro",
        "控制番茄钟（专注计时器）。动作：start 开始计时（mode=focus 专注，默认 25 分钟；mode=break 休息，默认 5 分钟；可选 minutes 自定义时长，范围 1-180）；autoBreak 开启/关闭自动开始休息（openEnable 布尔值）；autoFocus 开启/关闭自动开始专注（openEnable 布尔值）；stop 停止当前计时；status 查询当前运行状态。当用户要求定时、开始、停止、查询番茄钟或设置自动衔接时调用。",
        serde_json::json!({
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
                    "maximum": 180,
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
    )
}

#[cfg(test)]
mod grep_tests {
    use super::*;

    fn parsed(terms: &[&str]) -> ParsedPattern {
        ParsedPattern {
            exact: false,
            terms: terms.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn parse_pattern_handles_exact_phrase() {
        let p = parse_pattern("\"fn main()\"");
        assert!(p.exact);
        assert_eq!(p.terms, vec!["fn main()".to_string()]);
    }

    #[test]
    fn parse_pattern_splits_keywords() {
        let p = parse_pattern("LRU python");
        assert!(!p.exact);
        assert_eq!(p.terms, vec!["lru".to_string(), "python".to_string()]);
    }

    #[test]
    fn parse_pattern_cleans_stray_quotes() {
        let p = parse_pattern("\"LRU python");
        assert!(!p.exact);
        assert_eq!(p.terms, vec!["lru".to_string(), "python".to_string()]);
    }

    #[test]
    fn scan_and_requires_all_terms_in_file() {
        let text = "# LRU Cache\nclass LRUCache:\n    pass\n";
        let p = parsed(&["lru", "python"]);
        // 文件不包含 python → AND 不命中
        let (hit, _) = scan_content(text, &p, true, 0, false);
        assert!(!hit);

        let text2 = "# LRU cache in Python\nclass LRUCache:\n    pass\n";
        let (hit, lines) = scan_content(text2, &p, true, 0, false);
        assert!(hit);
        // 展示包含任一关键词的行（兼容旧格式 `  行号: 内容`）
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("LRU cache in Python"));
        assert!(lines[1].contains("LRUCache"));
    }

    #[test]
    fn scan_or_matches_any_term() {
        let text = "no match here\n";
        let p = parsed(&["lru", "python"]);
        let (hit, _) = scan_content(text, &p, false, 0, false);
        assert!(!hit);

        let text2 = "just python\n";
        let (hit, _) = scan_content(text2, &p, false, 0, false);
        assert!(hit);
    }

    #[test]
    fn scan_exact_phrase_requires_contiguous() {
        let p = ParsedPattern {
            exact: true,
            terms: vec!["lru cache".to_string()],
        };
        let (hit, _) = scan_content("LRU Cache is here\n", &p, true, 0, false);
        assert!(hit);
        // 单词被分隔 → 不是连续短语
        let (hit, _) = scan_content("LRU 和 Cache 分开\n", &p, true, 0, false);
        assert!(!hit);
    }

    #[test]
    fn scan_context_marks_match_lines() {
        let text = "line1\nline2\nLRU here\nline4\nline5\n";
        let p = parsed(&["lru"]);
        let (hit, lines) = scan_content(text, &p, true, 1, false);
        assert!(hit);
        // 命中行为第 3 行，窗口 = ±1 → 第 2/3/4 行（共 3 行）
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().any(|l| l.starts_with("> ")));
        assert!(lines.iter().any(|l| l.starts_with("  ")));
        assert!(lines.iter().all(|l| l.contains("line") || l.contains("LRU")));
    }

    #[test]
    fn scan_list_only_no_lines() {
        let text = "LRU here\n";
        let p = parsed(&["lru"]);
        let (hit, lines) = scan_content(text, &p, true, 0, true);
        assert!(hit);
        assert!(lines.is_empty());
    }

    #[test]
    fn glob_include_matches_ext() {
        let m = GlobMatcher::new(&["*.rs".to_string(), "*.md".to_string()]);
        assert!(m.is_match("src/main.rs"));
        assert!(m.is_match("README.md"));
        assert!(m.is_match("target/x.rs")); // *.rs 为 basename 匹配，任意层级均命中
        assert!(!m.is_match("src/main.py"));
        assert!(!m.is_match("docs/guide.txt"));
    }

    #[test]
    fn glob_exclude_dir_all() {
        let m = GlobMatcher::new(&["target/**".to_string()]);
        assert!(m.is_match("target/x.rs"));
        assert!(m.is_match("target/sub/y.rs"));
        assert!(!m.is_match("src/main.rs"));
    }

    #[test]
    fn blacklist_fingerprint_changes_with_config() {
        let empty = blacklist_fingerprint(&[], &[]);
        let with_dir = blacklist_fingerprint(&["target/**".to_string()], &[]);
        let with_file = blacklist_fingerprint(&[], &["*.log".to_string()]);
        assert_ne!(empty, with_dir);
        assert_ne!(empty, with_file);
        assert_ne!(with_dir, with_file);

        // 顺序无关：同一集合不同顺序指纹一致
        let a = blacklist_fingerprint(&["b/**".to_string(), "a/**".to_string()], &["x.txt".to_string()]);
        let b = blacklist_fingerprint(&["a/**".to_string(), "b/**".to_string()], &["x.txt".to_string()]);
        assert_eq!(a, b);
    }

    #[test]
    fn glob_double_star_any_depth() {
        let m = GlobMatcher::new(&["**/tests/**".to_string()]);
        assert!(m.is_match("src/tests/foo.rs"));
        assert!(m.is_match("tests/foo.rs"));
        assert!(!m.is_match("src/main.rs"));
    }

    #[test]
    fn glob_bare_dir_name_expands_to_subtree() {
        // include:["src"] 应匹配 src 目录树（裸目录名子树语义，避免静默零命中）
        let m = GlobMatcher::new(&["src".to_string()]);
        assert!(m.is_match("src/main.rs"));
        assert!(m.is_match("src/sub/deep.rs"));
        assert!(!m.is_match("other/main.rs"));
        assert!(!m.is_match("docs/src-note.md")); // 既不在 src 树下也不以 src 结尾

        // exclude:["target"] 应排除 target 目录树
        let m = GlobMatcher::new(&["target".to_string()]);
        assert!(m.is_match("target/debug/app"));
        assert!(m.is_match("target"));
        assert!(!m.is_match("src/main.rs"));
    }

    #[test]
    fn glob_trailing_slash_dir() {
        let m = GlobMatcher::new(&["dist/".to_string()]);
        assert!(m.is_match("dist/bundle.js"));
        assert!(!m.is_match("src/index.js"));
    }

    #[test]
    fn parse_str_list_accepts_array_and_string() {
        use serde_json::json;
        assert_eq!(
            parse_str_list(&json!(["*.rs", "*.md"])),
            vec!["*.rs".to_string(), "*.md".to_string()]
        );
        assert_eq!(
            parse_str_list(&json!("*.rs, *.md")),
            vec!["*.rs".to_string(), "*.md".to_string()]
        );
        assert_eq!(parse_str_list(&json!(123)), Vec::<String>::new());
    }

    #[test]
    fn scan_and_prefers_rare_term_line() {
        // term1 占满前 10 行、term2 只出现在第 11 行 → 展示必须包含 term2 所在行
        let mut text = String::new();
        for i in 0..10 {
            text.push_str(&format!("alpha line {}\n", i));
        }
        text.push_str("beta only here\n");
        let p = parsed(&["alpha", "beta"]);
        let (hit, lines) = scan_content(&text, &p, true, 0, false);
        assert!(hit);
        assert!(lines.iter().any(|l| l.contains("beta only here")));
        assert!(lines.iter().any(|l| l.contains("alpha line")));
        // 11 个命中行 > 展示上限 10 → 输出超限提示
        assert!(lines.iter().any(|l| l.contains("另有 1 个匹配行未展示")));
    }
}

// ─────────────────────────── 子代理深度调研工具（只读、隔离上下文） ───────────────────────────

/// 构建 deep_research 工具：派生隔离上下文的只读子代理执行深度调研。
///
/// 子代理使用独立 request_id 与只读工具子集（kb_search/code_lookup/read/grep/
/// list_files/git_status，不含 edit/delete/技能激活），不修改任何文件；
/// 返回有界摘要，完整输出经 read_subagent_result 分页读取（对齐 Reasonix
/// read_subagent_result 的"结果隔离 + 按需分页"思想）。
pub fn build_deep_research_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "deep_research",
        "派生一个隔离上下文的只读子代理进行深度调研：它可以检索知识库（kb_search）、读取与搜索文件（read/grep/list_files），适合需要阅读大量文件、跨文档总结、独立调查的任务。子代理不修改任何文件，也不共享当前对话的技能激活状态。返回有界摘要（含 subagent_id）；若需完整结果，用 read_subagent_result 指定 subagent_id 分页读取。",
        serde_json::json!({
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
        {
            let cfg = cfg.clone();
            move |_ctx: &mut ToolContext, args: serde_json::Value| {
                let cfg = cfg.clone();
                Box::pin(async move { run_deep_research(cfg, &args).await })
            }
        },
    )
}

/// 构建 read_subagent_result 工具：分页读取一次 deep_research 的完整输出。
pub fn build_read_subagent_result_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "read_subagent_result",
        "按 subagent_id 分页读取一次 deep_research 子代理调研的完整输出。offset 为字符偏移（默认 0），max_chars 控制本次读取长度（默认 8192）。首次读取可省略 offset；若返回末尾提示已截断，用上次 offset + 返回长度作为下次 offset 继续。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "subagent_id": {
                    "type": "string",
                    "description": "deep_research 返回的 subagent_id"
                },
                "offset": {
                    "type": "integer",
                    "description": "字符偏移（默认 0）",
                    "minimum": 0
                },
                "max_chars": {
                    "type": "integer",
                    "description": "本次读取最大字符数（默认 8192，最大 60000）",
                    "minimum": 1,
                    "maximum": 60000
                }
            },
            "required": ["subagent_id"]
        }),
        {
            let cfg = cfg.clone();
            move |_ctx: &mut ToolContext, args: serde_json::Value| {
                let cfg = cfg.clone();
                Box::pin(async move {
                    let id = args
                        .get("subagent_id")
                        .and_then(|s| s.as_str())
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if id.is_empty() {
                        return Err(tool_error("read_subagent_result", "subagent_id 不能为空"));
                    }
                    let offset = args
                        .get("offset")
                        .and_then(|o| o.as_u64())
                        .unwrap_or(0) as usize;
                    let max_chars = args
                        .get("max_chars")
                        .and_then(|o| o.as_u64())
                        .map(|v| v as usize)
                        .unwrap_or(8192)
                        .clamp(1, 60_000);
                    record_tool_call(&cfg, "read_subagent_result", &format!("{id} (offset={offset})"), Some(&args));
                    let state = cfg.app_handle.state::<crate::AppState>();
                    let full = match state.subagent_results.get(&id) {
                        Some(text) => text,
                        None => {
                            record_tool_result(&cfg, "read_subagent_result", false, "subagent_id 不存在或已过期", None);
                            return Err(tool_error(
                                "read_subagent_result",
                                &format!("subagent_id 不存在或已过期: {id}"),
                            ));
                        }
                    };
                    let total = full.chars().count();
                    if offset >= total {
                        record_tool_result(
                            &cfg,
                            "read_subagent_result",
                            true,
                            &format!("已达末尾 {total} 字符"),
                            None,
                        );
                        return Ok(ToolOutput::text(format!(
                            "(已读取到末尾：该调研共 {total} 字符，offset={offset} 已超出)"
                        )));
                    }
                    let slice: String = full.chars().skip(offset).take(max_chars).collect();
                    let next_offset = offset + slice.chars().count();
                    record_tool_result(
                        &cfg,
                        "read_subagent_result",
                        true,
                        &format!("{next_offset}/{total} 字符"),
                        Some(&slice),
                    );
                    let mut out = slice;
                    if next_offset < total {
                        out.push_str(&format!(
                            "\n\n…(已显示 {next_offset}/{total} 字符，继续调用请用 offset={next_offset})"
                        ));
                    }
                    Ok(ToolOutput::text(out))
                })
            }
        },
    )
}

/// 执行一次 deep_research：从 AppState 组装子代理并运行（独立 request_id + 全量入存储）。
async fn run_deep_research(
    cfg: KbSearchConfig,
    args: &serde_json::Value,
) -> Result<ToolOutput, ToolExecutionError> {
    let task = args
        .get("task")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if task.is_empty() {
        return Err(tool_error("deep_research", "task 不能为空"));
    }
    let max_turns = args
        .get("max_turns")
        .and_then(|m| m.as_u64())
        .map(|v| v as usize)
        .unwrap_or(SUBAGENT_MAX_TURNS)
        .clamp(1, 30);

    record_tool_call(
        &cfg,
        "deep_research",
        &format!("task_len={} max_turns={}", task.len(), max_turns),
        Some(args),
    );

    // 执行子代理（只读模式；独立 request_id + 全量入存储，复用公共执行器）
    let (sub_request_id, outcome) = match run_subagent_impl(
        &cfg,
        task,
        crate::core::subagent::SubagentMode::ReadOnly,
        max_turns,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            record_tool_result(&cfg, "deep_research", false, &e, Some(&e));
            return Err(tool_error("deep_research", &e));
        }
    };

    let mut out = format!(
        "子代理调研完成(subagent_id={sub_request_id}, max_turns={max_turns}, failed={})\n\n{}",
        outcome.failed, outcome.summary
    );
    if outcome.failed {
        out.push_str("\n\n提示：调研未完成，可重试或检查 LLM 配置。");
    } else {
        out.push_str("\n\n如需完整输出，调用 read_subagent_result，参数 subagent_id=\"{sub_request_id}\"。");
    }
    record_tool_result(
        &cfg,
        "deep_research",
        !outcome.failed,
        &format!("{} 字符摘要", outcome.summary.chars().count()),
        Some(&out),
    );
    Ok(ToolOutput::text(out))
}

// ─────────────────────────── 反思质量门工具（P1-8） ───────────────────────────

/// 构建 self_review 工具：反思/自我批评质量门。
///
/// 模型在产出初稿后自主调用：把「用户目标 + 初稿」交给独立审查 LLM 调用，
/// 返回结构化问题清单（P0-3 schema 校验）；无问题则答案达标，有问题则
/// 模型逐条修正后再输出最终答案。评审是独立非流式调用，不占执行轮次预算。
pub fn build_self_review_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "self_review",
        "在给出最终答案前自检：把用户目标与你的初稿交给独立审查，返回待修正问题清单。审查返回\"无问题\"时答案已达标，直接输出最终答案；返回问题列表时请逐条修正后再输出。适合长答案或多轮工具任务后使用。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "goal": { "type": "string", "description": "用户原始目标/问题（原样引用）" },
                "draft": { "type": "string", "description": "你的初稿答案（完整内容）" }
            },
            "required": ["goal", "draft"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let goal = args
                    .get("goal")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let draft = args
                    .get("draft")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                record_tool_call(
                    &cfg,
                    "self_review",
                    &format!("goal_len={} draft_len={}", goal.len(), draft.len()),
                    Some(&args),
                );
                if goal.is_empty() || draft.is_empty() {
                    let e = "goal 与 draft 均不能为空".to_string();
                    record_tool_result(&cfg, "self_review", false, &e, Some(&e));
                    return Err(tool_error("self_review", &e));
                }
                // 从 AppState 取 LLM 客户端（复用命令层缓存工厂）
                let state = cfg.app_handle.state::<crate::AppState>();
                let llm_cfg = state
                    .llm_config
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let llm = match state
                    .llm_client_for(&llm_cfg.endpoint, &llm_cfg.model, &llm_cfg.api_key)
                    .await
                {
                    Ok(client) => client,
                    Err(e) => {
                        record_tool_result(&cfg, "self_review", false, &format!("LLM 构建失败: {e}"), Some(&e));
                        return Err(tool_error("self_review", &format!("LLM 未配置或构建失败: {e}")));
                    }
                };
                let cancel = cfg
                    .cancel
                    .clone()
                    .unwrap_or_else(|| tokio_util::sync::CancellationToken::new());
                match llm.review_text(&goal, &draft, cancel).await {
                    Some(result) if result.needs_fix() => {
                        let mut out = format!(
                            "审查发现 {} 个问题，请逐条修正后输出最终答案：\n",
                            result.issues.len()
                        );
                        for (i, issue) in result.issues.iter().enumerate() {
                            out.push_str(&format!(
                                "{}. 问题：{}\n   修正建议：{}\n",
                                i + 1,
                                issue.issue,
                                issue.fix
                            ));
                        }
                        record_tool_result(&cfg, "self_review", true, &format!("{} 个问题", result.issues.len()), Some(&out));
                        Ok(ToolOutput::text(out))
                    }
                    Some(result) => {
                        let msg = format!("审查通过（verdict={}），初稿已达标，请直接输出最终答案。", result.verdict);
                        record_tool_result(&cfg, "self_review", true, "通过", Some(&msg));
                        Ok(ToolOutput::text(msg))
                    }
                    None => {
                        let msg = "审查不可用（LLM 未配置或评审失败），请自行检查初稿后输出最终答案。".to_string();
                        record_tool_result(&cfg, "self_review", false, "评审不可用", Some(&msg));
                        Ok(ToolOutput::text(msg))
                    }
                }
            })
        },
    )
}

// ─────────────────────────── 长期记忆工具（P0-2） ───────────────────────────

/// 构建 remember 工具：写入一条跨会话长期记忆。
///
/// 记忆随会话持久化（全局用户数据目录），后续请求按关键词检索注入，
/// 沉淀用户偏好、项目约定与已验证结论。
pub fn build_remember_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "remember",
        "把一条长期记忆写入跨会话存储（用户偏好、项目约定、已验证结论等），后续对话可检索引用。title 一句话概括，body 写完整事实；keywords 用空格分隔便于检索。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "记忆标题（一句话概括）" },
                "body": { "type": "string", "description": "记忆正文（完整事实/偏好/约定）" },
                "keywords": { "type": "string", "description": "检索关键词，空格分隔（可选）" },
                "scope": { "type": "string", "enum": ["project", "global"], "description": "作用域：project=当前知识库，global=全部（默认 project）" },
                "kind": { "type": "string", "enum": ["fact", "preference", "reference"], "description": "记忆类型（默认 fact）" }
            },
            "required": ["title", "body"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let preview: String = args
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .chars()
                    .take(40)
                    .collect();
                record_tool_call(&cfg, "remember", &preview, Some(&args));
                let input: crate::core::memory::MemoryInput =
                    serde_json::from_value(args).map_err(|e| tool_error("remember", &e.to_string()))?;
                let state = cfg.app_handle.state::<crate::AppState>();
                match state.memory_store.create(&input) {
                    Ok(item) => {
                        let msg = format!(
                            "已保存记忆（id={}，revision={}）：{}\n{}",
                            item.id, item.revision, item.title, item.body
                        );
                        record_tool_result(&cfg, "remember", true, &format!("id={} revision={}", item.id, item.revision), Some(&msg));
                        Ok(ToolOutput::text(msg))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "remember", false, &e, Some(&e));
                        Err(tool_error("remember", &e))
                    }
                }
            })
        },
    )
}

/// 构建 forget 工具：删除一条记忆。
pub fn build_forget_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "forget",
        "删除一条已保存的长期记忆（需要记忆 id，可用 search_memory 查询得到）。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "要删除的记忆 id" }
            },
            "required": ["id"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let id = args
                    .get("id")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                record_tool_call(&cfg, "forget", &format!("id={}", id), Some(&args));
                if id.is_empty() {
                    let e = "记忆 id 不能为空".to_string();
                    record_tool_result(&cfg, "forget", false, &e, Some(&e));
                    return Err(tool_error("forget", &e));
                }
                let state = cfg.app_handle.state::<crate::AppState>();
                match state.memory_store.delete(&id) {
                    Ok(true) => {
                        let msg = format!("已删除记忆 {id}");
                        record_tool_result(&cfg, "forget", true, &msg, Some(&msg));
                        Ok(ToolOutput::text(msg))
                    }
                    Ok(false) => {
                        let e = format!("记忆 {id} 不存在或已删除");
                        record_tool_result(&cfg, "forget", false, &e, Some(&e));
                        Err(tool_error("forget", &e))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "forget", false, &e, Some(&e));
                        Err(tool_error("forget", &e))
                    }
                }
            })
        },
    )
}

/// 构建 search_memory 工具：按关键词检索相关长期记忆（只读）。
pub fn build_search_memory_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "search_memory",
        "按关键词检索跨会话长期记忆（用户偏好、项目约定、已验证结论）。在需要回忆用户此前说过/偏好什么、或复用此前结论时调用。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "检索关键词（空格分隔多个词）" },
                "limit": { "type": "integer", "description": "最多返回条数（默认 5，最大 20）" }
            },
            "required": ["query"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
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
                record_tool_call(&cfg, "search_memory", &format!("query={} limit={}", query, limit), Some(&args));
                if query.is_empty() {
                    let e = "检索关键词不能为空".to_string();
                    record_tool_result(&cfg, "search_memory", false, &e, Some(&e));
                    return Err(tool_error("search_memory", &e));
                }
                let state = cfg.app_handle.state::<crate::AppState>();
                match state.memory_store.search(&query, limit) {
                    Ok(items) => {
                        if items.is_empty() {
                            let msg = format!("未找到与「{query}」相关的记忆");
                            record_tool_result(&cfg, "search_memory", true, &msg, Some(&msg));
                            return Ok(ToolOutput::text(msg));
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
                        record_tool_result(&cfg, "search_memory", true, &format!("{} 条", items.len()), Some(&out));
                        Ok(ToolOutput::text(out))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "search_memory", false, &e, Some(&e));
                        Err(tool_error("search_memory", &e))
                    }
                }
            })
        },
    )
}

// ─────────────────────────── 泛化子代理执行（P1-9） ───────────────────────────

/// 公共子代理执行器：从 AppState 组装 LLM 客户端与规约，构造
/// [`SubagentSpec`] 并运行 [`SubagentRunner`]，全量输出入 LRU 存储。
///
/// 返回 `(sub_request_id, outcome)`；deep_research / spawn_subagent /
/// parallel_research 共用（单一职责，避免三处重复组装逻辑）。
async fn run_subagent_impl(
    cfg: &KbSearchConfig,
    task: String,
    mode: SubagentMode,
    max_turns: usize,
) -> Result<(String, crate::core::subagent::SubagentOutcome), String> {
    let state = cfg.app_handle.state::<crate::AppState>();
    let llm_cfg = state
        .llm_config
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let llm = state
        .llm_client_for(&llm_cfg.endpoint, &llm_cfg.model, &llm_cfg.api_key)
        .await
        .map_err(|e| format!("LLM 未配置或构建失败: {e}"))?;
    let base_rules = crate::core::agent::load_agent_rules(&cfg.app_handle, "rag_agent.md");

    // 独立 request_id：子代理工具轨迹/事件与父链完全隔离
    let sub_request_id = format!("sub-{}", uuid::Uuid::new_v4());
    let spec = SubagentSpec {
        request_id: sub_request_id.clone(),
        task,
        max_turns: max_turns.clamp(1, 30),
        summary_chars: SUBAGENT_SUMMARY_CHARS,
        mode,
    };
    let outcome = SubagentRunner::run(
        llm.completion_model().clone(),
        cfg.clone(),
        state.skill_registry.clone(),
        base_rules,
        &spec,
    )
    .await;

    // 完整输出入存储（LRU 有界：最多保留 16 条，按最近访问淘汰）
    state
        .subagent_results
        .insert(sub_request_id.clone(), outcome.full_output.clone());
    Ok((sub_request_id, outcome))
}

/// 构建 spawn_subagent 工具：泛化子代理（只读调研 / 写型执行）。
///
/// 与 `deep_research` 的区别：`mode` 可指定 `write`（白名单含 edit/delete，
/// 每次写操作仍经审批门确认）；适合把独立子任务（实现/编辑）委托给子代理，
/// 或与 `parallel_research` 组合拆分任务。
pub fn build_spawn_subagent_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "spawn_subagent",
        "派生一个隔离子代理执行子任务：mode=readonly 做深度调研（白名单：检索/读/记忆检索，独立上下文，只返回有界摘要，完整输出可用 read_subagent_result 分页读取）；mode=write 可编辑/删除文件（每次写操作仍需用户确认）。适合委托独立子任务或并行拆分。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "子代理任务描述（自包含，含目标与边界）" },
                "mode": { "type": "string", "enum": ["readonly", "write"], "description": "readonly=只读调研（默认）；write=可编辑/删除文件（需用户确认）" },
                "max_turns": { "type": "integer", "minimum": 1, "maximum": 30, "description": "轮次上限（默认 12）" }
            },
            "required": ["task"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let task = args
                    .get("task")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let mode = match args.get("mode").and_then(|m| m.as_str()) {
                    Some("write") => SubagentMode::Write,
                    _ => SubagentMode::ReadOnly,
                };
                let max_turns = args
                    .get("max_turns")
                    .and_then(|m| m.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(SUBAGENT_MAX_TURNS)
                    .clamp(1, 30);
                let mode_label = if mode == SubagentMode::Write { "write" } else { "readonly" };
                record_tool_call(
                    &cfg,
                    "spawn_subagent",
                    &format!("mode={mode_label} task_len={} max_turns={}", task.len(), max_turns),
                    Some(&args),
                );
                if task.is_empty() {
                    let e = "task 不能为空".to_string();
                    record_tool_result(&cfg, "spawn_subagent", false, &e, Some(&e));
                    return Err(tool_error("spawn_subagent", &e));
                }
                match run_subagent_impl(&cfg, task, mode, max_turns).await {
                    Ok((sub_request_id, outcome)) => {
                        let mut out = format!(
                            "子代理执行完成(subagent_id={sub_request_id}, mode={mode_label}, max_turns={max_turns}, failed={})\n\n{}",
                            outcome.failed, outcome.summary
                        );
                        if outcome.failed {
                            out.push_str("\n\n提示：子代理未完成，可重试或检查 LLM 配置。");
                        } else {
                            out.push_str(&format!(
                                "\n\n如需完整输出，调用 read_subagent_result，参数 subagent_id=\"{sub_request_id}\"。"
                            ));
                        }
                        record_tool_result(
                            &cfg,
                            "spawn_subagent",
                            !outcome.failed,
                            &format!("{} 字符摘要", outcome.summary.chars().count()),
                            Some(&out),
                        );
                        Ok(ToolOutput::text(out))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "spawn_subagent", false, &e, Some(&e));
                        Err(tool_error("spawn_subagent", &e))
                    }
                }
            })
        },
    )
}

/// 构建 parallel_research 工具：并行派发多个只读调研子代理（P1-9）。
///
/// 各任务独立 request_id、独立上下文，`JoinSet` 并发执行；任一失败不影响
/// 其余（独立收集）；汇总各摘要返回，完整输出分别入 LRU 存储。
pub fn build_parallel_research_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "parallel_research",
        "并行派发 2-5 个只读调研子代理，各自独立上下文同时执行，汇总各摘要一次返回。适合从多个独立角度/主题同时调研（如分别调研 A、B、C 三个主题），显著节省串行时间。各子代理完整输出可用 read_subagent_result 分页读取。",
        serde_json::json!({
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
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
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
                let max_turns = args
                    .get("max_turns")
                    .and_then(|m| m.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(SUBAGENT_MAX_TURNS)
                    .clamp(1, 30);
                record_tool_call(
                    &cfg,
                    "parallel_research",
                    &format!("tasks={} max_turns={}", tasks.len(), max_turns),
                    Some(&args),
                );
                if tasks.len() < 2 {
                    let e = "至少需要 2 个调研任务".to_string();
                    record_tool_result(&cfg, "parallel_research", false, &e, Some(&e));
                    return Err(tool_error("parallel_research", &e));
                }
                if tasks.len() > 5 {
                    let e = "最多 5 个并行任务".to_string();
                    record_tool_result(&cfg, "parallel_research", false, &e, Some(&e));
                    return Err(tool_error("parallel_research", &e));
                }

                // 并行派发：JoinSet 并发执行，独立收集结果（任一失败不影响其余）
                let mut set = tokio::task::JoinSet::new();
                for task in tasks {
                    let cfg = cfg.clone();
                    set.spawn(async move { run_subagent_impl(&cfg, task, SubagentMode::ReadOnly, max_turns).await });
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
                let mut out = format!(
                    "并行调研完成（{} 个任务，{} 个失败）：\n",
                    entries.len(),
                    failed_count
                );
                for (i, (id, summary, failed)) in entries.iter().enumerate() {
                    out.push_str(&format!("\n── 任务 {} {} ──\n", i + 1, if *failed { "(失败)" } else { "" }));
                    out.push_str(summary);
                    if !id.is_empty() {
                        out.push_str(&format!("\n完整输出：read_subagent_result subagent_id=\"{id}\""));
                    }
                }
                record_tool_result(
                    &cfg,
                    "parallel_research",
                    failed_count == 0,
                    &format!("{} 任务 {} 失败", entries.len(), failed_count),
                    Some(&out),
                );
                Ok(ToolOutput::text(out))
            })
        },
    )
}
