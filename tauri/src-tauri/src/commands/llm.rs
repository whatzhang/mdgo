use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use rig_agent::agent::MultiTurnStreamItem;
use rig_agent::streaming::StreamingChat;
use rig_core::completion::message::{ToolCall, ToolFunction};
use rig_core::completion::{AssistantContent, Message};
use rig_core::streaming::StreamedAssistantContent;
use rig_core::OneOrMany;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::core::agent::{
    KbSearchConfig, aggregate_hits, build_chat_agent, build_context_text, build_rag_agent,
    load_agent_rules,
};
use crate::core::agent::tools::tool_call_bus;
use crate::core::context::{
    ChatTurn, ContextCompressor, SummarizeThenWindowCompressor, tokens_to_chars_budget,
};
use crate::core::skill::activation::{ActivationSource, ActiveSkillState};
use crate::core::skill::context::{SkillExecutionContext, build_skill_catalog, resolve_preactivated};
use crate::core::skill::SkillStore;
use crate::core::{call_embedding_query, SearchHit};
use crate::services::llm::{LLMClient, UsageInfo, usage_to_info};

// ─── 后端消息长度预算（集中定义见 crate::core::agent::limits） ───
use crate::core::agent::limits::{MAX_MESSAGE_CHARS, MAX_MESSAGE_TOKENS, SUMMARY_MAX_CHARS};

// ─── 事件类型 ───

#[derive(Clone, Serialize)]
pub struct RagStatus {
    pub request_id: String,
    pub stage: String,
    pub message: String,
}

#[derive(Clone, Serialize)]
pub struct RagDelta {
    pub request_id: String,
    pub content: String,
}

#[derive(Clone, Serialize)]
pub struct RagSource {
    pub doc_name: String,
    pub score: f32,
    pub text: String,
    /// OPML 节点路径 JSON 数组（仅 OPML 文件有值）
    pub path_json: Option<String>,
    /// 代码符号名（仅代码文件有值），前端可用于高亮匹配
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_name: Option<String>,
    /// 代码符号类型（仅代码文件有值）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct RagDone {
    pub request_id: String,
    pub content: String,
    pub sources: Vec<RagSource>,
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}

#[derive(Clone, Serialize)]
pub struct LlmDelta {
    pub request_id: String,
    pub content: String,
}

#[derive(Clone, Serialize)]
pub struct LlmDone {
    pub request_id: String,
    pub content: String,
}

#[derive(Clone, Serialize)]
pub struct CommandError {
    pub request_id: String,
    pub message: String,
}

// ─── AppState 扩展 ───

/// 可取消的任务注册表
pub struct TaskRegistry {
    pub tasks: Mutex<HashMap<String, CancellationToken>>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
        }
    }

    /// 注册一个可取消任务，返回 CancellationToken
    pub async fn register(&self, request_id: &str) -> CancellationToken {
        let token = CancellationToken::new();
        let mut map = self.tasks.lock().await;
        map.insert(request_id.to_string(), token.clone());
        token
    }

    /// 取消指定任务
    pub async fn cancel(&self, request_id: &str) {
        let mut map = self.tasks.lock().await;
        if let Some(token) = map.remove(request_id) {
            token.cancel();
        }
    }

    /// 任务完成后注销
    pub async fn unregister(&self, request_id: &str) {
        let mut map = self.tasks.lock().await;
        map.remove(request_id);
    }
}

// ─── 辅助函数 ───

/// 将消息历史（去掉最后一条当前问题）压缩到预算内。
///
/// 超预算时按策略压缩（摘要+滑窗，或纯滑窗兜底），压缩永不失败；
/// 返回压缩结果供调用方决定是否提示前端。
/// 应用会话压缩检查点（P0-5）：若检查点存在且历史中包含 cutoff 消息，
/// 则用摘要 system 消息替换 cutoff 之前的消息，避免每次请求对全部历史重算。
///
/// - 检查点不存在 / cutoff 消息已被前端裁剪 → 原样返回（安全降级为全量压缩）
/// - `cutoff_msg_id` 为 `None`（旧数据）→ 原样返回
fn apply_compaction_checkpoint(
    messages: &[crate::services::llm::ChatMessage],
    checkpoint: Option<&crate::core::context::CompactionState>,
) -> Vec<crate::services::llm::ChatMessage> {
    let Some(cp) = checkpoint else {
        return messages.to_vec();
    };
    let Some(cutoff_id) = &cp.cutoff_msg_id else {
        return messages.to_vec();
    };
    let Some(idx) = messages.iter().position(|m| m.id.as_deref() == Some(cutoff_id.as_str()))
    else {
        return messages.to_vec();
    };
    let mut out = Vec::with_capacity(messages.len() - idx + 1);
    out.push(crate::services::llm::ChatMessage {
        id: None,
        role: "system".into(),
        content: cp.summary.clone(),
        tool_calls: None,
        tool_call_id: None,
    });
    out.extend(messages[idx..].iter().cloned());
    out
}

async fn prepare_history(
    messages: &[crate::services::llm::ChatMessage],
    compressor: &dyn ContextCompressor,
    cancel: CancellationToken,
) -> crate::core::context::CompressedHistory {
    let turns: Vec<ChatTurn> = messages[..messages.len().saturating_sub(1)]
        .iter()
        .map(|m| ChatTurn {
            role: m.role.clone(),
            content: m.content.clone(),
            tool_calls: m.tool_calls.clone(),
            tool_call_id: m.tool_call_id.clone(),
        })
        .collect();
    compressor
        .compress(&turns, tokens_to_chars_budget(MAX_MESSAGE_TOKENS), cancel)
        .await
}

/// 将压缩后的历史轮次转为 Rig history
fn chat_turns_to_history(turns: &[ChatTurn]) -> Vec<Message> {
    // 统计历史中实际存在的 tool 结果 id：过滤「孤儿 tool_call」
    // （成功但空输出的工具其 result 为空串，前端不生成 tool 消息），
    // 否则 OpenAI 协议会因 tool_call 无配对结果而拒绝请求（review 修复）。
    let tool_result_ids: std::collections::HashSet<&str> = turns
        .iter()
        .filter(|t| t.role == "tool")
        .filter_map(|t| t.tool_call_id.as_deref())
        .collect();
    turns
        .iter()
        .map(|t| match t.role.as_str() {
            "system" => Message::system(&t.content),
            "assistant" => {
                let has_tools = t.tool_calls.as_ref().is_some_and(|c| !c.is_empty());
                if !has_tools {
                    return Message::assistant(&t.content);
                }
                let mut contents: Vec<AssistantContent> = Vec::new();
                if !t.content.is_empty() {
                    contents.push(AssistantContent::text(&t.content));
                }
                for tc in t.tool_calls.iter().flatten() {
                    // 仅保留历史中有对应 tool 结果消息的调用（孤儿调用剔除）
                    if !tool_result_ids.contains(tc.id.as_str()) {
                        continue;
                    }
                    // 参数为模型原始 JSON 字符串：解析失败时降级为空对象（防御，不阻断请求）
                    let args = serde_json::from_str(&tc.arguments)
                        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
                    contents.push(AssistantContent::ToolCall(ToolCall::new(
                        tc.id.clone(),
                        ToolFunction::new(tc.name.clone(), args),
                    )));
                }
                // 全部被过滤且无文本：补占位文本，避免空 assistant 消息（协议同样会拒绝）
                if contents.is_empty() {
                    contents.push(AssistantContent::text("（此前发起的部分工具调用结果为空，已省略）"));
                }
                Message::Assistant {
                    id: None,
                    content: OneOrMany::many(contents)
                        .expect("contents 至少含一个占位文本，expect 安全"),
                }
            }
            "tool" => Message::tool_result_with_call_id(
                t.tool_call_id.clone().unwrap_or_default(),
                t.tool_call_id.clone(),
                &t.content,
            ),
            _ => Message::user(&t.content),
        })
        .collect()
}

