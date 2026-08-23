//! 图谱 Tauri 命令（Graph Intelligence Layer 的 IPC 层）。
//!
//! 契约对齐 `docs/graph-os-frontend-design.md` §七 与前端 `graph-api.js`：
//! - `graph_status` / `graph_stats`：状态与统计
//! - `graph_related`：邻域查询（L1/L2 数据源，BFS + 截断）
//! - `graph_expand`：单节点增量展开
//! - `graph_search`：节点搜索
//! - `graph_overview`：L0 聚合概览（当前：全量节点 + 边，前端按 LOD 分层裁剪；
//!   百万级聚合由 Phase 3 聚类补充）
//! - P1/P2 AI 命令：graph_ai_*（抽取/摘要/候选/缺口/冲突/重复）、graph_query（GraphRAG）、
//!   graph_recommend、graph_favorite(s)、graph_evolution、graph_metrics
//!
//! 全部命令失败返回 Err（前端降级提示），不 panic。

use tauri::{AppHandle, Manager};

use crate::core::graph::ai::GraphAiService;
use crate::core::graph::model::Relation;
use crate::AppState;

/// 图构建状态。
#[tauri::command]
pub async fn graph_status(app: AppHandle, dir_path: String) -> Result<crate::core::graph::model::GraphStatus, String> {
    let state = app.state::<AppState>();
    state.graph_engine.status(&dir_path)
}

/// 图统计。
#[tauri::command]
pub async fn graph_stats(app: AppHandle, dir_path: String) -> Result<crate::core::graph::model::GraphStats, String> {
    let state = app.state::<AppState>();
    state.graph_engine.stats(&dir_path)
}

/// 邻域查询（BFS + 扇出截断；relations 为空 = 全部关系）。
#[tauri::command]
pub async fn graph_related(
    app: AppHandle,
    dir_path: String,
    node_id: String,
    depth: Option<u32>,
    max_nodes: Option<u32>,
    max_edges: Option<u32>,
    relations: Option<Vec<String>>,
    weight_min: Option<f32>,
) -> Result<crate::core::graph::model::GraphNeighborhood, String> {
    let state = app.state::<AppState>();
    let rels = relations.map(|rs| rs.iter().map(|r| Relation::from_str(r)).collect());
    state.graph_engine.neighborhood(
        &dir_path,
        &node_id,
        depth.unwrap_or(crate::core::graph::storage::DEFAULT_DEPTH),
        max_nodes.unwrap_or(crate::core::graph::storage::DEFAULT_MAX_NODES),
        max_edges.unwrap_or(crate::core::graph::storage::DEFAULT_MAX_EDGES),
        rels,
        weight_min.unwrap_or(crate::core::graph::storage::DEFAULT_WEIGHT_MIN),
    )
}

/// 单节点增量展开（1 跳）。
#[tauri::command]
pub async fn graph_expand(
    app: AppHandle,
    dir_path: String,
    node_id: String,
    max_nodes: Option<u32>,
    relations: Option<Vec<String>>,
    weight_min: Option<f32>,
    max_edges: Option<u32>,
) -> Result<crate::core::graph::model::GraphNeighborhood, String> {
    let state = app.state::<AppState>();
    let rels = relations.map(|rs| rs.iter().map(|r| Relation::from_str(r)).collect());
    state.graph_engine.expand(
        &dir_path,
        &node_id,
        max_nodes.unwrap_or(500),
        rels,
        weight_min.unwrap_or(crate::core::graph::storage::DEFAULT_WEIGHT_MIN),
        max_edges.unwrap_or(crate::core::graph::storage::DEFAULT_MAX_EDGES),
    )
}

/// 节点搜索。
#[tauri::command]
pub async fn graph_search(
    app: AppHandle,
    dir_path: String,
    keyword: String,
    limit: Option<u32>,
) -> Result<Vec<crate::core::graph::model::GraphNode>, String> {
    let state = app.state::<AppState>();
    state.graph_engine.search(&dir_path, &keyword, limit.unwrap_or(20))
}

