use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use crate::core::chat_types::{ChatMessage, ChatSession};
use crate::core::config::ConfigStore;
use crate::core::db::bm25::Bm25Index;
use crate::core::db::chunk_splitter::ChunkSplitterFactory;
use crate::core::db::lance::{DocumentChunk, LanceStore, SearchHit};
use crate::core::db::utils;
use crate::core::db::utils::IgnoreMatcher;
use crate::core::types::{FileTypeCount, IndexMeta, KbIndexResult, KbStatus};

const KB_SUPPORTED_EXTS: &[&str] = utils::KB_SUPPORTED_EXTS;
const BATCH_CHUNK_LIMIT: usize = 200;

/// 全局 ChunkSplitter 工厂（懒初始化，线程安全）
static CHUNK_SPLITTER_FACTORY: std::sync::OnceLock<ChunkSplitterFactory> = std::sync::OnceLock::new();
fn chunk_splitter_factory() -> &'static ChunkSplitterFactory {
    CHUNK_SPLITTER_FACTORY.get_or_init(ChunkSplitterFactory::new)
}

/// RRF 融合常数 K（值越大排名靠前的贡献越平滑，减少头部排名偏差）
///
/// 增大 K 值使 RRF 分数分布更均匀，避免向量检索的头部排名过度主导融合结果，
/// 让 BM25 关键词匹配和 Reranker 有更多机会影响最终排序。
const RRF_K: u32 = 60;
/// BM25 关键词匹配的 RRF 权重倍数。
///
/// 当 chunk 被 BM25 命中（存在关键词匹配）时，其 RRF 分数乘以该权重。
/// 值 > 1.0 使关键词精确匹配结果优先于纯语义相似结果。
/// 推荐值 2.0（提升关键词精确匹配的权重）。
const BM25_RRF_WEIGHT: f32 = 2.0;

/// 根据文件扩展名分类为 "Markdown" / "代码" / "数据" / "其他"
fn classify_ext(ext: &str) -> &'static str {
    match ext {
        "md" | "markdown" | "mdown" | "rst" => "Markdown",
        "csv" | "tsv" | "jsonl" | "parquet" | "arrow" | "feather" => "数据",
        _ if matches!(
            ext,
            "py" | "js" | "ts" | "rs" | "go" | "java" | "c" | "cpp" | "h"
            | "hpp" | "css" | "scss" | "html" | "json" | "yaml" | "yml"
            | "toml" | "xml" | "sql" | "sh" | "bash" | "zsh" | "fish"
            | "ps1" | "bat" | "rb" | "php" | "swift" | "kt" | "scala"
            | "dart" | "lua" | "r"
        ) => "代码",
        _ => "其他",
    }
}

/// 检查 parent_json 是否是 child_json 的**严格路径前缀**。
///
/// path_json 序列化为 JSON 字符串数组（如 `["A","B"]`），简单的字符串 `starts_with`
/// 会因 JSON 格式问题失效（`["A","B"].starts_with(["A"])` 返回 false）。
///
/// 本函数利用 JSON 数组特性：去掉父路径尾部 `]` 后，检查子路径是否以截断串开头
/// 且下一个字符是 `,`（表示父路径最后一个元素后有更多元素）。
fn is_path_prefix(parent_json: &str, child_json: &str) -> bool {
    if parent_json == child_json {
        return false;
    }
    // 父路径去掉尾部 `]` 及可能的空白
    let parent_trimmed = parent_json.trim_end_matches(']').trim_end();
    if parent_trimmed.len() >= parent_json.len() {
        return false;
    }
    if !child_json.starts_with(parent_trimmed) {
        return false;
    }
    // 下一个字符必须是 `,`（表示父路径后还有更多元素）
    child_json.as_bytes().get(parent_trimmed.len()) == Some(&b',')
}