/// 流式消费循环的"下一个事件或取消"等待器。
///
/// 用 `tokio::select!` 同时等待流事件与取消信号:
/// - `Ok(Some(item))`:正常事件
/// - `Ok(None)`:流正常结束
/// - `Err(())`:取消已触发 —— 调用方应立即 return,select 会丢弃挂起中的
///   stream future;rig 的流是惰性驱动的,drop 会尽力断开底层 reqwest 连接
///   (连接可能被连接池复用,但取消不再依赖下一个 SSE chunk 到达)。
async fn next_or_cancel<T>(
    stream: &mut (impl futures_util::Stream<Item = T> + Unpin),
    cancel: &CancellationToken,
) -> Result<Option<T>, ()> {
    tokio::select! {
        biased; // 取消与流事件同时就绪时取消优先(严格"立即断开")
        _ = cancel.cancelled() => Err(()),
        item = stream.next() => Ok(item),
    }
}

/// 计算各作用域技能基础目录（供 read 工具按需读取已激活技能的参考文档，渐进式披露 L3）。
///
/// - system：应用资源目录下的 `skills`（开发期资源未同步时回退到源码资源目录）
/// - global：用户全局技能目录 `{appdata}/com.mdgo/skills`
/// - project：`{打开目录}/.mdgo/skills`
///
/// read 工具按「激活技能 → 作用域匹配 → 基础目录/skill_id」定位，仅限已激活技能。
fn resolve_skill_bases(app: &AppHandle, dir_path: &str) -> Vec<(String, String)> {
    let mut bases = Vec::new();
    let sys = app
        .path()
        .resource_dir()
        .map(|r| r.join("skills"))
        .unwrap_or_default();
    let sys = if sys.exists() {
        sys
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("skills")
    };
    bases.push(("system".to_string(), sys.to_string_lossy().to_string()));
    bases.push((
        "global".to_string(),
        SkillStore::global_skills_dir().to_string_lossy().to_string(),
    ));
    bases.push((
        "project".to_string(),
        SkillStore::project_skills_dir(dir_path)
            .to_string_lossy()
            .to_string(),
    ));
    bases
}

/// 将检索命中构建为引用来源列表（按 doc_name 去重，合并文本与 path_json，取最高分）。
///
/// 预检索与 kb_search / code_lookup 工具命中共用此逻辑，保证引用格式一致。
fn build_sources(selected: &[(SearchHit, f32)]) -> Vec<RagSource> {
    let mut source_dedup: std::collections::HashMap<String, RagSource> = std::collections::HashMap::new();
    for (hit, _) in selected {
        let doc_name = hit.doc_name.clone();
        let text = hit.text.clone();
        let path_json = hit.path_json.clone();
        let score = hit.score;
        match source_dedup.entry(doc_name.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let existing = entry.get_mut();
                // 合并文本：仅当新文本未包含在已有文本中时才追加
                if !existing.text.contains(&text) && !text.contains(&existing.text) {
                    existing.text.push('\n');
                    existing.text.push_str(&text);
                }
                // 取最高分
                if score > existing.score {
                    existing.score = score;
                }
                // 合并 path_json（OPML/FreeMind 路径追加）
                if let Some(ref pj) = path_json {
                    match existing.path_json {
                        Some(ref mut existing_path) => {
                            if !existing_path.contains(pj) {
                                existing_path.push(',');
                                existing_path.push_str(pj);
                            }
                        }
                        None => {
                            existing.path_json = Some(pj.clone());
                        }
                    }
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(RagSource {
                    doc_name,
                    score,
                    text,
                    path_json,
                    symbol_name: hit.symbol_name.clone(),
                    symbol_kind: hit.symbol_kind.clone(),
                });
            }
        }
    }
    let mut sources: Vec<RagSource> = source_dedup.into_values().collect();
    sources.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    sources
}

