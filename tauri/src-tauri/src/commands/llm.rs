use std::collections::HashMap;

use futures::future::join_all;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::services::llm::{ChatMessage, LLMClient};
use crate::core::{call_embedding_query, SearchHit};

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

/// RRF 风格 rank 归一化分数（用于跨查询公平比较）
fn rank_to_score(rank: usize) -> f32 {
    1.0 / (rank as f32 + 60.0)
}

/// 校验消息总字符数是否超过上限，超限时返回错误描述
fn validate_messages_length(messages: &[ChatMessage]) -> Result<(), String> {
    let total: usize = messages.iter().map(|m| m.content.len()).sum();
    if total > MAX_MESSAGE_CHARS {
        return Err(format!(
            "对话历史过长（{} 字符 > 上限 {} 字符），请开始新对话",
            total, MAX_MESSAGE_CHARS
        ));
    }
    Ok(())
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

/// RAG 查询：查询扩展 → 混合检索 → 文档聚合 → LLM 生成（全流式）
#[tauri::command]
pub async fn kb_rag_query(
    app: AppHandle,
    state: tauri::State<'_, crate::AppState>,
    task_registry: tauri::State<'_, TaskRegistry>,
    dir_path: String,
    query: String,
    messages: Vec<ChatMessage>,
    request_id: String,
    top_k: u32,
) -> Result<(), String> {
    let cancel = task_registry.register(&request_id).await;

    log::info!("[rag_query] ENTRY request_id={} dir_path={} query_len={} msg_count={} top_k={}",
        request_id, dir_path, query.len(), messages.len(), top_k);

    // 从中央化内存配置读取 LLM 配置
    let llm_cfg = state.llm_config.read().unwrap_or_else(|e| e.into_inner()).clone();
    let llm = LLMClient::new(llm_cfg.endpoint.clone(), llm_cfg.model.clone(), llm_cfg.api_key.clone());
    log::info!("[rag_query] LLM config loaded endpoint={} model={}",
        llm_cfg.endpoint, llm_cfg.model);

    let emit_error = |msg: String| {
        let _ = app.emit("rag:error", CommandError {
            request_id: request_id.clone(),
            message: msg,
        });
    };

    if !llm.is_configured() {
        log::warn!("[rag_query] LLM not configured, aborting request_id={}", request_id);
        emit_error("LLM 未配置，请在设置中填写端点地址和模型名称".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // 后端兜底校验：消息总长度
    if let Err(e) = validate_messages_length(&messages) {
        log::warn!("[rag_query] validate_messages_length failed request_id={} err={}", request_id, e);
        emit_error(e);
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
                    hits.into_iter()
                        .enumerate()
                        .map(|(rank, h)| (h, rank_to_score(rank)))
                        .collect::<Vec<_>>()
                } else {
                    log::warn!("[rag_query] Embedding failed for query='{}', skipping", &q);
                    Vec::new()
                }
            }
        })
        .collect();

    let all_results = join_all(search_futures).await;
    log::info!("[rag_query] Stage2: all searches done request_id={} took={:?}",
        request_id, search_start.elapsed());

    // 展平所有结果
    let all_hits: Vec<(SearchHit, f32)> = all_results.into_iter().flatten().collect();
    log::info!("[rag_query] Stage2: total raw hits={}", all_hits.len());

    if cancel.is_cancelled() {
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    if all_hits.is_empty() {
        log::warn!("[rag_query] Stage2: no hits found, aborting request_id={}", request_id);
        emit_error("知识库中未找到相关内容".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // ── Stage 3: 文档级聚合 + 自适应阈值 ──
    log::info!("[rag_query] Stage3: aggregation start request_id={}", request_id);
    // 3a: 按 doc_name + chunk_index 去重，保留最高 mergeScore
    let mut seen: HashMap<(String, u32), (SearchHit, f32)> = HashMap::new();
    for (hit, score) in all_hits.into_iter() {
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
    log::debug!("[rag_query] Stage3a: unique chunks after dedup={}", seen.len());

    // 3b: 按 doc_name 聚合，每篇文档保留最高分的 chunk
    let mut doc_map: HashMap<String, (SearchHit, f32)> = HashMap::new();
    for (hit, score) in seen.into_values() {
        let doc_name = hit.doc_name.clone();
        match doc_map.entry(doc_name) {
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
    log::debug!("[rag_query] Stage3b: unique docs after doc-aggregation={}", doc_map.len());

    // 3c: 排序 + 自适应阈值
    let mut docs: Vec<(SearchHit, f32)> = doc_map.into_values().collect();
    docs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let max_score = docs.first().map(|(_, s)| *s).unwrap_or(0.0);
    let adapt_threshold = max_score * 0.5;
    let abs_min = rank_to_score(15);
    let final_threshold = adapt_threshold.max(abs_min);

    let selected: Vec<(SearchHit, f32)> = docs
        .into_iter()
        .filter(|(_, s)| *s >= final_threshold)
        .take(5)
        .collect();

    log::info!("[rag_query] Stage3c: threshold max_score={:.6} adapt={:.6} abs_min={:.6} final={:.6} selected={}",
        max_score, adapt_threshold, abs_min, final_threshold, selected.len());
    for (i, (hit, score)) in selected.iter().enumerate() {
        log::debug!("[rag_query] Stage3c: selected[{}] doc={} score={:.6} chunk={}", i, hit.doc_name, score, hit.chunk_index);
    }

    if selected.is_empty() {
        log::warn!("[rag_query] Stage3: no docs passed threshold, aborting request_id={}", request_id);
        emit_error("未找到足够相关的内容（请尝试更换关键词或扩展知识库）".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // ── Stage 4: 构建 context → LLM 生成 ──
    log::info!("[rag_query] Stage4: building context and LLM generation start request_id={}", request_id);
    let _ = app.emit(
        "rag:status",
        RagStatus {
            request_id: request_id.clone(),
            stage: "generating".into(),
            message: format!("正在生成回答（基于 {} 篇文档）...", selected.len()),
        },
    );

    // 优先使用 sentence_window（包含检索句子前后的上下文），fallback 到 chunk text
    let context: String = selected
        .iter()
        .map(|(hit, _)| hit.sentence_window.as_deref().unwrap_or(&hit.text))
        .collect::<Vec<&str>>()
        .join("\n\n---\n\n");
    log::debug!("[rag_query] Stage4: context built char_len={}", context.len());

    let sources: Vec<RagSource> = selected
        .into_iter()
        .map(|(hit, _)| RagSource {
            doc_name: hit.doc_name.clone(),
            score: hit.score,
            text: hit.text.clone(),
            path_json: hit.path_json.clone(),
        })
        .collect();
    let sources_clone = sources.clone();
    log::debug!("[rag_query] Stage4: sources count={}", sources.len());

    // 构建 System Prompt + Messages
    let system_prompt = format!(
        "你是一个知识库助手，请基于以下上下文回答问题。如果上下文中没有相关信息，请如实告知。\n\n上下文：\n{}",
        context
    );
    let system_prompt_len = system_prompt.len();

    let mut llm_messages = vec![ChatMessage {
        role: "system".to_string(),
        content: system_prompt,
    }];
    llm_messages.extend(messages);
    log::debug!("[rag_query] Stage4: LLM messages total={} system_len={}",
        llm_messages.len(), system_prompt_len);

    if cancel.is_cancelled() {
        log::info!("[rag_query] Cancelled before Stage4 stream request_id={}", request_id);
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // 流式生成
    log::info!("[rag_query] Stage4: starting LLM stream request_id={}", request_id);
    let llm_start = std::time::Instant::now();
    let mut rx = match llm
        .stream_chat_completion(&llm_messages, None, None, cancel.clone())
        .await
    {
        Ok(rx) => rx,
        Err(e) => {
            log::error!("[rag_query] Stage4: LLM stream_chat_completion failed request_id={} err={}", request_id, e);
            emit_error(format!("LLM 请求失败: {}", e));
            task_registry.unregister(&request_id).await;
            return Ok(());
        }
    };

    let mut full_content = String::new();
    let mut final_usage: Option<crate::services::llm::UsageInfo> = None;
    let mut delta_count = 0u64;
    while let Some(event) = rx.recv().await {
        match event {
            crate::services::llm::LLMEvent::Delta(text) => {
                full_content.push_str(&text);
                delta_count += 1;
                let _ = app.emit(
                    "rag:delta",
                    RagDelta {
                        request_id: request_id.clone(),
                        content: text,
                    },
                );
            }
            crate::services::llm::LLMEvent::Usage(usage) => {
                log::debug!("[rag_query] Stage4: received usage info request_id={} prompt={} completion={}",
                    request_id, usage.prompt_tokens, usage.completion_tokens);
                final_usage = Some(usage);
            }
        }
        if cancel.is_cancelled() {
            log::info!("[rag_query] Cancelled during Stage4 stream request_id={} accumulated={}",
                request_id, full_content.len());
            task_registry.unregister(&request_id).await;
            return Ok(());
        }
    }
    log::info!("[rag_query] Stage4: LLM stream done request_id={} took={:?} delta_count={} content_len={}",
        request_id, llm_start.elapsed(), delta_count, full_content.len());

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

/// 纯 LLM 对话
#[tauri::command]
pub async fn kb_llm_query(
    app: AppHandle,
    state: tauri::State<'_, crate::AppState>,
    task_registry: tauri::State<'_, TaskRegistry>,
    messages: Vec<ChatMessage>,
    request_id: String,
) -> Result<(), String> {
    let cancel = task_registry.register(&request_id).await;

    // 从中央化内存配置读取 LLM 配置
    let llm_cfg = state.llm_config.read().unwrap_or_else(|e| e.into_inner()).clone();
    let llm = LLMClient::new(llm_cfg.endpoint, llm_cfg.model, llm_cfg.api_key);

    let emit_error = |msg: String| {
        let _ = app.emit("llm:error", CommandError {
            request_id: request_id.clone(),
            message: msg,
        });
    };

    if !llm.is_configured() {
        emit_error("LLM 未配置".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // 后端兜底校验：消息总长度
    if let Err(e) = validate_messages_length(&messages) {
        log::warn!("[llm_query] validate_messages_length failed request_id={} err={}", request_id, e);
        emit_error(e);
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    let mut rx = match llm
        .stream_chat_completion(&messages, None, None, cancel.clone())
        .await
    {
        Ok(rx) => rx,
        Err(e) => {
            emit_error(format!("LLM 请求失败: {}", e));
            task_registry.unregister(&request_id).await;
            return Ok(());
        }
    };

    let mut full_content = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            crate::services::llm::LLMEvent::Delta(text) => {
                full_content.push_str(&text);
                let _ = app.emit(
                    "llm:delta",
                    LlmDelta {
                        request_id: request_id.clone(),
                        content: text,
                    },
                );
            }
            crate::services::llm::LLMEvent::Usage(usage) => {
                let _ = app.emit(
                    "llm:usage",
                    serde_json::json!({
                        "request_id": request_id,
                        "prompt_tokens": usage.prompt_tokens,
                        "completion_tokens": usage.completion_tokens,
                    }),
                );
            }
        }
        if cancel.is_cancelled() {
            task_registry.unregister(&request_id).await;
            return Ok(());
        }
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
