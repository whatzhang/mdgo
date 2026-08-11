use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use crate::core::{ChatMessage, ChatMessageSource, ChatSession, ChatSessionSearchResult};
use crate::core::Indexer;
use crate::services::chat::ChatStore;
use tauri::{AppHandle, Emitter};
use crate::AppState;

/// 同一会话同时只允许一个增量索引任务（去重集合）。
///
/// 增量索引（`index_chat_session`）纯追加、无幂等性：若 `chat_session_create` 与
/// `chat_session_index_current` 对同一会话并发索引，会用相同旧游标重复写入相同 chunk。
/// 任务持有集合项期间视为"该会话正在索引"，后到任务直接跳过；持锁后重读游标保证
/// 只有最先到达的任务执行索引。
///
/// 使用 std::sync::Mutex 而非 tokio::sync::Mutex：临界区仅 insert/remove（纳秒级、无
/// await 点），保证 RAII 守卫的 Drop 同步执行、绝不因异步锁竞争而永久泄漏集合项
/// （一旦泄漏，该会话将永远无法再被索引，直到重启）。
static INDEXING_SESSIONS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn indexing_sessions() -> &'static Mutex<HashSet<String>> {
    INDEXING_SESSIONS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 会话索引互斥的 RAII 守卫：`try_enter` 成功（集合中尚无该会话）时持有，Drop
/// （正常结束 / panic / 提前 return）时自动从集合移除，杜绝"insert 成功但 remove
/// 被跳过"导致的永久占用。
struct SessionIndexGuard {
    session_id: String,
}

impl SessionIndexGuard {
    /// 尝试独占该会话的索引权。已有任务在飞时返回 `None`（调用方应直接跳过）。
    fn try_enter(session_id: &str) -> Option<Self> {
        let mut set = indexing_sessions()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !set.insert(session_id.to_string()) {
            return None;
        }
        Some(Self {
            session_id: session_id.to_string(),
        })
    }
}

impl Drop for SessionIndexGuard {
    fn drop(&mut self) {
        indexing_sessions()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.session_id);
    }
}

/// 追平循环上限：防止会话持续写入时索引任务与写入形成活锁。达到上限后剩余缺口
/// 由下一次触发继续补齐（游标已落盘，重启自愈）。
const MAX_INDEX_ROUNDS: usize = 3;

