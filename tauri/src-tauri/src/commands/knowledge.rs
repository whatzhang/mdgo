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
    // 更新配置，保留已有值，新字段可选
    state.config_store.update(IndexerConfig {
        dir_blacklist,
        file_blacklist,
        chunk_size: chunk_size.unwrap_or(old_cfg.chunk_size),
        chunk_overlap: chunk_overlap.unwrap_or(old_cfg.chunk_overlap),
        top_k: top_k.unwrap_or(old_cfg.top_k),
        min_score: min_score.unwrap_or(old_cfg.min_score),
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
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut cfg = state.config_store.read();
    if let Some(v) = chunk_size { cfg.chunk_size = v; }
    if let Some(v) = chunk_overlap { cfg.chunk_overlap = v; }
    if let Some(v) = top_k { cfg.top_k = v; }
    if let Some(v) = min_score { cfg.min_score = v; }
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
        })
    })
    .await
    .map_err(|e| format!("Embedding 任务执行失败: {}", e))?
}

