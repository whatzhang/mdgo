use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use crate::core::chat_types::ChatMessage;
use crate::core::config::ConfigStore;
use crate::core::db::bm25::Bm25Index;
use crate::core::db::lance::{DocumentChunk, LanceStore, SearchHit};
use crate::core::db::utils;
use crate::core::db::utils::IgnoreMatcher;
use crate::core::pipeline;
use crate::core::search::query_plan::{CODE_EXTENSIONS, QueryPlanner, RetrievalIntent, RuleQueryPlanner};
use crate::core::search::rerank::{LocalBgeReranker, Reranker};
use crate::core::search::rrf::{rrf_fuse, RrfConfig};
use crate::core::types::{FileTypeCount, IndexMeta, KbIndexResult, KbStatus};

const KB_SUPPORTED_EXTS: &[&str] = utils::KB_SUPPORTED_EXTS;

/// 动态批次上限：按机器内存调整（64/128/256），低内存机器用小批次避免峰值内存过高。
/// 仅执行一次探测，进程生命周期内保持不变。
fn batch_chunk_limit() -> usize {
    static BATCH_LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BATCH_LIMIT.get_or_init(|| {
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let total_mb = sys.total_memory() / (1024 * 1024);
        if total_mb >= 16 * 1024 {
            256
        } else if total_mb >= 8 * 1024 {
            128
        } else {
            64
        }
    })
}

/// Watcher 批量索引单批文件数上限：防止整目录拷贝/解压时一次性载入海量 chunk。
const MAX_FILES_PER_BATCH: usize = 100;

/// 规范化路径字符串：统一 `/` 分隔符、去除 Windows verbatim（`\\?\`）前缀、
/// Windows 下大小写不敏感（转小写）、去除尾部 `/`。
fn norm_path(p: &str) -> String {
    let mut s = p.replace('\\', "/");
    s = s.trim_start_matches("//?/").to_string();
    #[cfg(windows)]
    {
        s = s.to_lowercase();
    }
    s.trim_end_matches('/').to_string()
}

/// 判断 `child` 是否位于 `base` 之内（或等于 base），规范化比较。
///
/// 处理 Windows 下 canonicalize 返回的 `\\?\` verbatim 前缀与用户传入普通路径的差异，
/// 以及大小写不敏感的文件系统。
fn path_is_within(base: &str, child: &str) -> bool {
    let b = norm_path(base);
    let c = norm_path(child);
    c == b || c.starts_with(&format!("{}/", b))
}

/// 计算 `child` 相对 `base` 的相对路径（`/` 分隔），不在 base 内则返回 None。
fn relative_to(base: &str, child: &str) -> Option<String> {
    let b = norm_path(base);
    let c = norm_path(child);
    c.strip_prefix(&b)
        .map(|r| r.trim_start_matches('/').to_string())
}

/// 判断是否为大纲类文档（OPML/FreeMind）。
///
/// 层级去重仅适用于这类「父节点聚合子节点文本」的格式；
/// Markdown 的语义分块内容互斥（父子节正文不重复），
/// 若对其做路径前缀去重会误删合法父节 chunk，因此必须按文件类型守卫。
fn is_outline_doc(doc_name: &str) -> bool {
    let lower = doc_name.to_lowercase();
    lower.ends_with(".opml") || lower.ends_with(".mm")
}

/// 对检索结果做文件扩展名过滤（元数据过滤的内存实现）。
///
/// 候选池规模为 top_k*10 级别，内存过滤成本可忽略，且不依赖
/// LanceDB/Tantivy 的过滤器语法，避免版本兼容问题。
fn filter_hits_by_ext(hits: Vec<SearchHit>, exts: &[&str]) -> Vec<SearchHit> {
    if exts.is_empty() {
        return hits;
    }
    // 预构建小写后缀（如 ".rs"），避免闭包内对每个命中×每个扩展名重复分配
    let suffixes: Vec<String> = exts
        .iter()
        .map(|e| format!(".{}", e.to_lowercase()))
        .collect();
    hits.into_iter()
        .filter(|h| {
            let name = h.doc_name.to_lowercase();
            suffixes.iter().any(|s| name.ends_with(s))
        })
        .collect()
}

/// 构建扩展名白名单的 SQL 过滤条件（LanceDB `only_if` 预过滤，Filter 前置）。
///
/// 例：`["rs", "py"]` → `(LOWER(doc_name) LIKE '%.rs' OR LOWER(doc_name) LIKE '%.py')`
/// 与 [`filter_hits_by_ext`] 保持同一份白名单语义；大小写不敏感（LOWER 归一）。
fn ext_filter_sql(exts: &[&str]) -> String {
    let clauses: Vec<String> = exts
        .iter()
        .map(|e| format!("LOWER(doc_name) LIKE '%.{}'", e.to_lowercase()))
        .collect();
    format!("({})", clauses.join(" OR "))
}

/// 计算向量/BM25 融合权重 α（0~1，越高越偏向语义向量）。
///
/// 在配置的基准权重上按**查询意图**微调（Dynamic Alpha）：
/// - 代码查询：标识符/符号适合 BM25 精确匹配 → 压低向量权重（BM25 偏重）
/// - 文档/大纲查询：概念语义为主 → 略提高向量权重
///
/// 再按查询长度微调：短查询依赖关键词精确匹配 → 降低向量权重；
/// 长查询语义丰富 → 略提高向量权重。
///
/// 对 CJK 中文做了显式感知：中文按字符数而非"词"计数（中文无空格分词），
/// 避免整句中文被计为 1 个 token 导致所有中文查询都落入"短查询"分支。
fn compute_alpha(query: &str, base_alpha: f32, intent: RetrievalIntent) -> f32 {
    let intent_delta = match intent {
        // 代码查询偏 BM25：符号/标识符匹配更可靠（配合符号注入与 Field Boost）
        RetrievalIntent::Code => -0.2,
        // 概念/文档查询偏语义向量
        RetrievalIntent::Document => 0.1,
        RetrievalIntent::Outline => 0.05,
        RetrievalIntent::General => 0.0,
    };
    let base = (base_alpha + intent_delta).clamp(0.3, 0.95);

    let ascii_words = query
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty() && t.chars().any(|c| c.is_ascii()))
        .count();
    let cjk_chars = query
        .chars()
        .filter(|c| !c.is_ascii() && c.is_alphabetic())
        .count();
    // 短查询：≤2 个 ASCII 词且中文 ≤6 字；长查询：≥5 个 ASCII 词或中文 ≥14 字
    let is_short = ascii_words <= 2 && cjk_chars <= 6;
    let is_long = ascii_words >= 5 || cjk_chars >= 14;
    if is_short {
        (base - 0.2).clamp(0.2, 1.0)
    } else if is_long {
        (base + 0.1).min(0.95)
    } else {
        base
    }
}

/// 根据文件扩展名分类为 "Markdown" / "代码" / "数据" / "其他"
///
/// 代码扩展名与 `CODE_EXTENSIONS` 保持单一来源（供意图过滤共用）。
fn classify_ext(ext: &str) -> &'static str {
    match ext {
        "md" | "markdown" | "mdown" | "rst" => "Markdown",
        "csv" | "tsv" | "jsonl" | "parquet" | "arrow" | "feather" => "数据",
        _ if CODE_EXTENSIONS.contains(&ext) => "代码",
        _ => "其他",
    }
}

