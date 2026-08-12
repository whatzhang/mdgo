//! Agent 模块：kb_search 工具 + Agent 构建（基于 Rig Agent）
//!
//! - [`build_kb_search_tool`]：将「嵌入 → 混合检索 → 文档聚合」封装为模型可调用的工具
//! - [`build_rag_agent`]：携带检索上下文与检索/文件/技能工具的 Agent（渐进式披露三级加载）
//! - [`build_chat_agent`]：无工具纯对话 Agent
//! - [`aggregate_hits`]：文档级聚合逻辑（与检索结果共享）
//! - [`SkillGateHook`] / [`SkillInstructionHook`]：由 [`ActiveSkillState`] 驱动的技能指令注入与工具兜底拦截

use std::collections::{HashMap, HashSet};
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
use tokio_util::sync::CancellationToken;

use crate::core::approval::ApprovalGate;
use crate::core::approval::hook::ApprovalGateHook;
use crate::core::skill::activation::{ActiveSkillState, MAX_SKILL_INJECTION_CHARS};
use crate::core::skill::SkillRegistry;

pub mod planner;
/// AI Agent 指标参数集中配置（单一来源）
pub mod limits;

pub use limits::{
    DEFAULT_MAX_TURNS, KB_TOP_K_SCHEMA_MAX, MAX_CONTEXT_CHARS, MAX_TOP_K, PERSISTENT_INJECTION,
};
use self::tool_registry::ToolRegistry;
use crate::core::{Indexer, SearchHit, call_embedding_query};

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
    log::info!("[agent_rules] 加载 Agent 规约: path={}", path.display());
    content
}

/// 内置工具集：文件只读、目录列举、Git 状态查询（含工具调用轨迹总线）
pub mod tools;
mod external_tools;
/// 工具注册表：按技能组织工具定义，统一管理工具的注册与构建
pub mod tool_registry;

/// 始终可用的基础工具（不随技能白名单窄化，对齐主流 Agent：文件操作与技能管理常驻）。
///
/// 检索类工具（kb_search / code_lookup）不在此列：它们仅当已激活技能声明时
/// 才可见可调——决策权在 LLM（先 activate_skill 再检索）。
pub const BASE_TOOLS: &[&str] = &[
    "activate_skill", "deactivate_skill", "read", "ls", "glob", "grep", "write", "edit", "multi_edit", "delete",
    "git_status", "git_diff", "git_commit", "git_checkout", "webfetch", "deep_research", "read_subagent_result",
    "remember", "forget", "search_memory",
    "todo_write",
    "spawn_subagent", "parallel_research", "self_review",
];

/// Agent 单次请求的模型调用总预算定义见 [`limits::DEFAULT_MAX_TURNS`]（集中配置）
///
/// 语义 = 模型调用次数上限（1-based）：第 1 次调用 turn=1，turn=10 是最后一次，
/// 第 11 次请求触发 MaxTurnsError。超出预算的流程由轮次预算预警 Hook 引导模型提前收尾。
// （常量定义已迁移至 limits.rs，经 pub use 再导出）

/// 调试用 Hook：在每次 LLM API 调用边界打印完整请求体与响应体。
///
/// 挂载到 AgentBuilder 后，无论流式（stream）还是非流式（completion）路径，
/// 都能在模型调用前拿到完整请求体（消息列表 + 运行上下文），
/// 在响应后拿到完整响应体（内容 + token 用量 + 消息 ID）。
#[derive(Clone, Debug)]
pub struct LlmTraceHook {
    /// 关联的请求 ID（build_rag_agent 传入；build_chat_agent 无检索链路时为 None）
    request_id: Option<String>,
}

impl LlmTraceHook {
    pub fn new(request_id: Option<String>) -> Self {
        Self { request_id }
    }
}

