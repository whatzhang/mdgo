use tauri::{AppHandle, Emitter, Manager};

use crate::core::types::{FileTypeCount, IndexMeta};
use crate::core::{IndexerConfig, KbIndexResult, KbProgress, KbStatus, SearchHit, call_embedding_query};
use crate::AppState;

// ─── 数据结构 ───

#[derive(Debug, serde::Serialize)]
pub struct KbDashboardStats {
    pub storage_size: String,
    pub type_distribution: Vec<FileTypeCount>,
}

// ─── 命令 ───

/// 索引当前目录（全量重建）
///
/// 使用本地模型，无需 API 配置。黑名单控制哪些目录/文件跳过。
/// P0-1 分块参数校验（🟠 M27 修复：`kb_index` / `kb_update_indexer_config` 共用同一规则，
/// 杜绝双份复制漂移；并新增 **size+overlap 联合校验**，对齐 I-budget-2 不变式）。
///
/// - `chunk_size` 必须在 [64, 窗口−8]（token）；
/// - `chunk_overlap` 必须 < `chunk_size / 2`；
/// - `chunk_size + chunk_overlap ≤ 窗口−8`：否则 `ChunkBudget::from_config` 会把 overlap
///   静默钳到 0（用户设置被悄悄丢弃），此处改为显式拒绝；
/// - 返回最终生效的 `(chunk_size, chunk_overlap)`（未提供的参数沿用当前配置值参与校验）。
fn validate_chunk_params(
    chunk_size: Option<usize>,
    chunk_overlap: Option<usize>,
    current_size: usize,
    current_overlap: usize,
) -> Result<(usize, usize), String> {
    let max_seq = crate::core::get_max_seq_len();
    let max_ok = crate::core::db::token_budget::max_chunk_tokens(max_seq);

    let size = match chunk_size {
        Some(v) if v < 64 || v > max_ok => {
            return Err(format!(
                "chunk_size 需在 [64, {}]（token）之间；当前 embedding 模型窗口 {}，预留 {} token 给 special tokens",
                max_ok, max_seq, 8
            ));
        }
        Some(v) => v,
        None => current_size,
    };
    let overlap = chunk_overlap.unwrap_or(current_overlap);
    if overlap >= size / 2 {
        return Err(format!(
            "chunk_overlap 需小于 chunk_size 的一半（当前 chunk_size={} token）",
            size
        ));
    }
    if size.saturating_add(overlap) > max_ok {
        return Err(format!(
            "chunk_size + chunk_overlap 需 ≤ 模型窗口预算 {}（当前 {} + {} = {}）；请减小分块大小或重叠",
            max_ok,
            size,
            overlap,
            size.saturating_add(overlap)
        ));
    }
    Ok((size, overlap))
}

/// 索引期间自动暂停 watcher 增量处理，避免并发写 DB 竞态。
#[tauri::command]
pub async fn kb_index(
    app: AppHandle,
    dir_path: String,
    dir_blacklist: Vec<String>,
    file_blacklist: Vec<String>,
    chunk_size: Option<usize>,
    chunk_overlap: Option<usize>,
    top_k: Option<u32>,
    min_score: Option<f32>,
) -> Result<KbIndexResult, String> {
    let state = app.state::<AppState>();

    let old_cfg = state.config_store.read();
    // P0-1：分块参数校验（🟠 M27：公共校验函数，含 size+overlap 联合校验）
    let (chunk_size_final, chunk_overlap_final) = validate_chunk_params(
        chunk_size,
        chunk_overlap,
        old_cfg.chunk_size,
        old_cfg.chunk_overlap,
    )?;
    // 更新配置，保留已有值，新字段可选
    state.config_store.update(IndexerConfig {
        dir_blacklist,
        file_blacklist,
        chunk_size: chunk_size_final,
        chunk_overlap: chunk_overlap_final,
        top_k: top_k.unwrap_or(old_cfg.top_k),
        min_score: min_score.unwrap_or(old_cfg.min_score),
        ..old_cfg
    });

    // 暂停 watcher 增量处理（避免 index_all 与 watcher 并发写 DB）
    state.watcher.pause();

    // 委托 Indexer 执行全量索引
    let result = state
        .indexer
        .index_all(&dir_path, |percent, msg| {
            let _ = app.emit(
                "kb-progress",
                KbProgress {
                    percent,
                    message: msg.to_string(),
                },
            );
        })
        .await;

    // 无论成功失败都恢复 watcher
    state.watcher.resume();

    result
}

