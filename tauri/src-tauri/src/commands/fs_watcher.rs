use std::path::Path;
use std::sync::Mutex;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tauri::{AppHandle, Emitter, Manager};

use crate::db::bm25::Bm25Index;
use crate::db::lance::LanceStore;
use crate::db::utils;
use crate::db::utils::IgnoreMatcher;

/// Tauri 托管状态：文件监听器
pub struct WatcherHandle {
    pub watcher: Mutex<Option<RecommendedWatcher>>,
}

impl WatcherHandle {
    pub fn new() -> Self {
        Self {
            watcher: Mutex::new(None),
        }
    }
}

/// 启动文件监听
#[tauri::command]
pub async fn kb_start_watcher(
    app: AppHandle,
    dir_path: String,
    embedding_endpoint: String,
    embedding_token: Option<String>,
    embedding_model: String,
    _embedding_dimension: u32,
    dir_blacklist: Vec<String>,
    file_blacklist: Vec<String>,
) -> Result<(), String> {
    let ignore = IgnoreMatcher::new(&dir_blacklist, &file_blacklist);
    let watch_path = dir_path.clone();
    let path = Path::new(&watch_path);
    if !path.exists() || !path.is_dir() {
        return Err(format!("目录不存在: {}", dir_path));
    }

    // 先停止已有监听
    let _ = kb_stop_watcher_inner(&app);

    let app_clone = app.clone();
    let dir_path_clone = dir_path.clone();

    let event_handler = move |event: Result<notify::Event, notify::Error>| {
        let event = match event {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[mdgo] 文件监听错误: {}", e);
                return;
            }
        };

        let is_modify = matches!(
            event.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
        );
        if !is_modify {
            return;
        }

        let is_remove = matches!(event.kind, EventKind::Remove(_));

        let paths: Vec<String> = event
            .paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        for event_path_str in &paths {
            let p = Path::new(event_path_str);

            // 硬编码排除 .mdgo 数据目录及其下的所有变更
            if event_path_str.contains(".mdgo") {
                continue;
            }

            // 使用 IgnoreMatcher 检查文件和目录
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");

            // 计算相对路径
            let rel_path = event_path_str
                .strip_prefix(&dir_path_clone)
                .unwrap_or(event_path_str);
            let rel = rel_path.replace('\\', "/");

            // 如果是目录路径，检查目录过滤
            if p.is_dir() && !ignore.is_kb_dir_allowed(name, &rel) {
                continue;
            }
            // 如果是文件路径，检查文件过滤
            if !p.is_dir() && !ignore.is_kb_file_allowed(name, &rel) {
                continue;
            }

            let rel_path = rel_path.replace('\\', "/");

            let app = app_clone.clone();
            let dir_path_for_watcher = dir_path_clone.clone();
            let ep = embedding_endpoint.clone();
            let et = embedding_token.clone();
            let em = embedding_model.clone();
            let eps = event_path_str.clone();

            tokio::spawn(async move {
                if is_remove {
                    // 文件删除：从 LanceDB 和 BM25 清除（解决 C3）
                    let data_dir = utils::get_data_dir(&dir_path_for_watcher);
                    let store = LanceStore::new(&data_dir, "vectors");
                    if store.open_table().await.is_ok() {
                        if let Err(e) = store.delete_document(&rel_path).await {
                            eprintln!("[mdgo] 删除 LanceDB 文档失败 ({}): {}", rel_path, e);
                        }
                    }

                    // 同步清除 BM25 索引
                    let bm25_dir = utils::get_bm25_dir(&dir_path_for_watcher);
                    if let Ok(bm25) = Bm25Index::open(&bm25_dir) {
                        if let Err(e) = bm25.delete_document(&rel_path) {
                            eprintln!("[mdgo] 删除 BM25 文档失败 ({}): {}", rel_path, e);
                        }
                    }

                    let _ = app.emit(
                        "kb-watcher-event",
                        serde_json::json!({
                            "type": "file_removed",
                            "path": rel_path,
                        }),
                    );
                } else {
                    // 文件创建/修改：重新索引该文件
                    let content = match std::fs::read_to_string(&eps) {
                        Ok(c) if c.len() >= 10 => c,
                        Ok(_) => return,
                        Err(e) => {
                            eprintln!("[mdgo] 读取文件失败 ({}): {}", eps, e);
                            return;
                        }
                    };

                    let chunks = utils::split_text(&content, 1000, 200);
                    if chunks.is_empty() {
                        return;
                    }

                    let doc_chunks = utils::build_document_chunks(&rel_path, &chunks);

                    let texts: Vec<&str> = doc_chunks.iter().map(|c| c.text.as_str()).collect();
                    let vectors = match utils::call_embedding(&ep, &et, &em, &texts).await {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("[mdgo] Embedding 失败 ({}): {}", eps, e);
                            return;
                        }
                    };

                    // 先写入新数据，再删除旧数据（P1-2: 防数据丢失）
                    let lance_dir = utils::get_data_dir(&dir_path_for_watcher);
                    let store = LanceStore::new(&lance_dir, "vectors");
                    if store.open_table().await.is_ok() {
                        if let Err(e) = store.add_chunks(&doc_chunks, &vectors).await {
                            eprintln!("[mdgo] 写入 LanceDB 失败 ({}): {}", rel_path, e);
                        } else {
                            // 写入成功后再删除旧数据
                            if let Err(e) = store.delete_document(&rel_path).await {
                                eprintln!("[mdgo] 清理旧 LanceDB 数据失败 ({}): {}", rel_path, e);
                            }
                        }
                    }

                    // 同步更新 BM25 索引（先写后删）
                    let bm25_dir = utils::get_bm25_dir(&dir_path_for_watcher);
                    if let Ok(bm25) = Bm25Index::open(&bm25_dir) {
                        if let Err(e) = bm25.add_documents(&doc_chunks) {
                            eprintln!("[mdgo] 写入 BM25 失败 ({}): {}", rel_path, e);
                        } else {
                            // 写入成功后再删除旧数据
                            if let Err(e) = bm25.delete_document(&rel_path) {
                                eprintln!("[mdgo] 清理旧 BM25 数据失败 ({}): {}", rel_path, e);
                            }
                        }
                    }

                    let _ = app.emit(
                        "kb-watcher-event",
                        serde_json::json!({
                            "type": "file_updated",
                            "path": rel_path,
                            "chunks": doc_chunks.len(),
                        }),
                    );
                }
            });
        }
    };

    let mut watcher = notify::recommended_watcher(event_handler)
        .map_err(|e| format!("创建文件监听器失败: {}", e))?;

    watcher
        .watch(path, RecursiveMode::Recursive)
        .map_err(|e| format!("开始监听目录失败: {}", e))?;

    // 存储 watcher 到 Tauri 状态
    {
        let state = app.state::<WatcherHandle>();
        let mut guard = state
            .watcher
            .lock()
            .map_err(|e| format!("获取 watcher 锁失败: {}", e))?;
        *guard = Some(watcher);
    }

    let _ = app.emit(
        "kb-watcher-event",
        serde_json::json!({
            "type": "watcher_started",
            "path": dir_path,
        }),
    );

    Ok(())
}

/// 停止文件监听
#[tauri::command]
pub async fn kb_stop_watcher(app: AppHandle) -> Result<(), String> {
    kb_stop_watcher_inner(&app);
    Ok(())
}

fn kb_stop_watcher_inner(app: &AppHandle) {
    let state = app.state::<WatcherHandle>();
    if let Ok(mut guard) = state.watcher.lock() {
        if let Some(watcher) = guard.take() {
            drop(watcher);
        }
    }
    let _ = app.emit(
        "kb-watcher-event",
        serde_json::json!({
            "type": "watcher_stopped",
        }),
    );
}