/// 检查 parent_json 是否是 child_json 的**严格路径前缀**。
///
/// path_json 序列化为 JSON 字符串数组（如 `["A","B"]`），使用 JSON 反序列化
/// 确保正确性，避免因元素内包含逗号导致的字符串截断误判。
fn is_path_prefix(parent_json: &str, child_json: &str) -> bool {
    if parent_json == child_json {
        return false;
    }
    let parent: Vec<String> = match serde_json::from_str(parent_json) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let child: Vec<String> = match serde_json::from_str(child_json) {
        Ok(v) => v,
        Err(_) => return false,
    };
    if parent.len() >= child.len() {
        return false;
    }
    parent.iter().zip(child.iter()).all(|(p, c)| p == c)
}

/// 比较已解析的路径数组：`parent` 是否为 `child` 的**严格前缀**。
///
/// 与 [`is_path_prefix`] 语义一致，但接收已反序列化的数组，
/// 供批量去重在预解析后使用（避免 O(n²) 内重复 JSON 解析）。
fn is_path_prefix_vec(parent: &[String], child: &[String]) -> bool {
    parent.len() < child.len() && parent.iter().zip(child.iter()).all(|(p, c)| p == c)
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
    /// 全量索引互斥锁（防止并发 kb_index）
    indexing_lock: Mutex<()>,
    /// 全量索引进行中标记（用于 watcher 路径检查，避免元数据竞态）
    reindex_in_progress: std::sync::atomic::AtomicBool,
}

