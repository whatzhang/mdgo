//! Bookmark Tauri 命令（UI 入口：导入/列表/搜索/统计/详情/树）。
//!
//! 边界：导入是 **UI 行为**，不暴露给 Agent 工具；
//! Agent 只经 `search_bookmarks` / `get_bookmark`（`core/agent/tools`）只读访问。

use tauri::{AppHandle, Manager};

use crate::AppState;
use crate::core::knowledge::bookmark::{Bookmark, BookmarkEntry, BookmarkImportStats, BookmarkSearchHit, BookmarkStats, tree::BookmarkTreeNode};

/// 打开（或惰性创建）某知识库目录的 BookmarkStore（共享 Arc<Mutex>）。
fn store(app: &AppHandle, dir_path: &str) -> Result<std::sync::Arc<std::sync::Mutex<crate::core::knowledge::bookmark::BookmarkStore>>, String> {
    app.state::<AppState>().bookmark_store(dir_path)
}

/// 导入书签（前端 `parseBookmarkHtml` 解析后的结构化 JSON；按 URL 去重，已存在跳过）。
///
/// **仅入库为 pending，不启动 Enrichment Worker**——后台分析只由
/// 「分析扫描」（`bookmark_worker_start`）手动触发，避免导入即自动消耗 LLM。
#[tauri::command]
pub async fn bookmark_import(
    app: AppHandle,
    dir_path: String,
    entries: Vec<BookmarkEntry>,
    source_file: Option<String>,
) -> Result<BookmarkImportStats, String> {
    log::info!(
        "[bookmark] 收到导入命令：知识库 {}，{} 条{}",
        dir_path, entries.len(),
        source_file.as_deref().map(|f| format!("，来源 {}", f)).unwrap_or_default()
    );
    let s = store(&app, &dir_path)?;
    let guard = s.lock().map_err(|e| e.to_string())?;
    guard.import_entries(entries, source_file.as_deref())
}

/// 书签列表（可选过滤；failed/dead 也展示，由前端按 status/dead 渲染）。
#[tauri::command]
pub async fn bookmark_list(
    app: AppHandle,
    dir_path: String,
    folder: Option<String>,
    category: Option<String>,
    status: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<Bookmark>, String> {
    let s = store(&app, &dir_path)?;
    let guard = s.lock().map_err(|e| e.to_string())?;
    guard.list(
        folder.as_deref(),
        category.as_deref(),
        status.as_deref(),
        limit.unwrap_or(100),
    )
}

/// 书签检索（LIKE ∪ 向量补位）。
#[tauri::command]
pub async fn bookmark_search(
    app: AppHandle,
    dir_path: String,
    query: String,
    limit: Option<usize>,
    category: Option<String>,
    folder: Option<String>,
) -> Result<Vec<BookmarkSearchHit>, String> {
    let q = query.trim().to_string();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let s = store(&app, &dir_path)?;
    crate::core::knowledge::bookmark::search::search_with_vectors(
        &*s,
        &dir_path,
        &q,
        limit.unwrap_or(10).clamp(1, 20),
        category.as_deref(),
        folder.as_deref(),
    )
    .await
}

/// 书签数量统计（UI 统计卡）。
#[tauri::command]
pub async fn bookmark_stat(app: AppHandle, dir_path: String) -> Result<BookmarkStats, String> {
    let s = store(&app, &dir_path)?;
    let guard = s.lock().map_err(|e| e.to_string())?;
    guard.stats()
}

/// 书签详情（UI / 工具共用）。
#[tauri::command]
pub async fn bookmark_get(
    app: AppHandle,
    dir_path: String,
    id: String,
) -> Result<Option<Bookmark>, String> {
    let s = store(&app, &dir_path)?;
    let guard = s.lock().map_err(|e| e.to_string())?;
    guard.get(&id)
}

/// 书签目录树（页面直读 DB 渲染；叶子带 status/dead 标记）。
#[tauri::command]
pub async fn bookmark_tree(app: AppHandle, dir_path: String) -> Result<BookmarkTreeNode, String> {
    let s = store(&app, &dir_path)?;
    let guard = s.lock().map_err(|e| e.to_string())?;
    guard.tree()
}

/// 「分析扫描」按钮：启动（或继续）书签 Enrichment Worker。
/// Worker 已在运行则无操作；空闲超时退出后经此重新启动。
#[tauri::command]
pub async fn bookmark_worker_start(app: AppHandle) -> Result<(), String> {
    let worker = app.state::<AppState>().bookmark_worker.clone();
    worker.ensure_running();
    Ok(())
}