/// 增量索引：仅索引未索引的文件（不清理已有索引）
///
/// 扫描目录后逐个检查 LanceDB，跳过已有 chunk 的文件，
/// 只对新文件执行 index_file。
#[tauri::command]
pub async fn kb_index_unindexed(
    app: AppHandle,
    dir_path: String,
    dir_blacklist: Vec<String>,
    file_blacklist: Vec<String>,
) -> Result<KbIndexResult, String> {
    let state = app.state::<AppState>();

    state.config_store.update(IndexerConfig {
        dir_blacklist,
        file_blacklist,
        ..state.config_store.read()
    });

    // 暂停 watcher 增量处理（避免并发写 DB）
    state.watcher.pause();

    let result = state
        .indexer
        .index_unindexed(&dir_path, |percent, msg| {
            let _ = app.emit(
                "kb-progress",
                KbProgress {
                    percent,
                    message: msg.to_string(),
                },
            );
        })
        .await;

    state.watcher.resume();

    result
}

/// 获取当前索引器配置
#[tauri::command]
pub async fn kb_get_indexer_config(app: AppHandle) -> Result<IndexerConfig, String> {
    let state = app.state::<AppState>();
    Ok(state.config_store.read())
}

/// 更新索引器配置（不影响当前索引状态）
#[tauri::command]
pub async fn kb_update_indexer_config(
    app: AppHandle,
    chunk_size: Option<usize>,
    chunk_overlap: Option<usize>,
    top_k: Option<u32>,
    min_score: Option<f32>,
    fusion_alpha: Option<f32>,
    max_context_docs: Option<usize>,
    max_chunks_per_doc: Option<usize>,
    candidate_k: Option<u32>,
    rrf_k: Option<u32>,
    vec_min_score: Option<f32>,
    rerank_min_score: Option<f32>,
    bm25_msm_ratio: Option<f32>,
    reranker_enabled: Option<bool>,
    // 🟠 M23 修复：证据校验开关接线（原为死配置——`evidence_check_enabled` 无任何
    // 命令参数与前端入口，C2 特性不可达；现可经此参数开启）
    evidence_check_enabled: Option<bool>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut cfg = state.config_store.read();

    // P0-1：分块参数校验（🟠 M27：公共校验函数，含 size+overlap 联合校验——
    // 拒绝非法值，不再静默接受；chunk 超模型窗口会被静默截断）
    let (size, overlap) = validate_chunk_params(chunk_size, chunk_overlap, cfg.chunk_size, cfg.chunk_overlap)?;
    cfg.chunk_size = size;
    cfg.chunk_overlap = overlap;
    if let Some(v) = top_k { cfg.top_k = v; }
    if let Some(v) = min_score { cfg.min_score = v; }
    if let Some(v) = fusion_alpha { cfg.fusion_alpha = v.clamp(0.0, 1.0); }
    if let Some(v) = max_context_docs { cfg.max_context_docs = v.max(1); }
    if let Some(v) = max_chunks_per_doc { cfg.max_chunks_per_doc = v.max(1); }
    if let Some(v) = candidate_k { cfg.candidate_k = v.max(10); }
    if let Some(v) = rrf_k { cfg.rrf_k = v.max(1); }
    if let Some(v) = vec_min_score { cfg.vec_min_score = v.clamp(0.0, 1.0); }
    if let Some(v) = rerank_min_score { cfg.rerank_min_score = v.clamp(0.0, 1.0); }
    if let Some(v) = bm25_msm_ratio { cfg.bm25_msm_ratio = v.clamp(0.0, 1.0); }
    if let Some(v) = reranker_enabled { cfg.reranker_enabled = v; }
    if let Some(v) = evidence_check_enabled { cfg.evidence_check_enabled = v; }
    state.config_store.update(cfg);
    Ok(())
}

/// 混合检索（向量 + BM25 + RRF）
///
/// 内部自动使用本地模型生成查询向量，前端只需传入文本。
#[tauri::command]
pub async fn kb_search_hybrid(
    app: AppHandle,
    dir_path: String,
    query: String,
    top_k: u32,
) -> Result<Vec<SearchHit>, String> {
    let state = app.state::<AppState>();

    // 本地生成查询向量（bge-small-zh-v1.5, 维度由 config.json 动态决定）
    let query_text = query.clone();
    let query_embedding = tokio::task::spawn_blocking(move || call_embedding_query(&query_text))
        .await
        .map_err(|e| format!("Embedding 任务执行失败: {}", e))?
        .map_err(|e| format!("生成查询向量失败: {}", e))?;
    let query_vec = query_embedding
        .into_iter()
        .next()
        .ok_or("Embedding 返回空向量")?;

    state
        .indexer
        .hybrid_search(&dir_path, &query_vec, &query, top_k)
        .await
}