impl Indexer {
    pub fn new(config_store: Arc<ConfigStore>) -> Self {
        Self {
            config_store,
            lance_cache: Mutex::new(None),
            chat_lance_cache: Mutex::new(None),
            bm25_cache: Mutex::new(None),
            indexing_lock: Mutex::new(()),
            reindex_in_progress: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 检测是否有全量索引正在执行（watcher 路径跳过 index_file 用）
    pub fn is_reindex_in_progress(&self) -> bool {
        self.reindex_in_progress
            .load(std::sync::atomic::Ordering::Relaxed)
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
        self.reindex_in_progress
            .store(true, std::sync::atomic::Ordering::Release);

        let config = self.config_store.read();
        let base_dir = Path::new(dir_path);
        if !base_dir.exists() {
            self.reindex_in_progress
                .store(false, std::sync::atomic::Ordering::Release);
            return Err(format!("目录不存在: {}", dir_path));
        }

        // ── 单调递增进度辅助（AtomicU8 支持跨线程 &self 内部可变性） ──
        let current_pct = std::sync::atomic::AtomicU8::new(0);
        let set_progress = |pct: u32, msg: String| {
            let pct_u8 = pct.min(100) as u8;
            if pct_u8 > current_pct.load(std::sync::atomic::Ordering::Relaxed) {
                current_pct.store(pct_u8, std::sync::atomic::Ordering::Relaxed);
                progress(pct_u8, &msg);
            }
        };
        let msg = |pct: u32, msg: &str| set_progress(pct, msg.to_string());

        // 清理旧索引数据 + 使缓存失效
        self.clear_inner(dir_path).await?;
        self.invalidate_cache().await;

        msg(0, "正在扫描目录...");
        let ignore = IgnoreMatcher::new(&config.dir_blacklist, &config.file_blacklist);
        let files = scan_directory(base_dir, &ignore)?;
        let total = files.len() as u32;
        if total == 0 {
            self.reindex_in_progress
                .store(false, std::sync::atomic::Ordering::Release);
            return Err("目录中没有可索引的文件".into());
        }
        msg(2, &format!("已发现 {} 个文件", total));

        // 预创建 LanceDB 表和 BM25 索引
        let store = self.get_lance_store(dir_path).await;
        store.create_table().await?;
        let bm25 = self.get_bm25_index(dir_path).await?;
        msg(5, "索引表就绪");

        // ── 核心文件处理循环（document_stage → chunk_stage → embedding_stage → index_stage）──
        let batch_limit = batch_chunk_limit();
        let mut batch_chunks: Vec<DocumentChunk> = Vec::with_capacity(batch_limit);
        let mut file_count = 0u32;
        let mut batch_file_count = 0u32;
        let mut total_chunks = 0u32;
        let mut total_vectors = 0u32;
        let mut type_counts: std::collections::HashMap<&'static str, u32> = std::collections::HashMap::new();

        let cfg = self.config_store.read();
        let html_matcher = html_render_matcher(dir_path);
        for (i, file_path) in files.iter().enumerate() {
            let content = match pipeline::read_document(file_path) {
                Some(c) if c.len() >= 10 => c,
                _ => continue,
            };

            let rel_path = file_path
                .strip_prefix(base_dir)
                .unwrap_or(file_path)
                .to_string_lossy()
                .to_string()
                .replace('\\', "/");

            let ext = rel_path.rsplit('.').next().unwrap_or("txt");
            let ft = classify_ext(ext);
            *type_counts.entry(ft).or_insert(0) += 1;
            let doc_chunks = pipeline::chunk_document(&rel_path, &content, cfg.chunk_size, cfg.chunk_overlap, html_matcher.as_ref());
            if doc_chunks.is_empty() {
                continue;
            }

            batch_chunks.extend(doc_chunks);
            file_count += 1;
            batch_file_count += 1;

            // 以文件数为基准计算线性进度 5% → 95%
            let base_pct = 5 + (file_count * 90 / total.max(1));
            msg(
                base_pct.min(94),
                &format!("读取文件 {}/{} (已缓存 {} 个文本块)", file_count, total, batch_chunks.len()),
            );

            // 分批触发 Embedding
            if batch_chunks.len() < batch_limit && i + 1 < total as usize {
                continue;
            }

            // Embedding + 写入
            let embed_start_file = file_count - batch_file_count + 1;
            let embed_base = 5 + ((embed_start_file - 1) * 90 / total.max(1));
            let embed_total_pct = (batch_file_count * 90 / total.max(1)).max(1); // 该批次占的百分点

            let total_chunks_pending = total_chunks + batch_chunks.len() as u32;
            let embed_progress = |done: usize, total_groups: usize, _msg: &str| {
                let pct = embed_base + (done as u32 * embed_total_pct / total_groups.max(1) as u32);
                set_progress(
                    pct.min(94),
                    format!("已完成 {}/{} 文件 ({} 文本块) - 向量化中", file_count, total, total_chunks_pending),
                );
            };

            let vectors = pipeline::embed_chunks(&batch_chunks, Some(&embed_progress)).await?;

            set_progress(
                embed_base + embed_total_pct,
                format!("已完成 {}/{} 文件 ({} 文本块) - 写入数据库", file_count, total, total_chunks + batch_chunks.len() as u32),
            );

            pipeline::write_chunks(&store, &bm25, &batch_chunks, &vectors).await?;

            total_chunks += batch_chunks.len() as u32;
            total_vectors += vectors.len() as u32;
            drop(vectors);
            batch_chunks.clear();
            batch_chunks.shrink_to_fit();
            batch_file_count = 0;

            set_progress(
                embed_base + embed_total_pct,
                format!("已完成 {}/{} 文件 ({} 文本块)", file_count, total, total_chunks),
            );
        }

        if total_chunks == 0 {
            let _ = self.clear_inner(dir_path).await;
            msg(100, "索引完成（无有效内容）");
            return Err("未能从文件中提取有效内容".into());
        }

        // 全部数据写入完成后创建 IVF-PQ 向量索引（消除检索时的全表扫描；失败仅告警）
        if let Err(e) = store.ensure_vector_index().await {
            log::warn!("[indexer] 创建向量索引失败（检索将退化为全表扫描）: {}", e);
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

        set_progress(100, "索引完成".to_string());
        self.reindex_in_progress
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(KbIndexResult { file_count, chunk_count: total_chunks, vector_count: total_vectors, indexed_at: now })
    }

    /// ─── 单文件索引（增量）───

    pub async fn index_file(&self, dir_path: &str, rel_path: &str, abs_path: &str) -> Result<(), String> {
        // document_stage → chunk_stage → embedding_stage → index_stage
        let content = match pipeline::read_document(Path::new(abs_path)) {
            Some(c) if c.len() >= 10 => c,
            _ => return Ok(()),
        };
        let html_matcher = html_render_matcher(dir_path);
        let cfg = self.config_store.read();
        let doc_chunks = pipeline::chunk_document(rel_path, &content, cfg.chunk_size, cfg.chunk_overlap, html_matcher.as_ref());
        drop(cfg);
        if doc_chunks.is_empty() {
            return Ok(());
        }

        let vectors = pipeline::embed_chunks(&doc_chunks, None).await?;

        // ── LanceDB：确保表存在，先删后写（统一入口 replace_document_chunks，A1）──
        let store = self.get_lance_store(dir_path).await;
        store.create_table().await?;
        let bm25 = self.get_bm25_index(dir_path).await.ok();

        // 统计旧 chunk 数，用于元数据差值计算（避免每次 index 后 chunk_count 累积增长）
        let old_chunks = self
            .replace_document_chunks(&store, bm25.as_deref(), rel_path, &doc_chunks, &vectors)
            .await?;

        let new_count = doc_chunks.len() as i32;
        let old_count = old_chunks as i32;
        let chunk_delta = new_count - old_count;
        let vector_delta = new_count - old_count; // 每个 chunk 生成一个向量
        let file_delta = if old_chunks == 0 { 1 } else { 0 };
        log::info!("[indexer] 更新元数据,new_count: {}, old_count: {}, file_delta: {}, chunk_delta: {}, vector_delta: {}", new_count, old_count, file_delta, chunk_delta, vector_delta);

        self.update_metadata_delta(dir_path, file_delta, chunk_delta, vector_delta).await;
        Ok(())
    }

    /// ─── 批量文件索引（Watcher 防抖合并后调用）───
    ///
    /// 与 `index_file` 单文件独立 Embedding 不同，本方法将同一批待处理文件
    /// 统一做 `chunk_stage` + 单次 `embedding_stage`（写放大从"每文件一次推理"
    /// 降为"每批一次推理"），再逐文件 先删后写（LanceDB + BM25）。
    ///
    /// 内存治理（三级 batch，防止大目录拷贝/解压时一次性载入海量 chunk）：
    /// 1. 文件级：每批至多 [MAX_FILES_PER_BATCH] 个文件
    /// 2. chunk 级：embedding 前按 [batch_chunk_limit]（64/128/256）再拆分
    /// 3. 推理级：embedding 内部按 BATCH_SIZE 分批执行
    pub async fn index_files_batch(
        &self,
        dir_path: &str,
        files: &[(String, String)],
    ) -> Result<(), String> {
        if files.is_empty() {
            return Ok(());
        }

        let cfg = self.config_store.read();
        let chunk_size = cfg.chunk_size;
        let chunk_overlap = cfg.chunk_overlap;
        drop(cfg);
        let html_matcher = html_render_matcher(dir_path);

        let store = self.get_lance_store(dir_path).await;
        store.create_table().await?;
        let bm25 = self.get_bm25_index(dir_path).await?;
        let chunk_limit = batch_chunk_limit();

        let mut file_delta = 0i32;
        let mut chunk_delta = 0i32;

        // ── 文件级分批：控制单批内存峰值 ──
        for files_chunk in files.chunks(MAX_FILES_PER_BATCH) {
            // ── chunk_stage：逐文件分块，保留文件分组用于先删后写 ──
            let mut groups: Vec<(String, Vec<DocumentChunk>)> = Vec::new();
            for (rel, abs) in files_chunk {
                let content = match pipeline::read_document(Path::new(abs)) {
                    Some(c) if c.len() >= 10 => c,
                    _ => continue,
                };
                let doc_chunks =
                    pipeline::chunk_document(rel, &content, chunk_size, chunk_overlap, html_matcher.as_ref());
                if doc_chunks.is_empty() {
                    continue;
                }
                groups.push((rel.clone(), doc_chunks));
            }
            if groups.is_empty() {
                continue;
            }

            // ── chunk 级分批：embedding + 写库按 chunk_limit 拆分 ──
            let total_groups = groups.len();
            let mut start = 0usize;
            while start < total_groups {
                let mut end = start;
                let mut count = 0usize;
                while end < total_groups {
                    let n = groups[end].1.len();
                    if end > start && count + n > chunk_limit {
                        break;
                    }
                    count += n;
                    end += 1;
                }

                // ── 单文件 chunk 数超过 chunk_limit：分片嵌入 + 写入 ──
                // 旧数据只删一次，再逐片写入，避免整体载入内存峰值过高（B4）
                if end == start + 1 && count > chunk_limit {
                    let (rel, chunks) = &groups[start];
                    let old_chunks = self.count_document_chunks(&store, rel).await;
                    if let Err(e) = store.delete_document(rel).await {
                        log::warn!("[indexer] 批量索引删除旧 LanceDB 数据失败 ({}): {}", rel, e);
                    }
                    if let Err(e) = bm25.delete_document(rel) {
                        log::warn!("[indexer] 批量索引删除 BM25 旧数据失败 ({}): {}", rel, e);
                    }
                    for slice in chunks.chunks(chunk_limit) {
                        let vectors = pipeline::embed_chunks(slice, None).await?;
                        store.add_chunks(slice, &vectors).await.map_err(|e| {
                            log::error!("[indexer] 批量索引写入 LanceDB 失败 ({}): {}", rel, e);
                            e
                        })?;
                        bm25.add_documents(slice).map_err(|e| {
                            log::error!("[indexer] 批量索引写入 BM25 失败 ({}): {}", rel, e);
                            e
                        })?;
                    }
                    chunk_delta += chunks.len() as i32 - old_chunks as i32;
                    if old_chunks == 0 {
                        file_delta += 1;
                    }
                    start = end;
                    continue;
                }

                // ── embedding_stage：本子批一次性向量化 ──
                let batch_chunks: Vec<DocumentChunk> = groups[start..end]
                    .iter()
                    .flat_map(|(_, chunks)| chunks.iter().cloned())
                    .collect();
                let vectors = pipeline::embed_chunks(&batch_chunks, None).await?;

                // ── index_stage：逐文件先删后写（统一入口），元数据用差值累加 ──
                let mut offset = 0usize;
                for (rel, doc_chunks) in &groups[start..end] {
                    let n = doc_chunks.len();
                    let vs = &vectors[offset..offset + n];
                    offset += n;

                    let old_chunks =
                        match self.replace_document_chunks(&store, Some(bm25.as_ref()), rel, doc_chunks, vs).await
                        {
                            Ok(n) => n,
                            Err(e) => {
                                log::error!("[indexer] 批量索引写入失败 ({}): {}", rel, e);
                                continue;
                            }
                        };
                    chunk_delta += n as i32 - old_chunks as i32;
                    if old_chunks == 0 {
                        file_delta += 1;
                    }
                }
                start = end;
            }
        }

        self.update_metadata_delta(dir_path, file_delta, chunk_delta, chunk_delta)
            .await;
        Ok(())
    }

    /// ─── 单文件删除（增量）───

    pub async fn remove_file(&self, dir_path: &str, rel_path: &str) -> Result<(), String> {
        let store = self.get_lance_store(dir_path).await;
        let mut deleted_chunks = 0u32;
        let mut bm25_deleted = 0usize;

        if store.open_table().await.is_ok() {
            deleted_chunks = self.count_document_chunks(&store, rel_path).await;
            if store.delete_document(rel_path).await.is_err() {
                log::error!("[indexer] 删除 LanceDB 文档失败: {}", rel_path);
            }
        }

        // 仅在 BM25 索引目录已存在时清理，避免为从未索引的文件创建空索引目录
        if Path::new(&utils::get_bm25_dir(dir_path)).exists() {
            if let Ok(bm25) = self.get_bm25_index(dir_path).await {
                match bm25.delete_document(rel_path) {
                    Ok(n) => bm25_deleted = n,
                    Err(e) => log::error!("[indexer] 删除 BM25 文档失败 ({}): {}", rel_path, e),
                }
            }
        }

        // 仅当确实删除到数据时才更新元数据，避免未索引文件导致 file_count 虚减
        if deleted_chunks > 0 || bm25_deleted > 0 {
            let chunk_delta = -(deleted_chunks.max(bm25_deleted as u32) as i32);
            log::info!("[indexer] 更新元数据,删除 chunk 数: {}, BM25 删除 chunk 数: {}, chunk_delta: {}", deleted_chunks, bm25_deleted, chunk_delta);
            self.update_metadata_delta(dir_path, -1, chunk_delta, chunk_delta).await;
        } else {
            log::warn!("[indexer] 文件无索引数据，跳过元数据更新: {}", rel_path);
        }

        Ok(())
    }

    /// 收集源文件/目录下需要清理索引的相对路径列表（磁盘删除前调用，目录可遍历）
    ///
    /// 单文件返回自身；目录递归收集其下所有已索引文件（跳过 .mdgo 数据目录）。
    /// 路径比较与相对路径计算均做规范化处理（统一分隔符、去除 Windows verbatim 前缀、大小写不敏感）。
    pub async fn collect_remove_targets(
        &self,
        dir_path: &str,
        abs_path: &str,
    ) -> Result<Vec<String>, String> {
        if !path_is_within(dir_path, abs_path) {
            return Err(format!("路径不在知识库目录内: {}", abs_path));
        }
        let target = Path::new(abs_path);
        let mut rels: Vec<String> = Vec::new();
        if target.is_dir() {
            for entry in walkdir::WalkDir::new(target)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| !(e.file_type().is_dir() && (e.file_name() == ".mdgo" || e.file_name() == utils::TRASH_DIR_NAME)))
            {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("[indexer] 收集待清理文件时跳过无法访问的路径: {}", e);
                        continue;
                    }
                };
                if entry.file_type().is_file() {
                    if let Some(rel) = relative_to(dir_path, &entry.path().to_string_lossy()) {
                        rels.push(rel);
                    }
                }
            }
        } else if let Some(rel) = relative_to(dir_path, abs_path) {
            rels.push(rel);
        }
        Ok(rels)
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

        log::info!("[indexer] 清除全部索引完成: {}", dir_path);
        Ok(())
    }

