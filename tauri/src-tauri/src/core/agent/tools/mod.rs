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
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::Manager;

use futures_util::StreamExt;

use crate::core::agent::limits::*;
use crate::core::agent::KbSearchConfig;
use crate::core::db::utils::IgnoreMatcher;
use crate::core::subagent::{
    SubagentMode, SubagentSpec, SUBAGENT_SUMMARY_CHARS,
};
use crate::core::SearchHit;
use scraper::{ElementRef, Html, Selector};

mod cache;
pub mod canvas;

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
    /// 结构化结果（可选）：按工具类型携带结构化数据（如 git_diff 的文件改动数组、
    /// grep/ls 的命中列表），前端据此渲染增强卡片；不影响 result 文本与 LLM 上下文
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured: Option<serde_json::Value>,
}

/// 按 `request_id` 记录工具调用轨迹的全局总线。
///
/// 工具闭包在 Rig 流式内部执行，无法直接访问 Tauri 事件发射器，
/// 因此先写入本总线，由 `commands/llm.rs` 的流式循环按请求 drain 并转发。
/// 全局总线跟踪的并发请求桶上限：超过后逐桶淘汰最旧（见 MAX_TRACKED_REQUESTS），
/// 防止异常路径（如子代理被取消）遗留的桶永久占用内存。

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
            // 容量治理：超限时逐桶淘汰（优先淘汰非当前请求的桶，保留本请求轨迹）。
            // 与旧版「清空整个 map」相比，并发子代理/多请求场景不会冲掉主链轨迹卡片。
            while map.len() >= MAX_TRACKED_REQUESTS {
                let victim = map.keys().find(|k| *k != request_id).cloned();
                match victim {
                    Some(k) => {
                        map.remove(&k);
                    }
                    None => break,
                }
            }
            map.entry(request_id.to_string()).or_default().push(ToolCallEvent {
                seq,
                kind: "call".into(),
                tool: tool.into(),
                call_id: call_id.into(),
                args_preview: args_preview.into(),
                arguments: truncate(arguments, MAX_TRACKED_ARGS_CHARS).into(),
                ok: false,
                summary: String::new(),
                result: String::new(),
                call_seq: 0,
                skill_id: skill_id.map(|s| s.to_string()),
                structured: None,
            });
        }
    }

    /// 记录带结构化数据的工具结果（OCP：文本字段与结构化字段并存，互不影响）。
    /// `structured=None` 时等价于普通文本结果（record_tool_result 走此路径）。
    fn record_result_structured(
        &self,
        request_id: &str,
        tool: &str,
        ok: bool,
        summary: &str,
        result: &str,
        structured: Option<serde_json::Value>,
    ) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut map) = self.map.lock() {
            // 容量治理：逐桶淘汰（与 record_call 一致，不清空整个 map）
            while map.len() >= MAX_TRACKED_REQUESTS {
                let victim = map.keys().find(|k| *k != request_id).cloned();
                match victim {
                    Some(k) => {
                        map.remove(&k);
                    }
                    None => break,
                }
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
                structured,
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
/// `args` 为模型原始参数（JSON），完整序列化用于会话历史回放；
/// 调用 ID 在本处生成（`call_{uuid}`），结果事件按 call_seq 配对继承。
pub fn record_tool_call(
    cfg: &KbSearchConfig,
    tool: &str,
    args_preview: &str,
    args: Option<&serde_json::Value>,
) {
    // P0-7：请求已取消时不再写入总线——取消路径已 clear 请求桶，此处写入只会
    // 重建桶造成内存残留与陈旧轨迹（取消后无人消费这些事件）。
    if cfg.cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
        return;
    }
    let skill_id = cfg
        .skill_state
        .active_only()
        .iter()
        .find(|s| s.tools.iter().any(|t| t == tool))
        .map(|s| format!("{}:{}", s.scope.as_str(), s.skill_id))
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
    record_tool_result_structured(cfg, tool, ok, summary, result, None);
}

/// 带结构化结果的记录版本（OCP 扩展：现有调用不受影响）。
///
/// `structured` 为可选的结构化数据（如 git_diff 的文件改动数组、grep/ls 的命中列表），
/// 前端据此渲染增强卡片；`result` 文本仍保留（LLM 上下文与历史回放不受影响）。
pub fn record_tool_result_structured(
    cfg: &KbSearchConfig,
    tool: &str,
    ok: bool,
    summary: &str,
    result: Option<&str>,
    structured: Option<serde_json::Value>,
) {
    // P0-7：请求已取消时不再写回总线（桶已 clear，重建只残留内存与陈旧轨迹），
    // 也不再计入质量计数（取消产生的"失败"不是工具真实失败）。
    if cfg.cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
        return;
    }
    const MAX_RESULT_CHARS: usize = 12_000;
    let result = result.map(|r| truncate(r, MAX_RESULT_CHARS)).unwrap_or_default();
    tool_call_bus().record_result_structured(&cfg.request_id, tool, ok, summary, &result, structured);
    // P2-9：质量计数（工具执行成功/失败）
    let q = super::agent_quality();
    if ok {
        q.tool_successes.fetch_add(1, Ordering::Relaxed);
    } else {
        q.tool_failures.fetch_add(1, Ordering::Relaxed);
    }
}

// ─────────────────────────── 文件读取工具 ───────────────────────────

/// 单次读取上限（避免大文件撑爆模型上下文）——见 limits::MAX_FILE_READ_CHARS
/// 目录列举上限——见 limits::MAX_LIST_ITEMS
/// 引用来源片段截断上限——见 limits::MAX_SOURCE_SNIPPET_CHARS

// ─────────────────────────── 重复调用熔断（Loop Guard） ───────────────────────────

/// 并发请求轨迹缓存上限（超限逐桶淘汰最旧，保留当前请求轨迹）
const MAX_TRACKED_REQUESTS: usize = 64;
/// 工具调用参数轨迹的截断上限（防 edit 大 new_string / remember 大 body 撑爆事件负载）
const MAX_TRACKED_ARGS_CHARS: usize = 12_000;

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
/// 使缓存快照失效并触发重建，保证 grep/ls 始终按最新黑名单过滤。
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

/// 解析「可能不存在」的目标文件路径（write 工具用）：父目录必须存在且 canonicalize
/// 校验在根目录内，文件名在父目录下拼接（允许新建），防路径穿越与目录外写入。
fn safe_resolve_new(dir_path: &str, rel: &str) -> Result<PathBuf, String> {
    let base = std::fs::canonicalize(dir_path)
        .map_err(|e| format!("无法访问目录: {}", e))?;
    let rel_path = Path::new(rel);
    // 拒绝绝对路径与含 `..` 的路径（防穿越）
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("路径越界：仅允许访问限定目录内的文件".into());
    }
    let parent = rel_path.parent().unwrap_or_else(|| Path::new(""));
    let file_name = rel_path
        .file_name()
        .ok_or_else(|| "非法文件路径".to_string())?;
    // 父目录必须已存在且位于根目录内（canonicalize 解析符号链接防逃逸）
    let full_parent = std::fs::canonicalize(base.join(parent))
        .map_err(|e| format!("目标目录不存在: {}", e))?;
    if !full_parent.starts_with(&base) {
        return Err("路径越界：仅允许访问限定目录内的文件".into());
    }
    Ok(full_parent.join(file_name))
}

