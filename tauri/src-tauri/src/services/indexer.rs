use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use crate::db::bm25::Bm25Index;
use crate::db::lance::{DocumentChunk, LanceStore, SearchHit};
use crate::db::utils;
use crate::db::utils::IgnoreMatcher;
use crate::services::chat::{ChatMessage, ChatSession};

use super::config::ConfigStore;
use super::types::{IndexMeta, KbIndexResult, KbStatus};

const KB_SUPPORTED_EXTS: &[&str] = utils::KB_SUPPORTED_EXTS;
const BATCH_CHUNK_LIMIT: usize = 200;
const RRF_K: u32 = 30;

/// 知识库索引引擎（纯逻辑，不依赖 Tauri）
///
/// 职责：
/// - 全量索引（`index_all`）
/// - 单文件增量索引（`index_file`/`remove_file`）
/// - 对话会话索引（`index_chat_session`/`remove_chat_session`/`search_chat_sessions`）
/// - 搜索/状态查询
///
/// 设计要点：
/// - 使用本地 bge-small-zh-v1.5 模型（384 维），无 API 依赖
/// - 缓存 LanceStore / Bm25Index 实例（Arc 共享），避免增量操作反复重建连接
/// - 文档索引与对话索引分离（不同 table / 目录），互不污染
/// - `indexing_lock` 防止并发 index_all 调用导致数据损坏
/// - 增量操作实时更新元数据文件
pub struct Indexer {
    config_store: Arc<ConfigStore>,
    /// 文档 LanceStore 缓存（table_name = "vectors"）
    lance_cache: Mutex<Option<(String, Arc<LanceStore>)>>,
    /// 对话 LanceStore 缓存（table_name = "chat_vectors"）
    chat_lance_cache: Mutex<Option<(String, Arc<LanceStore>)>>,
    /// 文档 BM25 缓存（目录 = bm25）
    bm25_cache: Mutex<Option<(String, Arc<Bm25Index>)>>,
    /// 对话 BM25 缓存（目录 = chat_bm25）
    chat_bm25_cache: Mutex<Option<(String, Arc<Bm25Index>)>>,
    /// 全量索引互斥锁（防止并发 kb_index）
    indexing_lock: Mutex<()>,
}

impl Indexer {
    pub fn new(config_store: Arc<ConfigStore>) -> Self {
        Self {
            config_store,
            lance_cache: Mutex::new(None),
            chat_lance_cache: Mutex::new(None),
            bm25_cache: Mutex::new(None),
            chat_bm25_cache: Mutex::new(None),
            indexing_lock: Mutex::new(()),
        }
    }

    /// 获取或创建缓存的 LanceStore（Arc 共享，复用内部连接缓存）
    async fn get_lance_store(&self, dir_path: &str) -> Arc<LanceStore> {
        let mut cache = self.lance_cache.lock().await;
        if let Some((ref cached_dir, ref store)) = *cache {
            if cached_dir == dir_path {
                return Arc::clone(store);
            }
        }
        let data_dir = utils::get_data_dir(dir_path);
        let store = Arc::new(LanceStore::new(&data_dir, "vectors"));
        *cache = Some((dir_path.to_string(), Arc::clone(&store)));
        store
    }

    /// 获取或创建缓存的文档 Bm25Index（目录 = bm25）
    async fn get_bm25_index(&self, dir_path: &str) -> Result<Arc<Bm25Index>, String> {
        let mut cache = self.bm25_cache.lock().await;
        if let Some((ref cached_dir, ref store)) = *cache {
            if cached_dir == dir_path {
                return Ok(Arc::clone(store));
            }
        }
        let bm25_dir = utils::get_bm25_dir(dir_path);
        let index = if Path::new(&bm25_dir).exists() {
            Arc::new(Bm25Index::open(&bm25_dir)?)
        } else {
            Arc::new(Bm25Index::create(&bm25_dir)?)
        };
        *cache = Some((dir_path.to_string(), Arc::clone(&index)));
        Ok(index)
    }

    /// 获取或创建缓存的对话 LanceStore（table_name = "chat_vectors"）
    async fn get_chat_lance_store(&self, dir_path: &str) -> Arc<LanceStore> {
        let mut cache = self.chat_lance_cache.lock().await;
        if let Some((ref cached_dir, ref store)) = *cache {
            if cached_dir == dir_path {
                return Arc::clone(store);
            }
        }
        let data_dir = utils::get_data_dir(dir_path);
        let store = Arc::new(LanceStore::new(&data_dir, "chat_vectors"));
        *cache = Some((dir_path.to_string(), Arc::clone(&store)));
        store
    }