    /**
     * 查询知识库索引状态
     * 
     * @param dir_path 知识库目录路径
     * @returns 知识库索引状态
     */
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

    /**
     * 混合检索（五层检索管线：Filter → Multi-Recall → RRF → Rerank → Diversity → Context）
     *
     * 生产级检索精度重构，替代旧 Retrieve→Fusion→Filter 的 alpha 线性加权方案：
     * - Filter 前置（P0 架构修复）：意图扩展名白名单在**检索前**限定候选范围——
     *   向量路在 LanceDB 查询层用 only_if SQL 预过滤，避免无关类型文档占用
     *   候选池名额把相关候选挤出 top_k 窗口（旧方案"查出许多不相关文档"的核心根因）
     * - Multi-Recall：向量 + BM25（minimum_should_match 严格语义）+ 代码符号三路召回
     * - RRF 融合：rank-based 加权融合，对分数尺度鲁棒（alpha 保留为每路权重偏置）
     * - 双阈值：纯向量噪声的绝对余弦阈值（vec_min_score）+ 精排 sigmoid 阈值
     * - 精排：本地 bge-reranker（cross-encoder），模型缺失/推理失败自动降级 RRF 排序
     * - Diversity：文件聚簇（每文档 chunk 上限）+ OPML 层级去重
     * - Context：相邻 chunk 语义合并填充 sentence_window
     *
     * 旧"文件名/代码符号事后加分（min 1.0）"逻辑已移除：文件名信号由精排
     * passage 前缀（feature 化）承载，符号信号由符号路 RRF 路由承载，
     * 避免手工加分破坏分数可比性、污染排序。
     *
     * @param dir_path 知识库目录路径
     * @param query_vector 查询向量
     * @param query 查询文本
     * @param top_k 返回结果数量
     * @returns 混合检索结果列表
     */
    pub async fn hybrid_search(
        &self,
        dir_path: &str,
        query_vector: &[f32],
        query: &str,
        top_k: u32,
    ) -> Result<Vec<SearchHit>, String> {
        let store = self.get_lance_store(dir_path).await;
        let config = self.config_store.read();

        // ── 0. 查询理解（SRP：RuleQueryPlanner 只负责把查询结构化为执行计划）──
        let plan = RuleQueryPlanner.plan(query);

        // ── 1. Filter 前置（P0 架构修复）──
        // 向量路在 LanceDB 查询层用 only_if SQL 预过滤；BM25 路（tantivy 无 SQL 级
        // 过滤）在 msm 严格检索之后做内存过滤（召回损失有界）。
        let filter_sql = plan.allowed_exts.map(ext_filter_sql);
        let vec_k = config.candidate_k.max(top_k);
        let bm25_k = config.candidate_k.max(top_k);

        // ── 2. Multi-Recall：向量 + BM25 并行（互不依赖，并行摊薄磁盘/CPU 延迟）──
        let bm25_index = self.get_bm25_index(dir_path).await;
        let vec_future = async {
            match &filter_sql {
                Some(sql) => store.search_vectors_with_filter(query_vector, vec_k, sql).await,
                None => store.search_vectors(query_vector, vec_k).await,
            }
        };
        let msm_ratio = config.bm25_msm_ratio;
        let bm25_future = async {
            let idx = match bm25_index {
                Ok(idx) => idx,
                Err(e) => {
                    log::warn!("[indexer] [混合检索] 获取 BM25 索引失败, error: {}", e);
                    return Vec::new();
                }
            };
            let idx = Arc::clone(&idx);
            let q = query.to_string();
            match tokio::task::spawn_blocking(move || idx.search_with_plan(&q, bm25_k, msm_ratio)).await {
                Ok(Ok(hits)) => hits,
                Ok(Err(e)) => {
                    log::warn!("[indexer] [混合检索] BM25 检索失败，本次查询退化为纯向量, error: {}", e);
                    Vec::new()
                }
                Err(e) => {
                    log::warn!("[indexer] [混合检索] BM25 检索任务执行失败, error: {}", e);
                    Vec::new()
                }
            }
        };
        let (vec_res, bm25_res) = tokio::join!(vec_future, bm25_future);
        let vec_hits = match vec_res {
            Ok(hits) => hits,
            Err(e) => {
                log::warn!("[indexer] [混合检索] 向量检索失败，本次查询退化为纯 BM25, error: {}", e);
                Vec::new()
            }
        };
        let mut bm25_hits = bm25_res;

        // BM25 路内存过滤（Filter 前置的 BM25 侧实现）
        bm25_hits = match plan.allowed_exts {
            Some(exts) => filter_hits_by_ext(bm25_hits, exts),
            None => bm25_hits,
        };

        // 符号路召回（Code 意图）：符号名精确/前缀匹配，独立于向量/BM25。
        // 多符号并行召回（join_all），符号路命中在步骤 4 通过 symbol_name 佐证放行。
        let mut symbol_hits: Vec<SearchHit> = Vec::new();
        if plan.intent == RetrievalIntent::Code && !plan.symbols.is_empty() {
            let tasks: Vec<_> = plan
                .symbols
                .iter()
                .take(3)
                .map(|symbol| {
                    let store = Arc::clone(&store);
                    let symbol = symbol.clone();
                    async move { store.search_symbols(&symbol, 5).await }
                })
                .collect();
            for res in futures::future::join_all(tasks).await {
                match res {
                    Ok(hits) => symbol_hits.extend(hits),
                    Err(e) => log::warn!("[indexer] [混合检索] 代码符号检索失败（忽略）: {}", e),
                }
            }
            // 符号路扩展名过滤（与 BM25 路一致：仅保留意图白名单内的文件类型）
            if let Some(exts) = plan.allowed_exts {
                symbol_hits = filter_hits_by_ext(symbol_hits, exts);
            }
        }
        log::info!(
            "[indexer] [混合检索] query='{}' intent={:?} vec_hits={} bm25_hits={} symbol_hits={}",
            query,
            plan.intent,
            vec_hits.len(),
            bm25_hits.len(),
            symbol_hits.len()
        );

        // ── 3. RRF 融合（SRP：rank-based 加权融合；alpha 保留为每路权重偏置）──
        let alpha = compute_alpha(query, config.fusion_alpha, plan.intent);
        let rrf_cfg = RrfConfig {
            k: config.rrf_k.max(1),
            weight_vec: alpha,
            weight_bm25: 1.0 - alpha,
            weight_symbol: 1.0,
        };
        let fused = rrf_fuse(vec_hits, bm25_hits, symbol_hits, &rrf_cfg);
        log::info!("[indexer] [混合检索] RRF 融合完成: alpha={:.2} candidates={}", alpha, fused.len());

        // ── 4. 双阈值（一）：纯向量噪声过滤 ──
        // 无 BM25/符号佐证（仅向量召回）且原始余弦低于绝对阈值 → 语义噪声，丢弃。
        // 符号路命中（symbol_name 非空）保留：代码符号精确匹配正是符号路的存在意义，
        // 若与向量/BM25 同 key 融合则 rrf 已叠加其贡献，纯符号命中必须放行。
        // 精排启用且模型就绪时：向量候选交由精排 sigmoid 阈值（rerank_min_score）裁决，
        // 此处不提前砍——避免余弦低但 cross-encoder 判定高度相关的候选被误杀（阈值协调）。
        let rerank_enabled = config.reranker_enabled;
        let rerank_min_score = config.rerank_min_score;
        let rerank_active = rerank_enabled && crate::core::model_download::is_reranker_cached();
        let pre_filter_len = fused.len();
        let candidates: Vec<SearchHit> = fused
            .into_iter()
            .filter(|h| {
                h.score_bm25 > 0.0
                    || h.symbol_name.is_some()
                    || rerank_active
                    || h.score_vec >= config.vec_min_score
            })
            .collect();
        log::info!(
            "[indexer] [混合检索] 向量阈值过滤(vec_min_score={}, rerank_active={}): {} → {}",
            config.vec_min_score,
            rerank_active,
            pre_filter_len,
            candidates.len()
        );

        // ── 5. 双阈值（二）：精排（可选；模型未就绪/推理失败自动降级 RRF 排序）──
        let results: Vec<SearchHit> = if rerank_active {
            let reranker = LocalBgeReranker;
            let q = query.to_string();
            let cands = candidates.clone();
            match tokio::task::spawn_blocking(move || {
                reranker.rerank(&q, &cands, rerank_min_score)
            })
            .await
            {
                Ok(Ok(hits)) => {
                    if log::log_enabled!(log::Level::Debug) {
                        let detail: Vec<String> = hits
                            .iter()
                            .map(|h| format!("{}: {:.3}", h.doc_name, h.score))
                            .collect();
                        log::info!(
                            "[indexer] [混合检索] 精排完成: {} → {} 通过阈值({}), sigmoid分数:\n{:?}",
                            candidates.len(),
                            hits.len(),
                            rerank_min_score,
                            detail
                        );
                    }
                    hits
                }
                Ok(Err(e)) => {
                    log::warn!("[indexer] [混合检索] 精排失败，回退 RRF 排序: {}", e);
                    candidates
                }
                Err(e) => {
                    log::warn!("[indexer] [混合检索] 精排任务执行失败，回退 RRF 排序: {}", e);
                    candidates
                }
            }
        } else {
            if rerank_enabled {
                // 模型未缓存：后台触发一次下载（失败自动重试），本次回退 RRF（检索永不阻断）
                trigger_reranker_download_background();
                log::info!("[indexer] [混合检索] reranker 模型未就绪，本次回退 RRF 排序");
            }
            candidates
        };

        // ── 6. Diversity：OPML 层级去重 + 文件聚簇（每文档 chunk 上限）──
        let deduped = Self::dedup_opml_hierarchy(results);
        log::info!("[indexer] [混合检索] OPML 层级去重: candidates={}", deduped.len());

        let max_per_doc = config.max_chunks_per_doc.max(1) as usize;
        let mut per_doc: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let clustered: Vec<SearchHit> = deduped
            .into_iter()
            .filter(|h| {
                let count = per_doc.entry(h.doc_name.clone()).or_insert(0);
                if *count >= max_per_doc {
                    false
                } else {
                    *count += 1;
                    true
                }
            })
            .collect();
        log::info!("[indexer] [混合检索] 文件聚簇(max_chunks_per_doc={}) → {}", max_per_doc, clustered.len());

        let mut result: Vec<SearchHit> = clustered.into_iter().take(top_k as usize).collect();

        // ── 7. Context（Post-Retrieval Context Window）：相邻 chunk 语义合并 ──
        // 为每个 top_k 结果的相邻 chunks 做上下文合并，填充 sentence_window。
        // 适用于 Markdown（按文档顺序）、OPML/FreeMind（DFS 顺序）所有文档类型。
        //
        // 性能：按 doc_name 分组，同文档的多个命中 chunk 合并为**一次**区间查询
        // （区间并集），全部文档查询用 join_all **并行**执行——把"N 次串行全表扫描"
        // 降为"唯一文档数 次并行单文档查询"，且单文档查询已由 fetch_chunks_between
        // 的 only_if(doc_name=...) 预过滤缩小扫描范围。
        if !result.is_empty() {
            let ctx_window = Self::compute_context_window(query);
            let mut groups: std::collections::HashMap<String, Vec<(usize, u32)>> =
                std::collections::HashMap::new();
            for (i, hit) in result.iter().enumerate() {
                groups
                    .entry(hit.doc_name.clone())
                    .or_default()
                    .push((i, hit.chunk_index));
            }
            let tasks: Vec<_> = groups
                .into_iter()
                .map(|(doc, entries)| {
                    let store = Arc::clone(&store);
                    let start = entries
                        .iter()
                        .map(|(_, ci)| ci.saturating_sub(ctx_window))
                        .min()
                        .unwrap_or(0);
                    let end = entries
                        .iter()
                        .map(|(_, ci)| ci.saturating_add(ctx_window))
                        .max()
                        .unwrap_or(0);
                    async move {
                        let chunks = store.fetch_chunks_between(&doc, start, end).await;
                        (doc, entries, chunks)
                    }
                })
                .collect();
            for (doc, entries, chunks_res) in futures::future::join_all(tasks).await {
                let chunks = match chunks_res {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!("[indexer] [混合检索] 上下文扩展查询失败({}): {}", doc, e);
                        continue;
                    }
                };
                for (result_idx, center) in entries {
                    // 提取该命中 chunk 的 ±window 子窗口（区间并集查询结果的子集）
                    let sub: Vec<(u32, String, Option<String>)> = chunks
                        .iter()
                        .filter(|(idx, _, _)| {
                            *idx >= center.saturating_sub(ctx_window) && *idx <= center.saturating_add(ctx_window)
                        })
                        .cloned()
                        .collect();
                    if sub.len() <= 1 {
                        continue; // 无扩展内容则保持原样
                    }
                    let hit = &mut result[result_idx];
                    let mut merged = String::new();
                    let mut has_parent = false;
                    for (idx, text, path_json) in &sub {
                        // OPML/FreeMind: 检测 path_json 是否为父节点（当前 chunk 路径的前缀）
                        if *idx != hit.chunk_index
                            && !has_parent
                            && path_json.is_some()
                            && hit.path_json.is_some()
                        {
                            if is_path_prefix(
                                path_json.as_deref().unwrap(),
                                hit.path_json.as_deref().unwrap(),
                            ) {
                                has_parent = true;
                            }
                        }
                        if merged.is_empty() {
                            merged.push_str(text);
                        } else {
                            merged.push('\n');
                            merged.push_str(text);
                        }
                    }
                    // 仅当有实质扩展内容时才设置 sentence_window
                    if merged.len() > hit.text.len() + 10 {
                        hit.sentence_window = Some(merged);
                    }
                }
            }
            log::info!("[indexer] [混合检索] 注入上下文扩展: candidates={}， ctx_window={}", result.len(), ctx_window);
        }