/// 读取已解析路径的文本内容（目录拒绝、超长分页）。
///
/// `offset` 为字符偏移（从 0 开始）：返回 `[offset, offset + MAX_FILE_READ_CHARS)` 区间的内容。
/// 文件仍有后续内容时，截断提示会给出总字符数与下一次读取的 offset，供模型分页续读，
/// 避免模型为读取长文件剩余部分而反复从头重读（浪费多轮工具调用）。
fn read_text(full: &Path, display: &str, offset: usize) -> Result<String, String> {
    let meta = std::fs::metadata(full).map_err(|e| format!("读取文件信息失败: {}", e))?;
    if meta.is_dir() {
        return Err(format!("{} 是目录，请改用 ls 查看目录内容", display));
    }
    // P1-12：工具结果缓存（mtime 一致命中，文件编辑后自动失效）
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let cache_key = format!("{display}|{offset}");
    if let Some(hit) = cache::tool_result_cache().get(&cache_key, mtime) {
        return Ok(hit);
    }
    let data = std::fs::read(full).map_err(|e| format!("读取文件失败: {}", e))?;
    let text = String::from_utf8_lossy(&data).into_owned();
    let total = text.chars().count();
    let result = if offset >= total {
        format!("[已达文件末尾（共 {total} 字符），offset={offset} 超出范围]")
    } else {
        let chunk: String = text.chars().skip(offset).take(MAX_FILE_READ_CHARS).collect();
        if offset + MAX_FILE_READ_CHARS >= total {
            chunk
        } else {
            format!(
                "{chunk}\n\n[内容过长：已显示第 {}~{} 字符（共 {total} 字符）。可再次调用 read 并指定 offset={} 读取后续内容]",
                offset + 1,
                offset + chunk.chars().count(),
                offset + MAX_FILE_READ_CHARS
            )
        }
    };
    cache::tool_result_cache().put(&cache_key, mtime, &result);
    Ok(result)
}

/// 将一次知识库内容读取/搜索命中记录为引用来源（score=0 表示非检索命中，引用列表中排后）。
///
/// 供 `read` / `grep` 等「直接读取知识库文件」的工具调用：这些路径此前不产生引用，
/// 导致同一 Agent 回答中引用「有时有、有时无」。命中写入 `search_sink` 后，
/// 在 `rag:done` 发射前由 `merge_search_sink` 与预检索来源合并，保证
/// 凡是用到知识库内容都有引用可查。
async fn push_kb_source(cfg: &KbSearchConfig, doc_name: &str, snippet: &str) {
    let text: String = snippet.chars().take(MAX_SOURCE_SNIPPET_CHARS).collect();
    if text.trim().is_empty() {
        return;
    }
    let hit = SearchHit {
        text,
        doc_name: doc_name.to_string(),
        chunk_index: 0,
        score: 0.0,
        score_vec: 0.0,
        score_bm25: 0.0,
        path_json: None,
        sentence_window: None,
        symbol_name: None,
        symbol_kind: None,
        chunk_type: None,
        tags: None,
        score_rerank: None,
        query_sources: Vec::new(),
    };
    let mut guard = cfg.search_sink.lock().await;
    guard.push((hit, 0.0));
}

/// 读取知识库（当前打开目录）内文件或当前激活技能的参考文档（渐进式披露 L3）。
///
/// `offset` 为字符偏移（从 0 开始，长文件分页续读用，见 [`read_text`]）。
///
/// 解析顺序：
/// 1. 知识库目录内的相对路径（如 `docs/note.md`）
/// 2. 已激活技能的 SKILL.md 完整正文（如 `outline-mindmap/SKILL.md`）——
///    从内存注册表直读（技能启动时已加载进系统，系统内置技能为编译期嵌入，
///    无需落盘；激活正文超预算截断时引导走此路径）
/// 3. 当前激活技能目录下的相对路径（如 `references/flowchart.md`），
///    按激活技能逐一尝试；技能基础目录由 `cfg.skill_bases` 提供，仅限已激活技能
pub async fn read(cfg: &KbSearchConfig, rel_path: &str, offset: usize) -> Result<String, String> {
    match safe_resolve(&cfg.dir_path, rel_path) {
        Ok(full) => {
            let result = read_text(&full, rel_path, offset)?;
            // 读取的是知识库（当前打开目录）内文件 → 记录为引用来源，
            // 使「直接读文件」的回答同样展示引用（与预检索/检索工具来源一致）
            push_kb_source(cfg, rel_path, &result).await;
            // 提示注入防护：不可信知识库文件内容原样进模型前包裹可疑指令
            // （与子代理摘要/预检索上下文处理保持一致；无命中时原样返回）
            return Ok(crate::core::security::wrap_suspicious(&result));
        }
        Err(e) => {
            // 知识库内未找到：已激活技能 SKILL.md 完整正文走内存注册表直读
            // （技能已随启动加载进系统，系统内置技能为编译期嵌入、不落盘；
            // 命中未激活的已知技能时返回「先 activate_skill」的明确引导）
            if let Some(text) = read_active_skill_md(cfg, rel_path, offset)? {
                return Ok(text);
            }
            if cfg.skill_state.active_only().is_empty() {
                // 无任何已激活（Active）技能：若目标是技能参考路径，明确指出需先激活技能，
                // 避免模型误以为文件不存在而反复尝试（浪费多轮工具调用）
                return Err(skill_ref_hint(rel_path, e));
            }
        }
    }
    let mut last_err = "文件不存在（知识库内与已激活技能的参考目录均未找到）".to_string();
    for skill in cfg.skill_state.active_only() {
        for (scope, base) in &cfg.skill_bases {
            if scope != skill.scope.as_str() {
                continue;
            }
            let dir = Path::new(base).join(&skill.skill_id);
            match safe_resolve_in(&dir, rel_path) {
                Ok(full) => {
                    let result = read_text(&full, rel_path, offset)?;
                    return Ok(crate::core::security::wrap_suspicious(&result));
                }
                Err(e) => last_err = e,
            }
        }
    }
    Err(skill_ref_hint(rel_path, last_err))
}

