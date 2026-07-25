use tauri::{AppHandle, Emitter, Manager};

use mdgo_core::{IndexerConfig, KbIndexResult, KbProgress, KbStatus, SearchHit, call_embedding};
use crate::AppState;

// ─── 数据结构 ───

#[derive(Debug, serde::Serialize)]
pub struct FileTypeCount {
    pub file_type: String,
    pub count: u32,
    pub percentage: f32,
}

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
) -> Result<KbIndexResult, String> {
    let state = app.state::<AppState>();

    // 更新黑名单配置
    state.config_store.update(IndexerConfig {
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
    let query_embedding = tokio::task::spawn_blocking(move || call_embedding(&[&query_text], None))
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

/// 获取知识库仪表板统计（占用空间 + 文档类型分布）
#[tauri::command]
pub async fn kb_dashboard_stats(
    dir_path: String,
) -> Result<KbDashboardStats, String> {
    let scan_path = std::path::Path::new(&dir_path)
        .join(".mdgo")
        .join("data")
        .join("index_file_scan_data.json");

    let content = match std::fs::read_to_string(&scan_path) {
        Ok(c) => c,
        Err(_) => {
            return Ok(KbDashboardStats {
                storage_size: "--".to_string(),
                type_distribution: vec![],
            });
        }
    };

    let root: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("解析扫描数据失败: {}", e))?;

    // 总大小
    let storage_size = root
        .get("stats")
        .and_then(|s| s.get("total_size_str"))
        .and_then(|v| v.as_str())
        .unwrap_or("--")
        .to_string();

    // 为每个文件句柄扩展名估算文件类型
    fn classify_ext(ext: &str) -> String {
        match ext.to_lowercase().as_str() {
            "md" | "markdown" | "mdown" | "rst" | "txt" => "Markdown",
            "csv" | "tsv" | "jsonl" | "parquet" | "arrow" | "feather" => "数据",
            _ if matches!(
                ext.to_lowercase().as_str(),
                "py" | "js" | "ts" | "rs" | "go" | "java" | "c" | "cpp" | "h"
                | "hpp" | "css" | "scss" | "html" | "json" | "yaml" | "yml"
                | "toml" | "xml" | "sql" | "sh" | "bash" | "zsh" | "fish"
                | "ps1" | "bat" | "rb" | "php" | "swift" | "kt" | "scala"
                | "dart" | "lua" | "r"
            ) => "代码",
            _ => "其他",
        }
        .to_string()
    }

    // 遍历所有文件统计类型分布
    let mut type_counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut total_files: u32 = 0;

    if let Some(files_map) = root.get("files").and_then(|f| f.as_object()) {
        for (_dir_key, file_list) in files_map {
            if let Some(arr) = file_list.as_array() {
                for file_val in arr {
                    let ext = file_val
                        .get("ext")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let ft = classify_ext(ext);
                    *type_counts.entry(ft.to_string()).or_insert(0) += 1;
                    total_files += 1;
                }
            }
        }
    }

    let type_distribution: Vec<FileTypeCount> = type_counts
        .into_iter()
        .map(|(file_type, count)| FileTypeCount {
            percentage: if total_files > 0 {
                (count as f32 / total_files as f32 * 100.0 * 10.0).round() / 10.0
            } else {
                0.0
            },
            file_type,
            count,
        })
        .collect();

    Ok(KbDashboardStats {
        storage_size,
        type_distribution,
    })
}