        Ok(result)
    }

    /// OPML 层级去重：同一大纲文档中具有路径前缀关系的 chunk 保留最深节点。
    ///
    /// 使用掩码方式：若 A 的路径是 B 路径的严格前缀，则 A 为冗余父节点（注意
    /// 子节点分数可能高于父节点，此时父节点作为前缀同样应被移除）。
    /// 仅对大纲类文档（OPML/FreeMind）执行：Markdown 语义分块父子节内容互斥，
    /// 去重会误删合法父节 chunk，因此必须按文件类型守卫。
    fn dedup_opml_hierarchy(hits: Vec<SearchHit>) -> Vec<SearchHit> {
        if hits.len() <= 1 {
            return hits;
        }
        let mut groups: std::collections::HashMap<String, Vec<SearchHit>> = std::collections::HashMap::new();
        for hit in hits {
            groups.entry(hit.doc_name.clone()).or_default().push(hit);
        }
        let mut deduped: Vec<SearchHit> = Vec::new();
        for (doc_name, hits) in groups {
            let n = hits.len();
            if n <= 1 || !is_outline_doc(&doc_name) {
                deduped.extend(hits);
                continue;
            }
            // 预解析 path_json：O(n) 一次反序列化，消除 O(n²) 对比较中的重复 JSON 解析
            let paths: Vec<Option<Vec<String>>> = hits
                .iter()
                .map(|h| {
                    h.path_json
                        .as_deref()
                        .and_then(|p| serde_json::from_str(p).ok())
                })
                .collect();
            let mut keep_mask = vec![true; n];
            for i in 0..n {
                if !keep_mask[i] {
                    continue;
                }
                let hp = match &paths[i] {
                    Some(p) => p,
                    None => continue, // 无路径信息的不参与去重
                };
                for j in 0..n {
                    if i == j || !keep_mask[j] {
                        continue;
                    }
                    let op = match &paths[j] {
                        Some(p) => p,
                        None => continue,
                    };
                    // 如果 i 的路径是 j 路径的严格前缀（预解析后切片比较，无重复 JSON 解析）
                    // → i 是父节点，冗余
                    if is_path_prefix_vec(hp, op) {
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
        deduped
    }


    /// 按代码符号名检索（精确/前缀匹配 `symbol_name`），用于 Agent 的 `code_lookup` 工具
    /// 与代码查询的符号命中注入。见 [`crate::core::db::lance::LanceStore::search_symbols`]。
    pub async fn search_symbols(
        &self,
        dir_path: &str,
        symbol: &str,
        top_k: u32,
    ) -> Result<Vec<SearchHit>, String> {
        let store = self.get_lance_store(dir_path).await;
        store.search_symbols(symbol, top_k).await
    }

    /// 根据查询长度动态计算上下文扩展窗口大小。
    ///
    /// - 短查询（≤3 词）：需要较大上下文定位 → 窗口 3（±3 chunks）
    /// - 中查询（4-10 词）：窗口 2
    /// - 长查询（>10 词）：查询已经够具体 → 窗口 1
    fn compute_context_window(query: &str) -> u32 {
        let word_count = query.split(|c: char| !c.is_alphanumeric() && c != '\'' && c != '"')
            .filter(|t| !t.is_empty())
            .count();
        if word_count <= 3 {
            3
        } else if word_count <= 10 {
            2
        } else {
            1
        }
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

    /// ─── 先删后写统一入口（LanceDB + BM25）───
    ///
    /// 删除 `rel_path` 的旧 chunk 并写入新数据，返回旧 chunk 数（供元数据差值计算）。
    /// 供 `index_file`（单文件增量）与 `index_files_batch`（Watcher 批量）共用，
    /// 消除两处六步"先删后写"的重复编排与行为分歧（A1）。
    ///
    /// - 删除旧数据失败仅告警：新数据写入后幂等覆盖，不阻断本次写入
    /// - `bm25` 为 None 时跳过 BM25（单文件索引场景允许 BM25 索引缺失）
    async fn replace_document_chunks(
        &self,
        store: &LanceStore,
        bm25: Option<&Bm25Index>,
        rel_path: &str,
        doc_chunks: &[DocumentChunk],
        vectors: &[Vec<f32>],
    ) -> Result<usize, String> {
        let old_chunks = self.count_document_chunks(store, rel_path).await;

        if let Err(e) = store.delete_document(rel_path).await {
            log::warn!("[indexer] 删除旧 LanceDB 数据失败 ({}): {}", rel_path, e);
        }
        if let Some(bm25) = bm25 {
            if let Err(e) = bm25.delete_document(rel_path) {
                log::warn!("[indexer] 删除 BM25 旧数据失败 ({}): {}", rel_path, e);
            }
        }

        store.add_chunks(doc_chunks, vectors).await.map_err(|e| {
            log::error!("[indexer] 写入 LanceDB 失败 ({}): {}", rel_path, e);
            e
        })?;
        if let Some(bm25) = bm25 {
            bm25.add_documents(doc_chunks).map_err(|e| {
                log::error!("[indexer] 写入 BM25 失败 ({}): {}", rel_path, e);
                e
            })?;
        }
        Ok(old_chunks as usize)
    }

    /// 统计某个 doc_name 下的 chunk 数量
    ///
    /// 流式迭代计数，避免 try_collect 全量加载到内存。
    async fn count_document_chunks(&self, store: &LanceStore, doc_name: &str) -> u32 {
        let table = match store.open_table().await {
            Ok(t) => t,
            Err(_) => return 0,
        };
        use lancedb::query::{ExecutableQuery, QueryBase};
        use futures::TryStreamExt;
        let escaped = crate::core::db::lance::escape_sql_string(doc_name);
        let result = table
            .query()
            .only_if(&format!("doc_name = '{}'", escaped))
            .execute()
            .await;
        match result {
            Ok(stream) => {
                let mut count = 0u32;
                let _ = stream.try_for_each(|batch| {
                    count += batch.num_rows() as u32;
                    futures::future::ready(Ok(()))
                }).await;
                count
            }
            Err(_) => 0,
        }
    }

    // ─── 对话会话索引（chat_vectors，与文档索引分离；增量追加，无 BM25）───

    /// 增量索引会话消息到对话向量库。
    ///
    /// - 单条消息作为一个 chunk
    /// - `doc_name` = `session_id`（便于按会话删除）
    /// - `chunk_index` = 消息在会话中的全局序号（= `start_from + offset`，保持稳定）
    /// - `text` = `[role] content`（包含角色前缀，提升检索语义）
    /// - `messages` = 未索引的增量消息（调用方已按 `start_from` 拉取）；`start_from` =
    ///   已索引消息条数，仅用于计算全局序号，写入时不删除已有数据
    pub async fn index_chat_session(
        &self,
        dir_path: &str,
        session_id: &str,
        messages: &[ChatMessage],
        start_from: usize,
    ) -> Result<(), String> {
        // 无新增消息 → 跳过（幂等，避免无意义的 embedding）
        if messages.is_empty() {
            return Ok(());
        }

        // 构建新增消息的 DocumentChunk（单条消息 = 一个 chunk）
        let chunks: Vec<DocumentChunk> = messages
            .iter()
            .enumerate()
            .map(|(offset, msg)| {
                let i = start_from + offset; // 全局序号，保证 chunk_index 与消息一一对应
                DocumentChunk {
                    id: format!("chat:{}:{}", session_id, i),
                    doc_name: session_id.to_string(),
                    chunk_index: i as u32,
                    text: format!("[{}] {}", msg.role, msg.content),
                    path_depth: None,
                    path_json: None,
                    sentence_window: None,
                    symbol_name: None,
                    symbol_kind: None,
                    embedding_text: None,
                    chunk_type: None,
                }
            })
            .collect();

        // 生成 embedding（批量，1 次调用，放入 spawn_blocking 避免阻塞 Tokio）
        let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
        let vectors = tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
            utils::call_embedding(&refs, None)
        })
        .await
        .map_err(|e| format!("Embedding 任务执行失败: chunk_count={}, {}", chunks.len(), e))??;

        // 增量写入 LanceDB（chat_vectors 表）：只追加新增消息，不删除已有数据。
        //
        // 与 index_all / index_unindexed 共用 indexing_lock 串行化：Lance 的 writer
        // 基于目录锁，并发写同一索引目录会失败。聊天索引是低频操作，串行等待代价可忽略。
        {
            let _guard = self.indexing_lock.lock().await;
            let store = self.get_chat_lance_store(dir_path).await;
            store.create_table().await?;
            store.add_chunks(&chunks, &vectors).await?;
        }

        log::info!(
            "[indexer] [对话增量索引] 会话ID={}， 增量索引（新增 {} 条，游标 {} → {}）",
            session_id,
            chunks.len(),
            start_from,
            messages.len()
        );
        Ok(())
    }

    /// 从对话索引中删除指定会话的所有消息
    pub async fn remove_chat_session(&self, dir_path: &str, session_id: &str) -> Result<(), String> {
        // 与 index_chat_session / index_all 共用 indexing_lock 串行化，
        // 避免并发 writer 目录锁冲突
        let _guard = self.indexing_lock.lock().await;

        let store = self.get_chat_lance_store(dir_path).await;
        if store.open_table().await.is_ok() {
            if let Err(e) = store.delete_document(session_id).await {
                log::error!("[indexer] 删除对话向量失败 ({}): {}", session_id, e);
            }
        }

        Ok(())
    }

    /// 向量检索对话：仅向量召回（BM25 已移除）。
    ///
    /// 返回 `(session_id, score, matched_text)` 列表，调用方根据 session_id
    /// 去 SQLite 查会话元信息组装最终结果。
    ///
    /// 与文档搜索（`hybrid_search`）完全隔离，只查 `chat_vectors`。
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
        let query_string_clone = query_string.clone();
        let query_embedding = tokio::task::spawn_blocking(move || utils::call_embedding_query(&query_string_clone))
            .await
            .map_err(|e| format!("Embedding 任务执行失败: query={}, {}", query_string, e))??;
        let query_vec = query_embedding
            .first()
            .ok_or_else(|| format!("查询向量为空: {}", query_string))?;

        // 2. 向量检索（chat_vectors 表）
        let store = self.get_chat_lance_store(dir_path).await;
        let vec_k = (top_k * 2).max(10);
        let vec_hits = store.search_vectors(query_vec, vec_k).await.unwrap_or_default();
        log::info!("[indexer] [对话向量检索] 完成，共 {} 个命中项，top_k={}", vec_hits.len(), top_k);

        // 3. 转换为 (session_id, score, matched_text)
        let results = vec_hits
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
            log::info!("[indexer] [启动同步] 无索引记录，跳过启动同步（由 index_all 全量处理）");
            return Ok(0);
        }

        let base_dir = Path::new(dir_path);
        if !base_dir.exists() {
            return Err(format!("[indexer] [启动同步] 目录不存在: {}", dir_path));
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
                .to_string()
                .replace('\\', "/");

            if let Err(e) = self.index_file(dir_path, &rel_path, &file_path.to_string_lossy()).await {
                log::warn!("[indexer] [启动同步] 文件 {} error: {}", rel_path, e);
                continue;
            }
            synced_count += 1;
            log::info!("[indexer] [启动同步] 索引修改文件成功 rel_path={}，synced_count={}", rel_path, synced_count);
        }

        log::info!("[indexer] [启动同步] 共同步 {} 个文件", synced_count);
        Ok(synced_count)
    }

    /// ─── 增量索引（仅索引未在 LanceDB 中的文件，批量化 Embedding）───
    ///
    /// 与 index_all 的区别：
    /// - 不清理已有索引（不调用 clear_inner）
    /// - 检查文件是否已存在 LanceDB 中，已存在的跳过
    /// - 所有未索引文件的 chunk 合并为一批进行 Embedding（避免 N 次推理调用）
    /// - 适用于向已有索引的知识库添加新文件后，快速增量补索引
    pub async fn index_unindexed(
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

        progress(0, "正在扫描目录...");
        let ignore = IgnoreMatcher::new(&config.dir_blacklist, &config.file_blacklist);
        let files = scan_directory(base_dir, &ignore)?;
        let total = files.len() as u32;
        if total == 0 {
            return Err("目录中没有可索引的文件".into());
        }
        progress(3, &format!("共有 {} 个文件，正在检查已索引状态...", total));

        // 确保 LanceDB 表存在
        let store = self.get_lance_store(dir_path).await;
        store.create_table().await?;

        // 批量获取已索引文档名（一次查询而非逐文件检查）
        progress(5, &format!("正在检查 {} 个文件的索引状态...", total));
        let existing_docs = store.list_document_names().await.unwrap_or_default();

        let mut unindexed_paths: Vec<(String, String)> = Vec::new();
        for file_path in files.iter() {
            let rel = file_path
                .strip_prefix(base_dir)
                .unwrap_or(file_path)
                .to_string_lossy()
                .to_string()
                .replace('\\', "/");
            if existing_docs.contains(&rel) {
                continue;
            }
            let abs = file_path.to_string_lossy().to_string();
            unindexed_paths.push((rel, abs));
        }

        let unindexed_count = unindexed_paths.len() as u32;
        if unindexed_count == 0 {
            progress(100, "所有文件均已索引，无需增量");
            return Ok(KbIndexResult {
                file_count: 0,
                chunk_count: 0,
                vector_count: 0,
                indexed_at: 0,
            });
        }
        log::info!("[indexer] [增量索引] 共发现 {} 个未索引文件, 共 {} 个文件", unindexed_count, total);

        // 先读取 + 分块所有未索引文件，合并 DocumentChunk
        progress(15, &format!("正在读取 {} 个未索引文件...", unindexed_count));
        let cfg = self.config_store.read();
        let html_matcher = html_render_matcher(dir_path);
        let mut all_file_data: Vec<(String, Vec<DocumentChunk>)> = Vec::new(); // (rel_path, chunks)
        let mut total_new_chunks: u32 = 0;
        let mut file_count: u32 = 0;

        for (idx, (rel, abs)) in unindexed_paths.iter().enumerate() {
            let content = match pipeline::read_document(Path::new(abs)) {
                Some(c) if c.len() >= 10 => c,
                _ => continue,
            };
            let doc_chunks = pipeline::chunk_document(rel, &content, cfg.chunk_size, cfg.chunk_overlap, html_matcher.as_ref());
            if doc_chunks.is_empty() {
                continue;
            }
            let n = doc_chunks.len() as u32;
            total_new_chunks += n;
            file_count += 1;
            all_file_data.push((rel.clone(), doc_chunks));

            let read_pct = 15 + ((idx + 1) * 5 / unindexed_paths.len().max(1)) as u8;
            progress(read_pct.min(19), &format!("读取文件 {}/{} (累积 {} 个文本块)", idx + 1, unindexed_paths.len(), total_new_chunks));
        }

        if all_file_data.is_empty() {
            progress(100, "增量索引完成（无有效内容）");
            return Ok(KbIndexResult { file_count: 0, chunk_count: 0, vector_count: 0, indexed_at: 0 });
        }

        // 合并所有 chunks，一次性批量 Embedding
        let all_chunks: Vec<DocumentChunk> = all_file_data.iter()
            .flat_map(|(_, chunks)| chunks.iter().cloned())
            .collect();

        progress(20, &format!("正在向量化 {} 个文本块（单批推理）...", all_chunks.len()));
        let embed_progress = |done: usize, total_groups: usize, msg: &str| {
            let embed_pct = 20 + (done * 60 / total_groups.max(1)) as u8;
            progress(embed_pct.min(80), msg);
        };
        let all_vectors = pipeline::embed_chunks(&all_chunks, Some(&embed_progress)).await?;

        // 分批写入 LanceDB + BM25
        progress(82, "正在写入数据库...");
        let batch_limit = batch_chunk_limit();
        let bm25 = self.get_bm25_index(dir_path).await?;
        for batch_idx in (0..all_chunks.len()).step_by(batch_limit) {
            let end = (batch_idx + batch_limit).min(all_chunks.len());
            let batch_chunks = &all_chunks[batch_idx..end];
            let batch_vectors = &all_vectors[batch_idx..end];

            pipeline::write_chunks(&store, &bm25, batch_chunks, batch_vectors)
                .await
                .map_err(|e| {
                    log::error!("[indexer] [增量索引] 写入数据库失败: {}", e);
                    e
                })?;

            let write_pct = 82 + ((batch_idx + batch_limit) * 13 / all_chunks.len().max(1)) as u8;
            progress(write_pct.min(95), &format!("写入数据库 {}/{} 文本块", end, all_chunks.len()));
        }

        // 批量更新元数据
        self.update_metadata_delta(dir_path, file_count as i32, total_new_chunks as i32, all_vectors.len() as i32).await;

        progress(100, &format!("增量索引完成: {} 文件, {} 文本块", file_count, total_new_chunks));
        log::info!("[indexer] [增量索引] 共索引 {} 个文件, {} 个文本块, {} 个向量", file_count, total_new_chunks, all_vectors.len() as u32);
        
        Ok(KbIndexResult {
            file_count,
            chunk_count: total_new_chunks,
            vector_count: all_vectors.len() as u32,
            indexed_at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
        })
    }
}