/// 读取已激活技能的 SKILL.md 完整正文（内存注册表直读，不经过磁盘）。
///
/// 支持 `{skill_id}/SKILL.md` 路径（激活正文超预算截断时的引导路径）：
/// 技能正文随注册表在启动时已加载进系统内存（系统内置技能为编译期
/// `include_str!` 嵌入），此处直接取用完整正文并按 [`read_text`] 相同的
/// offset 分页语义返回，避免模型为获取完整指令而去磁盘上找并不存在的文件。
///
/// 返回 `Ok(None)` 表示该路径不是技能正文路径（或技能未激活），由调用方继续
/// 走磁盘参考文档解析；命中未激活的已知技能时返回带激活引导的错误。
fn read_active_skill_md(
    cfg: &KbSearchConfig,
    rel_path: &str,
    offset: usize,
) -> Result<Option<String>, String> {
    let p = Path::new(rel_path);
    let is_skill_md = p.file_name().and_then(|n| n.to_str()) == Some("SKILL.md");
    if !is_skill_md {
        return Ok(None);
    }
    let id = p
        .parent()
        .and_then(|d| d.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    // 裸 `SKILL.md`（无技能 ID 前缀）：交给磁盘分支（base/{skill_id}/SKILL.md）
    if id.is_empty() {
        return Ok(None);
    }
    for act in cfg.skill_state.active_only() {
        if act.skill_id != id {
            continue;
        }
        let skill = cfg
            .skill_registry
            .get(act.scope, &act.skill_id)
            .or_else(|| cfg.skill_registry.find_enabled(&act.skill_id));
        if let Some(skill) = skill {
            let body = skill.body.trim();
            let full = if body.is_empty() {
                format!(
                    "技能 {}（{} v{}）无正文内容。",
                    skill.name, skill.id, skill.version
                )
            } else {
                format!(
                    "{}（{} v{}）SKILL.md 完整正文（系统内存直读）：\n\n{}",
                    skill.name, skill.id, skill.version, body
                )
            };
            let total = full.chars().count();
            if offset >= total {
                return Ok(Some(format!(
                    "[已达文件末尾（共 {total} 字符），offset={offset} 超出范围]"
                )));
            }
            let chunk: String = full.chars().skip(offset).take(MAX_FILE_READ_CHARS).collect();
            if offset + MAX_FILE_READ_CHARS >= total {
                return Ok(Some(chunk));
            }
            return Ok(Some(format!(
                "{chunk}\n\n[内容过长：已显示第 {}~{} 字符（共 {total} 字符）。可再次调用 read 并指定 offset={} 读取后续内容]",
                offset + 1,
                offset + chunk.chars().count(),
                offset + MAX_FILE_READ_CHARS
            )));
        }
    }
    // 是技能正文路径但该技能未激活：给出明确引导，避免模型误以为文件不存在而反复尝试
    if cfg.skill_registry.find_enabled(&id).is_some() {
        return Err(format!(
            "技能 '{id}' 未激活，无法读取其 SKILL.md。请先调用 activate_skill 激活该技能后重试。"
        ));
    }
    Ok(None)
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
    // 目录/文件黑名单（gitignore 风格，如 assets/、node_modules/、*.log）按用户配置过滤，
    // 与索引/文件树一致；`.mdgo` 等系统内置隐藏目录仍由 IgnoreMatcher 的隐藏/临时文件内置规则排除。
    let entries = get_or_refresh_cache(&cfg.dir_path, &cfg.dir_blacklist, &cfg.file_blacklist)?;
    let pattern = pattern.trim().to_lowercase();
    let max = (max_items as usize).clamp(1, MAX_LIST_ITEMS);

    // 先统计全部匹配数再取展示上限，超限时明确告知剩余数量（避免模型误判目录规模）
    let all_matched: Vec<&(String, u64)> = if pattern.is_empty() {
        entries.iter().collect()
    } else {
        entries
            .iter()
            .filter(|(rel, _)| rel.to_lowercase().contains(&pattern))
            .collect()
    };
    let total = all_matched.len();
    if total == 0 {
        return Ok(format!(
            "目录中未找到匹配的文件（模式：{}）",
            if pattern.is_empty() { "全部" } else { &pattern }
        ));
    }
    let shown = all_matched.iter().take(max);
    let lines: Vec<String> = shown
        .map(|(rel, size)| format!("{rel}  ({} 字节)", size))
        .collect();
    let mut out = format!("共 {} 项：\n{}", total, lines.join("\n"));
    if total > max {
        out.push_str(&format!("\n（另有 {} 项未展示，可加过滤条件或提高 max_items）", total - max));
    }
    Ok(out)
}

// ─────────────────────────── 内容搜索工具（grep） ───────────────────────────

/// 单次搜索最多返回的命中文件数——见 limits::MAX_GREP_FILES
/// 单文件最多返回的匹配行数（context=0 时即最大输出行数）——见 limits::MAX_GREP_OUTPUT_CHARS
const MAX_GREP_LINES_PER_FILE: usize = 10;
/// 匹配行最大显示长度（超长截断）
const MAX_GREP_LINE_CHARS: usize = 200;
/// context>0 时单文件最多输出的行数上限（含上下文行，防止输出爆炸）
const MAX_GREP_CONTEXT_OUTPUT_LINES: usize = 40;
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
            // 字符类：[abc]、[a-z]、[!abc]（取反）——保留类内容，转义类内特殊字符
            '[' => {
                let mut class = String::from("[");
                if chars.peek() == Some(&'!') {
                    chars.next();
                    class.push('^');
                }
                let mut closed = false;
                for c2 in chars.by_ref() {
                    if c2 == ']' {
                        class.push(']');
                        closed = true;
                        break;
                    }
                    if matches!(c2, '\\' | '^' | ']') {
                        class.push('\\');
                    }
                    class.push(c2);
                }
                if !closed {
                    // 未闭合的 `[` 按字面量处理
                    re.push_str("\\[");
                    continue;
                }
                re.push_str(&class);
            }
            '.' | '+' | '(' | ')' | '{' | '}' | '|' | '^' | '$' | '\\' => {
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
    // 过短限制仅针对单个 ASCII 字符（如 "a"、"1" 噪声大）；
    // 单个中文字符（如 "图"、"表"）是常见检索词，放行
    if pattern.chars().count() < 2 && pattern.chars().all(|c| c.is_ascii()) {
        return Err("搜索关键词过短（至少 2 个字符），请提供更具体的关键词".to_string());
    }
    let parsed = parse_pattern(pattern);
    if parsed.terms.is_empty() {
        return Err("搜索关键词为空，请提供 pattern 参数".to_string());
    }
    let mode_and = match_mode != "or";
    let context = context_lines.min(GREP_CONTEXT_MAX);
    let limit = (max_files as usize).clamp(1, MAX_GREP_FILES);

    // 缓存读取（冷缓存时为全量目录遍历）、候选过滤与文件匹配均为 CPU/IO 密集
    // 操作，整体移到阻塞线程执行，避免阻塞 tokio 执行线程与 agent 异步循环，
    // 也避免大知识库冷缓存时首次 grep 卡死（遍历无取消机制，绝不能跑在 async 线程上）。
    let dir_path = cfg.dir_path.clone();
    // 目录/文件黑名单（gitignore 风格）按用户配置过滤（与 ls/glob 一致）；
    // `.mdgo` 等系统内置隐藏目录仍由 IgnoreMatcher 内置规则排除
    let dir_blacklist = cfg.dir_blacklist.clone();
    let file_blacklist = cfg.file_blacklist.clone();
    let include_owned = include.to_vec();
    let exclude_owned = exclude.to_vec();
    let parsed_for_search = parsed.clone();
    let (hits, truncated, skipped, hit_total) = tokio::task::spawn_blocking(move || {
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
        let mut hit_total = 0u32;
        let mut scanned: u64 = 0;
        let mut truncated = false;
        let mut skipped = 0u32;
        for (rel, _) in candidates {
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
            hit_total += 1;
            // 已达展示上限：仅计数不再存储（扫描预算仍限制总耗时），输出时提示剩余数量
            if hits.len() >= limit {
                continue;
            }
            if list_only {
                hits.push((rel, Vec::new()));
            } else if !lines.is_empty() {
                hits.push((rel, lines));
            }
        }
        Ok::<_, String>((hits, truncated, skipped, hit_total))
    })
    .await
    .map_err(|e| format!("搜索文件内容失败: {}", e))??;

    // 读取失败提示：区分"术语不存在"与"文件不可读"，避免误导模型
    let skip_note = if skipped > 0 {
        format!("\n（注：{} 个文件读取失败被跳过，结果可能不完整）", skipped)
    } else {
        String::new()
    };

    // 将 grep 命中的知识库文件记录为引用来源（模型使用的知识库内容对用户透明），
    // 与 read / 预检索 / kb_search 的来源路径保持一致
    if !list_only {
        for (rel, lines) in &hits {
            if !lines.is_empty() {
                push_kb_source(cfg, rel, &lines.join("\n")).await;
            }
        }
    }

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

    // 命中文件数超限时明确告知总数，避免模型误判为只有展示的这些文件
    let mut out = if hit_total > hits.len() as u32 {
        format!(
            "搜索“{}”命中 {} 个文件（仅展示前 {} 个，可加 include/exclude 缩小范围）：\n",
            pattern, hit_total, hits.len()
        )
    } else {
        format!("搜索“{}”命中 {} 个文件：\n", pattern, hits.len())
    };
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
    // 提示注入防护：grep 命中的知识库文件内容进模型前包裹可疑指令（与 read 一致）
    Ok(crate::core::security::wrap_suspicious(&out))
}

/// 扫描字节/输出字符截断时的可执行优化建议（引导模型下一步动作）。
fn truncate_hint() -> String {
    "\n⚠️ 搜索已达到单次扫描/输出上限，结果存在截断。\n建议方案：\n1. 使用 include 参数限定文件后缀缩小扫描范围（如 include:[\"*.rs\"]）\n2. 使用更精准的关键词减少匹配范围\n3. 拆分搜索，分多次查询不同目录\n".to_string()
}

/// 构建 grep 工具：在知识库内搜索文件内容。
///
/// 参数与使用策略对齐 Claude Code / GitHub Codex 的 grep 习惯；新增参数全部带默认值，
/// 旧调用（仅 pattern/max_files）行为保持不变。

// ─────────────────────────── Git 状态工具（只读） ───────────────────────────

/// 查询知识库所在 Git 仓库的工作区状态（只读，不支持提交等写操作）。
pub async fn git_status(cfg: &KbSearchConfig) -> Result<String, String> {
    // 复用 run_git_tool（spawn_blocking + 超时 + 错误归一），避免重复实现
    let text = run_git_tool(&cfg.dir_path, &["status", "--short"], 10).await?;
    let total = text.lines().count();
    if total == 0 {
        return Ok("Git 工作区干净，当前无任何改动。".into());
    }
    let head: Vec<&str> = text.lines().take(200).collect();
    Ok(format!("Git 状态（共 {total} 项改动）：\n{}", head.join("\n")))
}

/// 执行 git 命令（spawn_blocking + 超时 + 错误归一），供 git_diff/git_commit/git_checkout 复用（SRP）。
async fn run_git_tool(dir: &str, args: &[&str], timeout_secs: u64) -> Result<String, String> {
    let dir_owned = dir.to_string();
    let args_owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        tokio::task::spawn_blocking(move || {
            let mut cmd = std::process::Command::new("git");
            // Windows 隐藏控制台窗口（与 commands/git.rs 的 git_cmd 一致）
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
            }
            cmd.arg("-C").arg(&dir_owned).arg("-c").arg("core.quotepath=false");
            cmd.args(&args_owned);
            cmd.output()
        }),
    )
    .await
    .map_err(|_| format!("Git 命令超时（{} 秒）", timeout_secs))?
    .map_err(|e| format!("git 执行任务失败: {}", e))?
    .map_err(|e| format!("git 执行失败（可能未安装 git）: {}", e))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("Git 命令失败: {}", err));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 查询 Git 工作区/暂存区差异（只读）。
