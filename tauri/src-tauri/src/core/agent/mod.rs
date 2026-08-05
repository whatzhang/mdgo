//! RAG Agent 模块：kb_search 工具 + Agent 构建（基于 Rig Agent）
//!
//! - [`build_kb_search_tool`]：将「嵌入 → 混合检索 → 文档聚合」封装为模型可调用的工具
//! - [`build_rag_agent`]：携带检索上下文与检索/文件/技能工具的 Agent（渐进式披露三级加载）
//! - [`build_chat_agent`]：无工具纯对话 Agent
//! - [`aggregate_hits`]：文档级聚合逻辑（与检索结果共享）
//! - [`SkillGateHook`] / [`SkillInstructionHook`]：由 [`ActiveSkillState`] 驱动的技能指令注入与工具兜底拦截

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use rig_agent::agent::hook::{
    AgentHook, CompletionCall, CompletionCallAction, CompletionResponse, HookContext, ObservationAction,
    RequestPatch, ToolCall, ToolCallAction,
};
use rig_agent::agent::{Agent, AgentBuilder};
use rig_agent::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use rig_core::providers::openai;
use tauri::{AppHandle, Manager};

use crate::core::skill::activation::ActiveSkillState;
use crate::core::skill::SkillRegistry;
use crate::core::{Indexer, SearchHit, call_embedding_query, route_intent};

/// 规约文档缓存：(文件名 → (最后修改时间, 内容))。
///
/// 进程级缓存 + mtime 热重载：mtime 未变化时直接复用缓存内容，
/// 避免每条消息请求都在磁盘热路径上读文件；规约文件变更后下一次
/// 请求自动重读（无需重启应用）。对齐 OpenClaw 的缓存策略，
/// 同时借鉴 Codex「会话内加载一次」的语义（未变更时零读盘）。
static RULES_CACHE: OnceLock<Mutex<HashMap<String, (SystemTime, String)>>> = OnceLock::new();

fn rules_cache() -> &'static Mutex<HashMap<String, (SystemTime, String)>> {
    RULES_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 解析规约文件绝对路径：运行时资源目录（打包后）优先，源码资源目录（开发期）回退。
fn resolve_agent_rules_path(app: &AppHandle, name: &str) -> Option<PathBuf> {
    if let Ok(dir) = app.path().resource_dir() {
        let candidate = dir.join("agent").join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("agent")
        .join(name);
    fallback.exists().then_some(fallback)
}

/// 内置兜底规约（资源文件缺失或为空时），保证任何环境下 Agent 都有明确的中文回答约束。
fn builtin_agent_rules(name: &str) -> String {
    format!(
        "你是 mdgo 应用的{}助手。默认使用简体中文回答；若用户使用其他语言提问，则跟随用户语言。",
        if name.starts_with("rag") { "知识库" } else { "对话" }
    )
}

/// 加载 Agent 规约文档（resources/agent/{name}）。
///
/// 使用进程级缓存 + mtime 热重载：
/// - 缓存命中且文件 mtime 未变 → 直接返回缓存内容（零磁盘 I/O，请求热路径不受影响）
/// - mtime 变化或首次加载 → 读盘并更新缓存（规约修改后下一次请求自动生效）
/// - 文件缺失/为空 → 返回内置兜底规约
pub fn load_agent_rules(app: &AppHandle, name: &str) -> String {
    let path = match resolve_agent_rules_path(app, name) {
        Some(p) => p,
        None => return builtin_agent_rules(name),
    };

    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    {
        let cache = rules_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached_mtime, content)) = cache.get(name) {
            if let Some(actual) = mtime {
                if actual == *cached_mtime {
                    return content.clone();
                }
            }
        }
    }

    // mtime 变化或首次加载：读盘并更新缓存
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    if content.trim().is_empty() {
        rules_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        return builtin_agent_rules(name);
    }
    let mut cache = rules_cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(actual) = mtime {
        cache.insert(name.to_string(), (actual, content.clone()));
    }
    content
}

/// 内置工具集：文件只读、目录列举、Git 状态查询（含工具调用轨迹总线）
pub mod tools;