// ─── 辅助函数 ───

/// 扫描目录，返回符合扩展名和过滤规则的绝对路径列表
/// 从 `{dir}/.mdgo/setting.json` 读取「HTML 渲染目录」（`htmlCodeShowBlacklist`，
/// gitignore 格式，与目录/文件黑名单一致）并构建目录匹配器。
///
/// 语义（对齐前端设置说明）：该配置路径内的 HTML 作为文档渲染/分块，
/// 其他目录的 HTML 默认按代码处理。未配置或解析失败返回 `None`
/// （分块层保持现状：全部 HTML 按文档分块）。
fn html_render_matcher(dir_path: &str) -> Option<IgnoreMatcher> {
    let setting_path = Path::new(dir_path).join(".mdgo").join("setting.json");
    let text = std::fs::read_to_string(setting_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let patterns: Vec<String> = json
        .get("htmlCodeShowBlacklist")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if patterns.is_empty() {
        return None;
    }
    Some(IgnoreMatcher::new(&patterns, &[]))
}

pub(crate) fn scan_directory(base_dir: &Path, ignore: &IgnoreMatcher) -> Result<Vec<std::path::PathBuf>, String> {
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
                if name == ".mdgo" || name == utils::TRASH_DIR_NAME {
                    return false;
                }
                let rel_path = e.path().strip_prefix(base_dir).unwrap_or(e.path());
                let rel = rel_path.to_string_lossy().replace('\\', "/");
                return ignore.is_kb_dir_allowed(&name, &rel);
            }
            true
        });

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                log::warn!("[scan_directory] 跳过无法访问的目录: {}", e);
                continue;
            }
        };
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
    log::info!("[scan_directory] 共发现 {} 个文件", files.len());
    Ok(files)
}

