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

use crate::core::agent::limits::*;
use crate::core::agent::KbSearchConfig;
use crate::core::db::utils::IgnoreMatcher;
use crate::core::skill::activation::{
    ActiveSkillState, ActivationSource, SkillLifetime, MAX_SKILL_BODY_CHARS,
};
use crate::core::skill::SkillRegistry;
use crate::core::subagent::{
    SubagentMode, SubagentRunner, SubagentSpec, SUBAGENT_MAX_TURNS, SUBAGENT_SUMMARY_CHARS,
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

    /// 查看最后一个成功工具调用的结果文本（不消费，不 drain）。
    ///
    /// 用于兜底：模型调用了工具并成功返回结果，但未生成文本回复时，
    /// 将工具**结果内容**（result，截断到轨迹上限）作为最终回复，
    /// 避免把 summary 元数据（如"7496 字符"）误当内容发给用户。
    pub fn peek_last_success_result(&self, request_id: &str) -> Option<String> {
        if let Ok(map) = self.map.lock() {
            if let Some(events) = map.get(request_id) {
                return events
                    .iter()
                    .rev()
                    .find(|e| e.kind == "result" && e.ok)
                    .map(|e| {
                        if e.result.trim().is_empty() {
                            e.summary.clone()
                        } else {
                            e.result.clone()
                        }
                    });
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
/// 2. 当前激活技能目录下的相对路径（如 `references/flowchart.md`），
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
        Err(e) if cfg.skill_state.active_only().is_empty() => {
            // 无任何已激活（Active）技能：若目标是技能参考路径，明确指出需先激活技能，
            // 避免模型误以为文件不存在而反复尝试（浪费多轮工具调用）
            return Err(skill_ref_hint(rel_path, e));
        }
        Err(_) => {}
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
        "在知识库目录内的文本文件中搜索关键词（大小写不敏感子串匹配，跳过二进制与超大文件）。已按用户配置的目录/文件黑名单过滤（如 assets/、node_modules/ 等配置的目录不会被搜索）。输出格式：每个命中文件先输出一行相对路径，随后每行\"  行号: 内容\"；context_lines>0 时匹配行以 \">\" 开头、上下文行以空格开头、非连续区间用 \"--\" 分隔；list_only=true 时仅输出文件名。pattern 支持多关键词（空格分隔）：默认 and 模式（文件需同时包含所有词，词可出现在不同行），可设 match_mode=\"or\"（含任一词即命中）；用双引号包裹 pattern 可精确搜索连续短语（如 pattern=\"\\\"fn main()\\\"\"）。include/exclude 支持 glob 与目录名：include:[\"*.rs\",\"*.md\"] 限定文件类型，exclude:[\"target/**\",\"dist/**\"] 排除目录，目录名（如 \"src\"）自动展开为其下全部文件。\n使用建议：\n- 快速定位哪些文件包含目标文本：list_only=true（只返回文件名，省 token）\n- 需要看懂代码片段周边逻辑：context_lines=3（返回命中行前后 3 行，最大 5）\n- 缩小搜索范围减少耗时：include:[\"*.rs\"] 或 include:[\"src\"]（目录名）\n- 搜索连续代码片段：用双引号包裹 pattern，如 pattern=\"\\\"fn handle_request(\\\"\"\n- 多个术语任选其一：match_mode=\"or\"\n定位后建议用 read 工具精读相关行（read 支持 offset 分页）。",
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
                    "maximum": MAX_GREP_FILES,
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
                    "maximum": GREP_CONTEXT_MAX,
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
    if content.chars().count() > MAX_EDIT_FILE_BYTES as usize {
        return Err(format!("{} 内容超过 1MB，write 单次写入上限为 1MB", rel_path));
    }
    // Canvas 格式自动处理：.canvas 内容先经确定性校验/规整，再落盘
    let effective = if rel_path.ends_with(".canvas") {
        canvas::validate_canvas_json(content, &cfg.dir_path)?
    } else {
        content.to_string()
    };
    // 自动创建父目录：拒绝绝对路径与 `..`（与 safe_resolve_new 一致的口径），
    // 先 base.join 创建目录，再交由 safe_resolve_new canonicalize 二次校验，
    // 符号链接逃逸仍会被拦截（父目录不在 base 内则报错）。
    let rel = std::path::Path::new(rel_path);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("路径越界：仅允许访问限定目录内的文件".into());
    }
    let base = std::fs::canonicalize(&cfg.dir_path).map_err(|e| format!("无法访问目录: {}", e))?;
    if let Some(parent) = rel.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(base.join(parent))
                .map_err(|e| format!("创建目录失败: {}", e))?;
        }
    }
    let full = safe_resolve_new(&cfg.dir_path, rel_path)?;
    // canonical 后二次校验 `.mdgo`（防 `..` 穿越；父目录已 canonicalize）
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
pub fn build_write_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "write",
        "创建新文件或整体覆盖当前打开知识库目录内的文本文件。content 为文件的完整新内容（覆盖写，非追加）。适合新建文档/笔记/代码文件，或整体重写小文件（≤1MB）。只允许在打开目录内写入，父目录不存在时会自动创建，不允许写入 .mdgo 内部数据。**当目标扩展名为 .canvas 时：内容必须是 JSON Canvas（{nodes, edges}），写入前系统校验 JSON 合法性、节点 id 唯一化、连线引用完整性、file 路径存在性与坐标/尺寸合法性——布局与坐标由模型提供并原样保留，系统不重排；内容不合法或节点缺有效尺寸时写入被拒绝并返回原因。** 写入为不可撤销操作，覆盖已有文件前请确认用户意图。",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "rel_path": {
                    "type": "string",
                    "description": "文件在知识库根目录下的相对路径，如 docs/new-note.md"
                },
                "content": {
                    "type": "string",
                    "maxLength": MAX_EDIT_FILE_BYTES as usize,
                    "description": "文件的完整新内容（UTF-8 文本，最大 1MB）"
                }
            },
            "required": ["rel_path", "content"]
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
                let content = args
                    .get("content")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string();
                if rel.is_empty() {
                    return Err(tool_error("write", "rel_path 为空"));
                }
                let preview = format!(
                    "{}: {} 字符",
                    rel,
                    content.chars().count()
                );
                record_tool_call(&cfg, "write", &preview, Some(&args));
                match write_file(&cfg, &rel, &content).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "write", true, &text, Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "write", false, &e, Some(&e));
                        Err(tool_error("write", &e))
                    }
                }
            })
        },
    )
}

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
fn tool_error(tool: &str, msg: &str) -> ToolExecutionError {
    ToolExecutionError::other(format!(
        "{tool} 执行失败: {msg}。请根据错误信息修正参数后重试，或改用其他工具；如问题持续存在，请如实告知用户，不要声称操作已成功。"
    ))
    .with_model_output(ToolOutput::text(format!(
        "{tool} 执行失败: {msg}。请修正参数后重试或改用其他工具。"
    )))
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
    cfg: KbSearchConfig,
) -> DynamicTool {
    DynamicTool::new(
        "activate_skill",
        "激活一个技能以加载其详细指令（SKILL.md 正文核心段，一次性提供、不重复注入）并解锁其声明的专用工具。技能 ID 见常驻技能目录；仅当目录中的技能与当前任务明确相关时才调用。激活后：1) 正文随本工具结果一次性进入上下文，后续轮次不再重复注入，请遵循其中的流程与输出规范；2) 其声明的检索工具（如 kb_search）将可用；3) 可用 read 工具读取其 references/ 下的参考资料；正文被截断时可用 read 读取 {skill_id}/SKILL.md 获取完整内容。重复激活同一技能只会返回已激活提示，不会重复返回正文。",
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
            let cfg = cfg.clone();
            Box::pin(async move {
                let id = args
                    .get("skill_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                // Mutation Verification 轨迹：skill 激活需走 ToolCallBus，前端 tool-trace
                // 依赖 agent:tool_call / agent:tool_result 事件（此前缺失导致不显示）
                record_tool_call(&cfg, "activate_skill", &id, Some(&args));
                // 内部 async 块：分支 `return` 提前结束本块并携带结果，外层统一 record
                let result: Result<ToolOutput, ToolExecutionError> = async {
                if id.is_empty() {
                    return Err(tool_error("activate_skill", "skill_id 为空"));
                }
                // 幂等：技能已激活且正文已注入 → 返回已激活提示，不重复返回正文
                if state.is_loaded(&id) {
                    let desc = registry
                        .find_enabled(&id)
                        .map(|s| s.description.trim().to_string())
                        .unwrap_or_default();
                    let mut msg = format!(
                        "技能 '{}' 已激活且指令已注入，本请求内不会重复注入正文。",
                        id
                    );
                    if !desc.is_empty() {
                        msg.push_str(&format!(" 说明：{}", desc));
                    }
                    return Ok(ToolOutput::text(msg));
                }
                let skill = registry.find_enabled(&id).ok_or_else(|| {
                    tool_error(
                        "activate_skill",
                        &format!("技能 '{}' 不存在或未启用，请从技能目录中选择", id),
                    )
                })?;
                // 正文核心段：截断到单次注入预算（完整内容由 read {id}/SKILL.md 兜底）
                let body = skill.body.trim();
                let body_chars = body.chars().count();
                let body_short: String = if body_chars > MAX_SKILL_BODY_CHARS {
                    body.chars().take(MAX_SKILL_BODY_CHARS).collect()
                } else {
                    body.to_string()
                };
                let truncated = body_short.chars().count() < body_chars;
                // 激活会话挂载（warm）中的技能时保留 Session 生命周期（P5 跨请求恢复）；
                // 其余动态激活为 Turn（请求结束失效）
                let lifetime = if state
                    .activated()
                    .iter()
                    .any(|a| a.skill_id == id && a.lifetime == SkillLifetime::Session)
                {
                    SkillLifetime::Session
                } else {
                    SkillLifetime::Turn
                };
                state.activate(&skill, lifetime, ActivationSource::Llm, true);
                // XML 标识包装：多技能并存时明确规则边界，避免模型混淆
                let mut msg = format!(
                    "<active_skill id=\"{}\" version=\"{}\" source=\"llm\">\n{}\n</active_skill>",
                    id, skill.version, body_short
                );
                if truncated {
                    msg.push_str(&format!(
                        "\n\n[技能正文超过单次注入预算（{} 字符），已显示前 {} 字符；如需完整内容，可用 read 读取 '{}/SKILL.md'（已激活技能目录内）]",
                        body_chars, MAX_SKILL_BODY_CHARS, id
                    ));
                }
                if !skill.description.trim().is_empty() {
                    msg.push_str(&format!("\n\n说明：{}", skill.description.trim()));
                }
                if !skill.tools.is_empty() {
                    msg.push_str(&format!(
                        "\n专用工具：{}",
                        skill.tools
                            .iter()
                            .map(|t| format!("`{t}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                Ok(ToolOutput::text(msg))
                }.await;
                // Mutation Verification 轨迹：统一记录激活结果（成功/失败）
                match &result {
                    Ok(out) => {
                        let t = out.as_text().unwrap_or("").to_string();
                        record_tool_result(&cfg, "activate_skill", true, &truncate(&t, 200), Some(&t));
                    }
                    Err(e) => {
                        let m = e.to_string();
                        record_tool_result(&cfg, "activate_skill", false, &truncate(&m, 200), Some(&m));
                    }
                }
                result
            })
        },
    )
}

/// 构建 deactivate_skill 工具：释放已激活技能（停止指令注入与专用工具，渐进式披露回退）。
pub fn build_deactivate_skill_tool(state: Arc<ActiveSkillState>, cfg: KbSearchConfig) -> DynamicTool {
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
            let cfg = cfg.clone();
            Box::pin(async move {
                let id = args
                    .get("skill_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                // Mutation Verification 轨迹：skill 停用需走 ToolCallBus（前端 tool-trace 依赖）
                record_tool_call(&cfg, "deactivate_skill", &id, Some(&args));
                let result: Result<ToolOutput, ToolExecutionError> = async {
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
                }.await;
                // Mutation Verification 轨迹：统一记录停用结果（成功/失败）
                match &result {
                    Ok(out) => {
                        let t = out.as_text().unwrap_or("").to_string();
                        record_tool_result(&cfg, "deactivate_skill", true, &truncate(&t, 200), Some(&t));
                    }
                    Err(e) => {
                        let m = e.to_string();
                        record_tool_result(&cfg, "deactivate_skill", false, &truncate(&m, 200), Some(&m));
                    }
                }
                result
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
                    if paths.len() > READ_PATHS_MAX {
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
            "additionalProperties": false,
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

/// 构建 multi_edit 工具：批量编辑多个文件（一次调用完成多文件精确替换，节省轮次预算）。
pub fn build_multi_edit_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "multi_edit",
        "批量编辑多个文件：一次调用对多个文件（或同一文件多处）执行精确替换。edits 为数组，每项含 rel_path/old_string/new_string，old_string 必须在对应文件中唯一匹配（先 read 确认原文）。所有编辑会先全量校验（路径安全/UTF-8/唯一性），任一失败则整体不执行任何修改；全部通过后一次性写入。最多 10 个 edit。适合一次修改多个文件的相似片段（如批量重命名、批量加注释），相比逐次调用 edit 更省轮次。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "edits": {
                    "type": "array",
                    "maxItems": MAX_MULTI_EDITS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "rel_path": {
                                "type": "string",
                                "description": "文件在知识库根目录下的相对路径，如 docs/note.md"
                            },
                            "old_string": {
                                "type": "string",
                                "description": "待替换的原文片段，必须与文件内容完全一致且在文件中唯一出现"
                            },
                            "new_string": {
                                "type": "string",
                                "description": "替换后的新内容"
                            }
                        },
                        "required": ["rel_path", "old_string", "new_string"]
                    }
                }
            },
            "required": ["edits"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let edits: Vec<(String, String, String)> = args
                    .get("edits")
                    .and_then(|e| e.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| {
                                let rel = item.get("rel_path").and_then(|s| s.as_str())?.trim().to_string();
                                let old = item.get("old_string").and_then(|s| s.as_str())?.to_string();
                                let new = item.get("new_string").and_then(|s| s.as_str())?.to_string();
                                Some((rel, old, new))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let preview = format!("{} 个文件编辑", edits.len());
                record_tool_call(&cfg, "multi_edit", &preview, Some(&args));
                match multi_edit_files(&cfg, &edits).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "multi_edit", true, &text, Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "multi_edit", false, &e, Some(&e));
                        Err(tool_error("multi_edit", &e))
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
            "additionalProperties": false,
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

/// 构建 ls 工具（对齐主流 Agent 的 ls 命名）。
fn build_list_files_dyn(name: &str, cfg: KbSearchConfig) -> DynamicTool {
    let name_owned = name.to_string();
    DynamicTool::new(
        name_owned.clone(),
        "列举知识库目录下的文件与子目录（返回相对路径与大小），支持按名称子串过滤，最多返回 60 项。已按用户配置的目录/文件黑名单过滤（如 assets/、node_modules/、dist/ 等配置的目录不会列出；系统内置的 .mdgo 内部数据同样排除）。当需要了解知识库目录结构、或不确定文件路径时调用。",
        serde_json::json!({
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
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            let name = name_owned.clone();
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
                record_tool_call(&cfg, &name, &preview, Some(&args));
                match list_files(&cfg, &pattern, max_items).await {
                    Ok(text) => {
                        record_tool_result(&cfg, &name, true, &format!("{} 项", text.lines().count().saturating_sub(1)), Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, &name, false, &e, Some(&e));
                        Err(tool_error(&name, &e))
                    }
                }
            })
        },
    )
}

/// 构建 ls 工具
pub fn build_ls_tool(cfg: KbSearchConfig) -> DynamicTool {
    build_list_files_dyn("ls", cfg)
}

/// 按 glob 模式列举知识库内匹配的文件（对齐主流 Agent 的 glob 能力）。
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

/// 构建 glob 工具：按 glob 模式列举知识库内匹配的文件。
pub fn build_glob_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "glob",
        "按 glob 模式列举当前打开知识库目录内匹配的文件（相对路径 + 字节大小）。模式支持 *（单层任意）、**（任意层级）、?（单字符）、[abc]（字符集）；含 / 的模式锚定根目录，裸文件名（如 *.rs）匹配任意层级的 basename；目录名（如 src）自动展开为其下全部文件。已按用户配置的目录/文件黑名单过滤（如 assets/、node_modules/ 等配置的目录不会出现）。最多返回 60 个匹配文件，超出会提示剩余数量。用于快速定位文件与批量确认路径，比 grep 更轻量。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "glob 模式，如 **/*.rs、docs/*.md、*.json"
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
                record_tool_call(&cfg, "glob", &pattern, Some(&args));
                match glob_files(&cfg, &pattern, max_items).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "glob", true, &format!("{} 项", text.lines().count().saturating_sub(1)), Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "glob", false, &e, Some(&e));
                        Err(tool_error("glob", &e))
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

/// 构建 git_diff 工具：查询 Git 工作区/暂存区差异（只读）。
pub fn build_git_diff_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "git_diff",
        "查看知识库所在 Git 仓库的工作区/暂存区差异（默认工作区未暂存改动；staged=true 查看已暂存改动；stat_only=true 只显示文件级统计）。只读操作。当需要了解具体改了什么、对比文件内容时调用。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "staged": {
                    "type": "boolean",
                    "description": "是否查看暂存区（已 git add）差异，默认 false 查看工作区差异"
                },
                "stat_only": {
                    "type": "boolean",
                    "description": "是否只显示文件级统计（--stat），默认 false 显示完整差异"
                }
            },
            "required": []
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let staged = args.get("staged").and_then(|v| v.as_bool()).unwrap_or(false);
                let stat_only = args.get("stat_only").and_then(|v| v.as_bool()).unwrap_or(false);
                record_tool_call(&cfg, "git_diff", &format!("staged={} stat={}", staged, stat_only), Some(&args));
                match git_diff(&cfg.dir_path, staged, stat_only).await {
                    Ok((text, structured)) => {
                        record_tool_result_structured(&cfg, "git_diff", true, &format!("{} 字符", text.chars().count()), Some(&text), structured);
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "git_diff", false, &e, Some(&e));
                        Err(tool_error("git_diff", &e))
                    }
                }
            })
        },
    )
}

/// 构建 git_commit 工具：提交暂存区改动（写操作，需用户确认）。
pub fn build_git_commit_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "git_commit",
        "将已暂存（git add）的改动提交为一次 Git commit（写操作，会修改仓库历史，需用户确认）。message 为提交说明。提交前建议先用 git_status 查看待提交内容、用 git_diff(staged=true) 确认差异。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "minLength": 1,
                    "description": "提交说明（commit message）"
                }
            },
            "required": ["message"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let message = args
                    .get("message")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string();
                let preview = format!("{} 字符", message.chars().count());
                record_tool_call(&cfg, "git_commit", &preview, Some(&args));
                match git_commit(&cfg.dir_path, &message).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "git_commit", true, &text, Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "git_commit", false, &e, Some(&e));
                        Err(tool_error("git_commit", &e))
                    }
                }
            })
        },
    )
}

/// 构建 git_checkout 工具：恢复工作区文件到 HEAD（写操作，需用户确认）。
pub fn build_git_checkout_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "git_checkout",
        "将工作区文件恢复到 HEAD 版本（丢弃未提交的修改，写操作且不可恢复，需用户确认）。paths 为要恢复的文件相对路径列表（最多 20 个）。恢复前请与用户确认，未暂存且未提交的修改会丢失。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "paths": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 20,
                    "items": { "type": "string" },
                    "description": "要恢复到 HEAD 的文件相对路径列表"
                }
            },
            "required": ["paths"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let paths: Vec<String> = args
                    .get("paths")
                    .and_then(|a| a.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                record_tool_call(&cfg, "git_checkout", &format!("{} 个文件", paths.len()), Some(&args));
                match git_checkout(&cfg.dir_path, &paths).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "git_checkout", true, &text, Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "git_checkout", false, &e, Some(&e));
                        Err(tool_error("git_checkout", &e))
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
                // 软门禁（替代 rig active_tools 硬过滤）：pomodoro/raw-parse 始终可见
                // 可调，但仅当声明它的技能已激活时才执行；未激活返回引导，避免
                // UnknownToolCall 导致整个流式请求失败。allowed_tools()=None（无技能
                // 激活，含子代理）→ 放行；Some 且未声明该工具 → 引导。
                let declared = cfg.skill_state.allowed_tools();
                let unlocked = declared
                    .as_ref()
                    .is_none_or(|list| list.iter().any(|t| t == &tool));
                if !unlocked {
                    let msg = format!(
                        "{} 需要先激活声明它的技能（调用 activate_skill，从技能目录选择）后才能执行，本次未执行。请先激活对应技能，再重新发起操作。",
                        tool
                    );
                    log::info!("[agent] {} 未激活技能被调用，返回引导 request_id={}", tool, cfg.request_id);
                    return Ok(ToolOutput::text(msg));
                }
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
    )
}

/// 构建 raw 工具：解析 RAW 照片文件（.arw/.cr2/.nef/.dng 等）的元数据。
///
/// 动作与参数对齐 `resources/skills/raw/SKILL.md`：
/// - `parse`：解析 RAW 文件元数据，返回三大类 Markdown（相机·镜头 / 拍摄参数 / 图像信息），
///   `path` 为知识库内相对路径
///
/// （`mdgo.core.raw.parse`），经 FrontendBridge 回传 Markdown 文本。
pub fn build_raw_tool(cfg: KbSearchConfig) -> DynamicTool {
    build_bridge_tool(
        cfg,
        "raw-parse",
        "解析 RAW 照片文件（.arw/.cr2/.nef/.dng/.orf 等）的元数据并输出为 Markdown。动作：parse（path 为知识库内 RAW 文件的相对路径，返回相机·镜头、拍摄参数、图像信息）。当用户要求查看 RAW 照片信息、解析相机拍摄参数时调用。",
        serde_json::json!({
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
    )
}

/// 构建 open-ui 工具：打开知识库文件 / 跳转应用页面（经 FrontendBridge 调前端 toggleFile）。
///
/// - `open_file`：打开知识库内文件（`relativePath` 为知识库内相对路径，前端校验防穿越 + 字符白名单；
///   会切换当前工作区到该文件，可能打断正在编辑的文件——需注意副作用）
/// - `open_page`：跳转应用页面/视图（`page` 仅支持枚举内 29 种：图谱/日历/思维导图/看板/AI 对话/
///   知识库/技能/MCP/文件类型分布/时间线/白板/词云/Git 记录/番茄钟/临时编辑器/URL 编码/视频/RAW/正则/
///   Cron/书签/目录空间/Swagger/GraphQL/OpenResty 等）
///
/// 仅打开查看，不修改文件内容；写操作（删除/复制/还原）不暴露给模型（白名单在
/// 前端 handler 侧实现）。复用 `build_bridge_tool`：软门禁 + 动作解析 + 轨迹 + 5s 桥超时兜底。
/// 业务结果结构化：前端 handler 失败时回传 {ok:false}（文件不存在/页面切换被取消不再谎报成功）。
pub fn build_open_ui_tool(cfg: KbSearchConfig) -> DynamicTool {
    build_bridge_tool(
        cfg,
        "open-ui",
        "打开知识库文件或跳转打开应用页面。动作：open_file 在系统中打开文件预览的 ui（会切换当前工作区到该文件，可能打断正在编辑的文件）；open_page 跳转系统 ui 页面/视图（仅支持下列 page 枚举，共 26 种：fileGraph 文件图谱、noteGraph 文档关联图谱、dashboard 系统首页、calendar 日历/日程、knowledge 知识库监控面板、skill 技能管理页面、mcp MCP 管理页面、timeline 文件时间线页面、canvas 画布、whiteboard 白板、mermaid mermaid 图表预览编辑页面、wordCloud 词云、gitRecords Git 管理页面、pomodoro 番茄钟页面、tempEditor 临时编辑器、urlEncoder 编码器页面、video 视频播放页面、raw RAW 照片预览页面、regexTest 正则表达式测试页面、cron Cron 表达式测试页面、bookmarks 书签预览页面、dirSpace 目录空间数据统计大屏、swaggerDemo swagger api 预览页面、graphQLPlayground GraphQL 预览接口测试页面、openRestyEditor nginx 配置编辑器、fileType 文件类型分布）。仅打开查看，不修改文件内容；打开文件/跳转页面会切换当前工作区视图。当用户要求打开某个文件、跳转到某页面、查看图谱/日历/看板/思维导图等时调用。",
        serde_json::json!({
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
    )
}

/// 构建日程工具：直接调用 Rust 引擎 `core::schedule`（不经 FrontendBridge——逻辑已在 Rust）。
///
/// 动作与参数对齐 `resources/skills/schedule/SKILL.md`：
/// - `list`：全部日程（紧凑文本）
/// - `add`：新建日程（title/start/end/desc?/color?/cron?/notify?/notify_before?/type?/priority?/related?/ai?，Rust 校验 + 冲突提示与备选建议）
/// - `update` / `remove`：按 id 更新/删除
/// - `conflicts`：与 [start,end) 重叠的日程
/// - `remind`：到点应提醒的日程（支持 notify_before 提前提醒）
/// - `lunar`：某日农历/节假日/调休
/// - `next_available`：下一个可安排时间段（可跳过休息日，项目独有特性）
/// - `plan`：把 AI 拆解的任务（title+hours）排布到 deadline 前（只出建议，不创建）
/// - `optimize`：时间投入统计（供 AI 生成优化建议）
/// - `review`：某日日程复盘统计（完成/进行中/未开始）
/// - `focus`：专注时间块（可推荐或按指定开始时间创建 type=focus）
/// - `today_plan`：某日日程 + 空闲时间段（供 AI 生成今日/明日计划）
pub fn build_schedule_tool(cfg: KbSearchConfig) -> DynamicTool {
    let tool_dir = cfg.dir_path.clone();
    let tool_app = cfg.app_handle.clone();
    DynamicTool::new(
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
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let dir = tool_dir.clone();
            let app = tool_app.clone();
            let cfg = cfg.clone();
            Box::pin(async move {
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
                        _ => return Err(tool_error("schedule", &format!("未知动作: {}", raw_action))),
                    }
                } else {
                    raw_action.as_str()
                };
                // 软门禁（替代 rig active_tools 硬过滤）：
                // - allowed_tools()=None（无技能激活，含子代理独立技能态）→ 放行：
                //   全量工具模式下 schedule 本就对模型可见，可直接执行；
                // - Some 且不含 schedule（激活了其它技能，模型在该模式下不应看到本工具）→ 引导，
                //   避免幻觉调用导致整个流式请求失败（回答为空）。
                let declared = cfg.skill_state.allowed_tools();
                let unlocked = declared
                    .as_ref()
                    .is_none_or(|list| list.iter().any(|t| t == "schedule"));
                if !unlocked {
                    let msg = "当前技能集未声明 schedule 工具（已激活的技能未包含日程管理）。如需日程功能，请先调用 activate_skill（skill_id='schedule'）激活 schedule 技能，再重新发起操作；本次未执行。";
                    log::info!("[agent] schedule 未声明于当前技能集被调用，返回引导 request_id={}", cfg.request_id);
                    return Ok(ToolOutput::text(msg));
                }
                // Mutation Verification 轨迹：schedule 需走 ToolCallBus，前端 tool-trace
                // 依赖 agent:tool_call / agent:tool_result 事件（此前缺失导致不显示）
                record_tool_call(&cfg, "schedule", &format!("action={}", action), Some(&args));
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
                let store_ref = state.schedule_store(&dir).map_err(|e| tool_error("schedule", &e))?;
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
                let result: Result<ToolOutput, ToolExecutionError> = async {
                match action {
                    "list" => {
                        let store = store_guard();
                        let mut events = store.list().map_err(|e| tool_error("schedule", &e))?;
                        events.retain(|e| !is_snooze_reminder(e)); // 稍后提醒对 Agent 不可见
                        if is_reminder_op {
                            // reminder_list：只列提醒（event_type=reminder 的单点事件）
                            events.retain(|e| e.event_type == "reminder");
                        }
                        if events.is_empty() {
                            return Ok(ToolOutput::text(if is_reminder_op {
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
                        Ok(ToolOutput::text(format!(
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
                            return Err(tool_error("schedule", "日程标题不能为空"));
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
                                event.validate().map_err(|e| tool_error("schedule", &e))?;
                                // 冲突提示（同一锁内，避免并发下读到过期快照）
                                let mut conflict_events: Vec<ScheduleEvent> = Vec::new();
                                // 提醒（单点）与 Cron 事件不做日程冲突检测（提醒到点弹窗，不占日程档期）
                                if event.cron.trim().is_empty() && event.event_type != "reminder" {
                                    if let (Some(s), Some(e)) =
                                        (rules::parse_local_time(&event.start), rules::parse_local_time(&event.end))
                                    {
                                        let existing: Vec<ScheduleEvent> = store.list()
                                            .map_err(|e| tool_error("schedule", &e))?
                                            .into_iter()
                                            .filter(|e| !is_snooze_reminder(e))
                                            .collect();
                                        conflict_events = rules::find_conflicts(&existing, s, e, None);
                                    }
                                }
                                store.upsert(event.clone()).map_err(|e| tool_error("schedule", &e))?;
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
                                let events_snapshot = list_visible().map_err(|e| tool_error("schedule", &e))?;
                                let provider = state.schedule_day_info.clone();
                                let end_dt = rules::parse_local_time(&event.end).unwrap_or(now);
                                let alts = tokio::task::spawn_blocking(move || {
                                    crate::core::schedule::planner::suggest_alternatives(
                                        &events_snapshot, provider.as_ref(), duration, end_dt, true,
                                    )
                                })
                                .await
                                .map_err(|e| tool_error("schedule", &format!("生成备选建议失败: {}", e)))?;
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
                            Ok(ToolOutput::text(msg))
                        } else {
                            // 单锁内完成 读→改→写（消除锁窗口：并发写者在此期间插入/删除不会被覆盖）
                            let mut store = store_guard();
                            let id = get("id");
                            let target_title = get("target_title");
                            let mut events: Vec<ScheduleEvent> = store
                                .list()
                                .map_err(|e| tool_error("schedule", &e))?
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
                                    0 => return Err(tool_error(
                                        "schedule",
                                        &format!("未找到标题为『{}』的日程，请先 list 确认标题", target_title),
                                    )),
                                    n => return Err(tool_error(
                                        "schedule",
                                        &format!("标题『{}』匹配 {} 个日程，请改用 id 或更精确的标题", target_title, n),
                                    )),
                                }
                            } else {
                                return Err(tool_error("schedule", "update 需要 id 或 target_title 定位"));
                            };
                            let Some(existing) = matched_idx.map(|i| &mut events[i]) else {
                                return Err(tool_error("schedule", "日程不存在"));
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
                            existing.validate().map_err(|e| tool_error("schedule", &e))?;
                            let updated = existing.clone();
                            store.replace_all(events).map_err(|e| tool_error("schedule", &e))?;
                            let _ = app.emit("schedule:changed", ()); // 通知前端刷新
                            Ok(ToolOutput::text(if is_reminder_op {
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
                            return Err(tool_error("schedule", "remove 需要 id 或 title 定位"));
                        }
                        let mut store = store_guard();
                        let events: Vec<ScheduleEvent> = store
                            .list()
                            .map_err(|e| tool_error("schedule", &e))?
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
                                0 => return Err(tool_error(
                                    "schedule",
                                    &format!("未找到标题为『{}』的日程，请先 list 确认标题", title),
                                )),
                                n => return Err(tool_error(
                                    "schedule",
                                    &format!("标题『{}』匹配 {} 个日程，请改用 id 或更精确的标题", title, n),
                                )),
                            }
                        };
                        store.remove(&remove_id).map_err(|e| tool_error("schedule", &e))?;
                        let _ = app.emit("schedule:changed", ()); // 通知前端刷新
                        let msg = if removed_title.is_empty() {
                            if is_reminder_op { "提醒已删除".to_string() } else { "日程已删除".to_string() }
                        } else if is_reminder_op {
                            format!("已删除提醒：{}", removed_title)
                        } else {
                            format!("已删除日程：{}", removed_title)
                        };
                        Ok(ToolOutput::text(msg))
                    }
                    "conflicts" => {
                        let s = rules::parse_local_time(&get("start")).ok_or_else(|| tool_error("schedule", "开始时间格式无效"))?;
                        let e = rules::parse_local_time(&get("end")).ok_or_else(|| tool_error("schedule", "结束时间格式无效"))?;
                        let ignore = if get("ignore_id").is_empty() { None } else { Some(get("ignore_id")) };
                        let events = list_visible().map_err(|e| tool_error("schedule", &e))?;
                        let conflicts = rules::find_conflicts(&events, s, e, ignore.as_deref());
                        if conflicts.is_empty() {
                            Ok(ToolOutput::text("该时间段无冲突"))
                        } else {
                            let lines: Vec<String> = conflicts.iter().map(|c| format!("- {}：{} ~ {}", c.title, disp(&c.start), disp(&c.end))).collect();
                            Ok(ToolOutput::text(format!("时间冲突 {} 项：\n{}", conflicts.len(), lines.join("\n"))))
                        }
                    }
                    "remind" => {
                        let events = list_visible().map_err(|e| tool_error("schedule", &e))?;
                        let due = rules::due_reminders(&events, now);
                        if due.is_empty() {
                            Ok(ToolOutput::text("当前无到点提醒"))
                        } else {
                            let lines: Vec<String> = due.iter().map(|e| format!("- {}：{} ~ {}", e.title, disp(&e.start), disp(&e.end))).collect();
                            Ok(ToolOutput::text(format!("到点提醒 {} 项：\n{}", due.len(), lines.join("\n"))))
                        }
                    }
                    "lunar" => {
                        let date = chrono::NaiveDate::parse_from_str(&get("date"), "%Y-%m-%d")
                            .map_err(|_| tool_error("schedule", "日期格式无效（应为 YYYY-MM-DD）"))?;
                        // day_info 内部可能触发 timor.tech blocking 网络，须在 blocking 线程执行
                        let provider = state.schedule_day_info.clone();
                        let info = tokio::task::spawn_blocking(move || provider.day_info(date))
                            .await
                            .map_err(|e| tool_error("schedule", &format!("农历/节假日计算失败: {}", e)))?;
                        let mut parts = vec![format!("农历 {}", if info.lunar_month.is_empty() { info.lunar_day.clone() } else { format!("{}{}", info.lunar_month, info.lunar_day) })];
                        if !info.festival.is_empty() {
                            parts.push(format!("节日 {}", info.festival));
                        }
                        parts.push(if info.is_workday { "调休班日" } else if info.is_rest_day { "休息日" } else { "工作日" }.to_string());
                        Ok(ToolOutput::text(format!("{}：{}", get("date"), parts.join("｜"))))
                    }
                    "next_available" => {
                        let duration = args.get("duration_minutes").and_then(|v| v.as_i64()).ok_or_else(|| tool_error("schedule", "缺少 duration_minutes"))?;
                        let start_after = if get("start_after").is_empty() { now } else { rules::parse_local_time(&get("start_after")).ok_or_else(|| tool_error("schedule", "start_after 格式无效"))? };
                        let skip = args.get("skip_rest_days").and_then(|v| v.as_bool()).unwrap_or(true);
                        // 临时 guard：取数后立即释放（guard 非 Send，不能跨 await）
                        let events = list_visible().map_err(|e| tool_error("schedule", &e))?;
                        // planner 内部调 day_info（可能 blocking 网络），须在 blocking 线程执行
                        let provider = state.schedule_day_info.clone();
                        let next = tokio::task::spawn_blocking(move || {
                            crate::core::schedule::planner::next_available(&events, provider.as_ref(), duration, start_after, skip)
                        })
                        .await
                        .map_err(|e| tool_error("schedule", &format!("查找可安排时间失败: {}", e)))?;
                        match next {
                            Some(t) => Ok(ToolOutput::text(format!("下一个可安排时间段：{}（持续 {} 分钟）", disp(&fmt(t)), duration))),
                            None => Err(tool_error("schedule", "30 天内未找到可安排时间段")),
                        }
                    }
                    "plan" => {
                        // 任务排布建议：AI 拆解任务（title+hours）→ 引擎排到 deadline 前（只建议，不创建）
                        let deadline = chrono::NaiveDate::parse_from_str(&get("deadline"), "%Y-%m-%d")
                            .map_err(|_| tool_error("schedule", "deadline 格式无效（应为 YYYY-MM-DD）"))?;
                        let tasks_raw = args
                            .get("tasks")
                            .and_then(|v| v.as_array())
                            .ok_or_else(|| tool_error("schedule", "缺少 tasks（任务数组，每项含 title/hours）"))?;
                        let tasks: Vec<crate::core::schedule::planner::PlannedTask> = tasks_raw
                            .iter()
                            .map(|t| crate::core::schedule::planner::PlannedTask {
                                title: t.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                hours: t.get("hours").and_then(|x| x.as_f64()).unwrap_or(0.0),
                            })
                            .filter(|t| !t.title.trim().is_empty() && t.hours > 0.0)
                            .collect();
                        if tasks.is_empty() {
                            return Err(tool_error("schedule", "tasks 为空或格式无效（每项需含 title 与 hours）"));
                        }
                        let ws = get_i64("work_start", 9) as u32;
                        let we = get_i64("work_end", 18) as u32;
                        let skip = args.get("skip_rest_days").and_then(|v| v.as_bool()).unwrap_or(true);
                        let events = list_visible().map_err(|e| tool_error("schedule", &e))?;
                        let provider = state.schedule_day_info.clone();
                        let tasks_for_plan = tasks.clone();
                        let results = tokio::task::spawn_blocking(move || {
                            crate::core::schedule::planner::plan_tasks(&events, provider.as_ref(), &tasks_for_plan, deadline, ws, we, skip, now)
                        })
                        .await
                        .map_err(|e| tool_error("schedule", &format!("任务排布失败: {}", e)))?;
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
                        Ok(ToolOutput::text(lines.join("\n")))
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
                                .map_err(|_| tool_error("schedule", "range 起始日期格式无效（应为 YYYY-MM-DD..YYYY-MM-DD）"))?;
                            let db = chrono::NaiveDate::parse_from_str(b.trim(), "%Y-%m-%d")
                                .map_err(|_| tool_error("schedule", "range 结束日期格式无效（应为 YYYY-MM-DD..YYYY-MM-DD）"))?;
                            if db < da {
                                return Err(tool_error("schedule", "range 结束日期早于起始日期"));
                            }
                            (da.and_hms_opt(0, 0, 0).unwrap(), db.and_hms_opt(23, 59, 0).unwrap())
                        } else {
                            return Err(tool_error("schedule", "range 格式无效（支持 7d / 30d / YYYY-MM-DD..YYYY-MM-DD）"));
                        };
                        let events = list_visible().map_err(|e| tool_error("schedule", &e))?;
                        let stats = tokio::task::spawn_blocking(move || {
                            crate::core::schedule::analyze::analyze_range(&events, from, to)
                        })
                        .await
                        .map_err(|e| tool_error("schedule", &format!("时间统计失败: {}", e)))?;
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
                        Ok(ToolOutput::text(lines.join("\n")))
                    }
                    "review" => {
                        // 日复盘统计：完成 / 进行中 / 未开始 + 投入时长（原因分析与建议由 AI 生成）
                        let date = if get("date").is_empty() {
                            now.date()
                        } else {
                            chrono::NaiveDate::parse_from_str(&get("date"), "%Y-%m-%d")
                                .map_err(|_| tool_error("schedule", "日期格式无效（应为 YYYY-MM-DD）"))?
                        };
                        let events = list_visible().map_err(|e| tool_error("schedule", &e))?;
                        let summary = tokio::task::spawn_blocking(move || {
                            crate::core::schedule::analyze::day_summary(&events, date, now)
                        })
                        .await
                        .map_err(|e| tool_error("schedule", &format!("复盘统计失败: {}", e)))?;
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
                        Ok(ToolOutput::text(lines.join("\n")))
                    }
                    "focus" => {
                        // 专注时间块：指定 start 时校验冲突并创建（type=focus）；未指定时只推荐时间段
                        let duration = args.get("duration_minutes").and_then(|v| v.as_i64()).ok_or_else(|| tool_error("schedule", "缺少 duration_minutes"))?;
                        if duration < 1 {
                            return Err(tool_error("schedule", "duration_minutes 必须大于 0"));
                        }
                        let task = get("task");
                        let title = if task.trim().is_empty() { "专注时间".to_string() } else { format!("专注：{}", task.trim()) };
                        let start_str = get("start");
                        if start_str.is_empty() {
                            // 未指定开始时间：推荐下一个空档（不创建）
                            let events = list_visible().map_err(|e| tool_error("schedule", &e))?;
                            let provider = state.schedule_day_info.clone();
                            let next = tokio::task::spawn_blocking(move || {
                                crate::core::schedule::planner::next_available(&events, provider.as_ref(), duration, now, true)
                            })
                            .await
                            .map_err(|e| tool_error("schedule", &format!("查找专注时间段失败: {}", e)))?;
                            match next {
                                Some(t) => Ok(ToolOutput::text(format!(
                                    "建议专注时间段：{} ~ {}（{} 分钟）。如需创建请确认后调用 add（event_type=focus）。",
                                    disp(&fmt(t)),
                                    disp(&fmt(t + chrono::Duration::minutes(duration))),
                                    duration
                                ))),
                                None => Err(tool_error("schedule", "30 天内未找到可安排时间段")),
                            }
                        } else {
                            let start = rules::parse_local_time(&start_str)
                                .ok_or_else(|| tool_error("schedule", "start 格式无效（应为 YYYY-MM-DDTHH:MM）"))?;
                            let end = start + chrono::Duration::minutes(duration);
                            let mut store = store_guard();
                            let existing: Vec<ScheduleEvent> = store
                                .list()
                                .map_err(|e| tool_error("schedule", &e))?
                                .into_iter()
                                .filter(|e| !is_snooze_reminder(e))
                                .collect();
                            let conflicts = rules::find_conflicts(&existing, start, end, None);
                            if !conflicts.is_empty() {
                                return Ok(ToolOutput::text(format!(
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
                            event.validate().map_err(|e| tool_error("schedule", &e))?;
                            store.upsert(event.clone()).map_err(|e| tool_error("schedule", &e))?;
                            let _ = app.emit("schedule:changed", ());
                            Ok(ToolOutput::text(format!(
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
                                .map_err(|_| tool_error("schedule", "日期格式无效（应为 YYYY-MM-DD）"))?
                        };
                        let ws = get_i64("work_start", 9) as u32;
                        let we = get_i64("work_end", 18) as u32;
                        let events = list_visible().map_err(|e| tool_error("schedule", &e))?;
                        let (day_events, blocks) = tokio::task::spawn_blocking(move || {
                            let day_events = rules::events_on_date(&events, date);
                            let blocks = crate::core::schedule::analyze::available_blocks(&events, date, ws, we);
                            (day_events, blocks)
                        })
                        .await
                        .map_err(|e| tool_error("schedule", &format!("生成当日计划失败: {}", e)))?;
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
                        Ok(ToolOutput::text(lines.join("\n")))
                    }
                    _ => Err(tool_error("schedule", &format!("未知动作: {}", action))),
                }
                }.await;
                // Mutation Verification 轨迹：统一记录 schedule 调用结果（成功/失败），
                // 前端 tool-trace 据此渲染卡片状态与参数摘要。
                match &result {
                    Ok(out) => {
                        let t = out.as_text().unwrap_or("").to_string();
                        record_tool_result(&cfg, "schedule", true, &truncate(&t, 200), Some(&t));
                    }
                    Err(e) => {
                        let m = e.to_string();
                        record_tool_result(&cfg, "schedule", false, &truncate(&m, 200), Some(&m));
                    }
                }
                result
            })
        },
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
/// ls/git_status，不含 edit/delete/技能激活），不修改任何文件；
/// 返回有界摘要，完整输出经 read_subagent_result 分页读取（对齐 Reasonix
/// read_subagent_result 的"结果隔离 + 按需分页"思想）。
pub fn build_deep_research_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "deep_research",
        "派生一个隔离上下文的只读子代理进行深度调研：它可以检索知识库（kb_search）、读取与搜索文件（read/grep/ls），适合需要阅读大量文件、跨文档总结、独立调查的任务。子代理不修改任何文件，也不共享当前对话的技能激活状态。返回有界摘要（含 subagent_id）；若需完整结果，用 read_subagent_result 指定 subagent_id 分页读取。",
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
                    "maximum": SUBAGENT_MAX_TURNS_LIMIT
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
                    "maximum": SUBAGENT_RESULT_MAX_CHARS
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
pub fn build_webfetch_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "webfetch",
        "抓取指定网页并提取正文文本（只读，不执行页面脚本）。仅支持 http/https；拒绝访问内网/本机地址（SSRF 防护）；响应体上限 200KB、提取文本上限 50000 字符（超出截断并提示）。适合获取网页内容用于总结、引用或信息检索。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "要抓取的网页 URL，如 https://example.com/docs"
                },
                "max_chars": {
                    "type": "integer",
                    "minimum": 1000,
                    "maximum": WEBFETCH_MAX_CHARS,
                    "description": "提取文本上限（字符，默认 50000）"
                }
            },
            "required": ["url"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let url = args
                    .get("url")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let max_chars = args
                    .get("max_chars")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(WEBFETCH_MAX_CHARS);
                record_tool_call(&cfg, "webfetch", &url, Some(&args));
                match webfetch(&url, max_chars).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "webfetch", true, &format!("{} 字符", text.chars().count()), Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "webfetch", false, &e, Some(&e));
                        Err(tool_error("webfetch", &e))
                    }
                }
            })
        },
    )
}

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
pub fn build_todo_write_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "todo_write",
        "维护当前任务的任务清单（用于长任务执行中跟踪进度、防止遗漏步骤）。action 支持：add（追加待办）、complete（标记完成，items 为空则全部完成）、remove（移除条目，items 为空则清空）、clear（清空清单）、replace（整体替换清单）。调用后返回最新清单（[x]=已完成，[ ]=待办）。任务结束时用 clear 清空。",
        serde_json::json!({
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
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let action = args
                    .get("action")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let items: Vec<String> = args
                    .get("items")
                    .and_then(|a| a.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                record_tool_call(&cfg, "todo_write", &format!("{}: {} 项", action, items.len()), Some(&args));
                match todo_write(&cfg, &action, &items).await {
                    Ok(text) => {
                        record_tool_result(&cfg, "todo_write", true, &text, Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "todo_write", false, &e, Some(&e));
                        Err(tool_error("todo_write", &e))
                    }
                }
            })
        },
    )
}

/// 构建 remember 工具：写入一条跨会话长期记忆。
///
/// 记忆随会话持久化（全局用户数据目录），后续请求按关键词检索注入，
/// 沉淀用户偏好、项目约定与已验证结论。
pub fn build_remember_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "remember",
        "把一条长期记忆写入跨会话存储（用户偏好、项目约定、已验证结论等），后续对话可检索引用。title 一句话概括，body 写完整事实；keywords 用空格分隔便于检索；expires_in_days 可设置过期天数（过期后不再召回）。",
        serde_json::json!({
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
                // O2：expires_in_days → 过期时间戳（毫秒）
                let expires_in_days = args.get("expires_in_days").and_then(|v| v.as_u64());
                let mut input: crate::core::memory::MemoryInput =
                    serde_json::from_value(args).map_err(|e| tool_error("remember", &e.to_string()))?;
                // 两级记忆（P0-3）：scope='global' 由存储归一为 ''（跨库常驻）；
                // scope='project'（默认）绑定当前知识库目录，切换目录后自然隔离。
                if input.scope.trim() != "global" {
                    input.dir_path = cfg.dir_path.clone();
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
                let state = cfg.app_handle.state::<crate::AppState>();
                let store = state.memory_store.clone();
                // SQLite 阻塞 IO 移入 blocking 线程，避免占住 agent 异步运行时
                match tokio::task::spawn_blocking(move || store.create(&input)).await {
                    Ok(Ok(item)) => {
                        let msg = format!(
                            "已保存记忆（id={}，revision={}）：{}\n{}",
                            item.id, item.revision, item.title, item.body
                        );
                        record_tool_result(&cfg, "remember", true, &format!("id={} revision={}", item.id, item.revision), Some(&msg));
                        Ok(ToolOutput::text(msg))
                    }
                    Ok(Err(e)) => {
                        record_tool_result(&cfg, "remember", false, &e, Some(&e));
                        Err(tool_error("remember", &e))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "remember", false, &e.to_string(), Some(&e.to_string()));
                        Err(tool_error("remember", &e.to_string()))
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
                let store = state.memory_store.clone();
                // SQLite 阻塞 IO 移入 blocking 线程，避免占住 agent 异步运行时
                let forget_id = id.clone();
                match tokio::task::spawn_blocking(move || store.delete(&forget_id)).await {
                    Ok(Ok(true)) => {
                        let msg = format!("已删除记忆 {id}");
                        record_tool_result(&cfg, "forget", true, &msg, Some(&msg));
                        Ok(ToolOutput::text(msg))
                    }
                    Ok(Ok(false)) => {
                        let e = format!("记忆 {id} 不存在或已删除");
                        record_tool_result(&cfg, "forget", false, &e, Some(&e));
                        Err(tool_error("forget", &e))
                    }
                    Ok(Err(e)) => {
                        record_tool_result(&cfg, "forget", false, &e, Some(&e));
                        Err(tool_error("forget", &e))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "forget", false, &e.to_string(), Some(&e.to_string()));
                        Err(tool_error("forget", &e.to_string()))
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
                // O1：融合检索（关键词 ∪ 向量，RRF；embedding 失败降级关键词）
                // 两级记忆（P0-3）：仅检索「当前知识库 ∪ 全局」的记忆
                match crate::core::memory::search_hybrid(
                    state.memory_store.clone(),
                    state.memory_vectors.clone(),
                    &query,
                    limit,
                    &cfg.dir_path,
                )
                .await
                {
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

/// 构建 search_bookmarks 工具：检索用户收藏的书签知识资产（只读）。
/// FTS5（title/description/summary/tags/category）∪ 向量补位；排除 ARCHIVED。
pub fn build_search_bookmarks_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
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
                let category = args.get("category").and_then(|c| c.as_str()).map(|s| s.to_string());
                let folder = args.get("folder").and_then(|f| f.as_str()).map(|s| s.to_string());
                record_tool_call(&cfg, "search_bookmarks", &format!("query={} limit={}", query, limit), Some(&args));
                if query.is_empty() {
                    let e = "检索关键词不能为空".to_string();
                    record_tool_result(&cfg, "search_bookmarks", false, &e, Some(&e));
                    return Err(tool_error("search_bookmarks", &e));
                }
                let state = cfg.app_handle.state::<crate::AppState>();
                let store = match state.bookmark_store(&cfg.dir_path) {
                    Ok(s) => s,
                    Err(e) => {
                        record_tool_result(&cfg, "search_bookmarks", false, &e, Some(&e));
                        return Err(tool_error("search_bookmarks", &e));
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
                            record_tool_result(&cfg, "search_bookmarks", false, &e, Some(&e));
                            return Err(tool_error("search_bookmarks", &e));
                        }
                    }
                };
                if hits.is_empty() {
                    let msg = format!("未找到与「{query}」相关的书签");
                    record_tool_result(&cfg, "search_bookmarks", true, &msg, Some(&msg));
                    return Ok(ToolOutput::text(msg));
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
                record_tool_result(&cfg, "search_bookmarks", true, &format!("{} 条", hits.len()), Some(&out));
                Ok(ToolOutput::text(out))
            })
        },
    )
}

/// 构建 get_bookmark 工具：按 id 获取书签详情（只读）。
pub fn build_get_bookmark_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "get_bookmark",
        "按 id 获取某个书签收藏的完整详情（含 AI 摘要、标签、分类、抓取正文、状态）。在 search_bookmarks 定位到具体收藏后需要深入了解时调用。只读。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "书签 id（search_bookmarks 返回）" }
            },
            "required": ["id"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let id = args
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                record_tool_call(&cfg, "get_bookmark", &format!("id={}", id), Some(&args));
                if id.is_empty() {
                    let e = "书签 id 不能为空".to_string();
                    record_tool_result(&cfg, "get_bookmark", false, &e, Some(&e));
                    return Err(tool_error("get_bookmark", &e));
                }
                let state = cfg.app_handle.state::<crate::AppState>();
                let store = match state.bookmark_store(&cfg.dir_path) {
                    Ok(s) => s,
                    Err(e) => {
                        record_tool_result(&cfg, "get_bookmark", false, &e, Some(&e));
                        return Err(tool_error("get_bookmark", &e));
                    }
                };
                let bookmark = {
                    let guard = match store.lock() {
                        Ok(g) => g,
                        Err(e) => {
                            let e = e.to_string();
                            record_tool_result(&cfg, "get_bookmark", false, &e, Some(&e));
                            return Err(tool_error("get_bookmark", &e));
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
                        record_tool_result(&cfg, "get_bookmark", true, "ok", Some(&out));
                        Ok(ToolOutput::text(out))
                    }
                    Ok(None) => {
                        let msg = format!("未找到书签: {}", id);
                        record_tool_result(&cfg, "get_bookmark", true, &msg, Some(&msg));
                        Ok(ToolOutput::text(msg))
                    }
                    Err(e) => {
                        record_tool_result(&cfg, "get_bookmark", false, &e, Some(&e));
                        Err(tool_error("get_bookmark", &e))
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
                "max_turns": { "type": "integer", "minimum": 1, "maximum": SUBAGENT_MAX_TURNS_LIMIT, "description": "轮次上限（默认 12）" }
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
                    "minItems": PARALLEL_TASKS_MIN,
                    "maxItems": PARALLEL_TASKS_MAX,
                    "items": { "type": "string" },
                    "description": "2-5 个独立调研任务（各自自包含）"
                },
                "max_turns": { "type": "integer", "minimum": 1, "maximum": SUBAGENT_MAX_TURNS_LIMIT, "description": "每个子代理轮次上限（默认 12）" }
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