///
/// `stat_only=true` 时以 `--numstat` 输出文件级统计，并返回结构化
/// `[{path, additions, deletions}]` 供前端增强卡片渲染。
pub async fn git_diff(
    dir: &str,
    staged: bool,
    stat_only: bool,
) -> Result<(String, Option<serde_json::Value>), String> {
    let mut args: Vec<&str> = Vec::new();
    if stat_only {
        args.push("diff");
        args.push("--numstat");
    } else {
        args.push("diff");
        args.push("--no-color");
    }
    if staged {
        args.push("--cached");
    }
    let text = run_git_tool(dir, &args, 10).await?;
    let mut structured: Option<serde_json::Value> = None;
    if stat_only {
        // numstat 格式：`adds\tdeletes\tpath`（二进制为 `-`）
        let files: Vec<serde_json::Value> = text
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('\t');
                let adds = parts.next().unwrap_or("");
                let dels = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("").trim();
                if path.is_empty() {
                    return None;
                }
                Some(serde_json::json!({
                    "path": path,
                    "additions": adds.parse::<u64>().unwrap_or(0),
                    "deletions": dels.parse::<u64>().unwrap_or(0),
                }))
            })
            .collect();
        if !files.is_empty() {
            structured = Some(serde_json::json!({ "files": files, "staged": staged }));
        }
    }
    let total_chars = text.chars().count();
    let limit = GIT_DIFF_MAX_CHARS;
    if total_chars > limit {
        let cut: String = text.chars().take(limit).collect();
        return Ok((format!("{}（差异过大已截断，共 {} 字符）", cut, total_chars), structured));
    }
    if text.trim().is_empty() {
        return Ok(("工作区无差异。".into(), structured));
    }
    Ok((text, structured))
}

/// 提交暂存区改动（写操作，需用户确认）：`git commit -m <message>`（使用仓库/全局 user 配置）。
pub async fn git_commit(dir: &str, message: &str) -> Result<String, String> {
    let msg = message.trim();
    if msg.is_empty() {
        return Err("commit message 不能为空".into());
    }
    // P0-9：与 edit/write/delete 对齐的 .mdgo 防护——暂存区含 .mdgo 内部数据时拒绝提交，
    // 防止模型经 git_commit 把应用内部状态（配置/技能/索引）写进仓库历史。
    let staged = run_git_tool(dir, &["diff", "--cached", "--name-only"], 10).await?;
    for line in staged.lines() {
        let p = line.trim();
        if is_mdgo_internal(p) {
            return Err(format!(
                "暂存区包含 .mdgo 内部数据（{}），不允许提交（.mdgo 为应用内部数据目录，配置/技能/索引不应进入 Git 历史）。请先移除 .mdgo 相关暂存再重试。",
                p
            ));
        }
    }
    let text = run_git_tool(dir, &["commit", "-m", msg], 15).await?;
    // Mutation Verification（P0-1）：提交后确认 HEAD 存在且最近提交 subject 与本次一致。
    // 注意 `git log --format=%s` 只输出规范化后的 subject（首行）；msg 可能含多行/尾随
    // 空格，故取 msg 首行 trim 后比对，避免多行 message 误报"验证失败"。
    let head = run_git_tool(dir, &["log", "-1", "--format=%s"], 10)
        .await?
        .trim()
        .to_string();
    let msg_subject = msg.lines().next().unwrap_or("").trim();
    if head.is_empty() {
        return Err("提交验证失败：HEAD 无提交记录，commit 可能未生效，请勿声称已提交".into());
    }
    if head != msg_subject {
        return Err(format!(
            "提交验证失败：HEAD 最近提交 subject（{}）与本次提交（{}）不一致，commit 可能未生效，请勿声称已提交",
            head, msg_subject
        ));
    }
    Ok(format!(
        "{}\n[verified] 已确认 HEAD 提交: {}",
        text.trim(),
        head
    ))
}

/// 恢复工作区文件到 HEAD（写操作，需用户确认）：`git checkout -- <paths>`。
pub async fn git_checkout(dir: &str, paths: &[String]) -> Result<String, String> {
    if paths.is_empty() {
        return Err("paths 不能为空，请指定要恢复的文件".into());
    }
    if paths.len() > 20 {
        return Err("paths 最多 20 个文件".into());
    }
    // P0-9：与 edit/write/delete 对齐的 .mdgo 防护——禁止恢复 .mdgo 内部数据
    // （应用内部状态不应被 git_checkout 覆盖/改动）。
    for p in paths {
        if is_mdgo_internal(p) {
            return Err(format!(
                "{} 为 .mdgo 内部数据（配置/技能/索引），不允许恢复（git_checkout）。请移除 .mdgo 路径后重试。",
                p
            ));
        }
    }
    let mut args: Vec<&str> = vec!["checkout", "--"];
    for p in paths {
        args.push(p);
    }
    run_git_tool(dir, &args, 15).await?;
    Ok(format!("已恢复 {} 个文件到 HEAD", paths.len()))
}

// ─────────────────────────── 文件编辑/删除工具（限打开目录） ───────────────────────────

/// 判断相对路径是否指向 `.mdgo` 内部数据（配置/技能/索引，禁止编辑/删除）。
fn is_mdgo_internal(rel: &str) -> bool {
    let norm = rel.trim_start_matches(['/', '\\']);
    norm.eq_ignore_ascii_case(".mdgo")
        || norm.starts_with(".mdgo/")
        || norm.starts_with(".mdgo\\")
}

/// 校验已解析（canonical）路径不在 `.mdgo` 内部数据目录内。
///
/// 在 `safe_resolve` 之后按 canonical 结果的相对组件逐段判断（忽略大小写），
/// 可防 `..` 穿越（如 `a/../.mdgo/config.yaml`）绕过 `is_mdgo_internal` 的字符串前缀检查。
fn ensure_not_mdgo(full: &Path, base: &Path, op: &str) -> Result<(), String> {
    let rel = full
        .strip_prefix(base)
        .map_err(|_| "路径越界：仅允许访问限定目录内的文件".to_string())?;
    if rel
        .components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case(".mdgo"))
    {
        return Err(format!(
            ".mdgo 为应用内部数据目录（配置/技能/索引），不允许{}",
            op
        ));
    }
    Ok(())
}

