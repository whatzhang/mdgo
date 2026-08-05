use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use rig_agent::agent::MultiTurnStreamItem;
use rig_agent::completion::{Chat, CompletionModel};
use rig_agent::streaming::StreamingChat;
use rig_agent::Agent;
use rig_core::completion::Message;
use rig_core::streaming::StreamedAssistantContent;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::core::agent::{
    KbSearchConfig, aggregate_hits, build_chat_agent, build_context_text, build_rag_agent,
    load_agent_rules,
};
use crate::core::agent::tools::tool_call_bus;
use crate::core::skill::activation::{ActivationSource, ActiveSkillState};
use crate::core::skill::context::{SkillExecutionContext, build_skill_catalog, resolve_preactivated};
use crate::core::skill::SkillStore;
use crate::core::{call_embedding_query, route_intent, SearchHit};
use crate::services::llm::{LLMClient, UsageInfo, chat_message_to_rig, usage_to_info};

// ─── 后端消息长度预算 ───
/// 消息总字符数上限（粗略估计 ~7500 tokens 的字符量，为 LLM 回复留出余量）
const MAX_MESSAGE_CHARS: usize = 30_000;

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

/// 校验消息总字符数是否超过上限，超限时返回错误描述
fn validate_messages_length(messages: &[crate::services::llm::ChatMessage]) -> Result<(), String> {
    let total: usize = messages.iter().map(|m| m.content.len()).sum();
    if total > MAX_MESSAGE_CHARS {
        return Err(format!(
            "对话历史过长（{} 字符 > 上限 {} 字符），请开始新对话",
            total, MAX_MESSAGE_CHARS
        ));
    }
    Ok(())
}