impl AgentHook for LlmTraceHook {
    async fn on_completion_call(
        &self,
        ctx: &HookContext,
        event: CompletionCall<'_>,
    ) -> CompletionCallAction {
        if log::log_enabled!(log::Level::Debug) {
            let mut messages = event.history.to_vec();
            messages.push(event.prompt.clone());
            let request_body = serde_json::json!({
                "turn": event.turn,
                "run_id": ctx.run_id().as_str(),
                "request_id": self.request_id,
                "agent_name": ctx.agent_name(),
                "is_streaming": ctx.is_streaming(),
                "messages": messages,
            });
            log::info!(
                "[llm_trace] >>> LLM 请求\n{}",
                serde_json::to_string_pretty(&request_body)
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
            let response_body = serde_json::json!({
                "message_id": event.message_id,
                "usage": event.usage,
                "content": event.content,
            });
            log::info!(
                "[llm_trace] <<< LLM 响应\n{}",
                serde_json::to_string_pretty(&response_body)
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
/// 技能门控 Hook：工具调用前的最后一道防线（兜底拦截 + 防重复熔断）。
///
/// 放行规则（任一命中即放行）：
/// - 基础工具（[`BASE_TOOLS`]）或 `allow_all`（子代理等受限场景）
/// - 已激活技能声明的工具
/// - 外部动态工具（P2-15，配置驱动，`allow_extra`）
#[derive(Clone, Debug)]
pub struct SkillGateHook {
    state: Arc<ActiveSkillState>,
    /// 是否放行全部已注册工具（子代理等受限场景：只读白名单已过滤注册表，无需技能声明）
    allow_all: bool,
    /// 外部动态工具名（P2-15 配置驱动工具；无需技能声明即可调用）
    allow_extra: Option<Arc<HashSet<String>>>,
}

impl SkillGateHook {
    pub fn new(
        state: Arc<ActiveSkillState>,
        allow_all: bool,
        allow_extra: Option<Arc<HashSet<String>>>,
    ) -> Self {
        Self {
            state,
            allow_all,
            allow_extra,
        }
    }
}

impl AgentHook for SkillGateHook {
    async fn on_tool_call(
        &self,
        ctx: &HookContext,
        event: ToolCall<'_>,
    ) -> ToolCallAction {
        // 防重复调用熔断（对所有工具生效，含基础工具）：同一 run 内
        // 「连续相同 (工具, 参数)」调用 ≥2 次后，第 3 次起跳过并引导模型
        // 改变策略，避免死循环浪费轮次预算。
        if let Some(warning) = tools::guard_duplicate_call(ctx.run_id().as_str(), event.tool_name, event.args) {
            log::warn!("[loop_guard] 熔断重复工具调用: {}", warning);
            return ToolCallAction::Skip(warning);
        }
        if BASE_TOOLS.contains(&event.tool_name)
            || self.allow_all
            || self
                .allow_extra
                .as_ref()
                .is_some_and(|set| set.contains(event.tool_name))
        {
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
            "工具 '{}' 当前不可用（未由任何已激活技能声明）。可先声明该工具的技能，或改用其他工具。",
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
#[derive(Clone)]
pub struct SkillInstructionHook {
    /// 静态基础 preamble（基础角色 + 预检索上下文 + L1 技能目录）
    base_preamble: String,
    /// 激活状态（动态读取工具白名单；回退模式下读取激活记录定位正文）
    state: Arc<ActiveSkillState>,
    /// 技能定义层（回退模式下据此重新查询已激活技能正文；一次性模式不使用）
    registry: Arc<SkillRegistry>,
    /// 本次请求的模型调用总预算（对齐 AgentBuilder::default_max_turns，
    /// 用于轮次预算预警：剩余不足时引导模型提前收敛）
    max_turns: usize,
    /// 是否按技能体系窄化模型可见工具（主对话 true；子代理等受限场景 false，
    /// 此时模型可见全部已注册工具——注册表层已用白名单过滤，天然安全）
    narrow_tools: bool,
    /// v2：MCP 工具名（窄化可见性时补齐，放行由 SkillGateHook.allow_extra 承担）
    mcp_tool_names: Vec<String>,
}

impl SkillInstructionHook {
    pub fn new(
        base_preamble: String,
        state: Arc<ActiveSkillState>,
        registry: Arc<SkillRegistry>,
        max_turns: usize,
        narrow_tools: bool,
        mcp_tool_names: Vec<String>,
    ) -> Self {
        Self {
            base_preamble,
            state,
            registry,
            max_turns,
            narrow_tools,
            mcp_tool_names,
        }
    }

    /// 回退模式：从 SkillRegistry 按激活记录重新查询已激活技能正文并拼接。
    ///
    /// 仅统计 `Active` 状态技能（warm/Candidate 未激活不注入）；
    /// 按激活顺序拼接，总量受 [`MAX_SKILL_INJECTION_CHARS`] 预算截断。
    fn persistent_instructions(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        let mut used = 0usize;
        for a in self.state.active_only() {
            if let Some(skill) = self.registry.get(a.scope, &a.skill_id) {
                let body = skill.body.trim();
                if body.is_empty() {
                    continue;
                }
                let block = format!("## {}\n\n{}", skill.name, body);
                let block_chars = block.chars().count();
                if used + block_chars > MAX_SKILL_INJECTION_CHARS {
                    break;
                }
                parts.push(block);
                used += block_chars;
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n---\n\n"))
        }
    }
}

impl AgentHook for SkillInstructionHook {
    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: CompletionCall<'_>,
    ) -> CompletionCallAction {
        // L2：技能正文注入——默认一次性（正文经 activate_skill 工具结果 / 请求入口
        // history 进入一次，此处不重复）；回退模式（PERSISTENT_INJECTION=true）
        // 每轮从定义层重新查询注入（三拆后激活状态不持有正文）。
        let mut preamble = self.base_preamble.clone();
        if PERSISTENT_INJECTION {
            if let Some(instructions) = self.persistent_instructions() {
                preamble.push_str("\n\n---\n\n请遵循以下已激活技能的指令：\n\n");
                preamble.push_str(&instructions);
            }
        }
        // 轮次预算预警：剩余模型调用轮次不足时（turn 从 1 开始计数），
        // 强制引导模型停止调用工具、基于已有信息直接给出最终答案，
        // 避免在第 6 次请求时触发 MaxTurnsError 导致整段回答丢失。
        let remaining = self.max_turns.saturating_sub(event.turn);
        if remaining <= 1 {
            preamble.push_str(&format!(
                "\n\n[预算提醒] 本次请求的模型调用预算为 {} 轮，当前已到最后 {} 轮。请停止调用任何工具，直接基于已有信息生成最终答案；如果信息不足，请如实说明缺口。",
                self.max_turns, remaining.max(1)
            ));
        }
        let mut patch = RequestPatch::new().preamble(&preamble);

        // 可见工具 = 基础工具 ∪ 已激活技能声明工具。
        // narrow_tools=false（子代理等受限场景）时不设 active_tools：
        // 模型可见全部已注册工具（注册表层已用白名单过滤）。
        if self.narrow_tools {
            let mut visible: Vec<String> = BASE_TOOLS.iter().map(|t| t.to_string()).collect();
            // 外部动态工具（配置驱动 HTTP 工具）常驻可见：SkillGateHook 已用
            // allow_extra 放行，此处补齐可见性——否则 active_tools 窄化后
            // 模型看不到外部工具，配置的 HTTP 工具永远不生效。
            for def in external_tools::load_external_tools_or_default() {
                if !visible.iter().any(|v| v == &def.name) {
                    visible.push(def.name.clone());
                }
            }
            // v2：MCP 工具补齐可见性（放行由 allow_extra 承担）
            for n in &self.mcp_tool_names {
                if !visible.iter().any(|v| v == n) {
                    visible.push(n.clone());
                }
            }
            if let Some(declared) = self.state.allowed_tools() {
                for t in declared {
                    if !visible.iter().any(|v| v == &t) {
                        visible.push(t);
                    }
                }
            }
            patch = patch.active_tools(visible);
        }
        CompletionCallAction::patch(patch)
    }
}

/// kb_search 工具允许的最大片段数（防止模型传入超大 top_k 触发全量检索/重排）
// MAX_TOP_K 定义已迁移至 limits.rs（pub use 再导出）

/// 聚合后送入模型上下文的总字符上限（约 3K token，避免超出模型窗口）。
/// kb_search 工具与 Agent 主链路共用此上限（单一来源）。
// MAX_CONTEXT_CHARS 定义已迁移至 limits.rs（pub use 再导出）

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
    /// 聚合绝对阈值（融合分数低于此值的命中不进入上下文；RRF 域）
    pub min_score: f32,
    /// 精排 sigmoid 阈值（`score_rerank` 非空的命中按此阈值裁决；与 pipeline 内精排阈值同语义）
    pub rerank_min_score: f32,
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
    /// 应用句柄：供前端通信桥工具（如 pomodoro）emit 事件并等待前端回传。
    pub app_handle: tauri::AppHandle,
    /// 当前请求的取消令牌：deep_research 等长耗时工具据此在父链取消时快速中止
    /// （无外部取消源的场景为 None）。
    pub cancel: Option<CancellationToken>,
}

/// 执行一次完整检索：嵌入 → 混合检索 → 文档级聚合 → 生成模型可读文本。
///
/// 返回的文本按文档分组，同文档的多个片段合并，供模型直接作为上下文。
pub async fn kb_search(cfg: &KbSearchConfig, query: &str, top_k: u32) -> Result<String, String> {
    log::info!("[skill] kb_search: query={}, top_k={}", query, top_k);
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

    let hits = cfg
        .indexer
        .hybrid_search(&cfg.dir_path, &embedding, query, top_k)
        .await?;
    if hits.is_empty() {
        return Ok("知识库中未找到相关内容。".to_string());
    }

    let hits_len = hits.len();
    let selected = aggregate_hits(
        hits,
        cfg.min_score,
        cfg.rerank_min_score,
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
    log::info!("[skill] kb_search 结果: 选中={}， 命中={} ，min_score={}， max_context_docs={}， max_chunks_per_doc={}", selected.len(), hits_len, cfg.min_score, cfg.max_context_docs, cfg.max_chunks_per_doc);
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
                    "minimum": 1,
                    "maximum": KB_TOP_K_SCHEMA_MAX,
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

                tools::record_tool_call(&cfg, "kb_search", &query, Some(&args));
                match kb_search(&cfg, &query, top_k).await {
                    Ok(text) => {
                        tools::record_tool_result(&cfg, "kb_search", true, &format!("{} 字符", text.chars().count()), Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        tools::record_tool_result(&cfg, "kb_search", false, &e, Some(&e));
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
                    "minimum": 1,
                    "maximum": KB_TOP_K_SCHEMA_MAX,
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

                tools::record_tool_call(&cfg, "code_lookup", &symbol, Some(&args));
                match code_search(&cfg, &symbol, top_k).await {
                    Ok(text) => {
                        tools::record_tool_result(&cfg, "code_lookup", true, &format!("{} 字符", text.chars().count()), Some(&text));
                        Ok(ToolOutput::text(text))
                    }
                    Err(e) => {
                        tools::record_tool_result(&cfg, "code_lookup", false, &e, Some(&e));
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
///
/// # 分数域契约（与 `Indexer::hybrid_search` 输出对齐）
/// `SearchHit.score` 在不同阶段承载不同分数域，本函数按域选择阈值：
/// - `score_rerank: Some(_)`：sigmoid 域（精排激活）→ 用 `rerank_min_score`（模型判定门槛，
///   与 pipeline 内精排阈值同语义，幂等兜底）
/// - `symbol_name: Some(_)` 且未精排：符号强信号 → 完全放行（符号路召回已按匹配质量
///   分级截断，且其 RRF 归一化分无判别力，不受融合阈值约束）
/// - 其余：RRF 归一化域（精排未激活/失败回退）→ 用 `min_score`（融合兜底阈值）
pub fn aggregate_hits(
    all_hits: Vec<SearchHit>,
    min_score: f32,
    rerank_min_score: f32,
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

    // 2. 按 doc_name 分组，分数域感知的绝对阈值过滤（见函数文档的分数域契约）
    let mut doc_map: HashMap<String, Vec<(SearchHit, f32)>> = HashMap::new();
    for (hit, score) in seen.into_values() {
        let is_sigmoid = hit.score_rerank.is_some();
        let is_symbol = hit.symbol_name.is_some();
        let pass = if is_sigmoid {
            score >= rerank_min_score
        } else if is_symbol {
            true
        } else {
            score >= min_score
        };
        if !pass {
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

/// 构建 Agent（渐进式披露三级加载）。
///
/// - `context`：预检索的知识库上下文，注入 system preamble
/// - `search_config`：用于构建检索/文件工具（模型可在生成过程中补充检索）
/// - `registry`：技能注册表（activate_skill 工具据此查找技能，LLM 自主决策 L2 加载）
/// - `catalog`：L1 技能目录（id + description，会话全程常驻，模型始终知道自己有哪些技能）
///
/// 工具与指令由 [`SkillInstructionHook`] / [`SkillGateHook`] 依据共享的
/// [`ActiveSkillState`]（在 `search_config.skill_state` 中）动态驱动。
///
/// 工具注册通过 [`ToolRegistry`] 统一管理：新增工具只需在 [`create_tool_registry`]
/// 中添加一行 `register` 调用，无需逐个手写 `.dynamic_tool(...)`。
pub fn build_rag_agent(
    model: openai::CompletionModel,
    context: &str,
    search_config: KbSearchConfig,
    registry: Arc<SkillRegistry>,
    catalog: String,
    base: String,
    // 审批门(破坏性操作确认);None = 不启用(保持原行为)
    approval_gate: Option<Arc<ApprovalGate>>,
    // 模型调用总预算(轮次上限):主对话用 DEFAULT_MAX_TURNS,子代理等场景可传更大值
    max_turns: usize,
    // 工具白名单:Some(子集) 时只注册白名单内工具(子代理只读调研);None = 全量
    tool_whitelist: Option<&HashSet<String>>,
    // 是否启用技能体系的 active_tools 窄化与工具门禁:
    // 主对话 true(工具可见性/放行由技能声明决定);子代理 false(白名单已过滤,全放行)
    narrow_tools: bool,
    // v2：MCP 工具（已连接服务器注册的 DynamicTool，命名 mcp:<server>:<tool>）
    mcp_tools: Vec<DynamicTool>,
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
    }

    log::info!("[agent_query] [4]: L1 技能目录注入: chars={}, preamble_len={}", catalog.len(), preamble.len());

    // P2-15：外部动态工具放行名（配置驱动 HTTP 工具；加载失败降级空集）
    let external_tool_names: std::sync::Arc<std::collections::HashSet<String>> =
        std::sync::Arc::new(
            external_tools::load_external_tools_or_default()
                .iter()
                .map(|d| d.name.clone())
                .collect(),
        );

    // v2：MCP 工具名并入放行集合（无需技能声明即可调用）
    let mcp_tool_names: Vec<String> = mcp_tools
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    let mut allow_extra: std::collections::HashSet<String> = external_tool_names.as_ref().clone();
    for n in &mcp_tool_names {
        allow_extra.insert(n.clone());
    }
    let allow_extra = std::sync::Arc::new(allow_extra);

    // 始终注册全部内置工具；每轮模型可见的工具列表由 SkillInstructionHook
    // 依据激活状态窄化（active_tools），SkillGateHook 作为兜底拦截越权调用。
    //
    // 工具通过 ToolRegistry 统一管理：注册表按技能组织，新增工具只需一行 register。
    let tool_reg = create_tool_registry(tool_whitelist);
    let tools = tool_reg.build_all(&search_config);
    let mut iter = tools.into_iter();
    // AgentBuilder 类型状态：第一个 dynamic_tool 从 NoToolConfig → WithBuilderTools。
    // 工具白名单恒非空（全量或只读子集均有内置工具），此处不会 panic；若未来引入
    // 空白名单场景，需先解决类型状态机约束再放开。
    let first = iter.next().expect("ToolRegistry 必须至少注册一个工具（白名单不能为空）");
    let mut builder = AgentBuilder::new(model).dynamic_tool(first);
    for tool in iter {
        builder = builder.dynamic_tool(tool);
    }

    // activate_skill / deactivate_skill 依赖 SkillRegistry/ActiveSkillState，
    // 不走通用注册表（参数签名不同），直接注册。
    // 尊重工具白名单：受限场景（如只读子代理）白名单不含技能激活时跳过注册，
    // 避免子代理通过激活技能注入 SKILL.md 指令（提示注入面）。
    if tool_whitelist.is_none_or(|set| set.contains("activate_skill")) {
        builder = builder
            .dynamic_tool(tools::build_activate_skill_tool(registry.clone(), skill_state.clone()));
    }
    if tool_whitelist.is_none_or(|set| set.contains("deactivate_skill")) {
        builder = builder.dynamic_tool(tools::build_deactivate_skill_tool(skill_state.clone()));
    }

    // v2：MCP 工具注册（命名 mcp:<server>:<tool>）
    for tool in mcp_tools {
        builder = builder.dynamic_tool(tool);
    }

    let mut builder = builder
        // 模型调用总预算：技能激活 + 文件读取 + 检索等流程通常需要多轮；
        // 剩余不足时由 SkillInstructionHook 注入预算预警引导模型提前收敛
        .default_max_turns(max_turns)
        .add_hook(LlmTraceHook::new(Some(search_config.request_id.clone())))
        .add_hook(SkillInstructionHook::new(preamble, skill_state.clone(), registry.clone(), max_turns, narrow_tools, mcp_tool_names))
        .add_hook(SkillGateHook::new(skill_state, !narrow_tools, Some(allow_extra)));
    // 审批门(可选)：先技能白名单、后审批，避免对「本就不该调用的工具」弹窗打扰用户。
    if let Some(gate) = approval_gate {
        builder = builder.add_hook(ApprovalGateHook::new(gate));
    }
    builder.build()
}

/// 创建工具注册表，注册所有业务工具。
///
/// 新增工具只需在此函数中添加一行 `register` 调用即可。
/// 工具的可见性由技能声明（`tools: [...]`）和 `active_tools` 过滤控制：
/// - BASE_TOOLS 中的工具始终可见
/// - 其余工具仅当已激活技能声明时才可见
///
/// 200+ 工具场景下，每个工具一行 `register`，按技能分组注释，维护成本低。
fn create_tool_registry(only: Option<&HashSet<String>>) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    let want = |name: &str| only.is_none_or(|set| set.contains(name));

    // ── 检索类工具（kb-search / code-lookup 技能声明） ──
    if want("kb_search") {
        reg.register("kb_search", Box::new(build_kb_search_tool));
    }
    if want("code_lookup") {
        reg.register("code_lookup", Box::new(build_code_lookup_tool));
    }

    // ── 文件操作工具（BASE_TOOLS，始终可见） ──
    if want("read") {
        reg.register("read", Box::new(tools::build_read_tool));
    }
    if want("grep") {
        reg.register("grep", Box::new(tools::build_grep_tool));
    }
    if want("edit") {
        reg.register("edit", Box::new(tools::build_edit_tool));
    }
    if want("multi_edit") {
        reg.register("multi_edit", Box::new(tools::build_multi_edit_tool));
    }
    if want("write") {
        reg.register("write", Box::new(tools::build_write_tool));
    }
    if want("delete") {
        reg.register("delete", Box::new(tools::build_delete_tool));
    }
    if want("ls") {
        reg.register("ls", Box::new(tools::build_ls_tool));
    }
    if want("glob") {
        reg.register("glob", Box::new(tools::build_glob_tool));
    }

    // ── Git 工具（repo-status 技能声明） ──
    if want("git_status") {
        reg.register("git_status", Box::new(tools::build_git_status_tool));
    }
    if want("git_diff") {
        reg.register("git_diff", Box::new(tools::build_git_diff_tool));
    }
    if want("git_commit") {
        reg.register("git_commit", Box::new(tools::build_git_commit_tool));
    }
    if want("git_checkout") {
        reg.register("git_checkout", Box::new(tools::build_git_checkout_tool));
    }
    if want("webfetch") {
        reg.register("webfetch", Box::new(tools::build_webfetch_tool));
    }

    // ── 番茄钟工具（pomodoro 技能声明，非 BASE_TOOLS） ──
    if want("pomodoro") {
        reg.register("pomodoro", Box::new(tools::build_pomodoro_tool));
    }

    // ── 子代理工具（全量注册；只读子代理注册表经白名单排除，防无限递归） ──
    if want("deep_research") {
        reg.register("deep_research", Box::new(tools::build_deep_research_tool));
    }
    if want("read_subagent_result") {
        reg.register("read_subagent_result", Box::new(tools::build_read_subagent_result_tool));
    }
    if want("spawn_subagent") {
        reg.register("spawn_subagent", Box::new(tools::build_spawn_subagent_tool));
    }
    if want("parallel_research") {
        reg.register("parallel_research", Box::new(tools::build_parallel_research_tool));
    }

    // ── 反思质量门工具（BASE_TOOLS） ──
    if want("self_review") {
        reg.register("self_review", Box::new(tools::build_self_review_tool));
    }

    // ── 长期记忆工具（BASE_TOOLS；remember/forget 为写操作，子代理白名单排除） ──
    if want("remember") {
        reg.register("remember", Box::new(tools::build_remember_tool));
    }
    if want("forget") {
        reg.register("forget", Box::new(tools::build_forget_tool));
    }
    if want("search_memory") {
        reg.register("search_memory", Box::new(tools::build_search_memory_tool));
    }
    if want("todo_write") {
        reg.register("todo_write", Box::new(tools::build_todo_write_tool));
    }

    // ── 外部动态工具（P2-15：配置驱动 HTTP 工具，跳过技能声明过滤但尊重白名单） ──
    // 放行由 SkillGateHook 的 allow_extra 承担；与内置工具重名时跳过并告警；
    // 子代理白名单（只读/写型集合）不含外部工具名 → want 为 false → 不注册，
    // 防止只读子代理意外暴露可任意发 HTTP 的外部写面（review 修复）。
    let builtin: std::collections::HashSet<String> = reg.tool_names().iter().map(|s| s.to_string()).collect();
    for def in external_tools::load_external_tools_or_default() {
        if !want(&def.name) {
            continue;
        }
        if builtin.contains(&def.name) {
            log::warn!("[external_tools] 外部工具「{}」与内置工具重名，跳过注册", def.name);
            continue;
        }
        let name = def.name.clone();
        reg.register(
            &name,
            Box::new(move |cfg| external_tools::build_external_tool(def.clone(), cfg)),
        );
    }

    reg
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
        .add_hook(LlmTraceHook::new(None))
        .build()
}