/// 获取索引状态
#[tauri::command]
pub async fn kb_status(app: AppHandle, dir_path: String) -> Result<KbStatus, String> {
    let state = app.state::<AppState>();
    state.indexer.status(&dir_path).await
}

/// 清除索引
#[tauri::command]
pub async fn kb_clear(app: AppHandle, dir_path: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.indexer.clear(&dir_path).await
}

/// 递归计算目录下所有文件的总字节数
fn calc_dir_size(path: &std::path::Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// 格式化字节数为人类可读字符串
fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{} {}", bytes, UNITS[unit_idx])
    } else {
        format!("{:.1} {}", size, UNITS[unit_idx])
    }
}

/// 获取知识库仪表板统计（占用空间 + 文档类型分布）
#[tauri::command]
pub async fn kb_dashboard_stats(
    dir_path: String,
) -> Result<KbDashboardStats, String> {
    // 计算 .mdgo 目录实际占用空间
    let mdgo_dir = std::path::Path::new(&dir_path).join(".mdgo");
    let storage_size = if mdgo_dir.exists() {
        let bytes = calc_dir_size(&mdgo_dir);
        format_size(bytes)
    } else {
        "--".to_string()
    };

    // 从索引元数据中获取已索引文件的类型分布
    let data_dir = crate::core::db::utils::get_data_dir(&dir_path);
    let meta_path = std::path::Path::new(&data_dir).join("index_meta.json");
    let type_distribution = match std::fs::read_to_string(&meta_path) {
        Ok(c) => serde_json::from_str::<IndexMeta>(&c)
            .map(|meta| meta.type_distribution)
            .unwrap_or_default(),
        Err(_) => vec![],
    };

    Ok(KbDashboardStats {
        storage_size,
        type_distribution,
    })
}

/// 获取嵌入模型信息（模型名称、向量维度、状态）
///
/// 模型下载由启动后台线程驱动；若尚未下载/部署完成，快速返回
/// `downloading` / `error` 状态，避免前端 invoke 挂起等待下载结束。
/// 已就绪时才进入 spawn_blocking 初始化 ONNX session 并返回真实维度。
#[tauri::command]
pub async fn kb_embedding_info() -> Result<crate::core::types::KbEmbeddingInfo, String> {
    use crate::core::db::utils as db_utils;

    // 模型既未在进程内就绪，也未在磁盘缓存 → 返回下载中/失败状态（非阻塞）
    if !db_utils::is_model_ready() && !crate::core::model_download::is_model_cached() {
        let status = if db_utils::model_load_error().is_some() {
            "error"
        } else {
            "downloading"
        };
        return Ok(crate::core::types::KbEmbeddingInfo {
            model_name: db_utils::get_local_embedding_model_name(),
            dimension: 0,
            status: status.into(),
            max_position_embeddings: 0,
        });
    }

    // 模型已就绪，在阻塞池中初始化并返回真实维度
    tokio::task::spawn_blocking(|| {
        let dimension = db_utils::get_local_embedding_dimension()?;
        let model_name = db_utils::get_local_embedding_model_name();
        Ok::<_, String>(crate::core::types::KbEmbeddingInfo {
            model_name,
            dimension,
            status: "loaded".into(),
            // P0-1：前端据此约束 chunk_size 上限（窗口 - special tokens 预留）
            max_position_embeddings: crate::core::get_max_seq_len() as u32,
        })
    })
    .await
    .map_err(|e| format!("Embedding 任务执行失败: {}", e))?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 🟠 M27：分块参数校验——范围 / overlap 上限 / size+overlap 联合校验
    #[test]
    fn validate_chunk_params_range_and_joint() {
        // 窗口 512 → max_ok = 504
        // 合法值
        let ok = validate_chunk_params(Some(448), Some(56), 448, 56).unwrap();
        assert_eq!(ok, (448, 56));

        // chunk_size 超窗口
        assert!(validate_chunk_params(Some(600), None, 448, 56).is_err());
        // chunk_size 低于下限
        assert!(validate_chunk_params(Some(32), None, 448, 56).is_err());
        // overlap ≥ size/2
        assert!(validate_chunk_params(Some(448), Some(224), 448, 56).is_err());
        // 联合校验：448 + 100 > 504 → 拒绝（旧实现会通过并静默钳 overlap 到 0）
        assert!(validate_chunk_params(Some(448), Some(100), 448, 56).is_err());
        // 边界：恰好等于预算
        assert!(validate_chunk_params(Some(448), Some(56), 448, 56).is_ok());
    }
}