/// 原子写文件：先写同目录临时文件再 rename 替换，避免写中途崩溃留下半写文件。
///
/// Unix 上 rename 为原子替换；Windows 上目标存在时 rename 失败，退化为
/// 「删除目标 + 重命名」（窗口期极短）。任何失败路径都会清理临时文件。
fn atomic_write_file(full: &Path, content: &[u8]) -> Result<(), String> {
    let mut tmp_name = full
        .file_name()
        .unwrap_or_default()
        .to_os_string();
    tmp_name.push(".mdgo-tmp");
    let tmp = full.with_file_name(tmp_name);
    std::fs::write(&tmp, content).map_err(|e| format!("写入临时文件失败: {}", e))?;
    match std::fs::rename(&tmp, full) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Windows：目标已存在导致 rename 失败，退化为删除+重命名
            if std::fs::remove_file(full).is_ok() {
                std::fs::rename(&tmp, full).map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    format!("写入文件失败: {}", e)
                })
            } else {
                let _ = std::fs::remove_file(&tmp);
                Err("写入文件失败：无法替换目标文件".into())
            }
        }
    }
}

/// Mutation Verification（P0-1）：回读文件并与预期内容比对，确认写操作实际生效。
///
/// 返回 `Ok(验证摘要)` 表示回读一致；返回 `Err` 表示写入后的实际内容与预期不一致
/// （操作可能未完全生效），调用方应视作失败返回——防止模型在工具静默失败时
/// 声称"已写入/已完成"（写操作幻觉的核心防线）。
fn verify_write_back(cfg: &KbSearchConfig, rel_path: &str, expected: &str) -> Result<String, String> {
    let full = safe_resolve(&cfg.dir_path, rel_path)?;
    let data = std::fs::read(&full)
        .map_err(|e| format!("回读验证失败（读取 {}）: {}", rel_path, e))?;
    let actual = String::from_utf8(data)
        .map_err(|_| format!("回读验证失败：{} 内容不是有效 UTF-8", rel_path))?;
    if actual == expected {
        Ok(format!("[verified] 已回读确认 {} 内容一致", rel_path))
    } else {
        Err(format!(
            "回读验证失败：{} 写入后的实际内容与预期不一致（实际 {} 字符 ≠ 预期 {} 字符），操作可能未完全生效，请勿声称成功；可用 read 查看实际内容后重试",
            rel_path,
            actual.chars().count(),
            expected.chars().count()
        ))
    }
}

/// 通知前端知识库文件已被 Agent 写入：前端据此做**增量**文件树更新
/// （监听 `agent:file-written`，见 css_js/modules/agent.js 与 main.html 的
/// `handleAgentFileWritten`）。
///
/// 覆盖 write / edit / multi_edit 三个写文件工具的成功路径。载荷：
/// - `rel_path`：知识库相对路径
/// - `created`：是否新建（false = 覆盖/编辑已有文件，前端无需改树，零成本）
/// - `size` / `mtime`：文件字节数与 unix 秒时间戳（前端增量补 _scanData 用）
///
/// 设计要点：**绝不触发全量扫描/重建**——10 万级知识库下每次写入全量刷新
/// （walkdir 全扫 + 大 IPC 载荷 + DOM 重建）不可接受；新建走增量插入，
/// 编辑/覆盖零成本。
fn notify_file_written(cfg: &KbSearchConfig, rel_path: &str, created: bool) {
    use tauri::Emitter;
    // 写入后单次 stat 获取元数据（廉价），供前端增量补 _scanData
    let full = safe_resolve(&cfg.dir_path, rel_path).ok();
    let meta = full.and_then(|p| std::fs::metadata(p).ok());
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let mtime = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = cfg.app_handle.emit(
        "agent:file-written",
        serde_json::json!({
            "rel_path": rel_path,
            "created": created,
            "size": size,
            "mtime": mtime,
        }),
    );
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
    // canonical 后二次校验 `.mdgo`（防 `..` 穿越绕过字符串前缀检查）
    let base = std::fs::canonicalize(&cfg.dir_path).map_err(|e| format!("无法访问目录: {}", e))?;
    ensure_not_mdgo(&full, &base, "编辑")?;
    let meta = std::fs::metadata(&full).map_err(|e| format!("读取文件信息失败: {}", e))?;
    if meta.is_dir() {
        return Err(format!("{} 是目录，仅支持编辑文本文件", rel_path));
    }
    if meta.len() > MAX_EDIT_FILE_BYTES {
        return Err(format!("{} 超过 1MB，请改用其他方式编辑", rel_path));
    }
    let data = std::fs::read(&full).map_err(|e| format!("读取文件失败: {}", e))?;
    // 二进制检测：含 NUL 字节的文件拒绝编辑，避免乱码污染上下文
    if data.contains(&0) {
        return Err(format!("{} 是二进制文件，仅支持编辑文本文件", rel_path));
    }
    // 严格 UTF-8 校验：拒绝无效 UTF-8（如 latin-1），避免 lossy 写回永久损坏文件
    let content = String::from_utf8(data)
        .map_err(|_| format!("{} 不是有效的 UTF-8 文本文件，拒绝编辑以避免损坏", rel_path))?;
    let occurrences: Vec<usize> = content.match_indices(old_string).map(|(i, _)| i).collect();
    match occurrences.len() {
        0 => Err("未在文件中找到与 old_string 完全匹配的内容，请先使用 read 读取文件确认原文（注意换行符、空格、大小写需完全一致）".into()),
        1 => {
            let start = occurrences[0];
            let mut new_content = String::with_capacity(content.len() + new_string.len());
            new_content.push_str(&content[..start]);
            new_content.push_str(new_string);
            new_content.push_str(&content[start + old_string.len()..]);
            atomic_write_file(&full, new_content.as_bytes())?;
            // Mutation Verification（P0-1）：回读确认替换实际生效
            let verified = verify_write_back(cfg, rel_path, &new_content)?;
            // 通知前端：编辑已有文件 → created=false，树节点已存在，前端零成本
            notify_file_written(cfg, rel_path, false);
            Ok(format!(
                "已更新 {}：替换 1 处（{} 字符 → {} 字符）；{}",
                rel_path,
                old_string.chars().count(),
                new_string.chars().count(),
                verified
            ))
        }
        n => Err(format!(
            "old_string 在文件中出现 {} 次，请提供更长的上下文使其唯一匹配",
            n
        )),
    }
}

