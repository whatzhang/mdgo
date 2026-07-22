use tauri::{AppHandle, Emitter, Manager};

use crate::services::{KbIndexResult, KbStatus};
use crate::AppState;

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
) -> Result<KbIndexResult, String> {
    let state = app.state::<AppState>();

    // 更新黑名单配置
    state.config_store.update(crate::services::IndexerConfig {
        dir_blacklist,
        file_blacklist,
    });

    // 暂停 watcher 增量处理（避免 index_all 与 watcher 并发写 DB）
    state.watcher.pause();

    // 委托 Indexer 执行全量索引
    let result = state
        .indexer
        .index_all(&dir_path, |percent, msg| {
            let _ = app.emit(
                "kb-progress",
                crate::db::utils::KbProgress {
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

/// 混合检索（向量 + BM25 + RRF）
///
/// 内部自动使用本地模型生成查询向量，前端只需传入文本。
#[tauri::command]
pub async fn kb_search_hybrid(
    app: AppHandle,
    dir_path: String,
    query: String,
    top_k: u32,
) -> Result<Vec<crate::db::lance::SearchHit>, String> {
    let state = app.state::<AppState>();

    // 本地生成查询向量（bge-small-zh-v1.5, 384 维）
    let query_text = query.clone();
    let query_embedding = tokio::task::spawn_blocking(move || crate::db::utils::call_embedding(&[&query_text]))
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