/// L0 概览（前端 LOD 概览层数据源；当前返回全部节点/边，前端裁剪）。
#[tauri::command]
pub async fn graph_overview(
    app: AppHandle,
    dir_path: String,
    max_nodes: Option<u32>,
) -> Result<crate::core::graph::model::GraphNeighborhood, String> {
    let state = app.state::<AppState>();
    let store = state.graph_engine.store(&dir_path)?;
    let guard = store.lock().unwrap_or_else(|e| e.into_inner());
    // 概览 = 全量节点 + 边（受 max_nodes 保护）；LOD 聚合由前端/后续聚类负责
    let limit = max_nodes.unwrap_or(5000).max(1).min(20_000);
    let nodes = guard.all_nodes(limit)?;
    let edges = guard.all_edges(limit as u32 * 2)?;
    // 命中上限即视为截断（前端据此提示数据不完整）
    let truncated = nodes.len() as u32 >= limit || edges.len() as u32 >= limit * 2;
    Ok(crate::core::graph::model::GraphNeighborhood {
        nodes,
        edges,
        truncated,
    })
}

/// 全库实体抽取（Level 1 规则 + 可选 LLM；同步执行，成功后返回抽取的实体候选数）。
/// 供「图谱页 → 实体图构建」按钮触发；耗时场景建议前端提示等待。
#[tauri::command]
pub async fn graph_extract_entities(app: AppHandle, dir_path: String) -> Result<usize, String> {
    let state = app.state::<AppState>();
    state.graph_engine.extract_entities_all(&dir_path)
}

/// 记录一条经验事件（Experience Brain：problem→solution→doc 图写入）。
/// 入参对齐 `ExperienceEvent`（source: git_commit/ai_operation/chat_message）。
/// LLM 已配置时做 P/S 精抽取富化（Phase 4.2）；未配置 → 规则降级（fail-open）。
#[tauri::command]
pub async fn graph_experience_record(
    app: AppHandle,
    dir_path: String,
    event: crate::core::graph::experience::ExperienceEvent,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let engine = state.graph_engine.clone();
    if crate::core::graph::worker::graph_llm_configured(&app) {
        let llm = crate::core::graph::worker::build_graph_llm(&app).await;
        let extractor =
            crate::core::graph::ai::LlmExperienceExtractor::new(std::sync::Arc::from(llm));
        engine
            .experience_record_ai(&dir_path, &event, Some(&extractor))
            .await
    } else {
        engine.experience_record_ai(&dir_path, &event, None).await
    }
}

/// 「类似问题」检索（Experience Brain：按 problem 关键词匹配历史 solution）。
#[tauri::command]
pub async fn graph_experience_search(
    app: AppHandle,
    dir_path: String,
    problem: String,
    limit: Option<usize>,
) -> Result<Vec<crate::core::graph::experience::ExperienceHit>, String> {
    let state = app.state::<AppState>();
    state
        .graph_engine
        .experience_search(&dir_path, &problem, limit.unwrap_or(10))
}

/// 全部经验事件列表。
#[tauri::command]
pub async fn graph_experience_events(
    app: AppHandle,
    dir_path: String,
) -> Result<Vec<crate::core::graph::experience::ExperienceEvent>, String> {
    let state = app.state::<AppState>();
    state.graph_engine.experience_events(&dir_path)
}

// ─── Cluster（L0 聚合单元；PRD §11/§13/§39） ───

/// 全部聚类（含簇间链接 links + 关键文件 top_files）。
#[tauri::command]
pub async fn graph_clusters(
    app: AppHandle,
    dir_path: String,
    limit: Option<u32>,
) -> Result<Vec<crate::core::graph::model::GraphCluster>, String> {
    let state = app.state::<AppState>();
    state.graph_engine.clusters(&dir_path, limit.unwrap_or(200))
}

/// 单聚类详情。
#[tauri::command]
pub async fn graph_cluster(
    app: AppHandle,
    dir_path: String,
    cluster_id: String,
) -> Result<Option<crate::core::graph::model::GraphCluster>, String> {
    let state = app.state::<AppState>();
    state.graph_engine.cluster(&dir_path, &cluster_id)
}