    /// 获取或创建缓存的对话 Bm25Index（目录 = chat_bm25）
    async fn get_chat_bm25_index(&self, dir_path: &str) -> Result<Arc<Bm25Index>, String> {
        let mut cache = self.chat_bm25_cache.lock().await;
        if let Some((ref cached_dir, ref store)) = *cache {
            if cached_dir == dir_path {
                return Ok(Arc::clone(store));
            }
        }
        let bm25_dir = utils::get_chat_bm25_dir(dir_path);
        let index = if Path::new(&bm25_dir).exists() {
            Arc::new(Bm25Index::open(&bm25_dir)?)
        } else {
            Arc::new(Bm25Index::create(&bm25_dir)?)
        };
        *cache = Some((dir_path.to_string(), Arc::clone(&index)));
        Ok(index)
    }

    /// 使文档缓存失效（clear 或 index_all 后调用）
    async fn invalidate_cache(&self) {
        *self.lance_cache.lock().await = None;
        *self.bm25_cache.lock().await = None;
    }

    /// ─── 全量索引（清空旧数据后重建）───

    pub async fn index_all(
        &self,
        dir_path: &str,
        progress: impl Fn(u8, &str) + Send + Sync,
    ) -> Result<KbIndexResult, String> {
        // 互斥锁：防止并发 index_all
        let _guard = self.indexing_lock.lock().await;

        let config = self.config_store.read();
        let base_dir = Path::new(dir_path);
        if !base_dir.exists() {
            return Err(format!("目录不存在: {}", dir_path));
        }

        // 清理旧索引数据 + 使缓存失效
        self.clear_inner(dir_path).await?;
        self.invalidate_cache().await;

        progress(0, "正在扫描目录...");
        let ignore = IgnoreMatcher::new(&config.dir_blacklist, &config.file_blacklist);
        let files = scan_directory(base_dir, &ignore)?;
        let total = files.len() as u32;
        if total == 0 {
            return Err("目录中没有可索引的文件".into());
        }
        progress(2, &format!("已发现 {} 个文件", total));

        // 预创建 LanceDB 表（本地模型固定 384 维）
        let store = self.get_lance_store(dir_path).await;
        store.create_table().await?;

        // 预创建 BM25 索引
        let bm25 = self.get_bm25_index(dir_path).await?;

        // 分批处理
        let mut batch_chunks: Vec<DocumentChunk> = Vec::with_capacity(BATCH_CHUNK_LIMIT);
        let mut file_count = 0u32;
        let mut total_chunks = 0u32;
        let mut total_vectors = 0u32;
        let mut batch_index = 0u32;

        for (i, file_path) in files.iter().enumerate() {
            let content = match read_file_content(file_path) {
                Some(c) if c.len() >= 10 => c,
                _ => continue,
            };

            let chunks = utils::split_text(&content, 1000, 200);
            if chunks.is_empty() {
                continue;
            }

            let rel_path = file_path
                .strip_prefix(base_dir)
                .unwrap_or(file_path)
                .to_string_lossy()
                .to_string();

            let doc_chunks = utils::build_document_chunks(&rel_path, &chunks);
            batch_chunks.extend(doc_chunks);
            file_count += 1;

            // 读取进度：0% → 20%（基于已扫描的文件比例）
            let read_pct = ((i + 1) * 20 / total.max(1) as usize) as u8;
            if batch_chunks.len() < BATCH_CHUNK_LIMIT && i + 1 < total as usize {
                progress(read_pct.min(19), &format!("读取文件 {}/{} (已缓存 {} 个文本块)", i + 1, total, batch_chunks.len()));
                continue;
            }

            // ── 批次已满或最后一批：处理 ──
            batch_index += 1;
            // Embedding + DB 写入进度：20% → 85%（基于已处理的文件比例）
            let embed_pct = 20 + ((i + 1) * 65 / total.max(1) as usize) as u8;
            progress(embed_pct.min(84), &format!("正在向量化第 {} 批 ({} 个文本块)", batch_index, batch_chunks.len()));

            let vectors = self.embed_batch(&batch_chunks).await?;

            store.add_chunks(&batch_chunks, &vectors).await?;
            bm25.add_documents(&batch_chunks)?;

            total_chunks += batch_chunks.len() as u32;
            total_vectors += vectors.len() as u32;
            batch_chunks.clear();

            progress(embed_pct.min(84), &format!("已处理 {}/{} 文件 (累计 {} 文本块, {} 向量)", i + 1, total, total_chunks, total_vectors));
        }

        if total_chunks == 0 {
            // 清理已创建的空表，避免留下不一致的空索引
            let _ = self.clear_inner(dir_path).await;
            return Err("未能从文件中提取有效内容".into());
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let meta = IndexMeta { file_count, chunk_count: total_chunks, vector_count: total_vectors, indexed_at: now };
        save_metadata(&utils::get_data_dir(dir_path), &meta);

        progress(100, "索引完成");
        Ok(KbIndexResult { file_count, chunk_count: total_chunks, vector_count: total_vectors, indexed_at: now })
    }

    /// ─── 单文件索引（增量）───

    pub async fn index_file(&self, dir_path: &str, rel_path: &str, abs_path: &str) -> Result<(), String> {
        let content = match read_file_content(Path::new(abs_path)) {
            Some(c) if c.len() >= 10 => c,
            _ => return Ok(()),
        };

        let chunks = utils::split_text(&content, 1000, 200);
        if chunks.is_empty() {
            return Ok(());
        }

        let doc_chunks = utils::build_document_chunks(rel_path, &chunks);
        let vectors = self.embed_batch(&doc_chunks).await?;

        // ── LanceDB：确保表存在，先删后写 ──
        let store = self.get_lance_store(dir_path).await;
        store.create_table().await?;
        let _ = store.delete_document(rel_path).await;
        store.add_chunks(&doc_chunks, &vectors).await.map_err(|e| {
            log::error!("[indexer] 写入 LanceDB 失败 ({}): {}", rel_path, e);
            e
        })?;

        // ── BM25：先删后写 ──
        if let Ok(bm25) = self.get_bm25_index(dir_path).await {
            let _ = bm25.delete_document(rel_path);
            bm25.add_documents(&doc_chunks).map_err(|e| {
                log::error!("[indexer] 写入 BM25 失败 ({}): {}", rel_path, e);
                e
            })?;
        }

        // 写入成功后更新元数据
        self.update_metadata_delta(dir_path, 0, doc_chunks.len() as i32, vectors.len() as i32).await;

        Ok(())
    }

    /// ─── 单文件删除（增量）───

    pub async fn remove_file(&self, dir_path: &str, rel_path: &str) -> Result<(), String> {
        let store = self.get_lance_store(dir_path).await;
        let mut deleted_chunks = 0u32;

        if store.open_table().await.is_ok() {
            // 先统计待删除的 chunk 数量
            deleted_chunks = self.count_document_chunks(&store, rel_path).await;
            if let Err(e) = store.delete_document(rel_path).await {
                log::error!("[indexer] 删除 LanceDB 文档失败 ({}): {}", rel_path, e);
            }
        }

        if let Ok(bm25) = self.get_bm25_index(dir_path).await {
            if let Err(e) = bm25.delete_document(rel_path) {
                log::error!("[indexer] 删除 BM25 文档失败 ({}): {}", rel_path, e);
            }
        }

        // 更新元数据（file_count = -1 表示文件被删除）
        if deleted_chunks > 0 {
            self.update_metadata_delta(dir_path, -1, 0, -(deleted_chunks as i32)).await;
        } else {
            // 即使没有 chunk 也要更新 file_count 和时间戳
            self.update_metadata_delta(dir_path, -1, 0, 0).await;
        }

        Ok(())
    }

    /// ─── 清除全部索引 ───

    pub async fn clear(&self, dir_path: &str) -> Result<(), String> {
        self.clear_inner(dir_path).await
    }

    async fn clear_inner(&self, dir_path: &str) -> Result<(), String> {
        let data_dir = utils::get_data_dir(dir_path);

        let store = self.get_lance_store(dir_path).await;
        if store.open_table().await.is_ok() {
            store.clear().await?;
        }

        // 使用缓存的 BM25 实例执行 clear（clear 内部会 invalidate reader）
        if let Ok(bm25) = self.get_bm25_index(dir_path).await {
            let _ = bm25.clear();
        }

        let meta_path = Path::new(&data_dir).join("index_meta.json");
        let _ = std::fs::remove_file(&meta_path);

        self.invalidate_cache().await;

        Ok(())
    }

    /// ─── 状态查询 ───

    pub async fn status(&self, dir_path: &str) -> Result<KbStatus, String> {
        let data_dir = utils::get_data_dir(dir_path);
        let store = self.get_lance_store(dir_path).await;
        let table_exists = store.open_table().await.is_ok();
        let meta = load_metadata(&data_dir);

        let (status, vector_count) = if table_exists {
            if let Some(ref m) = meta {
                ("indexed".into(), m.vector_count)
            } else {
                ("unknown".into(), 0)
            }
        } else {
            ("unknown".into(), 0)
        };

        Ok(KbStatus {
            file_count: meta.as_ref().map(|m| m.file_count).unwrap_or(0),
            chunk_count: meta.as_ref().map(|m| m.chunk_count).unwrap_or(0),
            vector_count,
            indexed_at: meta.as_ref().map(|m| m.indexed_at).unwrap_or(0),
            status,
        })
    }

    /// ─── 混合检索 ───

    pub async fn hybrid_search(
        &self,
        dir_path: &str,
        query_vector: &[f32],
        query: &str,
        top_k: u32,
    ) -> Result<Vec<SearchHit>, String> {
        let store = self.get_lance_store(dir_path).await;
        let bm25_dir = utils::get_bm25_dir(dir_path);

        let vec_k = (top_k * 2).max(10);
        let vec_hits = store.search_vectors(query_vector, vec_k).await.unwrap_or_default();

        let bm25_k = (top_k * 2).max(10);
        let bm25_hits = match Bm25Index::open(&bm25_dir) {
            Ok(idx) => idx.search(query, bm25_k).unwrap_or_default(),
            Err(_) => Vec::new(),
        };

        let fused = rrf_merge(&vec_hits, &bm25_hits, RRF_K);
        let result: Vec<SearchHit> = fused.into_iter().take(top_k as usize).collect();
        Ok(result)
    }

    // ─── 内部 Embedding（纯本地）───

    /// 对一组 DocumentChunk 批量 Embedding，返回向量列表。
    ///
    /// 调用本地 bge-small-zh-v1.5 模型（384 维），纯同步推理。
    async fn embed_batch(&self, chunks: &[DocumentChunk]) -> Result<Vec<Vec<f32>>, String> {
        log::info!("[indexer] embed_batch 开始，共 {} 个文本块", chunks.len());

        let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
        let all_vectors = tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
            utils::call_embedding(&refs)
        })
        .await
        .map_err(|e| {
            log::error!("[indexer] Embedding 任务执行失败: {}", e);
            format!("Embedding 任务执行失败: {}", e)
        })?
        .map_err(|e| {
            log::error!("[indexer] Embedding 失败: {}", e);
            format!("Embedding 失败: {}", e)
        })?;