/// 合并 kb_search / code_lookup 工具的检索命中到引用来源（按 doc_name 去重、保留最高分）。
///
/// 请求期间 Agent 调用的检索工具命中累积在 `search_sink`，rag:done 发射前
/// 与预检索来源合并，保证 LLM 驱动的检索同样出现在前端"引用"列表。
async fn merge_search_sink(
    sources: Vec<RagSource>,
    sink: &tokio::sync::Mutex<Vec<(SearchHit, f32)>>,
) -> Vec<RagSource> {
    let hits = {
        let mut guard = sink.lock().await;
        std::mem::take(&mut *guard)
    };
    if hits.is_empty() {
        return sources;
    }
    let mut map: std::collections::HashMap<String, RagSource> = sources
        .into_iter()
        .map(|s| (s.doc_name.clone(), s))
        .collect();
    for s in build_sources(&hits) {
        match map.entry(s.doc_name.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let existing = entry.get_mut();
                if !existing.text.contains(&s.text) && !s.text.contains(&existing.text) {
                    existing.text.push('\n');
                    existing.text.push_str(&s.text);
                }
                if s.score > existing.score {
                    existing.score = s.score;
                }
                if let Some(ref pj) = s.path_json {
                    match existing.path_json {
                        Some(ref mut ep) => {
                            if !ep.contains(pj) {
                                ep.push(',');
                                ep.push_str(pj);
                            }
                        }
                        None => {
                            existing.path_json = Some(pj.clone());
                        }
                    }
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(s);
            }
        }
    }
    let mut out: Vec<RagSource> = map.into_values().collect();
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// 发送错误事件（rag:error / llm:error）
fn emit_command_error(app: &AppHandle, channel: &str, request_id: &str, message: String) {
    let _ = app.emit(
        channel,
        CommandError {
            request_id: request_id.to_string(),
            message,
        },
    );
}

/// 转发该请求挂起的工具调用事件（消费式），供前端渲染工具调用轨迹。
///
/// 工具闭包在 Rig 流式内部执行，无法直接 emit Tauri 事件，故先写入
/// [`crate::core::agent::tools::ToolCallBus`]，由流式循环在此处统一转发。
fn emit_pending_tool_events(app: &AppHandle, request_id: &str) {
    for event in tool_call_bus().drain(request_id) {
        // 直接序列化 ToolCallEvent：skill_id 经 skip_serializing_if 仅在有值时输出，
        // 避免手动重建 JSON 丢失字段（如技能来源 skill_id）。
        let mut payload = match serde_json::to_value(&event) {
            Ok(v) => v,
            Err(_) => continue,
        };
        payload["request_id"] = serde_json::Value::String(request_id.to_string());
        let channel = if event.kind == "call" {
            "agent:tool_call"
        } else {
            "agent:tool_result"
        };
        let _ = app.emit(channel, payload);
    }
}

/// 消费式转发该请求的 trace 事件（`trace:event`，前端按 request_id 过滤渲染）。
fn emit_pending_trace_events(app: &AppHandle, request_id: &str) {
    let events = crate::core::trace::trace_bus().drain(request_id);
    if !events.is_empty() {
        let _ = app.emit(
            "trace:event",
            serde_json::json!({
                "request_id": request_id,
                "events": events,
            }),
        );
    }
}

/// 收集本次请求的技能执行输入（预激活 ∪ LLM 动态激活 ∪ 中途停用，去重），供批量落库。
///
/// 耗时按技能独立计时：优先取该技能「激活时刻 → 请求结束」的实际时长
/// （`ActiveSkillState::activated_elapsed`），查不到时回退请求总时长。
/// 中途被停用的技能经 `deactivated_elapsed` 补录，避免激活后又停用的执行漏记。
fn collect_skill_exec_inputs(
    skill_ctx: Option<&SkillExecutionContext>,
    active_skills: &ActiveSkillState,
    fallback_duration_ms: u64,
) -> Vec<crate::core::skill::metrics::ExecInput> {
    use crate::core::skill::metrics::ExecInput;
    use std::collections::{HashMap, HashSet};

    let mut recorded: HashSet<(String, String)> = HashSet::new();
    let mut inputs: Vec<ExecInput> = Vec::new();

    // 各技能的实际耗时（scope:id → ms）：轻量读取一次（不克隆 Skill body）
    let active = active_skills.activated_elapsed();
    let mut elapsed_by_key: HashMap<String, u64> = HashMap::new();
    for (scope, id, elapsed) in &active {
        elapsed_by_key.insert(format!("{}:{}", scope, id), *elapsed);
    }

    // 预激活技能（手动触发 / 会话挂载）
    if let Some(ctx) = skill_ctx {
        for m in &ctx.matches {
            recorded.insert((m.scope.clone(), m.skill_id.clone()));
            let key = format!("{}:{}", m.scope, m.skill_id);
            let duration = elapsed_by_key
                .get(&key)
                .copied()
                .unwrap_or(fallback_duration_ms);
            inputs.push(ExecInput {
                skill_id: m.skill_id.clone(),
                scope: m.scope.clone(),
                source: m.source,
                match_score: m.match_score,
                duration_ms: duration,
            });
        }
    }
    // LLM 会话中激活的技能（当前激活 + 中途停用，不在预激活上下文内）：按 Llm 来源补录，避免重复
    let deactivated = active_skills.deactivated_elapsed();
    for (scope, id, elapsed) in active.iter().chain(deactivated.iter()) {
        let key = (scope.clone(), id.clone());
        if recorded.insert(key) {
            inputs.push(ExecInput {
                skill_id: id.clone(),
                scope: scope.clone(),
                source: ActivationSource::Llm,
                match_score: 1.0,
                duration_ms: *elapsed,
            });
        }
    }
    inputs
}

/// 批量记录技能执行结果（在 spawn_blocking 中调用，避免阻塞 async runtime）。
///
/// 记录范围 = 预激活技能（手动触发/会话挂载，`skill_ctx.matches`）
/// ∪ 请求期间 LLM 经 `activate_skill` 激活的技能（主路径，`active_skills`），
/// 保证 LLM 驱动的激活同样进入指标闭环，而不是只统计预激活。
fn record_skill_execution(
    metrics: &crate::core::skill::metrics::SkillMetrics,
    dir_path: &str,
    inputs: Vec<crate::core::skill::metrics::ExecInput>,
    success: bool,
    error_code: Option<&str>,
    request_id: &str,
) {
    metrics.record_execution_batch(dir_path, inputs, success, error_code, request_id);
}

/// 获取或创建 LLM 客户端。
///
/// 按配置指纹缓存，复用内部 reqwest 连接池；配置热更新后指纹变化，自动重建。
/// 构建失败（非法 api_key 等）返回 Err，由调用方转为错误事件。
async fn get_or_create_llm_client(
    state: &tauri::State<'_, crate::AppState>,
    endpoint: &str,
    model: &str,
    api_key: &str,
) -> Result<LLMClient, String> {
    // 委托 AppState 的公共工厂:供 commands 层与工具闭包(子代理)共用
    state.llm_client_for(endpoint, model, api_key).await
}

// ─── Tauri 命令 ───

/// 取消正在运行的任务
#[tauri::command]
pub async fn kb_cancel_task(
    state: tauri::State<'_, TaskRegistry>,
    request_id: String,
) -> Result<(), String> {
    state.cancel(&request_id).await;
    Ok(())
}

/// RAG 查询：技能解析 → 查询扩展 → 混合检索 → 文档聚合 → RAG Agent 生成（全流式）
#[tauri::command]
pub async fn agent_query(
    app: AppHandle,
    state: tauri::State<'_, crate::AppState>,
    task_registry: tauri::State<'_, TaskRegistry>,
    dir_path: String,
    query: String,
    messages: Vec<crate::services::llm::ChatMessage>,
    request_id: String,
    top_k: u32,
    session_id: Option<String>,
) -> Result<(), String> {
    let cancel = task_registry.register(&request_id).await;
    // 后端防御：限制 top_k 范围（前端 UI 为 1-50），防止异常参数触发全量检索/重排
    let top_k = top_k.clamp(1, 50);

    // 请求级任务清单（todo_write 工具）隔离：新请求开始时清空上次残留
    crate::core::agent::tools::reset_todo(&request_id);

    log::info!("[agent_query] [0]: 开始 agent: request_id={} dir_path={} query_len={} msg_count={} top_k={}",
        request_id, dir_path, query.len(), messages.len(), top_k);

    // 从中央化内存配置读取 LLM 配置
    let llm_cfg = state.llm_config.read().unwrap_or_else(|e| e.into_inner()).clone();

    // 构建 LLM 客户端（失败转为错误事件，避免 panic 与注册表泄漏）
    let llm = match get_or_create_llm_client(&state, &llm_cfg.endpoint, &llm_cfg.model, &llm_cfg.api_key).await {
        Ok(llm) => llm,
        Err(e) => {
            log::error!("[agent_query] [0]: LLMClient 初始化失败: request_id={} err={}", request_id, e);
            emit_command_error(&app, "rag:error", &request_id, format!("LLM 客户端初始化失败: {}", e));
            task_registry.unregister(&request_id).await;
            return Ok(());
        }
    };

    if !llm.is_configured() {
        log::warn!("[agent_query] [0]: LLM 未配置: request_id={}", request_id);
        emit_command_error(&app, "rag:error", &request_id, "LLM 未配置，请在设置中填写端点地址和模型名称".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // 历史上下文压缩器：优先「摘要+滑窗」（依赖 LLM），否则纯滑窗兜底（压缩永不失败）
    // P0-6：摘要可用独立轻量模型（summary_model），缺省回退主模型
    let summary_llm = match &llm_cfg.summary_model {
        Some(_) => match state
            .llm_client_for_role(&llm_cfg, crate::ModelRole::Summary)
            .await
        {
            Ok(client) => client,
            Err(e) => {
                log::warn!("[agent_query] [0]: 摘要模型不可用，回退主模型: {}", e);
                llm.clone()
            }
        },
        None => llm.clone(),
    };
    let summarizer: Arc<dyn crate::core::context::HistorySummarizer> = Arc::new(summary_llm);
    let compressor: Arc<dyn ContextCompressor> = Arc::new(SummarizeThenWindowCompressor::new(
        summarizer,
        SUMMARY_MAX_CHARS,
    ));

    // ── Stage 0: 技能预激活（手动触发 / 会话挂载）──
    // 激活决策已交由 LLM（渐进式披露 L1/L2）：此处不做任何本地匹配，
    // 仅处理两类显式预激活并写入共享激活状态 active_skills，供 Agent 钩子
    // （L2 指令注入）与技能工具（activate_skill / deactivate_skill）后续使用。
    let active_skills = Arc::new(ActiveSkillState::new());
    // 闭包用 session_id 副本，避免 move 后原值不可用（检查点读写仍需 session_id）
    let session_id_for_closure = session_id.clone();
    let skill_resolved = {
        let registry = state.skill_registry.clone();
        // 会话挂载查询（rusqlite I/O）与技能解析同为阻塞操作，
        // 一并移入 spawn_blocking 调度，避免阻塞异步运行时
        let chat_store = match &session_id {
            Some(_) => state.get_chat_store(&dir_path).ok(),
            None => None,
        };
        let query_for_skill = query.clone();
        let request_id_for_log = request_id.clone();
        let dir_for_registry = dir_path.clone();
        let active = active_skills.clone();
        match tokio::task::spawn_blocking(move || {
            // 注册表未加载过时先重建（幂等；对话前前端已调用 skill_list，此处兜底）
            let _ = registry.ensure_loaded(&dir_for_registry);
            let attached_skills: Vec<(String, String)> = match (&chat_store, &session_id_for_closure) {
                (Some(store), Some(sid)) => store
                    .get_attached_skills(sid)
                    .map(|list| list.into_iter().map(|(s, id, _v)| (s, id)).collect())
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            resolve_preactivated(&query_for_skill, &registry, &attached_skills, &active)
        })
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                log::warn!("[agent_query] [0]: 技能预激活失败 request_id={} err={}", request_id_for_log, e);
                None
            }
            Err(e) => {
                log::warn!("[agent_query] [0]: 技能预激活任务失败 request_id={} err={}", request_id_for_log, e);
                None
            }
        }
    };
    let skill_ctx = skill_resolved.as_ref().map(|r| &r.context);
    // 手动触发时使用清理后的查询（剥离 /技能名 前缀），其余场景保持原查询；
    // filter 守卫：清理结果为空字符串时回退到原查询，避免空查询进入检索
    let query = skill_resolved
        .as_ref()
        .map(|r| r.cleaned_query.clone())
        .filter(|q| !q.trim().is_empty())
        .unwrap_or(query);
    // 防御：单条超长问题截断到预算上限（边缘 case；原"全量超限拒绝"已由历史压缩替代，
    // 当前问题不参与压缩预算，这里兜底避免超预算请求直接打 API）
    let query = if query.chars().count() > MAX_MESSAGE_CHARS {
        log::warn!("[agent_query] [0]: 当前问题超长({} 字符)，截断到 {} 字符 request_id={}",
            query.chars().count(), MAX_MESSAGE_CHARS, request_id);
        query.chars().take(MAX_MESSAGE_CHARS).collect()
    } else {
        query
    };
    // 调度计数：总数在请求起始计入（仅自增 total，不阻塞请求主链路）；
    // 是否命中由请求结束时按实际激活情况补记（见 4 个终态点，覆盖预激活 ∪ LLM 动态激活）。
    {
        let metrics = state.skill_metrics.clone();
        let dir = dir_path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            metrics.record_dispatch(&dir, false);
        });
    }
    if let Some(ctx) = skill_ctx {
        log::info!(
            "[agent_query] [0]: skills 手动触发 request_id={} skills={:?} manual={}",
            request_id,
            ctx.skill_ids,
            skill_resolved.as_ref().map(|r| r.is_manual).unwrap_or(false)
        );
    } else {
        log::info!(
            "[agent_query] [0]: 自动触发技能（技能激活交由 LLM 决策）request_id={}",
            request_id
        );
    }

    // 技能检索参数覆盖（技能优先：技能显式配置时以技能为准，可放宽全局限制；
    // 未配置时回退全局配置兜底），应用于主预检索（Stage 2/3）与 kb_search 工具（Stage 4）。
    // 多技能同时命中时，context 内部仍按最保守值合并（见 SkillExecutionContext::from_skills）
    let kb_cfg = state.config_store.read();
    let effective_top_k = skill_ctx
        .and_then(|c| c.top_k)
        .unwrap_or(top_k)
        .clamp(1, 50);
    let effective_min_score = skill_ctx
        .and_then(|c| c.min_score)
        .unwrap_or(kb_cfg.min_score)
        .clamp(0.0, 1.0);
    // 精排 sigmoid 阈值：与 pipeline 内精排阈值同语义，供下游聚合按分数域裁决
    let effective_rerank_min_score = kb_cfg.rerank_min_score.clamp(0.0, 1.0);
    let effective_max_docs = skill_ctx
        .and_then(|c| c.max_docs)
        .unwrap_or(kb_cfg.max_context_docs)
        .max(1);
    let effective_max_chunks = skill_ctx
        .and_then(|c| c.max_chunks_per_doc)
        .unwrap_or(kb_cfg.max_chunks_per_doc)
        .max(1);

    // 是否执行预检索（Stage1-3）：仅当预激活技能声明了检索工具（kb_search/code_lookup）时执行。
    // 无预激活技能或技能未声明检索时跳过预检索，由 Agent 按需调用检索工具（agentic 模式），
    // 避免无关消息触发昂贵的查询扩展与向量检索（RAG 预检索与 Agent 解耦）。
    let retrieval_enabled = active_skills.retrieval_enabled();

    // ── Stage 0.5: 轻量规划（仅复杂任务，规则路由判定；单模型 plan-then-execute）──
    // 规划是一次独立非流式调用（不占 DEFAULT_MAX_TURNS 执行预算）；
    // 失败/取消降级为"不规划"继续原流程（fail-open）。
    let mut task_plan: Option<crate::core::agent::planner::Plan> = None;
    if crate::core::agent::planner::should_plan(&query) {
        let planning_start = std::time::Instant::now();
        let _ = app.emit(
            "rag:status",
            RagStatus {
                request_id: request_id.clone(),
                stage: "planning".into(),
                message: "正在规划任务...".into(),
            },
        );
        crate::core::trace::stage_start(&request_id, "planning", &format!("query_len={}", query.len()));
        emit_pending_trace_events(&app, &request_id);
        // P0-6：规划可用独立轻量模型（planner_model），缺省回退主模型
        let plan_llm = match &llm_cfg.planner_model {
            Some(_) => match state
                .llm_client_for_role(&llm_cfg, crate::ModelRole::Planner)
                .await
            {
                Ok(client) => client,
                Err(e) => {
                    log::warn!("[agent_query] [0.5]: 规划模型不可用，回退主模型: {}", e);
                    llm.clone()
                }
            },
            None => llm.clone(),
        };
        // P0-3：结构化输出校验 + 修正重试（最多 3 次尝试：1 次原始 + 2 次修正）。
        // 校验失败用可读错误构造修正提示引导模型重发；全部失败 fail-open 不规划。
        const PLAN_JSON_MAX_ATTEMPTS: usize = 3;
        let mut plan: Option<crate::core::agent::planner::Plan> = None;
        let mut correction: Option<String> = None;
        for attempt in 0..PLAN_JSON_MAX_ATTEMPTS {
            let Some(plan_json) = plan_llm
                .generate_plan_json(&query, &messages, cancel.clone(), correction.as_deref())
                .await
            else {
                break; // 生成失败/取消：fail-open 不规划
            };
            if let Some(p) = crate::core::agent::planner::parse_plan(&plan_json) {
                plan = Some(p);
                break;
            }
            if attempt + 1 < PLAN_JSON_MAX_ATTEMPTS {
                let errors = crate::core::agent::planner::validate_plan_json(&plan_json)
                    .map(|_| Vec::new())
                    .unwrap_or_else(|e| e);
                correction = Some(crate::core::validation::build_fix_prompt(
                    &errors,
                    "请重新输出符合要求的计划 JSON（goal 目标、steps 步骤、acceptance 验收均必填且类型正确）。",
                ));
                log::warn!(
                    "[agent_query] [0.5]: 计划 JSON 校验失败，第 {} 次修正重试 request_id={}",
                    attempt + 1, request_id
                );
            }
        }
        if let Some(plan) = plan {
            log::info!(
                "[agent_query] [0.5]: 任务已规划，等待用户确认 request_id={} goal_len={} steps={}",
                request_id, plan.goal.len(), plan.steps.len()
            );
            // 请求用户确认：plan:request → 前端计划卡片 → plan_respond 回传；
            // 超时 60s fail-closed 按拒绝处理（与审批通道同构）。
            let plan_id = uuid::Uuid::new_v4().to_string();
            let (tx, rx) =
                tokio::sync::oneshot::channel::<crate::core::agent::planner::PlanDecision>();
            {
                let mut pending = state.plan_pending.lock().unwrap_or_else(|e| e.into_inner());
                pending.insert(plan_id.clone(), tx);
            }
            let _ = app.emit(
                "plan:request",
                serde_json::json!({
                    "plan_id": plan_id,
                    "request_id": request_id,
                    "plan": {
                        "goal": plan.goal,
                        "steps": plan.steps,
                        "acceptance": plan.acceptance,
                        "risks": plan.risks,
                        "touchpoints": plan.touchpoints,
                        "non_goals": plan.non_goals,
                        "rollback": plan.rollback,
                    }
                }),
            );
            // 等待用户确认:同时监听取消信号(点"停止"立即中止,不必等满 60s)
            let decision = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = state
                        .plan_pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&plan_id);
                    crate::core::trace::stage_end(
                        &request_id,
                        "planning",
                        "cancelled",
                        planning_start.elapsed().as_millis() as u64,
                        "等待确认时取消",
                    );
                    emit_pending_trace_events(&app, &request_id);
                    crate::core::trace::trace_bus().clear(&request_id);
                    log::info!("[agent_query] [0.5]: 等待计划确认时被取消 request_id={}", request_id);
                    let _ = app.emit(
                        "rag:done",
                        RagDone {
                            request_id: request_id.clone(),
                            content: String::new(),
                            sources: Vec::new(),
                            prompt_tokens: 0,
                            completion_tokens: 0,
                        },
                    );
                    task_registry.unregister(&request_id).await;
                    return Ok(());
                }
                res = tokio::time::timeout(std::time::Duration::from_secs(60), rx) => res,
            };
            match decision {
                Ok(Ok(crate::core::agent::planner::PlanDecision::Approved)) => {
                    task_plan = Some(plan);
                    crate::core::trace::stage_end(
                        &request_id,
                        "planning",
                        "ok",
                        planning_start.elapsed().as_millis() as u64,
                        "用户已批准计划",
                    );
                    emit_pending_trace_events(&app, &request_id);
                    log::info!("[agent_query] [0.5]: 用户已批准计划 request_id={}", request_id);
                }
                outcome => {
                    // 拒绝/通道异常/超时：清理挂起表并按拒绝中止
                    let _ = state
                        .plan_pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&plan_id);
                    let reason = match &outcome {
                        Ok(Ok(crate::core::agent::planner::PlanDecision::Denied(r))) => {
                            format!("原因：{}", r)
                        }
                        Ok(Ok(crate::core::agent::planner::PlanDecision::Approved)) => {
                            "计划状态异常".to_string()
                        }
                        Ok(Err(_)) => "确认通道异常".to_string(),
                        Err(_) => "未在 60 秒内确认，已按拒绝处理".to_string(),
                    };
                    crate::core::trace::stage_end(
                        &request_id,
                        "planning",
                        "denied",
                        planning_start.elapsed().as_millis() as u64,
                        &reason,
                    );
                    emit_pending_trace_events(&app, &request_id);
                    log::info!(
                        "[agent_query] [0.5]: 计划未获批准，中止执行 request_id={} reason={}",
                        request_id, reason
                    );
                    // 非用户主动拒绝（超时/通道异常）：前端右下角 sticky 提醒，用户自行点叉号关闭；
                    // 用户主动点「拒绝」时用户已知情，不重复打扰
                    if !matches!(
                        outcome,
                        Ok(Ok(crate::core::agent::planner::PlanDecision::Denied(_)))
                    ) {
                        let _ = app.emit(
                            "plan:rejected",
                            serde_json::json!({
                                "request_id": request_id.clone(),
                                "reason": reason,
                            }),
                        );
                    }
                    // content 置空：拒绝原因经日志/前端计划卡片传达，空内容使前端
                    // `if (fullContent)` 跳过 push 与落库，避免污染对话历史
                    let _ = app.emit(
                        "rag:done",
                        RagDone {
                            request_id: request_id.clone(),
                            content: String::new(),
                            sources: Vec::new(),
                            prompt_tokens: 0,
                            completion_tokens: 0,
                        },
                    );
                    task_registry.unregister(&request_id).await;
                    return Ok(());
                }
            }
        } else {
            // review 修复 A3：规划失败不再静默——发 rag:status 提示降级，避免前端无反馈
            log::warn!("[agent_query] [0.5]: 规划解析失败，降级为不规划 request_id={}", request_id);
            let _ = app.emit(
                "rag:status",
                RagStatus {
                    request_id: request_id.clone(),
                    stage: "planning".into(),
                    message: "规划生成失败，已降级为直接执行".into(),
                },
            );
        }
        // 检查取消（规划阶段同样可取消；补 rag:done 避免前端滞留 planning 状态）
        if cancel.is_cancelled() {
            log::info!("[agent_query] [0.5]: 规划阶段取消 request_id={}", request_id);
            crate::core::trace::stage_end(
                &request_id,
                "planning",
                "cancelled",
                planning_start.elapsed().as_millis() as u64,
                "规划阶段取消",
            );
            emit_pending_trace_events(&app, &request_id);
            crate::core::trace::trace_bus().clear(&request_id);
            let _ = app.emit(
                "rag:done",
                RagDone {
                    request_id: request_id.clone(),
                    content: String::new(),
                    sources: Vec::new(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                },
            );
            task_registry.unregister(&request_id).await;
            return Ok(());
        }
    }

    // ── Stage 1-3: 预检索（仅技能触发时执行）──
    let (context, sources, selected_count) = if retrieval_enabled {
        // ── Stage 1: 查询扩展 ──
        let _ = app.emit(
            "rag:status",
            RagStatus {
                request_id: request_id.clone(),
                stage: "expanding".into(),
                message: "正在扩展查询...".into(),
            },
        );
        let expanding_start = std::time::Instant::now();
        crate::core::trace::stage_start(&request_id, "expanding", "查询扩展");
        emit_pending_trace_events(&app, &request_id);

        let expanded = llm.expand_queries(&query, &messages, cancel.clone()).await;
        let mut queries = vec![query.clone()];
        queries.extend(expanded);
        log::info!("[agent_query] [1]: 查询扩展完成 request_id={} total_queries={} queries={:?}", request_id, queries.len(), queries);
        crate::core::trace::stage_end(
            &request_id,
            "expanding",
            "ok",
            expanding_start.elapsed().as_millis() as u64,
            &format!("queries={}", queries.len()),
        );
        emit_pending_trace_events(&app, &request_id);

        // 检查取消
        if cancel.is_cancelled() {
            log::info!("[agent_query] [1]: 对话取消，直接结束 request_id={}", request_id);
            task_registry.unregister(&request_id).await;
            return Ok(());
        }

        // ── Stage 2: 多查询混合检索（并行）──
        log::info!("[agent_query] [2]: 混合检索开始 request_id={} 语义扩展数量={}",  request_id, queries.len());
        let _ = app.emit(
            "rag:status",
            RagStatus {
                request_id: request_id.clone(),
                stage: "searching".into(),
                message: format!("正在检索知识库... ({} 组查询)", queries.len()),
            },
        );
        let searching_start = std::time::Instant::now();
        crate::core::trace::stage_start(&request_id, "searching", &format!("queries={}", queries.len()));
        emit_pending_trace_events(&app, &request_id);

        // 对每个查询：嵌入 → 混合检索
        let search_start = std::time::Instant::now();
        let search_futures: Vec<_> = queries
            .iter()
            .map(|q| {
                let dir = dir_path.clone();
                let state = state.clone();
                let q = q.clone();
                async move {
                    let q_for_embed = q.clone();
                    let embed_start = std::time::Instant::now();
                    let embedding = tokio::task::spawn_blocking(move || {
                        call_embedding_query(&q_for_embed)
                    })
                    .await
                    .ok()
                    .and_then(|e| e.ok())
                    .and_then(|v| v.into_iter().next());

                    log::info!("[agent_query] [2]: 语义扩展query向量化 query={} 耗时={:?} success={}",
                        &q, embed_start.elapsed(), embedding.is_some());

                    if let Some(vec) = embedding {
                        let start = std::time::Instant::now();
                        let hits = state
                            .indexer
                            .hybrid_search(&dir, &vec, &q, effective_top_k)
                            .await
                            .unwrap_or_default();

                        log::info!("[agent_query] [2]: 语义扩展query混合检索， query={} 命中 {} 条文档耗时={:?}",
                            &q, hits.len(), start.elapsed());

                        hits
                    } else {
                        log::warn!("[agent_query] [2]: 语义扩展query向量化失败 query={} skipping", &q);
                        Vec::new()
                    }
                }
            })
            .collect();

        let all_results: Vec<Vec<SearchHit>> = {
            // 可取消的并行检索（最多并发 4 个）：取消信号到达后停止消费新结果，
            // 已启动的检索会自然完成，不会拖住取消响应。
            let cancel_fut = {
                let cancel = cancel.clone();
                async move {
                    cancel.cancelled().await;
                }
            };
            futures::stream::iter(search_futures)
                .buffer_unordered(4)
                .take_until(cancel_fut)
                .collect()
                .await
        };

        // 展平所有结果
        let all_hits: Vec<SearchHit> = all_results.into_iter().flatten().collect();
        log::info!("[agent_query] [2]: 语义扩展query混合检索最终结果， request_id={} 命中 {} 条文档, 耗时={:?}", request_id, all_hits.len(), search_start.elapsed());
        crate::core::trace::stage_end(
            &request_id,
            "searching",
            "ok",
            searching_start.elapsed().as_millis() as u64,
            &format!("hits={}", all_hits.len()),
        );
        emit_pending_trace_events(&app, &request_id);

        if cancel.is_cancelled() {
            log::info!("[agent_query] [2]: 对话取消，直接结束 request_id={}", request_id);
            task_registry.unregister(&request_id).await;
            return Ok(());
        }

        // 预检索结果提取：无命中时降级为空上下文，交由 Agent 按需使用工具
        'retrieval: {
            if all_hits.is_empty() {
                log::warn!("[agent_query] [3]: 预检索降级为空上下文（agentic 模式）request_id={}", request_id);
                break 'retrieval (String::new(), Vec::new(), 0usize);
            }

            // ── Stage 3: 文档级聚合 + 绝对阈值（core::agent::aggregate_hits）──
            let aggregating_start = std::time::Instant::now();
            crate::core::trace::stage_start(&request_id, "aggregating", "文档级聚合");
            emit_pending_trace_events(&app, &request_id);
            let selected: Vec<(SearchHit, f32)> = aggregate_hits(
                all_hits,
                effective_min_score,
                effective_rerank_min_score,
                effective_max_docs,
                effective_max_chunks,
            );
            if log::log_enabled!(log::Level::Debug) {
                // 打印每个进入引用的命中的完整分数域（doc_name / score / score_rerank / symbol / vec / bm25），
                // 用于核对"代码文件混入引用"的根因：意图路由结果 + 精排 sigmoid 分数是否恰好通过阈值。
                log::info!("[agent_query] [3]: 文档聚合结果， request_id={} 命中 {} 条文档, effective_min_score={}， effective_max_docs={}, effective_max_chunks={}， doc=\n{:?}",
                 request_id, selected.len(), effective_min_score, effective_max_docs, effective_max_chunks,
                  selected.iter()
                    .map(|(hit, score)| {
                        format!(
                            "{} : {:.3} (rerank={:?} symbol={:?} vec={:.3} bm25={:.3})",
                            hit.doc_name,
                            score,
                            hit.score_rerank,
                            hit.symbol_name,
                            hit.score_vec,
                            hit.score_bm25
                        )
                    })
                    .collect::<Vec<_>>()
                );
            }
 
            if selected.is_empty() {
                log::info!("[agent_query] [3]: 没有文档符合阈值，预检索降级为空上下文（agentic 模式）request_id={}", request_id);
                break 'retrieval (String::new(), Vec::new(), 0usize);
            }

            // 按文档分组构建上下文：文档按分数降序、文档内按阅读顺序（chunk_index），
            // 优先使用 sentence_window（包含检索句子前后的上下文），fallback 到 chunk text，
            // 总字符数受 agent 模块的 MAX_CONTEXT_CHARS 限制避免超出模型窗口。
            let context = build_context_text(&selected, crate::core::agent::MAX_CONTEXT_CHARS);
            // P1-13：检索上下文提示注入防护——命中可疑指令时包裹并追加显式
            // 安全提示（不裁剪原文，可审计），引导模型忽略指令性内容
            let context = crate::core::security::wrap_suspicious(&context);
            log::info!( "[agent_query] [3]: 上下文构建结果， request_id={} 命中 {} 条文档, char_len={} preview={:?}",
                request_id, selected.len(), context.len(), context
            );

            // 构建引用来源（按 doc_name 去重，合并文本/path_json，取最高分；
            // 对 OPML/FreeMind 合并 path_json 层级路径展示）
            let sources = build_sources(&selected);
            log::info!("[agent_query] [3]: 引用来源去重结果， request_id={} 命中 {} 条文档, count={}", request_id, selected.len(), sources.len());
            crate::core::trace::stage_end(
                &request_id,
                "aggregating",
                "ok",
                aggregating_start.elapsed().as_millis() as u64,
                &format!("docs={} chars={}", selected.len(), context.len()),
            );
            emit_pending_trace_events(&app, &request_id);

            (context, sources, selected.len())
        }
    } else {
        log::info!("[agent_query] [3]: 未命中检索技能，跳过预检索（agentic 模式）request_id={}", request_id);
        (String::new(), Vec::new(), 0usize)
    };
    let sources_clone = sources.clone();

    // ── Stage 4: 构建 context → RAG Agent 生成（技能解析与参数覆盖已在 Stage 0 完成）──
    log::info!("[agent_query] [4]: 构建 context → Agent 生成 request_id={}", request_id);
    let status_msg = match selected_count {
        0 => "正在生成回答...".to_string(),
        n => format!("正在生成回答（基于 {} 个相关片段）...", n),
    };
    let _ = app.emit(
        "rag:status",
        RagStatus {
            request_id: request_id.clone(),
            stage: "generating".into(),
            message: status_msg,
        },
    );
    let generating_start = std::time::Instant::now();
    crate::core::trace::stage_start(&request_id, "generating", &format!("docs={}", selected_count));
    emit_pending_trace_events(&app, &request_id);

    if cancel.is_cancelled() {
        log::info!("[agent_query] [4]: 对话取消，直接结束 request_id={}", request_id);
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // 将任务计划注入 preamble（每轮可见，约束最强）；P3-2 起改为"经用户确认后注入"
    let context = if let Some(plan) = &task_plan {
        format!("{}\n\n{}", plan.to_preamble_text(), context)
    } else {
        context
    };

    // P0-2/O1：注入相关长期记忆（关键词 ∪ 向量融合检索，RRF；embedding
    // 不可用时 search_hybrid 内部降级纯关键词；检索失败/无命中不注入）。
    let memory_block = match crate::core::memory::search_hybrid(
        state.memory_store.clone(),
        state.memory_vectors.clone(),
        &query,
        3,
    )
    .await
    {
        Ok(items) if !items.is_empty() => {
            let mut s = String::from("\n\n【长期记忆（与本问题相关，供参考）】\n");
            for it in &items {
                s.push_str(&format!("- {}：{}\n", it.title, it.body));
            }
            s
        }
        _ => String::new(),
    };
    let context = format!("{}{}", context, memory_block);

    // 构建 RAG Agent：预载检索上下文 + 检索/文件/技能工具（模型可补充检索、按需激活技能）
    let model = llm.completion_model().clone();
    // 取第一个预激活技能的 ID 作为工具轨迹标注来源
    let primary_skill_id = skill_ctx.and_then(|c| c.skill_ids.first().cloned());
    // L1 技能目录（id + description，常驻 preamble，模型始终知道自己有哪些技能）
    let catalog = build_skill_catalog(&state.skill_registry);
    // 各作用域技能基础目录（供 read 工具按需读取已激活技能的参考文档，L3）
    let skill_bases = resolve_skill_bases(&app, &dir_path);
    // 检索命中收集器：kb_search / code_lookup 工具的命中经此回传，合并进 rag:done 引用来源
    let search_sink: Arc<tokio::sync::Mutex<Vec<(SearchHit, f32)>>> = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let search_config = KbSearchConfig {
        dir_path: dir_path.clone(),
        indexer: state.indexer.clone(),
        default_top_k: effective_top_k,
        request_id: request_id.clone(),
        min_score: effective_min_score,
        rerank_min_score: effective_rerank_min_score,
        max_context_docs: effective_max_docs,
        max_chunks_per_doc: effective_max_chunks,
        skill_id: primary_skill_id,
        skill_state: active_skills.clone(),
        skill_bases,
        search_sink: search_sink.clone(),
        app_handle: app.clone(),
        cancel: Some(cancel.clone()),
    };
    // Agent 规约（角色/语言/安全边界）从资源目录加载，打包后跟随安装包
    let agent_rules = load_agent_rules(&app, "rag_agent.md");
    let agent = build_rag_agent(
        model,
        &context,
        search_config,
        state.skill_registry.clone(),
        catalog,
        agent_rules,
        state.approval_gate.clone(),
        crate::core::agent::DEFAULT_MAX_TURNS,
        None, // 主对话全量工具
        true, // 主对话启用技能体系的工具窄化与门禁
    );
    log::info!("[agent_query] [4]: 构建 Agent 完成 request_id={}", request_id);

    // 技能执行计时起点（进入生成阶段即视为执行开始）
    let skill_exec_start = std::time::Instant::now();

    // 当前问题作为 prompt，历史消息（去掉最后一条当前问题）压缩后作为 history
    // P0-5：先应用会话压缩检查点（摘要 + cutoff 之后的增量消息），压缩后写回新检查点
    let checkpoint: Option<crate::core::context::CompactionState> = match (&session_id, &dir_path) {
        (Some(sid), _) => {
            let sid = sid.clone();
            let store = state.get_chat_store(&dir_path).ok();
            match store {
                Some(store) => tokio::task::spawn_blocking(move || {
                    store
                        .get_compaction_state(&sid)
                        .ok()
                        .flatten()
                        .and_then(|raw| crate::core::context::CompactionState::from_json(&raw))
                })
                .await
                .ok()
                .flatten(),
                None => None,
            }
        }
        _ => None,
    };
    let hist_messages = apply_compaction_checkpoint(&messages, checkpoint.as_ref());
    let compressed = prepare_history(&hist_messages, compressor.as_ref(), cancel.clone()).await;
    // 写回新检查点：仅摘要策略成功且消息带 id 时（可定位 cutoff），
    // 失败静默（检查点缺失只是失去增量优化，不影响正确性）
    if let (Some(sid), Some(store)) = (&session_id, state.get_chat_store(&dir_path).ok()) {
        if compressed.strategy == "summarize+window" {
            let summary = compressed
                .turns
                .iter()
                .find(|t| t.role == "system")
                .map(|t| t.content.clone())
                .unwrap_or_default();
            if !summary.is_empty() {
                if let Some(first_kept_id) = hist_messages
                    .get(compressed.kept_from)
                    .and_then(|m| m.id.clone())
                {
                    let new_state = crate::core::context::CompactionState {
                        summary,
                        cutoff_msg_id: Some(first_kept_id),
                        tokens_before: 0,
                    };
                    let sid = sid.clone();
                    let store = store.clone();
                    let json = new_state.to_json();
                    let _ = tokio::task::spawn_blocking(move || {
                        store.set_compaction_state(&sid, &json)
                    })
                    .await;
                }
            }
        }
    }
    if compressed.dropped_chars > 0 {
        log::info!(
            "[agent_query] [4]: 对话历史已压缩 request_id={} dropped={} strategy={}",
            request_id, compressed.dropped_chars, compressed.strategy
        );
        let _ = app.emit(
            "rag:status",
            RagStatus {
                request_id: request_id.clone(),
                stage: "generating".into(),
                message: format!(
                    "对话历史较长，已自动压缩旧消息（节省约 {} 字符）",
                    compressed.dropped_chars
                ),
            },
        );
    }
    // 压缩阶段取消只中断压缩，此处快速检查避免取消后再发起一次 HTTP 请求
    if cancel.is_cancelled() {
        log::info!("[agent_query] [4]: 对话在压缩后取消，不发起请求 request_id={}", request_id);
        task_registry.unregister(&request_id).await;
        return Ok(());
    }
    let history = chat_turns_to_history(&compressed.turns);
    let mut stream = agent
        .stream_chat(Message::user(query.clone()), history)
        .into_future()
        .await;

    // 流式生成
    let llm_start = std::time::Instant::now();
    let mut full_content = String::new();
    let mut final_usage: Option<UsageInfo> = None;
    let mut delta_count = 0u64;
    let mut stream_failed = false;
    let mut last_tool_summary: Option<String> = None;
    loop {
        let item = match next_or_cancel(&mut stream, &cancel).await {
            Err(()) => {
                log::info!("[agent_query] [4]: 对话取消，立即断开请求 request_id={} accumulated={}",
                    request_id, full_content.len());
                // 取消时保留已生成的部分内容：通过 rag:done 交给前端落库
                if !full_content.is_empty() {
                    let (prompt_tokens, completion_tokens) = final_usage
                        .as_ref()
                        .map(|u| (u.prompt_tokens, u.completion_tokens))
                        .unwrap_or((0, 0));
                    let _ = app.emit(
                        "rag:done",
                        RagDone {
                            request_id: request_id.clone(),
                            content: full_content.clone(),
                            sources: merge_search_sink(sources_clone.clone(), &search_sink).await,
                            prompt_tokens,
                            completion_tokens,
                        },
                    );
                }
                // 取消时补发残留工具事件并清理总线
                crate::core::trace::stage_end(
                    &request_id,
                    "generating",
                    "cancelled",
                    generating_start.elapsed().as_millis() as u64,
                    &format!("chars={}", full_content.len()),
                );
                emit_pending_trace_events(&app, &request_id);
                emit_pending_tool_events(&app, &request_id);
                tool_call_bus().clear(&request_id);
                {
                    let inputs = collect_skill_exec_inputs(skill_ctx, &active_skills, skill_exec_start.elapsed().as_millis() as u64);
                    let matched = !inputs.is_empty();
                    let metrics = state.skill_metrics.clone();
                    let dir = dir_path.clone();
                    let rid = request_id.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if matched {
                            metrics.record_dispatch_matched(&dir);
                        }
                        record_skill_execution(&metrics, &dir, inputs, false, Some("cancelled"), &rid);
                    })
                    .await;
                }
                task_registry.unregister(&request_id).await;
                return Ok(());
            }
            Ok(None) => break,
            Ok(Some(item)) => item,
        };
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall { tool_call, .. })) => {
                log::info!("[agent_query] [4]: 工具调用: name={} arguments={}",
                    tool_call.function.name, tool_call.function.arguments);
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                if text.text.is_empty() {
                    continue;
                }
                full_content.push_str(&text.text);
                delta_count += 1;
                let _ = app.emit(
                    "rag:delta",
                    RagDelta {
                        request_id: request_id.clone(),
                        content: text.text,
                    },
                );
            }
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                let usage = res.usage();
                if usage.has_values() {
                    log::info!("[agent_query] [4]: Agent 最终 token 使用: request_id={} input_tokens={} output_tokens={}",
                        request_id, usage.input_tokens, usage.output_tokens);
                    final_usage = Some(usage_to_info(&usage));
                }
            }
            Ok(MultiTurnStreamItem::CompletionCall(call)) => {
                if call.usage.has_values() {
                    final_usage = Some(usage_to_info(&call.usage));
                }
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!("[agent_query] [4]: Agent 流式响应错误: request_id={} err={}", request_id, e);
                stream_failed = true;
                break;
            }
        }
        // 捕获最后一个成功的工具调用结果（用于兜底：模型调用工具成功但未生成文本时）
        if let Some(summary) = tool_call_bus().peek_last_success_summary(&request_id) {
            last_tool_summary = Some(summary);
        }
        // 转发工具调用轨迹（工具在 Rig 流式内部执行，结果已写入总线）
        emit_pending_tool_events(&app, &request_id);
    }
    
     log::info!("[agent_query] [4]: Agent 流式响应完成: request_id={} took={:?} delta_count={} content_len={}",
        request_id, llm_start.elapsed(), delta_count, full_content.len());
    crate::core::trace::stage_end(
        &request_id,
        "generating",
        "ok",
        generating_start.elapsed().as_millis() as u64,
        &format!("chars={} delta={}", full_content.len(), delta_count),
    );
    emit_pending_trace_events(&app, &request_id);

    // 流式失败且无任何内容 → 显式报错，避免静默失败或空消息污染前端
    if stream_failed && full_content.is_empty() && !cancel.is_cancelled() {
        log::info!("[agent_query] [4]: 流式响应失败 request_id={}", request_id);
        crate::core::trace::stage_end(
            &request_id,
            "generating",
            "error",
            generating_start.elapsed().as_millis() as u64,
            "llm_stream_failed",
        );
        emit_pending_trace_events(&app, &request_id);
        emit_pending_tool_events(&app, &request_id);
        tool_call_bus().clear(&request_id);
        {
            let inputs = collect_skill_exec_inputs(skill_ctx, &active_skills, skill_exec_start.elapsed().as_millis() as u64);
            let matched = !inputs.is_empty();
            let metrics = state.skill_metrics.clone();
            let dir = dir_path.clone();
            let rid = request_id.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if matched {
                    metrics.record_dispatch_matched(&dir);
                }
                record_skill_execution(&metrics, &dir, inputs, false, Some("llm_stream_failed"), &rid);
            })
            .await;
        }
        emit_command_error(&app, "rag:error", &request_id, "LLM 生成失败，请检查模型服务是否可用".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // ── Done ──
    // 流式正常结束但内容为空：若工具调用成功，以工具结果兜底；否则报错。
    if full_content.trim().is_empty() {
        if let Some(summary) = last_tool_summary.take() {
            log::info!("[agent_query] [4]: 模型未生成文本但工具调用成功，以工具结果兜底 request_id={} summary={}",
                request_id, summary);
            full_content = summary;
            // 继续走到 rag:done 正常发射
        } else {
            log::warn!("[agent_query] [4]: 响应完成但内容为空 request_id={}", request_id);
            emit_pending_tool_events(&app, &request_id);
            tool_call_bus().clear(&request_id);
            {
                let inputs = collect_skill_exec_inputs(skill_ctx, &active_skills, skill_exec_start.elapsed().as_millis() as u64);
                let matched = !inputs.is_empty();
                let metrics = state.skill_metrics.clone();
                let dir = dir_path.clone();
                let rid = request_id.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if matched {
                        metrics.record_dispatch_matched(&dir);
                    }
                    record_skill_execution(&metrics, &dir, inputs, false, Some("llm_empty_output"), &rid);
                })
                .await;
            }
            emit_command_error(&app, "rag:error", &request_id, "LLM 生成失败，请检查模型服务是否可用".into());
            task_registry.unregister(&request_id).await;
            return Ok(());
        }
    }
    let (prompt_tokens, completion_tokens) = final_usage
        .map(|u| (u.prompt_tokens, u.completion_tokens))
        .unwrap_or((0, 0));

    log::info!("[agent_query] [4]: 响应完成: request_id={} content_len={} sources={} tokens_in={} tokens_out={}",
        request_id, full_content.len(), sources_clone.len(), prompt_tokens, completion_tokens);

    let _ = app.emit(
        "rag:done",
        RagDone {
            request_id: request_id.clone(),
            content: full_content,
            sources: merge_search_sink(sources_clone, &search_sink).await,
            prompt_tokens,
            completion_tokens,
        },
    );

    // 收尾：补发残留工具事件并清理总线
    emit_pending_tool_events(&app, &request_id);
    tool_call_bus().clear(&request_id);
    {
        let inputs = collect_skill_exec_inputs(skill_ctx, &active_skills, skill_exec_start.elapsed().as_millis() as u64);
        let matched = !inputs.is_empty();
        let metrics = state.skill_metrics.clone();
        let dir = dir_path.clone();
        let rid = request_id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if matched {
                metrics.record_dispatch_matched(&dir);
            }
            record_skill_execution(&metrics, &dir, inputs, true, None, &rid);
        })
        .await;
    }
    task_registry.unregister(&request_id).await;
    Ok(())
}