/// Cluster 子图（成员 + 簇内边；前端 Cluster 展开数据源）。
#[tauri::command]
pub async fn graph_cluster_subgraph(
    app: AppHandle,
    dir_path: String,
    cluster_id: String,
    max_nodes: Option<u32>,
) -> Result<crate::core::graph::model::GraphNeighborhood, String> {
    let state = app.state::<AppState>();
    state
        .graph_engine
        .cluster_subgraph(&dir_path, &cluster_id, max_nodes.unwrap_or(500))
}

/// 手动重算聚类（build_all/incremental 已自动调用）。
#[tauri::command]
pub async fn graph_rebuild_clusters(app: AppHandle, dir_path: String) -> Result<usize, String> {
    let state = app.state::<AppState>();
    state.graph_engine.rebuild_clusters(&dir_path)
}

// ─── Graph Query API（PRD §24/§36） ───

/// 图版本（每次图变更 +1；前端缓存失效依据，PRD §73）。
#[tauri::command]
pub async fn graph_version(app: AppHandle, dir_path: String) -> Result<u64, String> {
    let state = app.state::<AppState>();
    state.graph_engine.graph_version(&dir_path)
}

/// 两节点最短路径（PRD §24 find_path）。
#[tauri::command]
pub async fn graph_path(
    app: AppHandle,
    dir_path: String,
    source: String,
    target: String,
    max_depth: Option<u32>,
) -> Result<crate::core::graph::model::GraphPath, String> {
    let state = app.state::<AppState>();
    state
        .graph_engine
        .find_path(&dir_path, &source, &target, max_depth.unwrap_or(6))
}

/// 两节点共同邻居（PRD §24 find_common_neighbors）。
#[tauri::command]
pub async fn graph_common_neighbors(
    app: AppHandle,
    dir_path: String,
    a: String,
    b: String,
) -> Result<crate::core::graph::model::GraphNeighborhood, String> {
    let state = app.state::<AppState>();
    state.graph_engine.common_neighbors(&dir_path, &a, &b)
}

/// 子图查询（BFS 深度扩展；PRD §24 get_subgraph）。
#[tauri::command]
pub async fn graph_subgraph(
    app: AppHandle,
    dir_path: String,
    node_id: String,
    depth: Option<u32>,
    max_nodes: Option<u32>,
    max_edges: Option<u32>,
) -> Result<crate::core::graph::model::GraphNeighborhood, String> {
    let state = app.state::<AppState>();
    state.graph_engine.subgraph(
        &dir_path,
        &node_id,
        depth.unwrap_or(crate::core::graph::storage::DEFAULT_DEPTH),
        max_nodes.unwrap_or(crate::core::graph::storage::DEFAULT_MAX_NODES),
        max_edges.unwrap_or(crate::core::graph::storage::DEFAULT_MAX_EDGES),
    )
}

// ─── P1/P2 AI 命令（Graph AI Service；未配置 LLM 时规则降级） ───

