use std::sync::Arc;

use crate::services::chat::{ChatMessage, ChatMessageSource, ChatSession, ChatSessionSearchResult};
use crate::AppState;

#[tauri::command]
pub async fn chat_session_list(
    state: tauri::State<'_, AppState>,
    dir_path: String,
) -> Result<Vec<ChatSession>, String> {
    let store = state.get_chat_store(&dir_path)?;
    tokio::task::spawn_blocking(move || store.list_sessions())
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

/// 创建新会话。
///
/// 创建前会先把上一个同类型会话（按 updated_at DESC）的所有消息
/// 索引到 `chat_vectors` / `chat_bm25`，使其可被搜索召回。
/// 当前进行中的会话不入索引，避免频繁重建。
#[tauri::command]
pub async fn chat_session_create(
    state: tauri::State<'_, AppState>,
    dir_path: String,
    title: String,
    r#type: Option<String>,
) -> Result<ChatSession, String> {
    let store = state.get_chat_store(&dir_path)?;
    let indexer = state.indexer.clone();
    let session_type = r#type.unwrap_or_else(|| "regular".to_string());

    // 1. 获取上一个同类型会话（即将"结束"的会话）
    let prev_session = tokio::task::spawn_blocking({
        let store = Arc::clone(&store);
        let st = session_type.clone();
        move || store.get_last_session_by_type(&st)
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))??;

    // 2. 如果有上一个会话且有消息，异步索引它（避免阻塞新建会话）
    if let Some(prev) = prev_session {
        let prev_id = prev.id.clone();
        let messages = tokio::task::spawn_blocking({
            let store = Arc::clone(&store);
            move || store.get_session_messages(&prev_id)
        })
        .await
        .map_err(|e| format!("任务执行失败: {}", e))??;

        if !messages.is_empty() {
            let indexer = indexer.clone();
            let dir_path = dir_path.clone();
            let prev = prev.clone();
            // 使用超时等待索引完成（30s），确保数据落库后才返回
            // 如果超时则降级为异步后台执行，至少不阻塞会话创建
            if tokio::time::timeout(
                std::time::Duration::from_secs(30),
                indexer.index_chat_session(&dir_path, &prev, &messages),
            ).await.is_err() {
                log::warn!("[chat] 索引上一个会话超时（30s），降级为后台异步执行");
                tokio::spawn(async move {
                    if let Err(e) = indexer.index_chat_session(&dir_path, &prev, &messages).await {
                        log::warn!("[chat] 索引上一个会话失败: {}", e);
                    }
                });
            }
        }
    }

    // 3. 创建新会话
    tokio::task::spawn_blocking(move || store.create_session(&title, &session_type))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

/// 删除会话：同时从 SQLite（CASCADE 消息）和对话索引中删除
#[tauri::command]
pub async fn chat_session_delete(
    state: tauri::State<'_, AppState>,
    dir_path: String,
    id: String,
) -> Result<(), String> {
    let store = state.get_chat_store(&dir_path)?;
    let indexer = state.indexer.clone();
    let id_for_index = id.clone();

    // 1. 从 SQLite 删除会话（CASCADE 删除消息 + sources）
    tokio::task::spawn_blocking(move || store.delete_session(&id))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))??;

    // 2. 从对话索引中删除（向量 + BM25）
    if let Err(e) = indexer.remove_chat_session(&dir_path, &id_for_index).await {
        log::warn!("[chat] 删除对话索引失败: {}", e);
    }

    Ok(())
}

