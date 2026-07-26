use std::collections::HashMap;

use futures::future::join_all;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::services::llm::{ChatMessage, LLMClient};
use mdgo_core::{call_embedding, SearchHit};

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

    // 从中央化内存配置读取 LLM 配置
    let llm_cfg = state.llm_config.read().unwrap_or_else(|e| e.into_inner()).clone();
    let llm = LLMClient::new(llm_cfg.endpoint, llm_cfg.model, llm_cfg.api_key);

    let emit_error = |msg: String| {
        let _ = app.emit("rag:error", CommandError {
            request_id: request_id.clone(),
            message: msg,
        });
    };

    if !llm.is_configured() {
        emit_error("LLM 未配置，请在设置中填写端点地址和模型名称".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // ── Stage 1: 查询扩展 ──
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

    // 检查取消
    if cancel.is_cancelled() {
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // ── Stage 2: 多查询混合检索（并行）──
    let _ = app.emit(
        "rag:status",
        RagStatus {
            request_id: request_id.clone(),
            stage: "searching".into(),
            message: format!("正在检索知识库... ({} 组查询)", queries.len()),
        },
    );

    // 对每个查询：嵌入 → 混合检索
    let search_futures: Vec<_> = queries
        .iter()
        .map(|q| {
            let dir = dir_path.clone();
            let state = state.clone();
            let q = q.clone();
            async move {
                let q_for_embed = q.clone();
                let embedding = tokio::task::spawn_blocking(move || {
                    call_embedding(&[&q_for_embed], None)
                })
                .await
                .ok()
                .and_then(|e| e.ok())
                .and_then(|v| v.into_iter().next());

                if let Some(vec) = embedding {
                    state
                        .indexer
                        .hybrid_search(&dir, &vec, &q, top_k)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .enumerate()
                        .map(|(rank, h)| (h, rank_to_score(rank)))
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                }
            }
        })
        .collect();

    let all_results = join_all(search_futures).await;

    // 展平所有结果
    let all_hits: Vec<(SearchHit, f32)> = all_results.into_iter().flatten().collect();

    if cancel.is_cancelled() {
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    if all_hits.is_empty() {
        emit_error("知识库中未找到相关内容".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // ── Stage 3: 文档级聚合 + 自适应阈值 ──
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

    if selected.is_empty() {
        emit_error("未找到足够相关的内容（请尝试更换关键词或扩展知识库）".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // ── Stage 4: 构建 context → LLM 生成 ──
    let _ = app.emit(
        "rag:status",
        RagStatus {
            request_id: request_id.clone(),
            stage: "generating".into(),
            message: format!("正在生成回答（基于 {} 篇文档）...", selected.len()),
        },
    );

    let context: String = selected
        .iter()
        .map(|(hit, _)| hit.text.as_str())
        .collect::<Vec<&str>>()
        .join("\n\n---\n\n");

    let sources: Vec<RagSource> = selected
        .into_iter()
        .map(|(hit, _)| RagSource {
            doc_name: hit.doc_name.clone(),
            score: hit.score,
            text: hit.text.clone(),
        })
        .collect();
    let sources_clone = sources.clone();

    // 构建 System Prompt + Messages
    let system_prompt = format!(
        "你是一个知识库助手，请基于以下上下文回答问题。如果上下文中没有相关信息，请如实告知。\n\n上下文：\n{}",
        context
    );

    let mut llm_messages = vec![ChatMessage {
        role: "system".to_string(),
        content: system_prompt,
    }];
    llm_messages.extend(messages);

    if cancel.is_cancelled() {
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // 流式生成
    let mut rx = match llm
        .stream_chat_completion(&llm_messages, None, None, cancel.clone())
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
    let mut final_usage: Option<crate::services::llm::UsageInfo> = None;
    while let Some(event) = rx.recv().await {
        match event {
            crate::services::llm::LLMEvent::Delta(text) => {
                full_content.push_str(&text);
                let _ = app.emit(
                    "rag:delta",
                    RagDelta {
                        request_id: request_id.clone(),
                        content: text,
                    },
                );
            }
            crate::services::llm::LLMEvent::Usage(usage) => {
                final_usage = Some(usage);
            }
        }
        if cancel.is_cancelled() {
            task_registry.unregister(&request_id).await;
            return Ok(());
        }
    }

    // ── Done ──
    let (prompt_tokens, completion_tokens) = final_usage
        .map(|u| (u.prompt_tokens, u.completion_tokens))
        .unwrap_or((0, 0));
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
