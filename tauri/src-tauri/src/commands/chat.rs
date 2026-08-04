use std::sync::Arc;

use crate::core::{ChatMessage, ChatMessageSource, ChatSession, ChatSessionSearchResult};
use tauri::{AppHandle, Emitter};
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
/// 超过上限 100 条时会自动删除最旧的会话，并同时清理其向量+BM25 索引。
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
            // 后台异步索引上一个会话，不阻塞新建会话
            tokio::spawn(async move {
                if let Err(e) = indexer.index_chat_session(&dir_path, &prev, &messages).await {
                    log::warn!("[chat] 索引上一个会话失败: {}", e);
                }
            });
        }
    }

    // 3. 创建新会话（超出上限时自动删除最旧的，返回被删 ID）
    let (session, deleted_ids) = tokio::task::spawn_blocking(move || store.create_session(&title, &session_type))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))??;

    // 4. 清理被自动删除会话的向量+BM25 索引
    if !deleted_ids.is_empty() {
        for sid in &deleted_ids {
            let indexer = indexer.clone();
            let dp = dir_path.clone();
            let sid = sid.clone();
            tokio::spawn(async move {
                if let Err(e) = indexer.remove_chat_session(&dp, &sid).await {
                    log::warn!("[chat] 清理自动删除会话索引失败 ({}): {}", sid, e);
                }
            });
        }
    }

    Ok(session)
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
        .unwrap_or_else(|e| {
            log::warn!("[chat] 索引搜索失败，降级为纯 LIKE 搜索: {}", e);
            Vec::new()
        });

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
    tool_calls: Option<String>,
) -> Result<ChatMessage, String> {
    let store = state.get_chat_store(&dir_path)?;
    tokio::task::spawn_blocking(move || {
        store.save_message(&session_id, &role, &content, token_count, tool_calls.as_deref())
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
/// 自动去重：消息条数无变化时跳过，避免重复索引浪费性能。
#[tauri::command]
pub async fn chat_session_index_current(
    app: AppHandle,
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

    // 去重：消息条数无变化则跳过
    let indexed_count = tokio::task::spawn_blocking({
        let store = Arc::clone(&store);
        let sid = session_id.clone();
        move || store.get_indexed_message_count(&sid)
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))??;

    if indexed_count == session.message_count {
        return Ok(());
    }

    let messages = tokio::task::spawn_blocking({
        let store = Arc::clone(&store);
        let sid = session_id.clone();
        move || store.get_session_messages(&sid)
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))??;

    let msg_count = messages.len() as u32;

    if msg_count > 0 {
        let store_clone = store.clone();
        let sid = session_id.clone();
        let app_clone = app.clone();
        tokio::spawn(async move {
            if let Err(e) = indexer.index_chat_session(&dir_path, &session, &messages).await {
                log::error!("[chat] 索引当前会话失败: {}", e);
                let _ = app_clone.emit("chat-index-error", format!("索引会话失败: {}", e));
            } else {
                // 仅索引成功后才持久化已索引条数，避免失败后跳过后续重试
                let _ = tokio::task::spawn_blocking(move || {
                    store_clone.set_indexed_message_count(&sid, msg_count)
                })
                .await;
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
        // 统计当前目录下所有会话（按目录维度，不限制会话类型）
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

#[tauri::command]
pub async fn chat_session_set_last(
    state: tauri::State<'_, AppState>,
    dir_path: String,
    session_id: String,
    mode: String,
) -> Result<(), String> {
    let store = state.get_chat_store(&dir_path)?;
    tokio::task::spawn_blocking(move || store.set_last_session(&session_id, &mode))
        .await
        .map_err(|e| format!("任务执行失败: {}", e))?
}

#[derive(Debug, serde::Serialize)]
pub struct LastSessionInfo {
    pub session_id: String,
    pub mode: String,
}

#[tauri::command]
pub async fn chat_session_get_last(
    state: tauri::State<'_, AppState>,
    dir_path: String,
) -> Result<Option<LastSessionInfo>, String> {
    let store = state.get_chat_store(&dir_path)?;
    let result = tokio::task::spawn_blocking(move || store.get_last_session())
        .await
        .map_err(|e| format!("任务执行失败: {}", e))??;
    Ok(result.map(|(session_id, mode)| LastSessionInfo { session_id, mode }))
}