#[tauri::command]
pub async fn chat_session_rename(
    state: tauri::State<'_, AppState>,
    dir_path: String,
    id: String,
    title: String,
) -> Result<(), String> {
    let store = state.get_chat_store(&dir_path)?;
    tokio::task::spawn_blocking(move || store.rename_session(&id, &title))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

#[tauri::command]
pub async fn chat_session_toggle_favorite(
    state: tauri::State<'_, AppState>,
    dir_path: String,
    id: String,
) -> Result<bool, String> {
    let store = state.get_chat_store(&dir_path)?;
    tokio::task::spawn_blocking(move || store.toggle_favorite(&id))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

#[tauri::command]
pub async fn chat_session_messages(
    state: tauri::State<'_, AppState>,
    dir_path: String,
    session_id: String,
) -> Result<Vec<ChatMessage>, String> {
    let store = state.get_chat_store(&dir_path)?;
    tokio::task::spawn_blocking(move || store.get_session_messages(&session_id))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

/// 混合搜索对话历史：
/// 1. Indexer 做 1 次 query embedding + 向量检索 + BM25 检索 + RRF 融合
/// 2. ChatStore 做 SQL LIKE 模糊查询 + 根据候选 session_id 组装最终结果
#[tauri::command]
pub async fn chat_history_search(
    state: tauri::State<'_, AppState>,
    dir_path: String,
    query: String,
) -> Result<Vec<ChatSessionSearchResult>, String> {
    let store = state.get_chat_store(&dir_path)?;
    let indexer = state.indexer.clone();

    // 1. Indexer 混合检索（向量 + BM25 + RRF），返回 (session_id, score, matched_text)
    let indexer_hits = indexer
        .search_chat_sessions(&dir_path, &query, 20)
        .await
        .unwrap_or_default();

    // 2. ChatStore: SQL LIKE + 组装结果
    tokio::task::spawn_blocking(move || store.search_sessions(&query, &indexer_hits))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

#[tauri::command]
pub async fn chat_message_save(
    state: tauri::State<'_, AppState>,
    dir_path: String,
    session_id: String,
    role: String,
    content: String,
    token_count: i32,
) -> Result<ChatMessage, String> {
    let store = state.get_chat_store(&dir_path)?;
    tokio::task::spawn_blocking(move || {
        store.save_message(&session_id, &role, &content, token_count)
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))?
}

/// 清空会话消息：同时从 SQLite 和对话索引中删除
#[tauri::command]
pub async fn chat_session_clear_messages(
    state: tauri::State<'_, AppState>,
    dir_path: String,
    session_id: String,
) -> Result<(), String> {
    let store = state.get_chat_store(&dir_path)?;
    let indexer = state.indexer.clone();
    let sid = session_id.clone();

    // 1. 清空 SQLite 中的消息
    tokio::task::spawn_blocking(move || store.clear_session_messages(&session_id))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))??;

    // 2. 从对话索引中删除该会话的所有已索引消息
    if let Err(e) = indexer.remove_chat_session(&dir_path, &sid).await {
        log::warn!("[chat] 清空对话索引失败: {}", e);
    }

    Ok(())
}

#[tauri::command]
pub async fn chat_message_sources_save(
    state: tauri::State<'_, AppState>,
    dir_path: String,
    message_id: String,
    sources: Vec<ChatMessageSource>,
) -> Result<(), String> {
    let store = state.get_chat_store(&dir_path)?;
    tokio::task::spawn_blocking(move || store.save_message_sources(&message_id, &sources))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

#[tauri::command]
pub async fn chat_messages_sources(
    state: tauri::State<'_, AppState>,
    dir_path: String,
    message_ids: Vec<String>,
) -> Result<std::collections::HashMap<String, Vec<ChatMessageSource>>, String> {
    let store = state.get_chat_store(&dir_path)?;
    tokio::task::spawn_blocking(move || store.get_messages_sources(&message_ids))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

/// 主动索引当前会话。
///
/// 在会话切换、关闭页面或应用退出前调用，确保进行中的会话也能被搜索召回。
/// 索引在后台异步执行，不阻塞前端。
#[tauri::command]
pub async fn chat_session_index_current(
    state: tauri::State<'_, AppState>,
    dir_path: String,
    session_id: String,
) -> Result<(), String> {
    let store = state.get_chat_store(&dir_path)?;
    let indexer = state.indexer.clone();

    let session = tokio::task::spawn_blocking({
        let store = Arc::clone(&store);
        let sid = session_id.clone();
        move || store.get_session(&sid)
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))??;

    let session = match session {
        Some(s) => s,
        None => return Ok(()),
    };

    let messages = tokio::task::spawn_blocking({
        let store = Arc::clone(&store);
        let sid = session_id.clone();
        move || store.get_session_messages(&sid)
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))??;

    if !messages.is_empty() {
        tokio::spawn(async move {
            if let Err(e) = indexer.index_chat_session(&dir_path, &session, &messages).await {
                log::warn!("[chat] 索引当前会话失败: {}", e);
            }
        });
    }

    Ok(())
}

/// 获取聊天统计（供知识库面板使用）
#[derive(Debug, serde::Serialize)]
pub struct KbChatStats {
    pub session_count: u32,
    pub message_count: u32,
}

#[tauri::command]
pub async fn kb_chat_stats(
    state: tauri::State<'_, AppState>,
    dir_path: String,
) -> Result<KbChatStats, String> {
    let store = state.get_chat_store(&dir_path)?;
    let store_clone = Arc::clone(&store);
    let stats = tokio::task::spawn_blocking(move || {
        let session_count = store_clone.get_session_count();
        let message_count = store_clone.get_message_count();
        KbChatStats {
            session_count,
            message_count,
        }
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))?;
    Ok(stats)
}