/// 单次 multi_edit 最多提交的编辑数——见 limits::MAX_MULTI_EDITS
/// 批量编辑多个文件：所有编辑先全量校验（路径安全/UTF-8/old_string 唯一匹配），
/// 全部通过后再逐个原子写入（all-or-nothing 的校验阶段 + 顺序写入阶段）。
///
/// 相比逐次调用 edit，一次调用完成多文件修改，节省模型轮次预算（高可用）；
/// 校验失败时不写任何文件，避免部分成功造成的不一致。
pub async fn multi_edit_files(
    cfg: &KbSearchConfig,
    edits: &[(String, String, String)],
) -> Result<String, String> {
    if edits.is_empty() {
        return Err("edits 不能为空".into());
    }
    if edits.len() > MAX_MULTI_EDITS {
        return Err(format!("edits 最多 {} 个", MAX_MULTI_EDITS));
    }
    let base = std::fs::canonicalize(&cfg.dir_path).map_err(|e| format!("无法访问目录: {}", e))?;

    // ── 阶段 1：全量校验（任一失败即整体拒绝，不写任何文件）──
    let mut pending: Vec<(std::path::PathBuf, Vec<u8>)> = Vec::with_capacity(edits.len());
    for (i, (rel, old, new)) in edits.iter().enumerate() {
        let idx = i + 1;
        if old.is_empty() {
            return Err(format!("第 {} 个 edit 的 old_string 不能为空", idx));
        }
        if is_mdgo_internal(rel) {
            return Err(format!("第 {} 个 edit：{} 为 .mdgo 内部数据，不允许编辑", idx, rel));
        }
        let full = safe_resolve(&cfg.dir_path, rel)?;
        ensure_not_mdgo(&full, &base, "编辑")?;
        let meta = std::fs::metadata(&full).map_err(|e| format!("读取文件信息失败: {}", e))?;
        if meta.is_dir() {
            return Err(format!("第 {} 个 edit：{} 是目录，仅支持编辑文本文件", idx, rel));
        }
        if meta.len() > MAX_EDIT_FILE_BYTES {
            return Err(format!("第 {} 个 edit：{} 超过 1MB，请改用其他方式编辑", idx, rel));
        }
        let data = std::fs::read(&full).map_err(|e| format!("读取文件失败: {}", e))?;
        if data.contains(&0) {
            return Err(format!("第 {} 个 edit：{} 是二进制文件，仅支持编辑文本文件", idx, rel));
        }
        let content = String::from_utf8(data)
            .map_err(|_| format!("第 {} 个 edit：{} 不是有效的 UTF-8 文本文件，拒绝编辑以避免损坏", idx, rel))?;
        let occurrences: Vec<usize> = content.match_indices(old).map(|(i, _)| i).collect();
        match occurrences.len() {
            0 => return Err(format!(
                "第 {} 个 edit 未在 {} 中找到与 old_string 完全匹配的内容，请先使用 read 读取文件确认原文",
                idx, rel
            )),
            1 => {
                let start = occurrences[0];
                let mut new_content = String::with_capacity(content.len() + new.len());
                new_content.push_str(&content[..start]);
                new_content.push_str(new);
                new_content.push_str(&content[start + old.len()..]);
                pending.push((full, new_content.into_bytes()));
            }
            n => return Err(format!(
                "第 {} 个 edit：old_string 在 {} 中出现 {} 次，请提供更长的上下文使其唯一匹配",
                idx, rel, n
            )),
        }
    }

    // ── 阶段 2：逐文件原子写（校验已全部通过）──
    for (full, bytes) in &pending {
        atomic_write_file(full, bytes)?;
    }
    // Mutation Verification（P0-1）：逐文件回读比对——与阶段 1 构建的**完整新内容**
    // 全等比较（而非 new_string 片段，片段必然与整文件内容不一致会导致误报），
    // 任一不一致即整体失败返回（文件已写入，明确提示勿声称成功）。
    for ((rel, _, _), (_, bytes)) in edits.iter().zip(&pending) {
        let expected = String::from_utf8(bytes.clone())
            .map_err(|_| format!("回读验证失败：{} 预期内容非 UTF-8", rel))?;
        verify_write_back(cfg, rel, &expected)?;
    }
    // 通知前端：批量编辑均为覆盖已有文件 → created=false，前端零成本
    // （逐个发射，前端 600ms 防抖合并，仅对最后一个做定位）
    for (rel, _, _) in edits {
        notify_file_written(cfg, rel, false);
    }
    Ok(format!("已批量更新 {} 个文件；全部回读验证一致 [verified]", pending.len()))
}

/// 创建新文件或整体覆盖知识库（当前打开目录）内文本文件（对齐主流 Agent 的 write 能力）。
///
/// 安全边界：路径经 [`safe_resolve_new`] 限制在打开目录内（允许新建），父目录不存在时
/// 自动创建（先防穿越校验 + `base.join` 创建，再 canonicalize 二次校验），
/// 且拒绝 `.mdgo` 内部数据；写入为原子写（临时文件 + rename）。
///
/// 格式自动处理：目标扩展名为 `.canvas` 时，内容先经 [`super::canvas::validate_canvas_json`]
/// 校验管线（parse → schema/ID/edge/file 校验 → sanitize → 坐标/尺寸合法性校验 → 原样序列化），
/// 校验失败则拒绝写入——**布局由模型负责，本函数只做机器可验证的格式正确性校验，不重排坐标**。
pub async fn write_file(
    cfg: &KbSearchConfig,
    rel_path: &str,
    content: &str,
) -> Result<String, String> {
    if is_mdgo_internal(rel_path) {
        return Err(".mdgo 为应用内部数据目录（配置/技能/索引），不允许写入".into());
    }
    // P0-10：1MB 上限按**字节**计（UTF-8 中文 1 字 ≈ 3 字节；按字符数会低估实际体积，
    // 允许写入远超磁盘/内存预期的内容）
    if content.len() > MAX_EDIT_FILE_BYTES as usize {
        return Err(format!(
            "{} 内容超过 1MB（{} 字节），write 单次写入上限为 1MB",
            rel_path,
            content.len()
        ));
    }
    // Canvas 格式自动处理：.canvas 内容先经确定性校验/规整，再落盘
    let effective = if rel_path.ends_with(".canvas") {
        canvas::validate_canvas_json(content, &cfg.dir_path)?
    } else {
        content.to_string()
    };
    // P0-10：路径校验全部前置（先于任何文件系统副作用），失败不得残留目录。
    // 1) 词法拒绝：绝对路径与 `..` 穿越（与 safe_resolve_new 一致的口径）
    let rel = std::path::Path::new(rel_path);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("路径越界：仅允许访问限定目录内的文件".into());
    }
    // 2) 词法校验父目录组件不得含 `.mdgo`（is_mdgo_internal 的字符串前缀检查之外的
    //    路径形态，如大小写/多段组合，也能在建目录前被拦截）
    let parent = rel.parent().unwrap_or_else(|| std::path::Path::new(""));
    for comp in parent.components() {
        if comp.as_os_str().eq_ignore_ascii_case(".mdgo") {
            return Err(".mdgo 为应用内部数据目录（配置/技能/索引），不允许写入".into());
        }
    }
    let base = std::fs::canonicalize(&cfg.dir_path).map_err(|e| format!("无法访问目录: {}", e))?;
    let full_parent = base.join(parent);
    // 3) 词法防逃逸：拼接结果必须仍在根目录内（canonicalize 前的第一道闸）
    if !full_parent.starts_with(&base) {
        return Err("路径越界：仅允许访问限定目录内的文件".into());
    }
    // 4) 全部静态校验通过后才允许创建目录（失败路径不再产生副作用）
    if !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(&full_parent)
            .map_err(|e| format!("创建目录失败: {}", e))?;
    }
    let full = safe_resolve_new(&cfg.dir_path, rel_path)?;
    // canonical 后二次校验 `.mdgo`（防符号链接逃逸；父目录已 canonicalize）
    ensure_not_mdgo(&full, &base, "写入")?;
    // 写入前判断目标是否存在（写入后判断恒为真，无法区分新建/覆盖）
    let existed = full.exists();
    atomic_write_file(&full, effective.as_bytes())?;
    // Mutation Verification（P0-1）：回读确认写入实际生效
    let verified = verify_write_back(cfg, rel_path, &effective)?;
    // 通知前端：新建文件 → 增量插入树节点 + 补 _scanData；覆盖已有 → 零成本
    notify_file_written(cfg, rel_path, !existed);
    Ok(format!(
        "已{} {}（{} 字符）；{}",
        if existed { "覆盖写入" } else { "创建" },
        rel_path,
        effective.chars().count(),
        verified
    ))
}

/// 构建 write 工具：创建新文件或整体覆盖知识库内文本文件。

/// 删除知识库（当前打开目录）内的一个文件（不可恢复）。
///
/// 安全边界：路径经 `safe_resolve` 限制在打开目录内，且拒绝 `.mdgo` 内部数据。
pub async fn delete_file(cfg: &KbSearchConfig, rel_path: &str) -> Result<String, String> {
    if is_mdgo_internal(rel_path) {
        return Err(".mdgo 为应用内部数据目录（配置/技能/索引），不允许删除".into());
    }
    let full = safe_resolve(&cfg.dir_path, rel_path)?;
    // canonical 后二次校验 `.mdgo`（防 `..` 穿越绕过字符串前缀检查）
    let base = std::fs::canonicalize(&cfg.dir_path).map_err(|e| format!("无法访问目录: {}", e))?;
    ensure_not_mdgo(&full, &base, "删除")?;
    let meta = std::fs::metadata(&full).map_err(|e| format!("读取文件信息失败: {}", e))?;
    if meta.is_dir() {
        return Err(format!("{} 是目录，delete 仅支持删除文件，不支持目录", rel_path));
    }
    std::fs::remove_file(&full).map_err(|e| format!("删除文件失败: {}", e))?;
    // Mutation Verification（P0-1）：回读确认目标已不存在
    if full.exists() {
        return Err(format!(
            "删除验证失败：{} 仍然存在，操作可能未生效，请勿声称已删除",
            rel_path
        ));
    }
    Ok(format!("已删除文件 {} [verified] 回读确认不存在", rel_path))
}