/// 将消息列表转为 Rig history（去掉最后一条当前问题，它作为 prompt 单独发送）
fn messages_to_history(messages: &[crate::services::llm::ChatMessage]) -> Vec<Message> {
    messages[..messages.len().saturating_sub(1)]
        .iter()
        .map(chat_message_to_rig)
        .collect()
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

/// 流式失败后的非流式降级重试。
///
/// 部分 OpenAI 兼容服务器（如本地 GGUF 网关）不支持 SSE 流式：对 `stream=true`
/// 的请求直接返回 HTTP 200 + `application/json`，Rig 流式解析报
/// `InvalidContentType`。此时改走 Agent 的非流式接口（底层为 `completion`，
/// 请求体不注入 `stream`），可正常拿到完整回答，且保留 Agent 的工具/上下文行为。
async fn complete_fallback<M>(
    agent: &Agent<M>,
    prompt: Message,
    history: Vec<Message>,
) -> Result<String, String>
where
    M: CompletionModel + 'static,
{
    let mut history = history;
    agent
        .chat(prompt, &mut history)
        .await
        .map_err(|e| e.to_string())
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

/// 记录一次技能执行（成功/失败/取消均计入，供知识库索引页指标展示）。
///
/// 记录范围 = 预激活技能（手动触发/会话挂载，`skill_ctx.matches`）
/// ∪ 请求期间 LLM 经 `activate_skill` 激活的技能（主路径，`active_skills`），
/// 保证 LLM 驱动的激活同样进入指标闭环，而不是只统计预激活。
fn record_skill_execution(
    metrics: &crate::core::skill::metrics::SkillMetrics,
    skill_ctx: Option<&SkillExecutionContext>,
    active_skills: &ActiveSkillState,
    start: std::time::Instant,
    success: bool,
    error_code: Option<&str>,
) {
    let duration_ms = start.elapsed().as_millis() as u64;
    let mut recorded: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    if let Some(ctx) = skill_ctx {
        for m in &ctx.matches {
            recorded.insert((m.scope.clone(), m.skill_id.clone()));
            metrics.record_execution(
                m.skill_id.clone(),
                m.scope.clone(),
                duration_ms,
                success,
                error_code.map(|s| s.to_string()),
                m.source,
                m.match_score,
            );
        }
    }
    // LLM 会话中动态激活的技能（不在预激活上下文内）：按 Llm 来源补录，避免重复
    for skill in active_skills.activated() {
        let key = (skill.scope.as_str().to_string(), skill.id.clone());
        if recorded.insert(key) {
            metrics.record_execution(
                skill.id.clone(),
                skill.scope.as_str().to_string(),
                duration_ms,
                success,
                error_code.map(|s| s.to_string()),
                ActivationSource::Llm,
                1.0,
            );
        }
    }
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
    let fingerprint = format!("{}|{}|{}", endpoint, model, api_key);
    let mut cache = state.llm_client_cache.lock().await;
    if let Some((fp, client)) = cache.as_ref() {
        if fp == &fingerprint {
            return Ok(client.clone());
        }
    }
    let client = LLMClient::new(endpoint.to_string(), model.to_string(), api_key.to_string())?;
    *cache = Some((fingerprint, client.clone()));
    Ok(client)
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
pub async fn kb_rag_query(
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

    log::info!("[rag_query] ENTRY request_id={} dir_path={} query_len={} msg_count={} top_k={}",
        request_id, dir_path, query.len(), messages.len(), top_k);

    // 从中央化内存配置读取 LLM 配置
    let llm_cfg = state.llm_config.read().unwrap_or_else(|e| e.into_inner()).clone();
    log::info!("[rag_query] LLM config loaded endpoint={} model={}",
        llm_cfg.endpoint, llm_cfg.model);

    // 构建 LLM 客户端（失败转为错误事件，避免 panic 与注册表泄漏）
    let llm = match get_or_create_llm_client(&state, &llm_cfg.endpoint, &llm_cfg.model, &llm_cfg.api_key).await {
        Ok(llm) => llm,
        Err(e) => {
            log::error!("[rag_query] LLMClient init failed request_id={} err={}", request_id, e);
            emit_command_error(&app, "rag:error", &request_id, format!("LLM 客户端初始化失败: {}", e));
            task_registry.unregister(&request_id).await;
            return Ok(());
        }
    };

    if !llm.is_configured() {
        log::warn!("[rag_query] LLM not configured, aborting request_id={}", request_id);
        emit_command_error(&app, "rag:error", &request_id, "LLM 未配置，请在设置中填写端点地址和模型名称".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // 后端兜底校验：消息总长度
    if let Err(e) = validate_messages_length(&messages) {
        log::warn!("[rag_query] validate_messages_length failed request_id={} err={}", request_id, e);
        emit_command_error(&app, "rag:error", &request_id, e);
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // ── Stage 0: 技能预激活（手动触发 / 会话挂载）──
    // 激活决策已交由 LLM（渐进式披露 L1/L2）：此处不做任何本地匹配，
    // 仅处理两类显式预激活并写入共享激活状态 active_skills，供 Agent 钩子
    // （L2 指令注入）与技能工具（activate_skill / deactivate_skill）后续使用。
    let active_skills = Arc::new(ActiveSkillState::new());
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
            let attached_skills: Vec<(String, String)> = match (&chat_store, &session_id) {
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
                log::warn!("[rag_query] 技能预激活失败 request_id={} err={}", request_id_for_log, e);
                None
            }
            Err(e) => {
                log::warn!("[rag_query] 技能预激活任务失败 request_id={} err={}", request_id_for_log, e);
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
    if let Some(ctx) = skill_ctx {
        state.skill_metrics.record_dispatch(true);
        log::info!(
            "[rag_query] Stage0: skills pre-activated request_id={} skills={:?} manual={}",
            request_id,
            ctx.skill_ids,
            skill_resolved.as_ref().map(|r| r.is_manual).unwrap_or(false)
        );
    } else {
        state.skill_metrics.record_dispatch(false);
        log::info!(
            "[rag_query] Stage0: 无预激活技能（技能激活交由 LLM 决策）request_id={}",
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

    // ── Stage 1-3: 预检索（仅技能触发时执行）──
    let (context, sources, selected_count) = if retrieval_enabled {
        // ── Stage 1: 查询扩展 ──
        log::info!("[rag_query] Stage1: query expansion start request_id={}", request_id);
        let _ = app.emit(
            "rag:status",
            RagStatus {
                request_id: request_id.clone(),
                stage: "expanding".into(),
                message: "正在扩展查询...".into(),
            },
        );

        let expanded = llm.expand_queries(&query, &messages, cancel.clone()).await;
        let mut queries = vec![query.clone()];
        queries.extend(expanded);
        log::info!("[rag_query] Stage1: query expansion done request_id={} total_queries={} queries={:?}",
            request_id, queries.len(), queries);

        // 检查取消
        if cancel.is_cancelled() {
            log::info!("[rag_query] Cancelled after Stage1 request_id={}", request_id);
            task_registry.unregister(&request_id).await;
            return Ok(());
        }

        // ── Stage 2: 多查询混合检索（并行）──
        log::info!("[rag_query] Stage2: multi-query search start request_id={} query_count={}",
            request_id, queries.len());
        let _ = app.emit(
            "rag:status",
            RagStatus {
                request_id: request_id.clone(),
                stage: "searching".into(),
                message: format!("正在检索知识库... ({} 组查询)", queries.len()),
            },
        );

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
                    log::debug!("[rag_query] Embedding for query='{}' took={:?} success={}",
                        &q, embed_start.elapsed(), embedding.is_some());

                    if let Some(vec) = embedding {
                        let search_start = std::time::Instant::now();
                        // 轻量级意图路由 + 元数据过滤（按文件类型限定候选范围）
                        let intent = route_intent(&q);
                        let hits = state
                            .indexer
                            .hybrid_search(&dir, &vec, &q, effective_top_k, intent)
                            .await
                            .unwrap_or_default();
                        log::debug!("[rag_query] hybrid_search for query='{}' hits={} took={:?}",
                            &q, hits.len(), search_start.elapsed());
                        hits
                    } else {
                        log::warn!("[rag_query] Embedding failed for query='{}', skipping", &q);
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
        log::info!("[rag_query] Stage2: all searches done request_id={} took={:?}",
            request_id, search_start.elapsed());

        // 展平所有结果
        let all_hits: Vec<SearchHit> = all_results.into_iter().flatten().collect();
        log::info!("[rag_query] Stage2: total raw hits={}", all_hits.len());

        if cancel.is_cancelled() {
            task_registry.unregister(&request_id).await;
            return Ok(());
        }

        // 预检索结果提取：无命中时降级为空上下文，交由 Agent 按需使用工具
        'retrieval: {
            if all_hits.is_empty() {
                log::warn!("[rag_query] Stage2: no hits found, 预检索降级为空上下文（agentic 模式）request_id={}", request_id);
                break 'retrieval (String::new(), Vec::new(), 0usize);
            }

            // ── Stage 3: 文档级聚合 + 绝对阈值（core::agent::aggregate_hits）──
            log::info!("[rag_query] Stage3: aggregation start request_id={}", request_id);
            let selected: Vec<(SearchHit, f32)> = aggregate_hits(
                all_hits,
                effective_min_score,
                effective_max_docs,
                effective_max_chunks,
            );
            log::info!("[rag_query] Stage3: aggregation done request_id={} selected_chunks={}", request_id, selected.len());
            log::debug!(
                "[rag_query] Stage3: selected docs={:?}",
                selected
                    .iter()
                    .map(|(hit, score)| format!("{}:{:.3}", hit.doc_name, score))
                    .collect::<Vec<_>>()
            );

            if selected.is_empty() {
                log::warn!("[rag_query] Stage3: no docs passed threshold, 预检索降级为空上下文（agentic 模式）request_id={}", request_id);
                break 'retrieval (String::new(), Vec::new(), 0usize);
            }

            // 按文档分组构建上下文：文档按分数降序、文档内按阅读顺序（chunk_index），
            // 优先使用 sentence_window（包含检索句子前后的上下文），fallback 到 chunk text，
            // 总字符数受 agent 模块的 MAX_CONTEXT_CHARS 限制避免超出模型窗口。
            let context = build_context_text(&selected, crate::core::agent::MAX_CONTEXT_CHARS);
            log::debug!(
                "[rag_query] Stage4: context built char_len={} preview={:?}",
                context.len(),
                context
            );

            // 构建引用来源（按 doc_name 去重，合并文本/path_json，取最高分；
            // 对 OPML/FreeMind 合并 path_json 层级路径展示）
            let sources = build_sources(&selected);
            log::debug!("[rag_query] Stage4: sources deduped count={}", sources.len());

            (context, sources, selected.len())
        }
    } else {
        log::info!("[rag_query] 未命中检索技能，跳过预检索（agentic 模式）request_id={}", request_id);
        (String::new(), Vec::new(), 0usize)
    };
    let sources_clone = sources.clone();

    // ── Stage 4: 构建 context → RAG Agent 生成（技能解析与参数覆盖已在 Stage 0 完成）──
    log::info!("[rag_query] Stage4: building context and agent generation start request_id={}", request_id);
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

    if cancel.is_cancelled() {
        log::info!("[rag_query] Cancelled before Stage4 stream request_id={}", request_id);
        task_registry.unregister(&request_id).await;
        return Ok(());
    }
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
        dir_blacklist: kb_cfg.dir_blacklist,
        file_blacklist: kb_cfg.file_blacklist,
        min_score: effective_min_score,
        max_context_docs: effective_max_docs,
        max_chunks_per_doc: effective_max_chunks,
        skill_id: primary_skill_id,
        skill_state: active_skills.clone(),
        skill_bases,
        search_sink: search_sink.clone(),
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
    );

    // 技能执行计时起点（进入生成阶段即视为执行开始）
    let skill_exec_start = std::time::Instant::now();

    // 当前问题作为 prompt，历史消息（去掉最后一条当前问题）作为 history
    let history = messages_to_history(&messages);
    let mut stream = agent
        .stream_chat(Message::user(query.clone()), history)
        .into_future()
        .await;

    // 流式生成
    log::info!("[rag_query] Stage4: starting agent stream request_id={}", request_id);
    let llm_start = std::time::Instant::now();
    let mut full_content = String::new();
    let mut final_usage: Option<UsageInfo> = None;
    let mut delta_count = 0u64;
    let mut stream_failed = false;
    while let Some(item) = stream.next().await {
        if cancel.is_cancelled() {
            log::info!("[rag_query] Cancelled during Stage4 stream request_id={} accumulated={}",
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
            emit_pending_tool_events(&app, &request_id);
            tool_call_bus().clear(&request_id);
            record_skill_execution(&state.skill_metrics, skill_ctx, &active_skills, skill_exec_start, false, Some("cancelled"));
            task_registry.unregister(&request_id).await;
            return Ok(());
        }
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall { tool_call, .. })) => {
                log::debug!("[rag_query] Stage4: model tool call name={} arguments={}",
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
                    log::debug!("[rag_query] Stage4: agent final usage request_id={} prompt={} completion={}",
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
                log::warn!("[rag_query] Stage4: agent stream error request_id={} err={}", request_id, e);
                stream_failed = true;
                break;
            }
        }
        // 转发工具调用轨迹（工具在 Rig 流式内部执行，结果已写入总线）
        emit_pending_tool_events(&app, &request_id);
    }
    log::info!("[rag_query] Stage4: agent stream done request_id={} took={:?} delta_count={} content_len={}",
        request_id, llm_start.elapsed(), delta_count, full_content.len());

    // 流式失败且无任何内容 → 先走非流式降级重试，仍失败才显式报错，
    // 避免静默失败或空消息污染前端
    if stream_failed && full_content.is_empty() && !cancel.is_cancelled() {
        log::warn!("[rag_query] Stage4: stream failed, falling back to non-streaming request_id={}", request_id);
        match complete_fallback(&agent, Message::user(query.clone()), messages_to_history(&messages)).await {
            Ok(content) => {
                let content = content.trim().to_string();
                if content.is_empty() {
                    log::warn!("[rag_query] Stage4: non-streaming fallback returned empty request_id={}", request_id);
                    record_skill_execution(&state.skill_metrics, skill_ctx, &active_skills, skill_exec_start, false, Some("llm_stream_failed"));
                    emit_command_error(&app, "rag:error", &request_id, "LLM 生成失败，请检查模型服务是否可用".into());
                } else {
                    // 一次性推送完整内容：先 delta（保证前端创建消息 DOM），再 done 收尾
                    record_skill_execution(&state.skill_metrics, skill_ctx, &active_skills, skill_exec_start, true, None);
                    let _ = app.emit(
                        "rag:delta",
                        RagDelta {
                            request_id: request_id.clone(),
                            content: content.clone(),
                        },
                    );
                    let _ = app.emit(
                        "rag:done",
                        RagDone {
                            request_id: request_id.clone(),
                            content,
                            sources: merge_search_sink(sources_clone, &search_sink).await,
                            prompt_tokens: 0,
                            completion_tokens: 0,
                        },
                    );
                }
            }
            Err(e) => {
                log::warn!("[rag_query] Stage4: non-streaming fallback failed request_id={} err={}", request_id, e);
                record_skill_execution(&state.skill_metrics, skill_ctx, &active_skills, skill_exec_start, false, Some("llm_stream_failed"));
                emit_command_error(&app, "rag:error", &request_id, "LLM 生成失败，请检查模型服务是否可用".into());
            }
        }
        // 非流式降级期间可能执行过工具，补发一次并清理
        emit_pending_tool_events(&app, &request_id);
        tool_call_bus().clear(&request_id);
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // ── Done ──
    // 流式正常结束但内容为空（模型零输出）：按失败处理并显式报错，
    // 与"非流式降级返回空"的行为保持一致，避免空回复污染前端与指标
    if full_content.trim().is_empty() {
        log::warn!("[rag_query] Stage4: stream done but empty content request_id={}", request_id);
        emit_pending_tool_events(&app, &request_id);
        tool_call_bus().clear(&request_id);
        record_skill_execution(&state.skill_metrics, skill_ctx, &active_skills, skill_exec_start, false, Some("llm_empty_output"));
        emit_command_error(&app, "rag:error", &request_id, "LLM 生成失败，请检查模型服务是否可用".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }
    let (prompt_tokens, completion_tokens) = final_usage
        .map(|u| (u.prompt_tokens, u.completion_tokens))
        .unwrap_or((0, 0));
    log::info!("[rag_query] DONE request_id={} content_len={} sources={} tokens_in={} tokens_out={}",
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
    record_skill_execution(&state.skill_metrics, skill_ctx, &active_skills, skill_exec_start, true, None);
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
            log::error!("[llm_query] LLMClient init failed request_id={} err={}", request_id, e);
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

    // 后端兜底校验：消息总长度
    if let Err(e) = validate_messages_length(&messages) {
        log::warn!("[llm_query] validate_messages_length failed request_id={} err={}", request_id, e);
        emit_command_error(&app, "llm:error", &request_id, e);
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    if messages.is_empty() {
        emit_command_error(&app, "llm:error", &request_id, "消息不能为空".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    let prompt_content = messages.last().map(|m| m.content.clone()).unwrap_or_default();
    let history = messages_to_history(&messages);

    // Agent 规约（角色/语言/安全边界）从资源目录加载，打包后跟随安装包
    let agent_rules = load_agent_rules(&app, "chat_agent.md");
    let agent = build_chat_agent(llm.completion_model().clone(), agent_rules);
    let mut stream = agent
        .stream_chat(Message::user(prompt_content.clone()), history)
        .into_future()
        .await;

    let mut full_content = String::new();
    let mut stream_failed = false;
    while let Some(item) = stream.next().await {
        if cancel.is_cancelled() {
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
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall { tool_call, .. })) => {
                log::debug!("[llm_query] agent tool call name={} arguments={}",
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
                log::warn!("[llm_query] agent stream error request_id={} err={}", request_id, e);
                stream_failed = true;
                break;
            }
        }
    }

    // 流式失败且无任何内容 → 先走非流式降级重试，仍失败才显式报错，避免静默失败
    if stream_failed && full_content.is_empty() && !cancel.is_cancelled() {
        log::warn!("[llm_query] stream failed, falling back to non-streaming request_id={}", request_id);
        match complete_fallback(&agent, Message::user(prompt_content), messages_to_history(&messages)).await {
            Ok(content) => {
                let content = content.trim().to_string();
                if content.is_empty() {
                    log::warn!("[llm_query] non-streaming fallback returned empty request_id={}", request_id);
                    emit_command_error(&app, "llm:error", &request_id, "LLM 生成失败，请检查模型服务是否可用".into());
                } else {
                    // 一次性推送完整内容：先 delta（保证前端创建消息 DOM），再 done 收尾
                    let _ = app.emit(
                        "llm:delta",
                        LlmDelta {
                            request_id: request_id.clone(),
                            content: content.clone(),
                        },
                    );
                    let _ = app.emit(
                        "llm:done",
                        LlmDone {
                            request_id: request_id.clone(),
                            content,
                        },
                    );
                }
            }
            Err(e) => {
                log::warn!("[llm_query] non-streaming fallback failed request_id={} err={}", request_id, e);
                emit_command_error(&app, "llm:error", &request_id, "LLM 生成失败，请检查模型服务是否可用".into());
            }
        }
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

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

