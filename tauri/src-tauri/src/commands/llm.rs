use std::collections::HashMap;

use futures::StreamExt;
use rig_agent::agent::MultiTurnStreamItem;
use rig_agent::completion::{Chat, CompletionModel};
use rig_agent::streaming::StreamingChat;
use rig_agent::Agent;
use rig_core::completion::Message;
use rig_core::streaming::StreamedAssistantContent;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::core::agent::{KbSearchConfig, aggregate_hits, build_chat_agent, build_rag_agent};
use crate::core::{call_embedding_query, SearchHit};
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

/// RAG 查询：查询扩展 → 混合检索 → 文档聚合 → RAG Agent 生成（全流式）
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
                    let hits = state
                        .indexer
                        .hybrid_search(&dir, &vec, &q, top_k)
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

    if all_hits.is_empty() {
        log::warn!("[rag_query] Stage2: no hits found, aborting request_id={}", request_id);
        emit_command_error(&app, "rag:error", &request_id, "知识库中未找到相关内容".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // ── Stage 3: 文档级聚合 + 自适应阈值（core::agent::aggregate_hits）──
    log::info!("[rag_query] Stage3: aggregation start request_id={}", request_id);
    let selected: Vec<(SearchHit, f32)> = aggregate_hits(all_hits);
    log::info!("[rag_query] Stage3: aggregation done request_id={} selected_chunks={}", request_id, selected.len());
    log::debug!(
        "[rag_query] Stage3: selected docs={:?}",
        selected
            .iter()
            .map(|(hit, score)| format!("{}:{:.3}", hit.doc_name, score))
            .collect::<Vec<_>>()
    );

    if selected.is_empty() {
        log::warn!("[rag_query] Stage3: no docs passed threshold, aborting request_id={}", request_id);
        emit_command_error(&app, "rag:error", &request_id, "未找到足够相关的内容（请尝试更换关键词或扩展知识库）".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // ── Stage 4: 构建 context → RAG Agent 生成 ──
    log::info!("[rag_query] Stage4: building context and agent generation start request_id={}", request_id);
    let status_msg = format!("正在生成回答（基于 {} 个相关片段）...", selected.len());
    let _ = app.emit(
        "rag:status",
        RagStatus {
            request_id: request_id.clone(),
            stage: "generating".into(),
            message: status_msg,
        },
    );

    // 按文档分组构建上下文：同一文档的多个 chunks 合并为一个段落块，
    // 优先使用 sentence_window（包含检索句子前后的上下文），fallback 到 chunk text。
    // Markdown/OPML/FreeMind 所有文档类型均适用。
    let mut context_parts: Vec<String> = Vec::new();
    let mut last_doc = String::new();
    for (hit, _) in &selected {
        let doc_name = &hit.doc_name;
        let text = hit.sentence_window.as_deref().unwrap_or(&hit.text);
        if *doc_name != last_doc {
            if !last_doc.is_empty() {
                context_parts.push(String::new()); // 文档间空行分隔
            }
            context_parts.push(format!("--- {} ---", doc_name));
            last_doc = doc_name.clone();
        }
        context_parts.push(text.to_string());
    }
    let context = context_parts.join("\n");
    log::debug!(
        "[rag_query] Stage4: context built char_len={} preview={:?}",
        context.len(),
        context
    );

    // 构建引用来源：按 doc_name 去重，同一文档只保留最高分条目，
    // 但合并所有 chunks 的文本（去重文本内容），确保来源完备不冗余。
    // 对 OPML/FreeMind：合并 path_json 层级路径展示。
    let mut source_dedup: std::collections::HashMap<String, RagSource> = std::collections::HashMap::new();
    for (hit, _) in &selected {
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
    log::debug!("[rag_query] Stage4: sources deduped count={}", sources.len());
    let sources_clone = sources.clone();

    if cancel.is_cancelled() {
        log::info!("[rag_query] Cancelled before Stage4 stream request_id={}", request_id);
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // 构建 RAG Agent：预载检索上下文 + kb_search 工具（模型可补充检索）
    let model = llm.completion_model().clone();
    let search_config = KbSearchConfig {
        dir_path: dir_path.clone(),
        indexer: state.indexer.clone(),
        default_top_k: top_k,
    };
    let agent = build_rag_agent(model, &context, search_config);

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
                        sources: sources_clone.clone(),
                        prompt_tokens,
                        completion_tokens,
                    },
                );
            }
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
                    emit_command_error(&app, "rag:error", &request_id, "LLM 生成失败，请检查模型服务是否可用".into());
                } else {
                    // 一次性推送完整内容：先 delta（保证前端创建消息 DOM），再 done 收尾
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
                            sources: sources_clone,
                            prompt_tokens: 0,
                            completion_tokens: 0,
                        },
                    );
                }
            }
            Err(e) => {
                log::warn!("[rag_query] Stage4: non-streaming fallback failed request_id={} err={}", request_id, e);
                emit_command_error(&app, "rag:error", &request_id, "LLM 生成失败，请检查模型服务是否可用".into());
            }
        }
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // ── Done ──
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
            sources: sources_clone,
            prompt_tokens,
            completion_tokens,
        },
    );

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

    let agent = build_chat_agent(llm.completion_model().clone());
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