/// 保存索引元数据到 {data_dir}/index_meta.json
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

/// 触发 reranker 模型后台下载（进程内幂等，失败自动重试）。
///
/// 不阻塞检索：下载完成前精排降级为 RRF 排序；下载完成后后续检索自动启用精排。
/// 下载源优先级与 Embedding 模型一致（ModelScope → hf-mirror → HuggingFace），
/// 见 [`crate::core::model_download::ensure_reranker_downloaded`]。
fn trigger_reranker_download_background() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    // 下载进行中或已成功 → 跳过（避免并发重复下载/重复 spawn）
    static IN_FLIGHT_OR_DONE: AtomicBool = AtomicBool::new(false);
    // 最近一次失败时刻（UNIX 秒），用于失败防抖重试
    static LAST_FAIL_AT: AtomicU64 = AtomicU64::new(0);

    // 下载失败后的重试间隔（秒）：避免失败后每次检索都触发下载风暴
    const RETRY_INTERVAL_SECS: u64 = 120;

    if IN_FLIGHT_OR_DONE.load(Ordering::Relaxed) {
        return;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let last_fail = LAST_FAIL_AT.load(Ordering::Relaxed);
    if last_fail != 0 && now.saturating_sub(last_fail) < RETRY_INTERVAL_SECS {
        return;
    }
    if IN_FLIGHT_OR_DONE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return; // 并发窗口内已由其他线程触发
    }

    std::thread::spawn(move || {
        match crate::core::model_download::ensure_reranker_downloaded() {
            Ok(dir) => {
                // 成功：保持 IN_FLIGHT_OR_DONE=true，进程内不再重试
                log::info!("[indexer] reranker 模型后台下载完成: {}", dir.display());
            }
            Err(e) => {
                // 失败：重置标志并记录时间戳，后续检索按防抖间隔自动重试
                log::warn!(
                    "[indexer] reranker 模型后台下载失败（{}s 后自动重试）: {}",
                    RETRY_INTERVAL_SECS,
                    e
                );
                IN_FLIGHT_OR_DONE.store(false, Ordering::Release);
                LAST_FAIL_AT.store(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    Ordering::Release,
                );
            }
        }
    });
}