/// 纯 LLM 对话（Rig Agent，无工具）
#[tauri::command]
pub async fn kb_llm_query(
    app: AppHandle,
    state: tauri::State<'_, crate::AppState>,
    task_registry: tauri::State<'_, TaskRegistry>,
    messages: Vec<crate::services::llm::ChatMessage>,
    request_id: String,
) -> Result<(), String> {
    let cancel = task_registry.register(&request_id).await;

    // 从中央化内存配置读取 LLM 配置
    let llm_cfg = state.llm_config.read().unwrap_or_else(|e| e.into_inner()).clone();

    // 构建 LLM 客户端（失败转为错误事件，避免 panic 与注册表泄漏）
    let llm = match get_or_create_llm_client(&state, &llm_cfg.endpoint, &llm_cfg.model, &llm_cfg.api_key).await {
        Ok(llm) => llm,
        Err(e) => {
            log::error!("[kb_llm_query] [0]: LLM 客户端初始化失败: request_id={} err={}", request_id, e);
            emit_command_error(&app, "llm:error", &request_id, format!("LLM 客户端初始化失败: {}", e));
            task_registry.unregister(&request_id).await;
            return Ok(());
        }
    };

    if !llm.is_configured() {
        emit_command_error(&app, "llm:error", &request_id, "LLM 未配置".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // 历史上下文压缩器：无工具对话同样适用，避免长会话被直接拒绝
    // P0-6：摘要可用独立轻量模型（summary_model），缺省回退主模型
    let summary_llm = match &llm_cfg.summary_model {
        Some(_) => match state
            .llm_client_for_role(&llm_cfg, crate::ModelRole::Summary)
            .await
        {
            Ok(client) => client,
            Err(e) => {
                log::warn!("[kb_llm_query] [0]: 摘要模型不可用，回退主模型: {}", e);
                llm.clone()
            }
        },
        None => llm.clone(),
    };
    let summarizer: Arc<dyn crate::core::context::HistorySummarizer> = Arc::new(summary_llm);
    let compressor: Arc<dyn ContextCompressor> = Arc::new(SummarizeThenWindowCompressor::new(
        summarizer,
        SUMMARY_MAX_CHARS,
    ));

    if messages.is_empty() {
        emit_command_error(&app, "llm:error", &request_id, "消息不能为空".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    let prompt_content = messages.last().map(|m| m.content.clone()).unwrap_or_default();
    let compressed = prepare_history(&messages, compressor.as_ref(), cancel.clone()).await;
    if compressed.dropped_chars > 0 {
        log::info!(
            "[kb_llm_query] [0]: 对话历史已压缩 request_id={} dropped={} strategy={}",
            request_id, compressed.dropped_chars, compressed.strategy
        );
    }
    // 压缩阶段取消只中断压缩，此处快速检查避免取消后再发起一次 HTTP 请求
    if cancel.is_cancelled() {
        log::info!("[kb_llm_query] [1]: 对话在压缩后取消，不发起请求 request_id={}", request_id);
        task_registry.unregister(&request_id).await;
        return Ok(());
    }
    let history = chat_turns_to_history(&compressed.turns);

    // Agent 规约（角色/语言/安全边界）从资源目录加载，打包后跟随安装包
    let agent_rules = load_agent_rules(&app, "chat_agent.md");
    let agent = build_chat_agent(llm.completion_model().clone(), agent_rules);
    let mut stream = agent
        .stream_chat(Message::user(prompt_content.clone()), history)
        .into_future()
        .await;

    let kb_gen_start = std::time::Instant::now();
    crate::core::trace::stage_start(&request_id, "generating", "kb_llm_query");
    emit_pending_trace_events(&app, &request_id);

    let mut full_content = String::new();
    let mut stream_failed = false;
    loop {
        let item = match next_or_cancel(&mut stream, &cancel).await {
            Err(()) => {
                log::info!("[kb_llm_query] [1]: 对话取消，立即断开请求 request_id={} accumulated={}",
                    request_id, full_content.len());
                crate::core::trace::stage_end(
                    &request_id,
                    "generating",
                    "cancelled",
                    kb_gen_start.elapsed().as_millis() as u64,
                    &format!("chars={}", full_content.len()),
                );
                emit_pending_trace_events(&app, &request_id);
                crate::core::trace::trace_bus().clear(&request_id);
                // 取消时保留已生成的部分内容：通过 llm:done 交给前端落库
                if !full_content.is_empty() {
                    let _ = app.emit(
                        "llm:done",
                        LlmDone {
                            request_id: request_id.clone(),
                            content: full_content.clone(),
                        },
                    );
                }
                task_registry.unregister(&request_id).await;
                return Ok(());
            }
            Ok(None) => break,
            Ok(Some(item)) => item,
        };
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall { tool_call, .. })) => {
                log::info!("[kb_llm_query] [1]: agent 工具调用: name={} arguments={}",
                    tool_call.function.name, tool_call.function.arguments);
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                if text.text.is_empty() {
                    continue;
                }
                full_content.push_str(&text.text);
                let _ = app.emit(
                    "llm:delta",
                    LlmDelta {
                        request_id: request_id.clone(),
                        content: text.text,
                    },
                );
            }
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                let usage = res.usage();
                if usage.has_values() {
                    let _ = app.emit(
                        "llm:usage",
                        serde_json::json!({
                            "request_id": request_id,
                            "prompt_tokens": usage.input_tokens,
                            "completion_tokens": usage.output_tokens,
                        }),
                    );
                }
            }
            Ok(MultiTurnStreamItem::CompletionCall(call)) => {
                if call.usage.has_values() {
                    let _ = app.emit(
                        "llm:usage",
                        serde_json::json!({
                            "request_id": request_id.clone(),
                            "prompt_tokens": call.usage.input_tokens,
                            "completion_tokens": call.usage.output_tokens,
                        }),
                    );
                }
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!("[kb_llm_query] [1]: agent 流式错误: request_id={} err={}", request_id, e);
                stream_failed = true;
                break;
            }
        }
    }

    // 流式失败且无任何内容 → 显式报错，避免静默失败或空消息污染前端
    if stream_failed && full_content.is_empty() && !cancel.is_cancelled() {
        log::warn!("[kb_llm_query] [1]: 流式响应失败: request_id={}", request_id);
        crate::core::trace::stage_end(
            &request_id,
            "generating",
            "error",
            kb_gen_start.elapsed().as_millis() as u64,
            "llm_stream_failed",
        );
        emit_pending_trace_events(&app, &request_id);
        crate::core::trace::trace_bus().clear(&request_id);
        emit_command_error(&app, "llm:error", &request_id, "LLM 生成失败，请检查模型服务是否可用".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }
    crate::core::trace::stage_end(
        &request_id,
        "generating",
        "ok",
        kb_gen_start.elapsed().as_millis() as u64,
        &format!("chars={}", full_content.len()),
    );
    emit_pending_trace_events(&app, &request_id);
    crate::core::trace::trace_bus().clear(&request_id);

    let _ = app.emit(
        "llm:done",
        LlmDone {
            request_id: request_id.clone(),
            content: full_content,
        },
    );

    task_registry.unregister(&request_id).await;
    Ok(())
}

