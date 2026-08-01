//! RAG Agent 模块：kb_search 工具 + Agent 构建（基于 Rig Agent）
//!
//! - [`build_kb_search_tool`]：将「嵌入 → 混合检索 → 文档聚合」封装为模型可调用的工具
//! - [`build_rag_agent`]：携带检索上下文与 kb_search 工具的知识库问答 Agent
//! - [`build_chat_agent`]：无工具纯对话 Agent
//! - [`aggregate_hits`]：文档级聚合逻辑（与检索结果共享）

use std::collections::HashMap;
use std::sync::Arc;

use rig_agent::agent::hook::{
    AgentHook, CompletionCall, CompletionCallAction, CompletionResponse, HookContext, ObservationAction,
};
use rig_agent::agent::{Agent, AgentBuilder};
use rig_agent::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use rig_core::providers::openai;

use crate::core::{Indexer, SearchHit, call_embedding_query};

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
        log::debug!(
            "[llm_trace] completion_call turn={} messages={}",
            event.turn,
            serde_json::to_string(&messages)
                .unwrap_or_else(|e| format!("<serialize failed: {}>", e))
        );
        CompletionCallAction::Continue
    }

    async fn on_completion_response(
        &self,
        _ctx: &HookContext,
        event: CompletionResponse<'_>,
    ) -> ObservationAction {
        log::debug!(
            "[llm_trace] completion_response content={} usage={}",
            serde_json::to_string(event.content)
                .unwrap_or_else(|e| format!("<serialize failed: {}>", e)),
            serde_json::to_string(&event.usage)
                .unwrap_or_else(|e| format!("<serialize failed: {}>", e))
        );
        ObservationAction::Continue
    }
}

/// RRF 风格 rank 归一化分数（用于跨查询公平比较）
pub fn rank_to_score(rank: usize) -> f32 {
    1.0 / (rank as f32 + 60.0)
}

/// kb_search 工具允许的最大片段数（防止模型传入超大 top_k 触发全量检索/重排）
const MAX_TOP_K: u32 = 20;

/// kb_search 工具的运行参数
#[derive(Clone)]
pub struct KbSearchConfig {
    /// 检索的知识库目录
    pub dir_path: String,
    /// 索引器（混合检索）
    pub indexer: Arc<Indexer>,
    /// 默认返回的片段数量（模型未指定 top_k 时使用）
    pub default_top_k: u32,
}

/// 执行一次完整检索：嵌入 → 混合检索 → 文档级聚合 → 生成模型可读文本。
///
/// 返回的文本按文档分组，同文档的多个片段合并，供模型直接作为上下文。
pub async fn kb_search(cfg: &KbSearchConfig, query: &str, top_k: u32) -> Result<String, String> {
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

    let hits = cfg.indexer.hybrid_search(&cfg.dir_path, &embedding, query, top_k).await?;
    if hits.is_empty() {
        return Ok("知识库中未找到相关内容。".to_string());
    }

    let selected = aggregate_hits(hits);
    if selected.is_empty() {
        return Ok("知识库中未找到足够相关的内容。".to_string());
    }

    let mut parts: Vec<String> = Vec::new();
    let mut last_doc = String::new();
    for (hit, _) in &selected {
        let text = hit.sentence_window.as_deref().unwrap_or(&hit.text);
        if hit.doc_name != last_doc {
            if !last_doc.is_empty() {
                parts.push(String::new());
            }
            parts.push(format!("--- {} ---", hit.doc_name));
            last_doc = hit.doc_name.clone();
        }
        parts.push(text.to_string());
    }
    Ok(parts.join("\n"))
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

                match kb_search(&cfg, &query, top_k).await {
                    Ok(text) => Ok(ToolOutput::text(text)),
                    Err(e) => Err(ToolExecutionError::other(format!("知识库检索失败: {}", e))
                        .with_model_output(ToolOutput::text(format!("知识库检索失败: {}", e)))),
                }
            })
        },
    )
}

/// 文档级聚合：按 doc+chunk 去重 → 按文档分组 → 自适应阈值 → 取 top 5 文档。
///
/// 返回聚合后的 `(SearchHit, score)` 列表，score 为命中原始分。
pub fn aggregate_hits(all_hits: Vec<SearchHit>) -> Vec<(SearchHit, f32)> {
    // 3a: 按 doc_name + chunk_index 去重，保留最高 mergeScore
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

    // 3b: 按 doc_name 聚合，保留该文档中所有达到阈值的 chunks（而非仅最高分 chunk）
    let max_score_overall = seen.values().map(|(_, s)| *s).fold(0.0_f32, f32::max);
    let doc_threshold = max_score_overall * 0.3;
    let mut doc_map: HashMap<String, Vec<(SearchHit, f32)>> = HashMap::new();
    for (hit, score) in seen.into_values() {
        if score < doc_threshold {
            continue;
        }
        let doc_name = hit.doc_name.clone();
        doc_map.entry(doc_name).or_default().push((hit, score));
    }
    // 每篇文档内按分数降序，最多保留 5 个 chunks
    for chunks in doc_map.values_mut() {
        chunks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        chunks.truncate(5);
    }

    // 3c: 以每篇文档的最佳 chunk 分数作为文档代表分排序，应用自适应阈值，取 top 5 文档
    let mut doc_scores: Vec<(Vec<(SearchHit, f32)>, f32)> = doc_map
        .into_values()
        .map(|chunks| {
            let best = chunks.first().map(|(_, s)| *s).unwrap_or(0.0);
            (chunks, best)
        })
        .collect();
    doc_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let max_doc_score = doc_scores.first().map(|(_, s)| *s).unwrap_or(0.0);
    let adapt_threshold = max_doc_score * 0.5;
    let abs_min = rank_to_score(15);
    let final_threshold = adapt_threshold.max(abs_min);

    doc_scores
        .into_iter()
        .filter(|(_, s)| *s >= final_threshold)
        .take(5)
        .flat_map(|(chunks, _)| chunks)
        .collect()
}

/// 构建 RAG 问答 Agent。
///
/// - `context`：预检索的知识库上下文，注入 system preamble
/// - `search_config`：用于构建 kb_search 工具（模型可在生成过程中补充检索）
pub fn build_rag_agent(
    model: openai::CompletionModel,
    context: &str,
    search_config: KbSearchConfig,
) -> Agent<openai::CompletionModel> {
    let preamble = format!(
        "你是一个知识库助手，请优先基于系统提供的上下文回答问题；如果上下文信息不足，可以调用 kb_search 工具检索更多资料。回答时请结合检索到的内容，对引用内容标注来源。如果知识库中确实没有相关信息，请如实告知。\n\n上下文：\n{}",
        context
    );
    AgentBuilder::new(model)
        .preamble(&preamble)
        .dynamic_tool(build_kb_search_tool(search_config))
        .default_max_turns(4)
        .add_hook(LlmTraceHook)
        .build()
}

/// 构建无工具纯对话 Agent
pub fn build_chat_agent(model: openai::CompletionModel) -> Agent<openai::CompletionModel> {
    AgentBuilder::new(model).add_hook(LlmTraceHook).build()
}