/// 始终可用的基础工具（不随技能白名单窄化，对齐主流 Agent：文件操作与技能管理常驻）。
///
/// 检索类工具（kb_search / code_lookup）不在此列：它们仅当已激活技能声明时
/// 才可见可调——决策权在 LLM（先 activate_skill 再检索）。
pub const BASE_TOOLS: &[&str] = &[
    "activate_skill", "deactivate_skill", "read", "list_files", "edit", "delete",
    "render_mermaid", "git_status",
];

/// 调试用 Hook：在每次 LLM API 调用边界打印请求消息与响应体内容。
///
/// 挂载到 AgentBuilder 后，无论流式（stream）还是非流式（completion）路径，
/// 都能在模型调用前拿到完整请求消息列表（preamble + history + prompt），
/// 在响应后拿到规范化响应内容与 token 用量。
#[derive(Clone, Debug)]
pub struct LlmTraceHook;

impl AgentHook for LlmTraceHook {
    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: CompletionCall<'_>,
    ) -> CompletionCallAction {
        let mut messages = event.history.to_vec();
        messages.push(event.prompt.clone());
        if log::log_enabled!(log::Level::Debug) {
            log::debug!(
                "[llm_trace] [调用 LLM API] 次数={}, 请求消息=\n{}",
                event.turn,
                serde_json::to_string_pretty(&messages)
                .unwrap_or_else(|e| format!("<serialize failed: {}>", e))
            );
        }
        CompletionCallAction::Continue
    }

    async fn on_completion_response(
        &self,
        _ctx: &HookContext,
        event: CompletionResponse<'_>,
    ) -> ObservationAction {
        if log::log_enabled!(log::Level::Debug) {
            log::debug!(
                "[llm_trace]  [调用 LLM API] token用量={}, 响应内容=\n{} ",
                 serde_json::to_string_pretty(&event.usage)
                .unwrap_or_else(|e| format!("<serialize failed: {}>", e)),
                serde_json::to_string_pretty(&event.content)
                .unwrap_or_else(|e| format!("<serialize failed: {}>", e))
            );
        }
        ObservationAction::Continue
    }
}

/// 技能工具白名单 Hook（Rig 原生 `on_tool_call` 机制，兜底拦截）。
///
/// 从 [`ActiveSkillState`] 动态读取当前激活技能的声明工具（决策在 LLM，此处仅做安全兜底）：
/// - 基础工具（[`BASE_TOOLS`]）始终放行
/// - 检索类工具（kb_search / code_lookup）仅在已激活技能声明时放行
///
/// 拦截时将原因反馈给模型，由 Agent 自主调整策略（先 activate_skill 或改用其他工具），
/// 而非硬报错。
#[derive(Clone, Debug)]
pub struct SkillGateHook {
    state: Arc<ActiveSkillState>,
}

impl SkillGateHook {
    pub fn new(state: Arc<ActiveSkillState>) -> Self {
        Self { state }
    }
}

impl AgentHook for SkillGateHook {
    async fn on_tool_call(
        &self,
        _ctx: &HookContext,
        event: ToolCall<'_>,
    ) -> ToolCallAction {
        if BASE_TOOLS.contains(&event.tool_name) {
            return ToolCallAction::Run;
        }
        let declared: Vec<String> = self.state.allowed_tools().unwrap_or_default();
        if declared.iter().any(|t| t == event.tool_name) {
            return ToolCallAction::Run;
        }
        log::warn!(
            "[skill_gate] 拦截未授权工具调用: {}（当前激活技能声明: {:?}）",
            event.tool_name,
            declared
        );
        ToolCallAction::Skip(format!(
            "工具 '{}' 当前不可用（未由任何已激活技能声明）。可先调用 activate_skill 激活声明该工具的技能，或改用其他工具。",
            event.tool_name
        ))
    }
}

/// 技能指令 Hook：每个模型调用（completion call）边界动态注入
/// L1 技能目录（常驻）+ 已激活技能的 L2 指令正文，并窄化本轮模型可见的工具列表。
///
/// 使用 Rig 原生 [`RequestPatch`] 机制：
/// - `preamble`：基础角色 + 预检索上下文 + L1 技能目录（静态）+ 已激活技能指令（动态）
/// - `active_tools`：基础工具 ∪ 已激活技能声明工具（Rig 原生过滤，模型不会发起范围外的调用）
#[derive(Clone, Debug)]
pub struct SkillInstructionHook {
    /// 静态基础 preamble（基础角色 + 预检索上下文 + L1 技能目录）
    base_preamble: String,
    /// 激活状态（动态读取 L2 指令与工具白名单）
    state: Arc<ActiveSkillState>,
}