        log::info!("[indexer] embed_batch 完成，共 {} 个向量", all_vectors.len());
        Ok(all_vectors)
    }

    // ─── 元数据增量更新 ───

    /// 增量更新元数据（file_delta: 新增文件数, chunk_delta: 新增块数, vector_delta 正=新增 负=删除）
    /// 总是更新 indexed_at 到当前时间，确保增量操作后状态查询返回最新时间戳
    async fn update_metadata_delta(&self, dir_path: &str, file_delta: i32, chunk_delta: i32, vector_delta: i32) {
        let data_dir = utils::get_data_dir(dir_path);
        let meta = load_metadata(&data_dir).unwrap_or(IndexMeta {
            file_count: 0,
            chunk_count: 0,
            vector_count: 0,
            indexed_at: 0,
        });

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let new_meta = IndexMeta {
            file_count: (meta.file_count as i32 + file_delta).max(0) as u32,
            chunk_count: (meta.chunk_count as i32 + chunk_delta).max(0) as u32,
            vector_count: (meta.vector_count as i32 + vector_delta).max(0) as u32,
            indexed_at: now,
        };
        save_metadata(&data_dir, &new_meta);
    }

    /// 统计某个 doc_name 下的 chunk 数量
    ///
    /// 使用 only_if 过流式计数，避免 limit 截断导致计数不准。
    async fn count_document_chunks(&self, store: &LanceStore, doc_name: &str) -> u32 {
        let table = match store.open_table().await {
            Ok(t) => t,
            Err(_) => return 0,
        };
        use lancedb::query::{ExecutableQuery, QueryBase};
        use futures::TryStreamExt;
        let escaped = doc_name.replace('\'', "''").replace('\\', "\\\\");
        let result = table
            .query()
            .only_if(&format!("doc_name = '{}'", escaped))
            .execute()
            .await;
        match result {
            Ok(stream) => match stream.try_collect::<Vec<_>>().await {
                Ok(batches) => batches.iter().map(|b| b.num_rows()).sum::<usize>() as u32,
                Err(_) => 0,
            },
            Err(_) => 0,
        }
    }

    // ─── 对话会话索引（chat_vectors / chat_bm25，与文档索引分离）───

    /// 索引一个会话的所有消息到对话向量库和 BM25 索引。
    ///
    /// - 单条消息作为一个 chunk
    /// - `doc_name` = `session.id`（便于按会话删除）
    /// - `chunk_index` = 消息在会话中的序号
    /// - `text` = `[role] content`（包含角色前缀，提升检索语义）
    ///
    /// 调用前应确保该会话已结束（用户新建了下一个会话）。
    pub async fn index_chat_session(
        &self,
        dir_path: &str,
        session: &ChatSession,
        messages: &[ChatMessage],
    ) -> Result<(), String> {
        if messages.is_empty() {
            return Ok(());
        }

        // 构建 DocumentChunk 列表（单条消息 = 一个 chunk）
        let chunks: Vec<DocumentChunk> = messages
            .iter()
            .enumerate()
            .map(|(i, msg)| DocumentChunk {
                id: msg.id.clone(),
                doc_name: session.id.clone(),
                chunk_index: i as u32,
                text: format!("[{}] {}", msg.role, msg.content),
            })
            .collect();

        // 生成 embedding（批量，1 次调用，放入 spawn_blocking 避免阻塞 Tokio）
        let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
        let vectors = tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
            utils::call_embedding(&refs)
        })
        .await
        .map_err(|e| format!("Embedding 任务执行失败: {}", e))??;

        // 写入 LanceDB（chat_vectors 表）：先删后写，保证幂等
        let store = self.get_chat_lance_store(dir_path).await;
        store.create_table().await?;
        let _ = store.delete_document(&session.id).await;
        store.add_chunks(&chunks, &vectors).await?;

        // 写入 BM25（chat_bm25 索引）：先删后写
        let bm25 = self.get_chat_bm25_index(dir_path).await?;
        let _ = bm25.delete_document(&session.id);
        bm25.add_documents(&chunks)?;

        log::info!(
            "[indexer] 对话会话 {} 已索引（{} 条消息）",
            session.id,
            messages.len()
        );
        Ok(())
    }

    /// 从对话索引中删除指定会话的所有消息
    pub async fn remove_chat_session(&self, dir_path: &str, session_id: &str) -> Result<(), String> {
        let store = self.get_chat_lance_store(dir_path).await;
        if store.open_table().await.is_ok() {
            if let Err(e) = store.delete_document(session_id).await {
                log::error!("[indexer] 删除对话向量失败 ({}): {}", session_id, e);
            }
        }

        if let Ok(bm25) = self.get_chat_bm25_index(dir_path).await {
            if let Err(e) = bm25.delete_document(session_id) {
                log::error!("[indexer] 删除对话 BM25 失败 ({}): {}", session_id, e);
            }
        }

        Ok(())
    }

    /// 混合检索对话：向量检索 + BM25 全文检索 + RRF 融合。
    ///
    /// 返回 `(session_id, score, matched_text)` 列表，调用方根据 session_id
    /// 去 SQLite 查会话元信息组装最终结果。
    ///
    /// 与文档搜索（`hybrid_search`）完全隔离，只查 `chat_vectors` / `chat_bm25`。
    pub async fn search_chat_sessions(
        &self,
        dir_path: &str,
        query: &str,
        top_k: u32,
    ) -> Result<Vec<(String, f32, String)>, String> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // 1. 生成查询向量（1 次 ONNX 推理，放入 spawn_blocking 避免阻塞 Tokio）
        let query_string = query.to_string();
        let query_embedding = tokio::task::spawn_blocking(move || utils::call_embedding(&[&query_string]))
            .await
            .map_err(|e| format!("Embedding 任务执行失败: {}", e))??;
        let query_vec = query_embedding
            .first()
            .ok_or_else(|| "查询向量为空".to_string())?;

        // 2. 向量检索（chat_vectors 表）
        let store = self.get_chat_lance_store(dir_path).await;
        let vec_k = (top_k * 2).max(10);
        let vec_hits = store.search_vectors(query_vec, vec_k).await.unwrap_or_default();

        // 3. BM25 检索（chat_bm25 索引）
        let bm25_k = (top_k * 2).max(10);
        let bm25_hits = match self.get_chat_bm25_index(dir_path).await {
            Ok(idx) => idx.search(query, bm25_k).unwrap_or_default(),
            Err(_) => Vec::new(),
        };

        // 4. RRF 融合
        let fused = rrf_merge(&vec_hits, &bm25_hits, RRF_K);

        // 5. 转换为 (session_id, score, matched_text)
        let results = fused
            .into_iter()
            .take(top_k as usize)
            .map(|hit| (hit.doc_name, hit.score, hit.text))
            .collect();

        Ok(results)
    }
}