/// 知识库索引引擎（纯逻辑，不依赖 Tauri）
///
/// 职责：
/// - 全量索引（`index_all`）
/// - 单文件增量索引（`index_file`/`remove_file`）
/// - 对话会话索引（`index_chat_session`/`remove_chat_session`/`search_chat_sessions`）
/// - 搜索/状态查询
///
/// 设计要点：
/// - 使用本地 bge-small-zh-v1.5 模型（维度由 config.json 决定），无 API 依赖
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

        // 预创建 LanceDB 表和 BM25 索引
        let store = self.get_lance_store(dir_path).await;
        store.create_table().await?;

        let bm25 = self.get_bm25_index(dir_path).await?;

        let mut batch_chunks: Vec<DocumentChunk> = Vec::with_capacity(BATCH_CHUNK_LIMIT);
        let mut file_count = 0u32;
        let mut total_chunks = 0u32;
        let mut total_vectors = 0u32;
        let mut type_counts: std::collections::HashMap<&'static str, u32> = std::collections::HashMap::new();

        let cfg = self.config_store.read();
        for (i, file_path) in files.iter().enumerate() {
            let content = match read_file_content(file_path) {
                Some(c) if c.len() >= 10 => c,
                _ => continue,
            };

            let rel_path = file_path
                .strip_prefix(base_dir)
                .unwrap_or(file_path)
                .to_string_lossy()
                .to_string();

            let ext = rel_path.rsplit('.').next().unwrap_or("txt");
            let ft = classify_ext(ext);
            *type_counts.entry(ft).or_insert(0) += 1;
            let splitter = chunk_splitter_factory().get_splitter(ext);
            let chunks = splitter.split(&content, cfg.chunk_size, cfg.chunk_overlap);
            if chunks.is_empty() {
                continue;
            }

            let doc_chunks = utils::build_document_chunks(&rel_path, &chunks);
            batch_chunks.extend(doc_chunks);
            file_count += 1;

            // 读取进度：0% → 20%（基于已扫描的文件比例）
            let read_pct = ((i + 1) * 20 / total.max(1) as usize) as u8;
            if batch_chunks.len() < BATCH_CHUNK_LIMIT && i + 1 < total as usize {
                progress(read_pct.min(19), &format!("读取文件 {}/{} (已缓存 {} 个文本块)", i + 1, total, batch_chunks.len()));
                continue;
            }

            // 进度 20%：模型加载（仅首次约需 30-60 秒，已预热则瞬间完成）
            progress(20, "正在加载向量模型...");

            // 向量化进度回调：嵌入过程中实时更新进度
            let total_chunks_pending = total_chunks + batch_chunks.len() as u32;
            let embed_progress = |done: usize, total_groups: usize, msg: &str| {
                // 向量化占比 20% → 80%
                let embed_pct = 20 + (done * 60 / total_groups.max(1)) as u8;
                progress(
                    embed_pct.min(80),
                    &format!("已完成 {} 文件 ({} 文本块) / {} 文件 - {}", file_count, total_chunks_pending, total, msg),
                );
            };

            let vectors = self.embed_batch(&batch_chunks, Some(&embed_progress)).await?;

            // 进度 80% → 85%：写入数据库
            progress(82, &format!("已完成 {} 文件 ({} 文本块) / {} 文件 - 写入数据库", file_count, total_chunks + batch_chunks.len() as u32, total));

            store.add_chunks(&batch_chunks, &vectors).await?;
            bm25.add_documents(&batch_chunks)?;

            total_chunks += batch_chunks.len() as u32;
            total_vectors += vectors.len() as u32;
            batch_chunks.clear();

            // 进度 85%：单批完成
            let done_pct = 20 + (file_count * 65 / total.max(1)) as u8;
            progress(
                done_pct.min(99),
                &format!("已完成 {} 文件 ({} 文本块) / {} 文件", file_count, total_chunks, total),
            );
        }

        if total_chunks == 0 {
            // 无有效内容时清理空索引表
            let _ = self.clear_inner(dir_path).await;
            progress(100, "索引完成（无有效内容）");
            return Err("未能从文件中提取有效内容".into());
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let total_type_count = type_counts.values().sum::<u32>().max(1);
        let type_distribution: Vec<FileTypeCount> = type_counts
            .into_iter()
            .map(|(file_type, count)| FileTypeCount {
                percentage: (count as f32 / total_type_count as f32 * 100.0 * 10.0).round() / 10.0,
                file_type: file_type.to_string(),
                count,
            })
            .collect();

        let meta = IndexMeta { file_count, chunk_count: total_chunks, vector_count: total_vectors, indexed_at: now, type_distribution };
        if let Err(e) = std::fs::write(
            &Path::new(&utils::get_data_dir(dir_path)).join("index_meta.json"),
            &serde_json::to_string(&meta).unwrap_or_default(),
        ) {
            log::error!("[indexer] 保存元数据失败: {}", e);
        }

        progress(100, "索引完成");
        Ok(KbIndexResult { file_count, chunk_count: total_chunks, vector_count: total_vectors, indexed_at: now })
    }

    /// ─── 单文件索引（增量）───

    pub async fn index_file(&self, dir_path: &str, rel_path: &str, abs_path: &str) -> Result<(), String> {
        let content = match read_file_content(Path::new(abs_path)) {
            Some(c) if c.len() >= 10 => c,
            _ => return Ok(()),
        };

        let ext = rel_path.rsplit('.').next().unwrap_or("txt");
        let splitter = chunk_splitter_factory().get_splitter(ext);
        let cfg = self.config_store.read();
        let chunks = splitter.split(&content, cfg.chunk_size, cfg.chunk_overlap);
        if chunks.is_empty() {
            return Ok(());
        }

        let doc_chunks = utils::build_document_chunks(rel_path, &chunks);
        let vectors = self.embed_batch(&doc_chunks, None).await?;

        // ── LanceDB：确保表存在，先删后写 ──
        let store = self.get_lance_store(dir_path).await;
        store.create_table().await?;

        // 统计旧 chunk 数，用于元数据差值计算（避免每次 index 后 chunk_count 累积增长）
        let old_chunks = self.count_document_chunks(&store, rel_path).await;
        if let Err(e) = store.delete_document(rel_path).await {
            log::warn!("[indexer] 删除旧 LanceDB 数据失败 ({}): {}，将继续写入新数据", rel_path, e);
        }
        store.add_chunks(&doc_chunks, &vectors).await.map_err(|e| {
            log::error!("[indexer] 写入 LanceDB 失败 ({}): {}", rel_path, e);
            e
        })?;

        // ── BM25：先删后写 ──
        if let Ok(bm25) = self.get_bm25_index(dir_path).await {
            if let Err(e) = bm25.delete_document(rel_path) {
                log::warn!("[indexer] 删除 BM25 旧数据失败 ({}): {}，将继续写入新数据", rel_path, e);
            }
            bm25.add_documents(&doc_chunks).map_err(|e| {
                log::error!("[indexer] 写入 BM25 失败 ({}): {}", rel_path, e);
                e
            })?;
        }

        // 写入成功后更新元数据（用新旧差值，避免重复 index 导致数据膨胀）
        let new_count = doc_chunks.len() as i32;
        let old_count = old_chunks as i32;
        let chunk_delta = new_count - old_count;
        let vector_delta = new_count - old_count; // 每个 chunk 生成一个向量
        let file_delta = if old_chunks == 0 { 1 } else { 0 };
        self.update_metadata_delta(dir_path, file_delta, chunk_delta, vector_delta).await;

        Ok(())
    }

    /// ─── 单文件删除（增量）───

    pub async fn remove_file(&self, dir_path: &str, rel_path: &str) -> Result<(), String> {
        let store = self.get_lance_store(dir_path).await;
        let mut deleted_chunks = 0u32;

        if store.open_table().await.is_ok() {
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
            self.update_metadata_delta(dir_path, -1, -(deleted_chunks as i32), -(deleted_chunks as i32)).await;
        } else {
            // 即使没有 chunk 也要更新 file_count 和时间戳
            self.update_metadata_delta(dir_path, -1, 0, 0).await;
        }

        Ok(())
    }

    // ─── 清除全部索引 ───

    pub async fn clear(&self, dir_path: &str) -> Result<(), String> {
        self.clear_inner(dir_path).await
    }

    async fn clear_inner(&self, dir_path: &str) -> Result<(), String> {
        let data_dir = utils::get_data_dir(dir_path);

        // 仅清理文档相关数据（"vectors" 表），不碰 "chat_vectors" 表
        let store = self.get_lance_store(dir_path).await;
        if store.open_table().await.is_ok() {
            store.drop_table_only().await?;
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

    // ─── 状态查询 ───

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

    // ─── 混合检索 ───

    pub async fn hybrid_search(
        &self,
        dir_path: &str,
        query_vector: &[f32],
        query: &str,
        top_k: u32,
    ) -> Result<Vec<SearchHit>, String> {
        let store = self.get_lance_store(dir_path).await;
        let bm25_dir = utils::get_bm25_dir(dir_path);

        // 候选池扩大到 top_k * 5，为 Reranker 提供更多候选
        let vec_k = (top_k * 5).max(15);
        let vec_hits = store.search_vectors(query_vector, vec_k).await.unwrap_or_default();

        let bm25_k = (top_k * 5).max(15);
        let bm25_hits = match Bm25Index::open(&bm25_dir) {
            Ok(idx) => idx.search(query, bm25_k).unwrap_or_default(),
            Err(_) => Vec::new(),
        };

        log::debug!("[indexer] hybrid_search query='{}' vec_k={} vec_hits={} bm25_k={} bm25_hits={}",
            query, vec_k, vec_hits.len(), bm25_k, bm25_hits.len());

        let mut fused = rrf_fuse(&vec_hits, &bm25_hits, RRF_K);
        log::debug!("[indexer] after RRF fuse: candidates={}", fused.len());

        // Reranker 重排序（降级安全：模型不存在时静默跳过）
        if !fused.is_empty() {
            let candidates: Vec<&str> = fused.iter().map(|h| h.text.as_str()).collect();
            let model_dir = utils::get_model_dir();
            match crate::core::embedding::rerank(query, &candidates, &model_dir, 32) {
                Ok(scores) => {
                    log::debug!("[indexer] reranker applied: scores_count={}", scores.len());
                    // 将 reranker 分数写回 SearchHit（保留原始 RRF 排序作为 fallback）
                    for (i, hit) in fused.iter_mut().enumerate() {
                        if i < scores.len() {
                            hit.score = scores[i];
                        }
                    }
                    // 按 reranker 分数降序排列
                    fused.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
                }
                Err(_) => {
                    log::debug!("[indexer] reranker unavailable, using RRF ranking");
                    // Reranker 不可用，直接使用 RRF 排序结果
                }
            }
        }

        // ── 文件名匹配加分 ──
        // 当查询词与 doc_name 匹配时额外加分，提升"按文件名搜索"的召回准确率
        let query_lower = query.to_lowercase();
        let query_tokens: Vec<&str> = query_lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() >= 2)
            .collect();
        if !query_tokens.is_empty() {
            for hit in fused.iter_mut() {
                let name_lower = hit.doc_name.to_lowercase();
                let query_matches = query_tokens
                    .iter()
                    .filter(|t| name_lower.contains(*t))
                    .count();
                if query_matches > 0 {
                    // 每匹配一个关键词加 0.15 分，文件名精准匹配大加分
                    let boost = 0.15 * query_matches as f32;
                    hit.score = (hit.score + boost).min(1.0);
                }
            }
            // 加分后重新排序
            fused.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        }

        // ── OPML 层级去重 ──
        // 对于同一文档中具有路径前缀关系的 chunk（如父节点与子节点），保留最深节点。
        // 使用掩码方式：如果 A 的路径是 B 的路径的严格前缀，则标记 A 为冗余。
        // 注意：必须处理子节点分数高于父节点的情况（此时父节点是前缀，应被移除）。
        if fused.len() > 1 {
            let mut groups: std::collections::HashMap<String, Vec<SearchHit>> = std::collections::HashMap::new();
            for hit in fused {
                groups.entry(hit.doc_name.clone()).or_default().push(hit);
            }
            let mut deduped: Vec<SearchHit> = Vec::new();
            for (_, hits) in groups {
                let n = hits.len();
                if n <= 1 {
                    deduped.extend(hits);
                    continue;
                }
                let mut keep_mask = vec![true; n];
                for i in 0..n {
                    if !keep_mask[i] { continue; }
                    let hp = hits[i].path_json.as_deref();
                    let hp = match hp {
                        Some(p) => p,
                        None => continue, // 无路径信息的不参与去重
                    };
                    for j in 0..n {
                        if i == j || !keep_mask[j] { continue; }
                        let op = match hits[j].path_json.as_deref() {
                            Some(p) => p,
                            None => continue,
                        };
                        // 如果 i 的路径是 j 路径的严格前缀（使用 JSON 数组格式感知比较）
                        // → i 是父节点，冗余
                        if is_path_prefix(hp, op) {
                            keep_mask[i] = false;
                            break;
                        }
                    }
                }
                for (i, hit) in hits.into_iter().enumerate() {
                    if keep_mask[i] {
                        deduped.push(hit);
                    }
                }
            }
            // 去重后重新按 score 排序
            deduped.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
            fused = deduped;
        }

        let result: Vec<SearchHit> = fused.into_iter().take(top_k as usize).collect();
        Ok(result)
    }

    // ─── 内部 Embedding（纯本地）───

    /// 对一组 DocumentChunk 批量 Embedding，返回向量列表。
    ///
    /// 调用本地 bge-small-zh-v1.5 模型（维度由 config.json 决定），纯同步推理。
    /// progress 回调：(已完成组数, 总组数, "状态消息")
    async fn embed_batch(
        &self,
        chunks: &[DocumentChunk],
        progress: Option<&(dyn Fn(usize, usize, &str) + Send + Sync)>,
    ) -> Result<Vec<Vec<f32>>, String> {
        use tokio::sync::mpsc;

        log::debug!("[indexer] embed_batch 开始，共 {} 个文本块", chunks.len());

        let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<(usize, usize, String)>();

        // 启动阻塞任务进行嵌入
        let mut handle = tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
            let pg = |done: usize, total: usize, msg: &str| {
                let _ = progress_tx.send((done, total, msg.to_string()));
            };
            utils::call_embedding(&refs, Some(&pg))
        });

        // 轮询 channel，实时调用 progress 回调
        let mut result: Option<Result<Vec<Vec<f32>>, String>> = None;
        while result.is_none() {
            tokio::select! {
                Some((done, total, msg)) = progress_rx.recv() => {
                    if let Some(p) = progress.as_ref() {
                        p(done, total, &msg);
                    }
                }
                joined = &mut handle => {
                    result = Some(match joined {
                        Ok(Ok(v)) => Ok(v),
                        Ok(Err(e)) => Err(e),
                        Err(e) => Err(format!("Embedding 任务执行失败: {}", e)),
                    });
                }
            }
        }

        let all_vectors = result.unwrap()?;
        log::debug!("[indexer] embed_batch 完成，共 {} 个向量", all_vectors.len());
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
            type_distribution: vec![],
        });

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let new_meta = IndexMeta {
            file_count: (meta.file_count as i32 + file_delta).max(0) as u32,
            chunk_count: (meta.chunk_count as i32 + chunk_delta).max(0) as u32,
            vector_count: (meta.vector_count as i32 + vector_delta).max(0) as u32,
            indexed_at: now,
            type_distribution: meta.type_distribution,
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
                path_depth: None,
                path_json: None,
            })
            .collect();

        // 生成 embedding（批量，1 次调用，放入 spawn_blocking 避免阻塞 Tokio）
        let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
        let vectors = tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
            utils::call_embedding(&refs, None)
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
        let query_embedding = tokio::task::spawn_blocking(move || utils::call_embedding(&[&query_string], None))
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

        // 4. RRF 融合+实际分数
        let fused = rrf_fuse(&vec_hits, &bm25_hits, RRF_K);

        // 5. 转换为 (session_id, score, matched_text)
        let results = fused
            .into_iter()
            .take(top_k as usize)
            .map(|hit| (hit.doc_name, hit.score, hit.text))
            .collect();

        Ok(results)
    }

    // ─── Watcher 启动同步 ───

    /// 文件监听启动时执行一致性检查：对比当前文件修改时间与上次索引时间戳，
    /// 自动索引新增或修改过的文件，确保索引与文件系统一致。
    ///
    /// 返回同步的文件数量。
    pub async fn sync_on_start(&self, dir_path: &str) -> Result<u32, String> {
        let data_dir = utils::get_data_dir(dir_path);
        let meta = load_metadata(&data_dir);
        let indexed_at = meta.as_ref().map(|m| m.indexed_at).unwrap_or(0);

        if indexed_at == 0 {
            log::info!("[indexer] sync_on_start: 无索引记录，跳过启动同步（由 index_all 全量处理）");
            return Ok(0);
        }

        let base_dir = Path::new(dir_path);
        if !base_dir.exists() {
            return Err(format!("目录不存在: {}", dir_path));
        }

        let config = self.config_store.read();
        let ignore = IgnoreMatcher::new(&config.dir_blacklist, &config.file_blacklist);
        let files = scan_directory(base_dir, &ignore)?;

        let indexed_time = UNIX_EPOCH + std::time::Duration::from_millis(indexed_at);
        let mut synced_count = 0u32;

        for file_path in &files {
            let metadata = match std::fs::metadata(file_path) {
                Ok(m) => m,
                Err(_) => continue,
            };

            let modified = match metadata.modified() {
                Ok(t) => t,
                Err(_) => continue,
            };

            // 给予 1 秒缓冲区（避免文件系统时间精度导致的误判）
            if modified <= indexed_time + std::time::Duration::from_secs(1) {
                continue;
            }

            let rel_path = file_path
                .strip_prefix(base_dir)
                .unwrap_or(file_path)
                .to_string_lossy()
                .to_string();

            log::info!("[indexer] 启动同步: 索引修改文件 {}", rel_path);
            if let Err(e) = self.index_file(dir_path, &rel_path, &file_path.to_string_lossy()).await {
                log::warn!("[indexer] 启动同步: 文件 {} 索引失败: {}", rel_path, e);
                continue;
            }
            synced_count += 1;
        }

        log::info!("[indexer] 启动同步完成: 共同步 {} 个文件", synced_count);
        Ok(synced_count)
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

/// 读取文件内容，非 UTF-8 则跳过（PDF 文件使用 pdf-extract 提取文本）
fn read_file_content(path: &Path) -> Option<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    #[cfg(feature = "pdf-extract")]
    if ext == "pdf" {
        return match pdf_extract::extract_text(path) {
            Ok(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    log::warn!("跳过 PDF 文件 {}: 未提取到文本内容", path.display());
                    None
                } else {
                    Some(trimmed.to_string())
                }
            }
            Err(e) => {
                log::warn!("跳过 PDF 文件 {}: 提取文本失败: {}", path.display(), e);
                None
            }
        };
    }

    // 非 PDF 文件（或 pdf-extract 未启用）：读取 UTF-8 文本
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
        if let Err(e) = std::fs::write(&path, &json) {
            log::error!("[indexer] 保存元数据失败 ({}): {}", path.display(), e);
        }
    }
}