impl SkillInstructionHook {
    pub fn new(base_preamble: String, state: Arc<ActiveSkillState>) -> Self {
        Self {
            base_preamble,
            state,
        }
    }
}

impl AgentHook for SkillInstructionHook {
    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        _event: CompletionCall<'_>,
    ) -> CompletionCallAction {
        // L2：已激活技能的指令正文（多技能按激活顺序拼接）
        let mut preamble = self.base_preamble.clone();
        let instructions = self.state.instructions();
        if !instructions.is_empty() {
            preamble.push_str("\n\n---\n\n");
            preamble.push_str("请遵循以下已激活技能的指令：\n\n");
            preamble.push_str(&instructions);
        }
        let mut patch = RequestPatch::new().preamble(&preamble);

        // 可见工具 = 基础工具 ∪ 已激活技能声明工具
        let mut visible: Vec<String> = BASE_TOOLS.iter().map(|t| t.to_string()).collect();
        if let Some(declared) = self.state.allowed_tools() {
            for t in declared {
                if !visible.iter().any(|v| v == &t) {
                    visible.push(t);
                }
            }
        }
        patch = patch.active_tools(visible);
        CompletionCallAction::patch(patch)
    }
}

/// kb_search 工具允许的最大片段数（防止模型传入超大 top_k 触发全量检索/重排）
const MAX_TOP_K: u32 = 20;

/// 聚合后送入模型上下文的总字符上限（约 3K token，避免超出模型窗口）。
/// kb_search 工具与 RAG 主链路共用此上限（单一来源）。
pub(crate) const MAX_CONTEXT_CHARS: usize = 12_000;

/// kb_search 工具的运行参数
#[derive(Clone)]
pub struct KbSearchConfig {
    /// 检索的知识库目录
    pub dir_path: String,
    /// 索引器（混合检索）
    pub indexer: Arc<Indexer>,
    /// 默认返回的片段数量（模型未指定 top_k 时使用）
    pub default_top_k: u32,
    /// 当前请求 ID（用于工具调用轨迹的事件关联）
    pub request_id: String,
    /// 目录黑名单（gitignore 语法，供 list_files/walk_dir 过滤，与索引/监视逻辑一致）
    pub dir_blacklist: Vec<String>,
    /// 文件黑名单（gitignore 语法，供 list_files/walk_dir 过滤，与索引/监视逻辑一致）
    pub file_blacklist: Vec<String>,
    /// 聚合绝对阈值（融合分数低于此值的命中不进入上下文）
    pub min_score: f32,
    /// 送入上下文的最大文档数
    pub max_context_docs: usize,
    /// 单文档最多保留的 chunk 数
    pub max_chunks_per_doc: usize,
    /// 触发本次请求的技能 ID（格式：scope:skill_id），用于工具调用轨迹标注
    pub skill_id: Option<String>,
    /// 当前请求的激活技能状态（`read` 工具按需读取 L3 references；钩子动态注入 L2 指令）
    pub skill_state: Arc<ActiveSkillState>,
    /// 各作用域技能基础目录（(scope, 绝对路径)），`read` 工具据此定位已激活技能的 references
    pub skill_bases: Vec<(String, String)>,
    /// 检索命中收集器：kb_search / code_lookup 工具将聚合后的命中写入，
    /// 命令层在请求结束（rag:done）时合并进引用来源列表，供前端渲染"引用"。
    pub search_sink: Arc<tokio::sync::Mutex<Vec<(SearchHit, f32)>>>,
}