// ─── 辅助函数 ───

/// 扫描目录，返回符合扩展名和过滤规则的绝对路径列表
fn scan_directory(base_dir: &Path, ignore: &IgnoreMatcher) -> Result<Vec<std::path::PathBuf>, String> {
    let mut files = Vec::new();
    let walker = walkdir::WalkDir::new(base_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            if e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                if name == ".mdgo" {
                    return false;
                }
                let rel_path = e.path().strip_prefix(base_dir).unwrap_or(e.path());
                let rel = rel_path.to_string_lossy().replace('\\', "/");
                return ignore.is_kb_dir_allowed(&name, &rel);
            }
            true
        });

    for entry in walker {
        let entry = entry.map_err(|e| format!("扫描目录失败: {}", e))?;
        if entry.file_type().is_file() {
            let file_name = entry.file_name().to_string_lossy().to_string();
            let rel_path = entry.path().strip_prefix(base_dir).unwrap_or(entry.path());
            let rel = rel_path.to_string_lossy().replace('\\', "/");
            if !ignore.is_kb_file_allowed(&file_name, &rel) {
                continue;
            }
            if let Some(ext) = entry.path().extension() {
                let ext = ext.to_string_lossy().to_lowercase();
                if KB_SUPPORTED_EXTS.contains(&ext.as_str()) {
                    files.push(entry.path().to_path_buf());
                }
            }
        }
    }
    Ok(files)
}