// ─────────────────────────── 工具构建 ───────────────────────────

/// 统一工具错误包装（P2-8 Error Recovery Protocol）。
///
/// 模型收到的错误消息包含「失败事实 + 恢复引导」：建议修正参数重试或改用其他工具，
/// 并明确「不要声称操作已成功」（与 Mutation Verification 的 [verified] 语义闭环）。
/// 对齐 Anthropic 工具最佳实践（is_error + informative message，让模型换路而非瞎试）。

/// 截断长字符串（用于工具轨迹参数摘要，避免撑爆事件负载）
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

pub async fn glob_files(
    cfg: &KbSearchConfig,
    pattern: &str,
    max_items: u32,
) -> Result<String, String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Err("glob 模式不能为空，如 **/*.rs、docs/*.md".to_string());
    }
    // 目录/文件黑名单（gitignore 风格）按用户配置过滤（与 ls/grep 一致；`.mdgo` 等隐藏目录仍由内置规则排除）
    let entries = get_or_refresh_cache(&cfg.dir_path, &cfg.dir_blacklist, &cfg.file_blacklist)?;
    let matcher = GlobMatcher::new(&[pattern.to_string()]);
    let max = (max_items as usize).clamp(1, MAX_LIST_ITEMS);
    let matched: Vec<&(String, u64)> = entries
        .iter()
        .filter(|(rel, _)| matcher.is_match(rel))
        .collect();
    let total = matched.len();
    if total == 0 {
        return Ok(format!("未找到匹配 {} 的文件", pattern));
    }
    let shown = matched.iter().take(max);
    let lines: Vec<String> = shown
        .map(|(rel, size)| format!("{rel}  ({} 字节)", size))
        .collect();
    let mut out = format!("共 {} 个文件匹配 {}：\n{}", total, pattern, lines.join("\n"));
    if total > max {
        out.push_str(&format!("\n（另有 {} 个文件未展示，可缩小模式范围）", total - max));
    }
    Ok(out)
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
// ─────────────────────────── 网页抓取工具（webfetch，对齐主流 WebFetch） ───────────────────────────

/// 拒绝访问内网/本机地址（SSRF 防护）：localhost、回环、链路本地、私有网段与未指定地址。
fn is_private_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']);
    if host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("::1")
        || host == "0.0.0.0"
    {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_private() || v4.is_unspecified() || v4.is_link_local()
            }
            std::net::IpAddr::V6(v6) => {
                // IPv4-mapped IPv6（如 ::ffff:127.0.0.1）按 IPv4 语义判断，防绕过
                if let Some(v4) = v6.to_ipv4_mapped() {
                    v4.is_loopback() || v4.is_private() || v4.is_unspecified() || v4.is_link_local()
                } else {
                    v6.is_loopback()
                        || v6.is_unspecified()
                        || v6.is_unicast_link_local()
                        || v6.is_unique_local()
                }
            }
        }
    } else {
        false // 域名不做 DNS 解析（避免 DNS 重绑定，域名默认放行）
    }
}

/// 从 HTML 中提取可读正文文本（scraper 结构化提取，跳过 script/style/nav 等）。
fn extract_readable_text(html: &str) -> String {
    let doc = Html::parse_document(html);
    // 优先 article/main，回退 body，再回退根元素
    for sel in ["article", "main", "body"] {
        if let Ok(selector) = Selector::parse(sel) {
            if let Some(el) = doc.select(&selector).next() {
                let mut out = String::new();
                collect_readable(&el, &mut out, 0);
                let t = out.trim();
                if !t.is_empty() {
                    return t.to_string();
                }
            }
        }
    }
    doc.root_element()
        .text()
        .collect::<String>()
        .trim()
        .to_string()
}

/// 递归收集块级可读文本：p/h1-h6/li/pre/blockquote 直接输出文本，
/// div/section/table 等容器递归进入；script/style/nav/form 等跳过。
fn collect_readable(el: &ElementRef<'_>, out: &mut String, depth: usize) {
    if depth > 12 {
        return;
    }
    let name: String = el.value().name.local.as_ref().to_string();
    if matches!(
        name.as_str(),
        "script" | "style" | "nav" | "iframe" | "noscript" | "svg" | "form" | "button" | "input"
    ) {
        return;
    }
    let is_leaf_block = matches!(
        name.as_str(),
        "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li" | "pre" | "blockquote"
    );
    if is_leaf_block {
        let t = el.text().collect::<String>();
        let trimmed = t.trim();
        if !trimmed.is_empty() {
            out.push_str(trimmed);
            out.push('\n');
        }
        return;
    }
    for child in el.children() {
        if let Some(ce) = ElementRef::wrap(child) {
            collect_readable(&ce, out, depth + 1);
        }
    }
}

/// 抓取网页并提取正文文本（对齐主流 Agent 的 WebFetch）。
///
/// 安全与护栏：仅 http/https、拒绝内网地址（SSRF）、响应 ≤200KB、10s 超时、
/// 提取文本 ≤50K 字符截断、内容过提示注入防护；标题经 ammonia 消毒。
pub async fn webfetch(url: &str, max_chars: usize) -> Result<String, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("URL 解析失败: {}", e))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("仅支持 http/https 协议".into());
    }
    // 禁止自动重定向：手动逐跳校验目标地址（防公网 302 → 内网/云元数据的 SSRF 绕过）
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(WEBFETCH_TIMEOUT_SECS))
        .user_agent("mdgo-agent/1.0")
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("HTTP 客户端构建失败: {}", e))?;
    let mut current = parsed.clone();
    let mut redirects = 0u32;
    let resp = loop {
        // 每跳（含初始 URL）校验 host：IP 直接判断；域名先解析，解析出的任一地址
        // 命中内网/回环即拒绝（防 DNS 重绑定与指向内网的域名绕过 SSRF 防护）
        let host = current
            .host_str()
            .ok_or_else(|| "URL 缺少主机名".to_string())?
            .to_string();
        if is_private_host(&host) {
            return Err("拒绝访问内网/本机地址（SSRF 防护）".into());
        }
        if current.host().is_some() && current.host_str().is_some_and(|h| h.parse::<std::net::IpAddr>().is_err()) {
            // 域名：解析所有地址并校验
            let addr = current
                .socket_addrs(|| None)
                .map_err(|e| format!("域名解析失败: {}", e))?;
            if addr.iter().any(|sa| is_private_host(&sa.ip().to_string())) {
                return Err("拒绝访问解析到内网/本机地址的域名（SSRF 防护）".into());
            }
        }
        // 重定向后复查协议（防 https→http 降级与非 http(s) location）
        if current.scheme() != "http" && current.scheme() != "https" {
            return Err("仅支持 http/https 协议".into());
        }
        let resp = client
            .get(current.clone())
            .send()
            .await
            .map_err(|e| format!("请求失败: {}", e))?;
        if resp.status().is_redirection() {
            redirects += 1;
            if redirects > WEBFETCH_MAX_REDIRECTS {
                return Err("重定向次数过多".into());
            }
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| "重定向缺少 Location 头".to_string())?;
            current = current
                .join(loc)
                .map_err(|e| format!("重定向地址解析失败: {}", e))?;
            continue;
        }
        break resp;
    };
    if !resp.status().is_success() {
        return Err(format!("HTTP {} 错误", resp.status()));
    }
    // 流式读取并截断（防恶意服务器无限发送耗尽内存）
    let mut body: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("读取响应失败: {}", e))?;
        body.extend_from_slice(&chunk);
        if body.len() > WEBFETCH_MAX_BODY_BYTES {
            return Err("响应体超过 200KB，拒绝抓取".into());
        }
    }
    let html = String::from_utf8_lossy(&body);
    // 正文提取：readability（去除导航/页脚噪音）→ ammonia 消毒 → htmd 转 Markdown；
    // 任一环节失败回退 scraper 文本提取，保证健壮。
    let html_owned = html.to_string();
    let mut cursor = std::io::Cursor::new(html_owned.as_bytes());
    let (mut text, title) = match readability::extractor::extract(&mut cursor, &parsed) {
        Ok(product) => {
            let clean = ammonia::clean(&product.content);
            let body_text = match htmd::convert(&clean) {
                Ok(md) => md,
                Err(_) => extract_readable_text(&clean),
            };
            let body_text = if body_text.trim().is_empty() {
                extract_readable_text(&html)
            } else {
                body_text
            };
            (body_text, product.title.trim().to_string())
        }
        Err(_) => (extract_readable_text(&html), String::new()),
    };
    // 标题（不可信来源）经 ammonia 消毒后附加（纵深防御）
    if !title.is_empty() {
        let clean_title = ammonia::clean(&title).trim().to_string();
        if !clean_title.is_empty() {
            text = format!("标题：{}\n\n{}", clean_title, text);
        }
    }
    let limit = max_chars.clamp(1000, WEBFETCH_MAX_CHARS);
    let out = if text.chars().count() > limit {
        let cut: String = text.chars().take(limit).collect();
        format!("{}（内容过长已截断，共 {} 字符）", cut, text.chars().count())
    } else {
        text
    };
    Ok(crate::core::security::wrap_suspicious(&out))
}