/// 执行一次完整检索：嵌入 → 混合检索 → 文档级聚合 → 生成模型可读文本。
///
/// 返回的文本按文档分组，同文档的多个片段合并，供模型直接作为上下文。
pub async fn kb_search(cfg: &KbSearchConfig, query: &str, top_k: u32) -> Result<String, String> {
    log::debug!("[skill] kb_search: query={}, top_k={}", query, top_k);
    let embedding = tokio::task::spawn_blocking({
        let query = query.to_string();
        move || call_embedding_query(&query)
    })
    .await
    .map_err(|e| format!("查询向量计算任务失败: {}", e))?
    .map_err(|e| e)?
    .into_iter()
    .next()
    .ok_or_else(|| "生成查询向量失败".to_string())?;

    let intent = route_intent(query);
    let hits = cfg
        .indexer
        .hybrid_search(&cfg.dir_path, &embedding, query, top_k, intent)
        .await?;
    if hits.is_empty() {
        return Ok("知识库中未找到相关内容。".to_string());
    }

    let hits_len = hits.len();
    let selected = aggregate_hits(
        hits,
        cfg.min_score,
        cfg.max_context_docs,
        cfg.max_chunks_per_doc,
    );
    if selected.is_empty() {
        return Ok("知识库中未找到足够相关的内容。".to_string());
    }

    // 命中回传：合并进 rag:done 的引用来源（供前端渲染"引用"）
    cfg.search_sink
        .lock()
        .await
        .extend(selected.iter().cloned());
    log::debug!("[skill] kb_search 结果: 选中={}， 命中={} ，min_score={}， max_context_docs={}， max_chunks_per_doc={}", selected.len(), hits_len, cfg.min_score, cfg.max_context_docs, cfg.max_chunks_per_doc);
    Ok(build_context_text(&selected, MAX_CONTEXT_CHARS))
}

/// 构建 kb_search 工具。
///
/// 模型可通过该工具在知识库中检索片段；工具内部执行嵌入、混合检索与文档级聚合，
/// 返回按文档分组的可读文本。
pub fn build_kb_search_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "kb_search",
        "在用户指定的本地知识库中检索与问题相关的文档片段。当回答需要知识库内容支撑、或当前信息不足时，调用本工具获取参考资料；可多次调用以从不同角度检索。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "用于检索知识库的问题或关键词，应聚焦单一角度"
                },
                "top_k": {
                    "type": "integer",
                    "description": "期望返回的文档片段数量，默认 5"
                }
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
                if query.is_empty() {
                    return Err(ToolExecutionError::other("检索关键词为空").with_model_output(
                        ToolOutput::text("检索关键词为空，请提供 query 参数"),
                    ));
                }
                let top_k = args
                    .get("top_k")
                    .and_then(|t| t.as_u64())
                    .map(|v| v as u32)
                    .filter(|v| *v > 0)
                    .map(|v| v.min(MAX_TOP_K))
                    .unwrap_or(cfg.default_top_k.min(MAX_TOP_K));

                tools::record_tool_call(&cfg, "kb_search", &query);
                match kb_search(&cfg, &query, top_k).await {
                    Ok(text) => {
                        tools::record_tool_result(&cfg, "kb_search", true, &format!("{} 字符", text.chars().count()));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        tools::record_tool_result(&cfg, "kb_search", false, &e);
                        Err(ToolExecutionError::other(format!("知识库检索失败: {}", e))
                            .with_model_output(ToolOutput::text(format!("知识库检索失败: {}", e))))
                    }
                }
            })
        },
    )
}

/// 执行代码符号检索：按符号名（函数/类/方法名等）精确或前缀匹配定位代码定义。
///
/// 返回的文本按文档分组，供模型直接作为上下文。与 `kb_search`（语义检索）互补。
pub async fn code_search(cfg: &KbSearchConfig, symbol: &str, top_k: u32) -> Result<String, String> {
    let hits = cfg.indexer.search_symbols(&cfg.dir_path, symbol, top_k).await?;
    if hits.is_empty() {
        return Ok(format!("知识库中未找到与符号 '{}' 相关的代码。", symbol));
    }

    let mut parts: Vec<String> = Vec::new();
    let mut last_doc = String::new();
    for hit in &hits {
        if hit.doc_name != last_doc {
            if !last_doc.is_empty() {
                parts.push(String::new());
            }
            parts.push(format!("--- {} ---", hit.doc_name));
            last_doc = hit.doc_name.clone();
        }
        let kind = hit.symbol_kind.as_deref().unwrap_or("symbol");
        let name = hit.symbol_name.as_deref().unwrap_or("");
        let text = hit.sentence_window.as_deref().unwrap_or(&hit.text);
        parts.push(format!("[{kind}] {name}\n{text}"));
    }

    // 命中回传：合并进 rag:done 的引用来源（供前端渲染"引用"）
    cfg.search_sink
        .lock()
        .await
        .extend(hits.iter().map(|h| (h.clone(), h.score)));

    Ok(parts.join("\n"))
}