/// 增量索引单个会话：游标重读 → 增量拉取 → 写向量 → 推进游标。
///
/// 以"追平循环"消除漏索引竞态：任务在飞期间新到达的消息（如离开会话时正在保存的
/// 半截回复）若只在首轮读取，游标推进后会被永久跳过。因此每轮结束重读真实消息数，
/// 游标仍落后则继续补一轮；`indexed_count > total`（会话被清空）时从 0 全量重建。
///
/// 调用方需先取得 `SessionIndexGuard`（会话级互斥），保证同一会话只有该任务在索引。
async fn index_session_catchup(
    store: &Arc<ChatStore>,
    indexer: &Indexer,
    dir_path: &str,
    session_id: &str,
    max_rounds: usize,
) -> Result<(), String> {
    let mut round = 0usize;
    loop {
        round += 1;

        // 单次 DB 访问取 (已索引数, 实际消息数)。锁内重读游标：可能已被并发任务推进，
        // 重读才不重复写入；重读实际消息数：上一轮索引执行期间可能已有新消息到达。
        let (indexed_count, total) = tokio::task::spawn_blocking({
            let store = Arc::clone(store);
            let sid = session_id.to_string();
            move || store.get_index_progress(&sid)
        })
        .await
        .map_err(|e| format!("任务执行失败: {}", e))??;
        let indexed_count = indexed_count as usize;
        let total = total as usize;

        // 游标失效防御：会话被清空后游标 > 实际条数时重置为 0 全量重建
        // （清空操作已删除向量并重置游标，此处为兜底；从 0 重建即补齐）
        let start_from = if indexed_count > total { 0 } else { indexed_count };

        // 游标已追平实际消息数 → 无缺口，结束
        if start_from >= total {
            break;
        }

        // 只拉未索引的增量消息
        let messages = tokio::task::spawn_blocking({
            let store = Arc::clone(store);
            let sid = session_id.to_string();
            move || store.get_session_messages_from(&sid, start_from)
        })
        .await
        .map_err(|e| format!("任务执行失败: {}", e))??;

        if messages.is_empty() {
            break;
        }

        indexer
            .index_chat_session(dir_path, session_id, &messages, start_from)
            .await?;

        // 乐观并发校验：索引执行期间会话可能被并发删除/清空/替换（前端"离开会话
        // 触发索引后立即删除/清空"是常见操作，embedding 耗时窗口足以重叠）。若本
        // 轮拉取的消息区间已变更，刚写入的向量即成孤儿/错误召回，回滚删除并放弃
        // 本轮游标推进，避免永久脏数据（删除路径无再清理入口）。
        let snapshot_ok = tokio::task::spawn_blocking({
            let store = Arc::clone(store);
            let sid = session_id.to_string();
            let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
            move || store.verify_chat_messages_unmodified(&sid, start_from, &ids)
        })
        .await
        .map_err(|e| format!("任务执行失败: {}", e))??;
        if !snapshot_ok {
            log::info!("[chat] 会话 {} 索引期间消息已变更，回滚本次写入", session_id);
            let _ = indexer.remove_chat_session(dir_path, session_id).await;
            return Ok(());
        }

        // 索引成功且快照校验通过后才推进游标，避免失败后跳过后续重试
        tokio::task::spawn_blocking({
            let store = Arc::clone(store);
            let sid = session_id.to_string();
            let new_count = (start_from + messages.len()) as u32;
            move || store.set_indexed_message_count(&sid, new_count)
        })
        .await
        .map_err(|e| format!("任务执行失败: {}", e))??;

        // 达到轮次上限：与持续写入解耦，剩余缺口由下一次触发继续补齐
        if round >= max_rounds {
            break;
        }
    }
    Ok(())
}

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
/// 增量索引到 `chat_vectors`，使其可被搜索召回。
/// 当前进行中的会话不入索引，避免频繁重建。
/// 超过上限 100 条时会自动删除最旧的会话，并同时清理其向量索引。
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

    // 2. 如果有上一个会话，后台异步增量索引它（避免阻塞新建会话）。
    //    会话级互斥（RAII 守卫）+ 追平循环保证：并发路径（create × index_current）
    //    不会用相同旧游标重复写入；索引期间新到达的消息会被追平补齐。
    if let Some(prev) = prev_session {
        let indexer = indexer.clone();
        let dir_path = dir_path.clone();
        let store = Arc::clone(&store);
        // 后台异步增量索引上一个会话，不阻塞新建会话
        tokio::spawn(async move {
            let Some(_guard) = SessionIndexGuard::try_enter(&prev.id) else {
                log::info!("[chat] 会话 {} 已有索引任务在飞，跳过", prev.id);
                return;
            };
            if let Err(e) = index_session_catchup(&store, &indexer, &dir_path, &prev.id, MAX_INDEX_ROUNDS).await {
                log::warn!("[chat] 索引上一个会话失败: {}", e);
            }
        });
    }

    // 3. 创建新会话（超出上限时自动删除最旧的，返回被删 ID）
    let (session, deleted_ids) = tokio::task::spawn_blocking(move || {
        crate::core::db::with_busy_retry(3, || store.create_session(&title, &session_type))
    })
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
/// 从源会话的指定消息序号处派生分支会话（P1-11）。
///
/// 复制源会话前 `message_seq` 条消息到新会话（含引用来源），新会话挂
/// `parent_id`/`branch_point`；分支点之后的消息不复制，用户从该点改写重发。
#[tauri::command]
pub async fn chat_fork(
    state: tauri::State<'_, AppState>,
    dir_path: String,
    session_id: String,
    message_seq: usize,
    title: Option<String>,
) -> Result<ChatSession, String> {
    let store = state.get_chat_store(&dir_path)?;
    tokio::task::spawn_blocking(move || {
        store.fork_session(&session_id, message_seq, &title.unwrap_or_else(|| format!("{} 的分支", session_id)))
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))?
}

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
    tokio::task::spawn_blocking(move || {
        crate::core::db::with_busy_retry(3, || store.delete_session(&id))
    })
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
    tokio::task::spawn_blocking(move || {
        crate::core::db::with_busy_retry(3, || store.rename_session(&id, &title))
    })
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
    tokio::task::spawn_blocking(move || {
        crate::core::db::with_busy_retry(3, || store.toggle_favorite(&id))
    })
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
        crate::core::db::with_busy_retry(3, || {
            store.save_message(&session_id, &role, &content, token_count, tool_calls.as_deref())
        })
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
    tokio::task::spawn_blocking(move || {
        crate::core::db::with_busy_retry(3, || store.clear_session_messages(&session_id))
    })
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
    tokio::task::spawn_blocking(move || {
        crate::core::db::with_busy_retry(3, || store.save_message_sources(&message_id, &sources))
    })
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

    // 去重：消息条数无变化则跳过（快速路径，不 spawn 任务）。
    // 注意：若此前已有任务在飞，这里跳过的是"启动新任务"；在飞任务内部的
    // 追平循环会补齐索引期间新到达的消息，不会漏索引。
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

    let sid = session_id.clone();
    let app_clone = app.clone();
    let indexer = indexer.clone();
    let dir_path = dir_path.clone();
    // 索引在会话级互斥内执行（与 chat_session_create 共用）：后到任务直接跳过，
    // 在飞任务内部的追平循环会补齐索引期间新到达的消息，不会漏索引。
    tokio::spawn(async move {
        // RAII 守卫：同一会话并发索引去重；任务结束（含 panic/早退）时自动释放
        let Some(_guard) = SessionIndexGuard::try_enter(&sid) else {
            log::debug!("[chat] 会话 {} 已有索引任务在飞，跳过", sid);
            return;
        };
        if let Err(e) = index_session_catchup(&store, &indexer, &dir_path, &sid, MAX_INDEX_ROUNDS).await {
            log::error!("[chat] 索引当前会话失败: {}", e);
            let _ = app_clone.emit("chat-index-error", format!("索引会话失败: {}", e));
        }
    });

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
    tokio::task::spawn_blocking(move || {
        crate::core::db::with_busy_retry(3, || store.set_last_session(&session_id, &mode))
    })
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