/// 构建 webfetch 工具：抓取网页正文（只读）。

// ─────────────────────────── 任务清单工具（todo，对齐主流 TodoWrite） ───────────────────────────

/// 任务清单条目（完成状态 + 文本）
#[derive(Clone)]
struct TodoItem {
    done: bool,
    text: String,
}

/// 任务清单存储：按 request_id 隔离（一次 Agent 请求内多轮工具调用共享）。
///
/// 请求级生命周期：`agent_query` 开始时 `reset_todo` 清空；容量上限兜底防泄漏
/// （超限清最旧，与 ToolCallBus 同策略），保证高并发下不膨胀。
static TODO_STORE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, Vec<TodoItem>>>,
> = std::sync::OnceLock::new();

/// 单个请求最多缓存的 todo 条目（防止超长清单撑爆上下文）
const MAX_TODO_ITEMS: usize = 50;
/// 并发请求 todo 缓存上限（超限清最旧）
const MAX_TODO_REQUESTS: usize = 128;

fn todo_store() -> &'static std::sync::Mutex<std::collections::HashMap<String, Vec<TodoItem>>> {
    TODO_STORE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 清空指定请求的任务清单（agent_query 开始时调用，保证请求级隔离）。
pub fn reset_todo(request_id: &str) {
    if let Ok(mut store) = todo_store().lock() {
        store.remove(request_id);
    }
}

/// 执行一次任务清单操作，返回更新后的清单文本（模型据此跟踪任务进度）。
pub async fn todo_write(
    cfg: &KbSearchConfig,
    action: &str,
    items: &[String],
) -> Result<String, String> {
    let mut store = todo_store().lock().unwrap_or_else(|e| e.into_inner());
    // 容量上限：新请求进入且已满时淘汰最旧请求的清单（防泄漏）
    if store.len() >= MAX_TODO_REQUESTS && !store.contains_key(&cfg.request_id) {
        if let Some(oldest) = store.keys().next().cloned() {
            store.remove(&oldest);
        }
    }
    let list = store.entry(cfg.request_id.clone()).or_default();
    match action {
        "add" => {
            for it in items {
                let text = it.trim();
                if !text.is_empty() && list.len() < MAX_TODO_ITEMS {
                    list.push(TodoItem { done: false, text: text.to_string() });
                }
            }
        }
        "complete" => {
            if items.is_empty() {
                for it in list.iter_mut() {
                    it.done = true;
                }
            } else {
                for it in list.iter_mut() {
                    if items.iter().any(|t| t.trim() == it.text) {
                        it.done = true;
                    }
                }
            }
        }
        "remove" => {
            if items.is_empty() {
                list.clear();
            } else {
                list.retain(|it| !items.iter().any(|t| t.trim() == it.text));
            }
        }
        "clear" => list.clear(),
        "replace" => {
            list.clear();
            for it in items {
                let text = it.trim();
                if !text.is_empty() && list.len() < MAX_TODO_ITEMS {
                    list.push(TodoItem { done: false, text: text.to_string() });
                }
            }
        }
        _ => {
            return Err(format!(
                "未知 action：{}（支持 add/complete/remove/clear/replace）",
                action
            ));
        }
    }
    if list.is_empty() {
        return Ok("任务清单为空".into());
    }
    let lines: Vec<String> = list
        .iter()
        .enumerate()
        .map(|(i, it)| format!("{}. [{}] {}", i + 1, if it.done { "x" } else { " " }, it.text))
        .collect();
    Ok(format!("任务清单（{} 项）：\n{}", list.len(), lines.join("\n")))
}

/// 构建 todo_write 工具：模型在长任务执行中维护任务清单（对齐主流 Agent 的 TodoWrite）。

// ─────────────────────────── 用户澄清提问工具（P1-4） ───────────────────────────

/// 构建 ask_user_question 工具：任务信息不足时向用户提出澄清问题（对齐 DSH
/// `ask_user_question` seam）。模型在歧义场景（需求含糊、多选一、缺关键参数）
/// 应主动询问而非猜测。
///
/// 通道与审批/规划确认同构：oneshot 挂起表（`AppState.user_question_pending`）
/// + `question:request` 事件 → 前端弹窗 → `question_respond` IPC 回传。
/// 超时（见 `limits::ASK_USER_TIMEOUT_SECS`）与父链取消均视为「未回答」，
/// 返回引导让模型改用已有信息作答或如实说明缺口。

/// 构建 remember 工具：写入一条跨会话长期记忆。
///
/// 记忆随会话持久化（全局用户数据目录），后续请求按关键词检索注入，
/// 沉淀用户偏好、项目约定与已验证结论。

/// 构建 forget 工具：删除一条记忆。

/// 构建 search_memory 工具：按关键词检索相关长期记忆（只读）。

/// 构建 search_bookmarks 工具：检索用户收藏的书签知识资产（只读）。
/// FTS5（title/description/summary/tags/category）∪ 向量补位；排除 ARCHIVED。

/// 构建 get_bookmark 工具：按 id 获取书签详情（只读）。

// ─────────────────────────── 泛化子代理执行（P1-9） ───────────────────────────

/// 公共子代理执行器：从 AppState 组装 LLM 客户端与规约，构造
/// [`SubagentSpec`] 并运行 [`SubagentRunner`]，全量输出入 LRU 存储。
///
/// 返回 `(sub_request_id, outcome)`；deep_research / spawn_subagent /
/// parallel_research 共用（单一职责，避免三处重复组装逻辑）。
/// `pub(crate)`：loop_tools（新内核迁移工具）同样复用。
pub(crate) async fn run_subagent_impl(
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
    if llm_cfg.endpoint.trim().is_empty() || llm_cfg.model.trim().is_empty() {
        return Err("LLM 未配置或构建失败".to_string());
    }
    let adapter = crate::core::agent::loop_tools::build_loop_adapter(&llm_cfg);
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
    let outcome =
        crate::core::subagent::SubagentRunner::run(adapter, cfg.clone(), base_rules, &spec).await;

    // 完整输出入存储（LRU 有界：最多保留 16 条，按最近访问淘汰）
    state
        .subagent_results
        .insert(sub_request_id.clone(), outcome.full_output.clone());
    Ok((sub_request_id, outcome))
}