/// 构建 code_lookup 工具。
///
/// 模型在问题涉及具体代码符号（函数名、类名、方法名等）时调用，按符号名
/// 定位代码定义所在文件与片段，补充语义检索容易漏掉的"符号定义"命中。
pub fn build_code_lookup_tool(cfg: KbSearchConfig) -> DynamicTool {
    DynamicTool::new(
        "code_lookup",
        "在知识库中按符号名（函数名、类名、方法名、变量名等）定位代码定义位置。当问题涉及具体的函数/类/方法名、或需要查找某段代码在哪个文件实现时，调用本工具；符号名越精确，检索效果越好。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "symbol": {
                    "type": "string",
                    "description": "要查找的代码符号名，如 handle_timeout、LRUCache、parseJSON"
                },
                "top_k": {
                    "type": "integer",
                    "description": "期望返回的代码片段数量，默认 5"
                }
            },
            "required": ["symbol"]
        }),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            Box::pin(async move {
                let symbol = args
                    .get("symbol")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if symbol.is_empty() {
                    return Err(ToolExecutionError::other("符号名为空").with_model_output(
                        ToolOutput::text("请提供要查找的代码符号名"),
                    ));
                }
                let top_k = args
                    .get("top_k")
                    .and_then(|t| t.as_u64())
                    .map(|v| v as u32)
                    .filter(|v| *v > 0)
                    .map(|v| v.min(MAX_TOP_K))
                    .unwrap_or(5);

                tools::record_tool_call(&cfg, "code_lookup", &symbol);
                match code_search(&cfg, &symbol, top_k).await {
                    Ok(text) => {
                        tools::record_tool_result(&cfg, "code_lookup", true, &format!("{} 字符", text.chars().count()));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        tools::record_tool_result(&cfg, "code_lookup", false, &e);
                        Err(ToolExecutionError::other(format!("代码检索失败: {}", e))
                            .with_model_output(ToolOutput::text(format!("代码检索失败: {}", e))))
                    }
                }
            })
        },
    )
}

/// 文档级聚合：按 doc+chunk 去重 → 绝对阈值过滤 → 每文档截断 → 按文档取 top-N。
///
/// 替换旧的"相对自适应阈值"方案（max*0.3 / max*0.5 在分数整体偏低时会放水，
/// 在偏高时可能误杀）。融合分数已归一化到 [0,1]，`min_score` 作为绝对阈值有
/// 确定语义，配合硬截断保证送入上下文的都是高置信命中。
///
/// 返回按文档分数降序、文档内按分数降序的 `(SearchHit, score)` 列表。
pub fn aggregate_hits(
    all_hits: Vec<SearchHit>,
    min_score: f32,
    max_docs: usize,
    max_chunks_per_doc: usize,
) -> Vec<(SearchHit, f32)> {
    // 1. 按 doc_name + chunk_index 去重，保留最高分
    let mut seen: HashMap<(String, u32), (SearchHit, f32)> = HashMap::new();
    for hit in all_hits {
        let score = hit.score;
        let key = (hit.doc_name.clone(), hit.chunk_index);
        match seen.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if score > entry.get().1 {
                    entry.insert((hit, score));
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((hit, score));
            }
        }
    }

    // 2. 按 doc_name 分组，绝对阈值过滤
    let mut doc_map: HashMap<String, Vec<(SearchHit, f32)>> = HashMap::new();
    for (hit, score) in seen.into_values() {
        if score < min_score {
            continue;
        }
        let doc_name = hit.doc_name.clone();
        doc_map.entry(doc_name).or_default().push((hit, score));
    }

    // 3. 以每篇文档的最佳 chunk 分数作为文档代表分排序（同分按 doc_name 字典序保证确定性），
    //    每篇文档内按分数降序截断，再取 top max_docs 文档
    let mut doc_scores: Vec<(String, f32, Vec<(SearchHit, f32)>)> = doc_map
        .into_values()
        .map(|mut chunks| {
            chunks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            chunks.truncate(max_chunks_per_doc.max(1));
            let best = chunks.first().map(|(_, s)| *s).unwrap_or(0.0);
            let doc = chunks
                .first()
                .map(|(h, _)| h.doc_name.clone())
                .unwrap_or_default();
            (doc, best, chunks)
        })
        .collect();
    doc_scores.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    doc_scores.truncate(max_docs.max(1));

    doc_scores
        .into_iter()
        .flat_map(|(_, _, chunks)| chunks)
        .collect()
}