/// 读取文件内容，非 UTF-8 则跳过
fn read_file_content(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(c) => Some(c),
        Err(e) => {
            log::warn!(
                "跳过文件 {}: {}",
                path.display(),
                if e.kind() == std::io::ErrorKind::InvalidData {
                    "非 UTF-8 编码".to_string()
                } else {
                    e.to_string()
                }
            );
            None
        }
    }
}

fn save_metadata(data_dir: &str, meta: &IndexMeta) {
    let path = Path::new(data_dir).join("index_meta.json");
    if let Ok(json) = serde_json::to_string(meta) {
        let _ = std::fs::write(&path, &json);
    }
}

fn load_metadata(data_dir: &str) -> Option<IndexMeta> {
    let path = Path::new(data_dir).join("index_meta.json");
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// RRF（倒数排名融合）
fn rrf_merge(vec_hits: &[SearchHit], bm25_hits: &[SearchHit], k: u32) -> Vec<SearchHit> {
    use std::collections::HashMap;

    let mut score_map: HashMap<(String, u32), (f32, String)> = HashMap::new();

    for (rank, hit) in vec_hits.iter().enumerate() {
        let key = (hit.doc_name.clone(), hit.chunk_index);
        let rrf_score = 1.0 / (k as f32 + rank as f32);
        score_map
            .entry(key)
            .or_insert_with(|| (0.0, hit.text.clone()))
            .0 += rrf_score;
    }

    for (rank, hit) in bm25_hits.iter().enumerate() {
        let key = (hit.doc_name.clone(), hit.chunk_index);
        let rrf_score = 1.0 / (k as f32 + rank as f32);
        let entry = score_map
            .entry(key)
            .or_insert_with(|| (0.0, hit.text.clone()));
        entry.0 += rrf_score;
        if entry.1.len() < hit.text.len() {
            entry.1 = hit.text.clone();
        }
    }

    let mut results: Vec<SearchHit> = score_map
        .into_iter()
        .map(|((doc_name, chunk_index), (score, text))| SearchHit { text, doc_name, chunk_index, score: score.min(1.0) })
        .collect();

    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    results
}