fn load_metadata(data_dir: &str) -> Option<IndexMeta> {
    let path = Path::new(data_dir).join("index_meta.json");
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// RRF 融合 + 实际分数报告
///
/// 排序使用 RRF（倒数排名融合），保留对双系统共识信号的敏感度；
/// `score` 字段报告向量/BM25 的实际相似度最大值（归一化到 [0,1]），
/// 确保前端阈值过滤（如 0.3）能正常工作。
///
/// 相比纯 RRF（分数 < 0.1），本方案既保留了排序质量，又提供了
/// 语义上有意义的分数。
fn rrf_fuse(vec_hits: &[SearchHit], bm25_hits: &[SearchHit], k: u32) -> Vec<SearchHit> {
    use std::collections::HashMap;

    #[derive(Default)]
    struct Entry {
        rrf_score: f32,
        sim_score: f32,
        score_vec: f32,
        score_bm25: f32,
        text: String,
        path_json: Option<String>,
    }

    let mut score_map: HashMap<(String, u32), Entry> = HashMap::new();

    // ── 遍历向量结果集 ──
    for (rank, hit) in vec_hits.iter().enumerate() {
        let key = (hit.doc_name.clone(), hit.chunk_index);
        let entry = score_map.entry(key).or_default();
        entry.rrf_score += 1.0 / (k as f32 + rank as f32);
        if hit.score > entry.sim_score {
            entry.sim_score = hit.score;
        }
        if hit.score_vec > entry.score_vec {
            entry.score_vec = hit.score_vec;
        }
        // 向量结果集的 text 作为基准
        entry.text = hit.text.clone();
        // 保存 OPML 层级路径（BM25 结果无此字段，优先使用向量结果的 path_json）
        if entry.path_json.is_none() {
            entry.path_json = hit.path_json.clone();
        }
    }

    // ── 遍历 BM25 结果集（关键词匹配，带权重 BM25_RRF_WEIGHT）──
    for (rank, hit) in bm25_hits.iter().enumerate() {
        let key = (hit.doc_name.clone(), hit.chunk_index);
        let entry = score_map.entry(key).or_default();
        // BM25 匹配的 RRF 分数乘以权重，使关键词精确匹配优先
        entry.rrf_score += BM25_RRF_WEIGHT / (k as f32 + rank as f32);
        if hit.score > entry.sim_score {
            entry.sim_score = hit.score;
        }
        if hit.score_bm25 > entry.score_bm25 {
            entry.score_bm25 = hit.score_bm25;
        }
        // 仅当 vec 未覆盖此 key 时才用 BM25 的 text
        if entry.text.is_empty() {
            entry.text = hit.text.clone();
        }
    }

    // ── 按 RRF 排序，报告实际相似度 ──
    let mut entries: Vec<_> = score_map.drain().collect();
    entries.sort_by(|a, b| b.1.rrf_score.partial_cmp(&a.1.rrf_score).unwrap_or(std::cmp::Ordering::Equal));

    entries
        .into_iter()
        .map(|((doc_name, chunk_index), entry)| SearchHit {
            text: entry.text,
            doc_name,
            chunk_index,
            score: entry.sim_score.min(1.0).max(0.0),
            score_vec: entry.score_vec.min(1.0).max(0.0),
            score_bm25: entry.score_bm25.min(1.0).max(0.0),
            path_json: entry.path_json,
        })
        .collect()
}