/// 将聚合后的命中组装为模型可读的上下文文本。
///
/// 文档顺序（分数降序，即 `aggregate_hits` 输出顺序）保持不变；
/// 文档内按 `chunk_index` 阅读顺序重排，保证送入模型的片段连贯可读。
/// 总字符数受 `max_chars` 限制，超出部分截断。
pub fn build_context_text(selected: &[(SearchHit, f32)], max_chars: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut used = 0usize;

    let mut i = 0usize;
    while i < selected.len() {
        let doc = selected[i].0.doc_name.clone();
        // 收集同文档连续片段（aggregate_hits 保证同文档相邻），按阅读顺序重排
        let mut run: Vec<&SearchHit> = Vec::new();
        while i < selected.len() && selected[i].0.doc_name == doc {
            run.push(&selected[i].0);
            i += 1;
        }
        run.sort_by_key(|h| h.chunk_index);

        if !parts.is_empty() {
            parts.push(String::new());
        }
        let header = format!("--- {} ---", doc);
        if used + header.len() > max_chars {
            break;
        }
        used += header.len();
        parts.push(header);

        for hit in run {
            let text = hit.sentence_window.as_deref().unwrap_or(&hit.text);
            if used + text.len() + 1 > max_chars {
                break;
            }
            parts.push(text.to_string());
            used += text.len() + 1;
        }
    }

    parts.join("\n")
}

/// 构建 RAG 问答 Agent（渐进式披露三级加载）。
///
/// - `context`：预检索的知识库上下文，注入 system preamble
/// - `search_config`：用于构建检索/文件工具（模型可在生成过程中补充检索）
/// - `registry`：技能注册表（activate_skill 工具据此查找技能，LLM 自主决策 L2 加载）
/// - `catalog`：L1 技能目录（id + description，会话全程常驻，模型始终知道自己有哪些技能）
///
/// 工具与指令由 [`SkillInstructionHook`] / [`SkillGateHook`] 依据共享的
/// [`ActiveSkillState`]（在 `search_config.skill_state` 中）动态驱动。
pub fn build_rag_agent(
    model: openai::CompletionModel,
    context: &str,
    search_config: KbSearchConfig,
    registry: Arc<SkillRegistry>,
    catalog: String,
    base: String,
) -> Agent<openai::CompletionModel> {
    let skill_state = search_config.skill_state.clone();

    // base 为 Agent 规约（resources/agent/rag_agent.md，由调用方经 load_agent_rules
    // 从资源目录加载，打包后跟随安装包）；此处仅追加动态的检索上下文与技能目录。
    let mut preamble = if context.trim().is_empty() {
        base
    } else {
        format!("{}\n\n检索到的知识库上下文：\n{}", base, context)
    };
    if !catalog.trim().is_empty() {
        preamble.push_str("\n\n---\n\n");
        preamble.push_str(&catalog);
        log::info!("[agent] L1 技能目录注入: chars={}", catalog.len());
    }

    let builder = AgentBuilder::new(model);

    // 始终注册全部内置工具；每轮模型可见的工具列表由 SkillInstructionHook
    // 依据激活状态窄化（active_tools），SkillGateHook 作为兜底拦截越权调用。
    let builder = builder
        .dynamic_tool(build_kb_search_tool(search_config.clone()))
        .dynamic_tool(build_code_lookup_tool(search_config.clone()))
        .dynamic_tool(tools::build_activate_skill_tool(registry.clone(), skill_state.clone()))
        .dynamic_tool(tools::build_deactivate_skill_tool(skill_state.clone()))
        .dynamic_tool(tools::build_read_tool(search_config.clone()))
        .dynamic_tool(tools::build_edit_tool(search_config.clone()))
        .dynamic_tool(tools::build_delete_tool(search_config.clone()))
        .dynamic_tool(tools::build_list_files_tool(search_config.clone()))
        .dynamic_tool(tools::build_render_mermaid_tool(search_config.clone()))
        .dynamic_tool(tools::build_git_status_tool(search_config));

    builder
        .default_max_turns(4)
        .add_hook(LlmTraceHook)
        .add_hook(SkillInstructionHook::new(preamble, skill_state.clone()))
        .add_hook(SkillGateHook::new(skill_state))
        .build()
}