/// 可选 LLM 适配器由 `core::graph::worker::build_graph_llm` 统一提供
/// （命令层与后台 worker 共用；未配置 → NullGraphLlm 规则降级）。
/// AI 实体关系抽取（PRD §26 Level 3）：单文档或高价值文档批量（limit 成本控制，PRD §75-76）。
/// 返回新增候选数。
#[tauri::command]
pub async fn graph_ai_extract(
    app: AppHandle,
    dir_path: String,
    node_id: Option<String>,
    limit: Option<u32>,
) -> Result<usize, String> {
    let state = app.state::<AppState>();
    let service = GraphAiService::new(state.graph_engine.clone());
    let llm = crate::core::graph::worker::build_graph_llm(&app).await;

    let rel_paths: Vec<String> = if let Some(nid) = node_id {
        let store = state.graph_engine.store(&dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get_node(&nid)? {
            Some(n) if n.path.is_some() => vec![n.path.unwrap()],
            // 实体/簇等无 path 节点无文件内容可抽取 → 返回 0
            _ => return Ok(0),
        }
    } else {
        let store = state.graph_engine.store(&dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.priority_doc_paths(limit.unwrap_or(10).min(50))?
    };

    let mut count = 0usize;
    for rel in rel_paths {
        let abs = std::path::Path::new(&dir_path).join(&rel);
        let content = match std::fs::read_to_string(&abs) {
            Ok(c) => c,
            Err(_) => continue,
        };
        count += service
            .extract_relations(&dir_path, &rel, &content, llm.as_ref())
            .await?;
    }
    Ok(count)
}

/// 全库 AI 重新入队（D4）：按最新重要度重排队列（done 项不重复处理，failed 项回退重试）。
/// 供「重新分析」按钮 / LLM 配置变更后触发后台 worker 处理；返回参与入队/更新的文档数。
#[tauri::command]
pub async fn graph_ai_enqueue_all(app: AppHandle, dir_path: String) -> Result<usize, String> {
    let state = app.state::<AppState>();
    state.graph_engine.requeue_all(&dir_path)
}

/// AI 簇摘要（PRD §29/§17）：为尚未 AI 描述的簇生成 description + tags。
#[tauri::command]
pub async fn graph_ai_summarize_clusters(
    app: AppHandle,
    dir_path: String,
    limit: Option<u32>,
) -> Result<usize, String> {
    let state = app.state::<AppState>();
    let service = GraphAiService::new(state.graph_engine.clone());
    let llm = crate::core::graph::worker::build_graph_llm(&app).await;
    let clusters = state.graph_engine.clusters(&dir_path, limit.unwrap_or(20).min(100))?;
    let mut done = 0usize;
    for c in clusters {
        // 跳过已有 AI 描述（含「标签：」后缀）
        if c.description.as_deref().map(|d| d.contains("标签：")).unwrap_or(false) {
            continue;
        }
        if service.summarize_cluster(&dir_path, &c.id, llm.as_ref()).await?.is_some() {
            done += 1;
        }
    }
    Ok(done)
}

/// AI 候选关系列表（PRD §27-28；status: candidate/confirmed/auto_confirmed/rejected）
#[tauri::command]
pub async fn graph_ai_candidates(
    app: AppHandle,
    dir_path: String,
    status: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<crate::core::graph::model::GraphAiCandidate>, String> {
    let state = app.state::<AppState>();
    let store = state.graph_engine.store(&dir_path)?;
    let guard = store.lock().unwrap_or_else(|e| e.into_inner());
    guard.list_candidates(status.as_deref(), limit.unwrap_or(100))
}

/// 确认候选关系（落正式边；PRD §28/§49）
#[tauri::command]
pub async fn graph_ai_confirm(
    app: AppHandle,
    dir_path: String,
    candidate_id: String,
) -> Result<Option<crate::core::graph::model::GraphAiCandidate>, String> {
    let state = app.state::<AppState>();
    let store = state.graph_engine.store(&dir_path)?;
    let guard = store.lock().unwrap_or_else(|e| e.into_inner());
    guard.update_candidate_status(&candidate_id, "confirmed")
}

/// 拒绝候选关系
#[tauri::command]
pub async fn graph_ai_reject(
    app: AppHandle,
    dir_path: String,
    candidate_id: String,
) -> Result<Option<crate::core::graph::model::GraphAiCandidate>, String> {
    let state = app.state::<AppState>();
    let store = state.graph_engine.store(&dir_path)?;
    let guard = store.lock().unwrap_or_else(|e| e.into_inner());
    guard.update_candidate_status(&candidate_id, "rejected")
}

/// 知识缺口检测（PRD §52：LLM 建议缺失概念；规则降级 = 相邻簇实体差集）
#[tauri::command]
pub async fn graph_ai_gaps(
    app: AppHandle,
    dir_path: String,
    cluster_id: String,
) -> Result<Vec<crate::core::graph::model::GraphGap>, String> {
    let state = app.state::<AppState>();
    let service = GraphAiService::new(state.graph_engine.clone());
    let llm = crate::core::graph::worker::build_graph_llm(&app).await;
    service.detect_gaps(&dir_path, &cluster_id, Some(llm.as_ref())).await
}

/// 知识冲突检测（PRD §54：LLM 比较两来源上下文）
#[tauri::command]
pub async fn graph_ai_conflicts(
    app: AppHandle,
    dir_path: String,
) -> Result<Vec<crate::core::graph::model::GraphConflict>, String> {
    let state = app.state::<AppState>();
    let service = GraphAiService::new(state.graph_engine.clone());
    let llm = crate::core::graph::worker::build_graph_llm(&app).await;
    service.detect_conflicts(&dir_path, llm.as_ref()).await
}

/// 知识重复检测（PRD §53：规则式规范化名相似度；spawn_blocking 避免阻塞 UI）
#[tauri::command]
pub async fn graph_ai_duplicates(
    app: AppHandle,
    dir_path: String,
) -> Result<Vec<crate::core::graph::model::GraphDuplicate>, String> {
    let state = app.state::<AppState>();
    let service = GraphAiService::new(state.graph_engine.clone());
    tokio::task::spawn_blocking(move || service.detect_duplicates(&dir_path, 50))
        .await
        .map_err(|e| format!("重复检测任务失败: {}", e))?
}

/// GraphRAG 问答（PRD §22-23：实体检测 → 图扩展 + 混合检索 → 上下文 → LLM + 证据）
#[tauri::command]
pub async fn graph_query(
    app: AppHandle,
    dir_path: String,
    query: String,
    top_k: Option<u32>,
) -> Result<crate::core::graph::model::GraphQueryResult, String> {
    let state = app.state::<AppState>();
    let service = GraphAiService::new(state.graph_engine.clone());
    let llm = crate::core::graph::worker::build_graph_llm(&app).await;

    // 混合检索命中（embedding 模型未就绪时降级为空 → 纯图证据）
    let hybrid_hits: Vec<(String, String, f32, u32)> = {
        let q = query.clone();
        match tokio::task::spawn_blocking(move || crate::core::db::utils::call_embedding_query(&q))
            .await
        {
            Ok(Ok(vec)) => {
                let query_vec = match vec.into_iter().next() {
                    Some(v) => v,
                    None => Vec::new(),
                };
                if query_vec.is_empty() {
                    Vec::new()
                } else {
                    match state
                        .indexer
                        .hybrid_search(&dir_path, &query_vec, &query, top_k.unwrap_or(20).min(50))
                        .await
                    {
                        Ok(hits) => hits
                            .into_iter()
                            .map(|h| (h.doc_name, h.text, h.score, h.chunk_index))
                            .collect(),
                        Err(_) => Vec::new(),
                    }
                }
            }
            _ => Vec::new(),
        }
    };

    service.graph_rag(&dir_path, &query, llm.as_ref(), &hybrid_hits).await
}

/// 基于图的推荐（PRD §51：你可能还需要了解）
#[tauri::command]
pub async fn graph_recommend(
    app: AppHandle,
    dir_path: String,
    node_id: String,
    limit: Option<usize>,
) -> Result<Vec<crate::core::graph::model::GraphRecommendation>, String> {
    let state = app.state::<AppState>();
    let service = GraphAiService::new(state.graph_engine.clone());
    service.recommend(&dir_path, &node_id, limit.unwrap_or(8))
}

/// 收藏 / 取消收藏节点（PRD §50：My Knowledge）
#[tauri::command]
pub async fn graph_favorite(
    app: AppHandle,
    dir_path: String,
    node_id: String,
    on: bool,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let store = state.graph_engine.store(&dir_path)?;
    let guard = store.lock().unwrap_or_else(|e| e.into_inner());
    guard.favorite(&node_id, on)
}

/// 收藏列表
#[tauri::command]
pub async fn graph_favorites(
    app: AppHandle,
    dir_path: String,
    limit: Option<u32>,
) -> Result<Vec<crate::core::graph::model::GraphNode>, String> {
    let state = app.state::<AppState>();
    let store = state.graph_engine.store(&dir_path)?;
    let guard = store.lock().unwrap_or_else(|e| e.into_inner());
    guard.list_favorites(limit.unwrap_or(100))
}

/// 知识演化统计 + AI 洞察（PRD §30-31；with_ai=false 时仅统计）
#[tauri::command]
pub async fn graph_evolution(
    app: AppHandle,
    dir_path: String,
    with_ai: Option<bool>,
) -> Result<EvolutionPayload, String> {
    let state = app.state::<AppState>();
    let service = GraphAiService::new(state.graph_engine.clone());
    if with_ai.unwrap_or(true) {
        let llm = crate::core::graph::worker::build_graph_llm(&app).await;
        let (evolution, insight) = service
            .evolution_insights(&dir_path, Some(llm.as_ref()))
            .await?;
        Ok(EvolutionPayload { evolution, insight })
    } else {
        Ok(EvolutionPayload {
            evolution: service.evolution(&dir_path)?,
            insight: None,
        })
    }
}

/// 图可观测性指标（PRD §74）
#[tauri::command]
pub async fn graph_metrics(
    app: AppHandle,
    dir_path: String,
) -> Result<crate::core::graph::model::GraphMetrics, String> {
    let state = app.state::<AppState>();
    let store = state.graph_engine.store(&dir_path)?;
    let guard = store.lock().unwrap_or_else(|e| e.into_inner());
    guard.metrics()
}

// ─── Chunk 图（知识图谱底座 Layer 1；build_all/incremental 已自动构建） ───

/// 手动重建全部 chunk/section 子图（幂等；供「重建内容层」入口）。
#[tauri::command]
pub async fn graph_build_chunks(app: AppHandle, dir_path: String) -> Result<ChunkBuildPayload, String> {
    let state = app.state::<AppState>();
    let engine = state.graph_engine.clone();
    let stats = tokio::task::spawn_blocking(move || engine.rebuild_chunks(&dir_path))
        .await
        .map_err(|e| format!("Chunk 构建任务失败: {}", e))??;
    Ok(ChunkBuildPayload {
        docs: stats.docs,
        chunks: stats.chunks,
        sections: stats.sections,
    })
}

/// 某文档的内容节点子图（chunk/section + doc→section→chunk 边；L4 细粒度数据源）。
#[tauri::command]
pub async fn graph_chunks(
    app: AppHandle,
    dir_path: String,
    node_id: String,
) -> Result<crate::core::graph::model::GraphNeighborhood, String> {
    let state = app.state::<AppState>();
    state.graph_engine.neighborhood(&dir_path, &node_id, 2, 2000, 3000, None, 0.0)
}

/// Chunk 相似边构建（SIMILAR_TO）：高价值文档的 chunk 向量检索 top-k，
/// 落 chunk↔chunk 语义相似边（Phase 1b；需本地 embedding 模型）。
#[tauri::command]
pub async fn graph_chunk_similarity(
    app: AppHandle,
    dir_path: String,
    top_k: Option<u32>,
) -> Result<usize, String> {
    let state = app.state::<AppState>();
    // 1) 候选 chunk：高价值文档（按度数 Top 30，上限 400 个 chunk；成本控制 PRD §75）
    let candidates: Vec<(String, String)> = {
        let store = state.graph_engine.store(&dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let paths = guard.priority_doc_paths(30)?;
        let mut out = Vec::new();
        for p in paths {
            for n in guard.list_content_nodes_for_doc(&p, 100)? {
                if n.node_type == crate::core::graph::model::NodeType::Chunk {
                    if let Some(c) = n.content {
                        if !c.trim().is_empty() {
                            out.push((n.id, c));
                        }
                    }
                }
            }
        }
        out.truncate(400);
        out
    };
    if candidates.is_empty() {
        return Ok(0);
    }
    // 2) 批量本地 embedding（spawn_blocking）
    let texts: Vec<String> = candidates.iter().map(|(_, t)| t.clone()).collect();
    let embeddings = tokio::task::spawn_blocking(move || {
        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        crate::core::db::utils::call_embedding(&refs, None)
    })
    .await
    .map_err(|e| format!("Embedding 任务失败: {}", e))?
    .map_err(|e| format!("生成 Embedding 失败: {}", e))?;

    // 3) LanceDB 向量检索 top-k（复用索引中的 chunk 向量）
    let uri = crate::core::db::utils::get_data_dir(&dir_path);
    let lance = crate::core::db::lance::LanceStore::new(&uri, "vectors");
    let k = top_k.unwrap_or(3).clamp(1, 5);
    let mut edges: Vec<(String, String, f32)> = Vec::new();
    for (i, emb) in embeddings.into_iter().enumerate() {
        if emb.is_empty() {
            continue;
        }
        let hits = lance
            .search_vectors(&emb, k)
            .await
            .map_err(|e| format!("向量检索失败: {}", e))?;
        for h in hits {
            let tid = crate::core::graph::chunk::chunk_node_id(&h.doc_name, h.chunk_index);
            if tid == candidates[i].0 || h.score < 0.55 {
                continue;
            }
            // 无向去重：按 id 排序
            let (a, b) = if candidates[i].0 < tid {
                (candidates[i].0.clone(), tid)
            } else {
                (tid, candidates[i].0.clone())
            };
            edges.push((a, b, h.score));
        }
    }
    // 4) 写 SIMILAR_TO 边（两端节点必须存在；去重）
    let store = state.graph_engine.store(&dir_path)?;
    let guard = store.lock().unwrap_or_else(|e| e.into_inner());
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut written = 0usize;
    for (a, b, score) in edges {
        if !guard.get_node(&a)?.is_some() || !guard.get_node(&b)?.is_some() {
            continue;
        }
        if !seen.insert((a.clone(), b.clone())) {
            continue;
        }
        guard.upsert_edge(
            &crate::core::graph::model::GraphEdge {
                source: a,
                target: b,
                relation: crate::core::graph::model::Relation::SimilarTo,
                weight: Some(score),
                confidence: Some(score),
            },
            None,
        )?;
        written += 1;
    }
    Ok(written)
}

/// graph_build_chunks 返回值
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChunkBuildPayload {
    pub docs: u32,
    pub chunks: u32,
    pub sections: u32,
}

/// 重新聚类（PRD §11.1）：mode = directory（目录结构，零成本）| embedding（本地 BGE 语义聚类）。
/// embedding 模式需要本地模型已下载；未就绪时返回 Err 提示。
#[tauri::command]
pub async fn graph_recluster(app: AppHandle, dir_path: String, mode: String) -> Result<usize, String> {
    let state = app.state::<AppState>();
    match mode.as_str() {
        "directory" => state.graph_engine.rebuild_clusters(&dir_path),
        "embedding" => {
            // 语义聚类（含本地 embedding 推理 → spawn_blocking 避免阻塞 UI）
            let engine = state.graph_engine.clone();
            tokio::task::spawn_blocking(move || engine.embed_clusters(&dir_path, 500))
                .await
                .map_err(|e| format!("语义聚类任务失败: {}", e))?
        }
        other => Err(format!("未知聚类模式: {}", other)),
    }
}

/// graph_evolution 返回值
#[derive(Debug, Clone, serde::Serialize)]
pub struct EvolutionPayload {
    pub evolution: crate::core::graph::model::GraphEvolution,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insight: Option<String>,
}

/// 记录一条知识偏好（PRD §60 Memory Graph；preference=true → PREFERS，false → AVOIDS）。
#[tauri::command]
pub async fn graph_memory_set(
    app: AppHandle,
    dir_path: String,
    topic: String,
    preference: bool,
    source: Option<String>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let service = GraphAiService::new(state.graph_engine.clone());
    service.memory_set(&dir_path, &topic, preference, source.as_deref())
}

/// 我的知识偏好列表（Memory Graph；My Knowledge 上下文源）
#[tauri::command]
pub async fn graph_memory_preferences(
    app: AppHandle,
    dir_path: String,
) -> Result<Vec<crate::core::graph::model::GraphRecommendation>, String> {
    let state = app.state::<AppState>();
    let service = GraphAiService::new(state.graph_engine.clone());
    service.memory_preferences(&dir_path)
}
