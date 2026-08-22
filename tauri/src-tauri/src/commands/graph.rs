//! 图谱 Tauri 命令（Graph Intelligence Layer 的 IPC 层）。
//!
//! 契约对齐 `docs/graph-os-frontend-design.md` §七 与前端 `graph-api.js`：
//! - `graph_status` / `graph_stats`：状态与统计
//! - `graph_related`：邻域查询（L1/L2 数据源，BFS + 截断）
//! - `graph_expand`：单节点增量展开
//! - `graph_search`：节点搜索
//! - `graph_overview`：L0 聚合概览（当前：全量节点 + 边，前端按 LOD 分层裁剪；
//!   百万级聚合由 Phase 3 聚类补充）
//!
//! 全部命令失败返回 Err（前端降级提示），不 panic。

use tauri::{AppHandle, Manager};

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
    Ok(crate::core::graph::model::GraphNeighborhood {
        nodes,
        edges,
        truncated: false,
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
#[tauri::command]
pub async fn graph_experience_record(
    app: AppHandle,
    dir_path: String,
    event: crate::core::graph::experience::ExperienceEvent,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.graph_engine.experience_record(&dir_path, &event)
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