/// 构建无工具纯对话 Agent
pub fn build_chat_agent(
    model: openai::CompletionModel,
    base: String,
) -> Agent<openai::CompletionModel> {
    // Chat 模式无 L1 技能目录注入，角色与语言约束来自调用方传入的规约
    // （resources/agent/chat_agent.md，经 load_agent_rules 从资源目录加载）
    AgentBuilder::new(model)
        .preamble(&base)
        .add_hook(LlmTraceHook)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(doc: &str, chunk: u32, score: f32) -> SearchHit {
        SearchHit {
            text: format!("text-{}-{}", doc, chunk),
            doc_name: doc.to_string(),
            chunk_index: chunk,
            score,
            score_vec: score,
            score_bm25: 0.0,
            path_json: None,
            sentence_window: None,
            symbol_name: None,
            symbol_kind: None,
        }
    }

    #[test]
    fn aggregate_dedups_keep_max_score() {
        let hits = vec![
            hit("a.md", 0, 0.5),
            hit("a.md", 0, 0.7), // 同 doc+chunk 重复，应保留最高分
        ];
        let selected = aggregate_hits(hits, 0.3, 4, 3);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].1, 0.7);
    }

    #[test]
    fn aggregate_filters_below_min_score() {
        let hits = vec![
            hit("a.md", 0, 0.8),
            hit("b.md", 0, 0.2), // 低于绝对阈值 0.3，应被过滤
        ];
        let selected = aggregate_hits(hits, 0.3, 4, 3);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].0.doc_name, "a.md");
    }

    #[test]
    fn aggregate_truncates_docs_and_chunks() {
        let hits = vec![
            hit("a.md", 0, 0.9),
            hit("a.md", 1, 0.8),
            hit("a.md", 2, 0.7),
            hit("a.md", 3, 0.6), // 第 4 块，应被 max_chunks_per_doc=3 截断
            hit("b.md", 0, 0.1), // 最低分文档，应被 max_docs=1 截断
        ];
        let selected = aggregate_hits(hits, 0.3, 1, 3);
        assert_eq!(selected.len(), 3);
        assert!(selected.iter().all(|(h, _)| h.doc_name == "a.md"));
    }

    #[test]
    fn aggregate_ties_broken_by_doc_name() {
        let hits = vec![
            hit("b.md", 0, 0.8),
            hit("a.md", 0, 0.8), // 与 b.md 同分，按 doc_name 字典序应排在前面（确定性）
        ];
        let selected = aggregate_hits(hits, 0.3, 4, 3);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].0.doc_name, "a.md");
        assert_eq!(selected[1].0.doc_name, "b.md");
    }

    #[test]
    fn build_context_sorts_by_reading_order_and_caps() {
        let hits = vec![
            hit("a.md", 2, 0.9),
            hit("a.md", 0, 0.8),
            hit("a.md", 1, 0.7),
            hit("b.md", 0, 0.5),
        ];
        let selected = aggregate_hits(hits, 0.3, 4, 3);
        let text = build_context_text(&selected, usize::MAX);
        // 文档 a 的片段应按 chunk_index 阅读顺序（0,1,2），而非分数顺序（2,0,1）
        let a_pos = text.find("text-a.md-0").unwrap();
        let b_pos = text.find("text-a.md-1").unwrap();
        let c_pos = text.find("text-a.md-2").unwrap();
        assert!(a_pos < b_pos && b_pos < c_pos, "文档内应按阅读顺序重排: {}", text);
        // 文档按分数降序：a（0.9）在 b（0.5）之前
        assert!(text.find("--- a.md ---").unwrap() < text.find("--- b.md ---").unwrap());

        // 容量上限：max_chars 只允许头部与第一个片段
        let capped = build_context_text(&selected, 32);
        assert!(!capped.contains("text-a.md-1"), "超出上限的片段应被截断: {}", capped);
    }
}
