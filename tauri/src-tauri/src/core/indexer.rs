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

/// 检索意图（轻量级规则路由，替代单一 is_code_query 启发式）。
///
/// 不同意图限定不同的候选文件范围（元数据过滤），并决定是否注入代码符号命中，
/// 从而减少跨类型文件的噪声匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrievalIntent {
    /// 代码相关查询（函数/类/符号/接口等）→ 仅检索代码类文件
    Code,
    /// 文档/笔记查询（Markdown 等）→ 仅检索文档类文件
    Document,
    /// 大纲类查询（OPML/FreeMind 思维导图）→ 仅检索大纲类文件
    Outline,
    /// 通用查询 → 不限定文件类型
    General,
}

/// 根据查询文本进行轻量级意图路由（规则启发式，无 LLM 开销）。
///
/// 优先判定代码（符号/关键字/代码语法特征），其次大纲（opml/思维导图等），
/// 再文档（readme/markdown 等），默认通用。
pub fn route_intent(query: &str) -> RetrievalIntent {
    if is_code_query(query) {
        return RetrievalIntent::Code;
    }
    let lower = query.to_lowercase();
    // 仅使用具象的大纲文件格式词，避免泛化词"大纲"把普通文档查询误路由为 Outline
    let outline_kws = [
        "opml", "freemind", "free mind", "思维导图", "outline",
    ];
    if outline_kws.iter().any(|kw| lower.contains(kw)) {
        return RetrievalIntent::Outline;
    }
    // 仅使用名词性文档词，避免动词性"说明/解释"等把中文代码提问误路由为文档
    let doc_kws = ["readme", "markdown", "文档", "笔记", "文章"];
    if doc_kws.iter().any(|kw| lower.contains(kw)) {
        return RetrievalIntent::Document;
    }
    RetrievalIntent::General
}

/// 代码文件扩展名统一清单。
///
/// 单一来源：`classify_ext`（索引类型统计）与 `intent_allowed_exts`（意图过滤）
/// 共用本清单，避免两套列表漂移导致"已索引的代码文件在 Code 意图下被漏检"。
const CODE_EXTENSIONS: &[&str] = &[
    "py", "js", "ts",  "rs", "go", "java", "lua", "sh",
    "bat", "sql", "yaml", "yml", "toml", "conf",
];

/// 意图 → 允许检索的文件扩展名白名单（元数据过滤）。
fn intent_allowed_exts(intent: RetrievalIntent) -> Option<&'static [&'static str]> {
    match intent {
        RetrievalIntent::Code => Some(CODE_EXTENSIONS),
        RetrievalIntent::Document => Some(&["md", "markdown", "mdown", "rst", "txt"]),
        RetrievalIntent::Outline => Some(&["opml", "mm"]),
        RetrievalIntent::General => None,
    }
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

/// 计算向量/BM25 融合权重 α（0~1，越高越偏向语义向量）。
///
/// 在配置的基准权重上微调：短查询依赖关键词精确匹配 → 降低向量权重；
/// 长查询语义丰富 → 略提高向量权重。
///
/// 对 CJK 中文做了显式感知：中文按字符数而非"词"计数（中文无空格分词），
/// 避免整句中文被计为 1 个 token 导致所有中文查询都落入"短查询"分支。
fn compute_alpha(query: &str, base_alpha: f32) -> f32 {
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
        (base_alpha - 0.2).clamp(0.2, 1.0)
    } else if is_long {
        (base_alpha + 0.1).min(0.95)
    } else {
        base_alpha
    }
}

/// 检测查询是否为"代码风格"的查询。
///
/// 代码查询的特征（满足任一即可）：
/// - 包含 `::`、`->`、`()` 等代码特定符号
/// - 包含 CamelCase 标识符（如 "LRUCache"、"parseJSON"）
/// - 包含 snake_case 标识符（如 "lru_cache"、"index_all"）
/// - 包含代码文件扩展名（如 ".py"、".rs"、".ts"）
/// - 包含常见代码关键字（function、class、fn、def、struct）
fn is_code_query(query: &str) -> bool {
    // 代码语法特征
    if query.contains("::") || query.contains("->") || query.contains("()") {
        return true;
    }
    // 代码文件扩展名
    if query.contains(".py") || query.contains(".rs") || query.contains(".ts")
        || query.contains(".js") || query.contains(".go") || query.contains(".java")
        || query.contains(".cpp") || query.contains(".c") || query.contains(".h")
        || query.contains(".rb") || query.contains(".php")
    {
        return true;
    }
    // 代码关键字
    let lower = query.to_lowercase();
    // fn 单独处理：检查是否为独立词（fn()、fn_main、fn 都算）
    let has_fn = query.split(|c: char| !c.is_alphanumeric())
        .any(|word| word == "fn");
    let code_keywords = [
        "function", "class", "struct", "enum", "trait",
        "interface", "namespace", "lambda", "async", "await",
        "callback", "prototype", "constructor",
    ];
    if has_fn || code_keywords.iter().any(|kw| lower.contains(kw)) {
        return true;
    }
    // CamelCase 检测：连续大写字母 + 小写字母（如 "LRUCache", "parseJSON"）
    if query.chars().any(|c| c.is_uppercase()) {
        // 至少一个完整的词包含大小写混合
        let has_camel = query.split(|c: char| !c.is_alphanumeric())
            .any(|word| {
                word.len() >= 3
                    && word.chars().any(|c| c.is_uppercase())
                    && word.chars().any(|c| c.is_lowercase())
                    && !word.chars().all(|c| c.is_uppercase())
            });
        if has_camel {
            return true;
        }
    }
    // snake_case 检测（'_' 视为词内字符，避免 "handle_timeout" 被拆成两段而漏判）
    let has_snake = query.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|word| word.contains('_') && word.len() >= 3);
    if has_snake {
        return true;
    }
    false
}

/// 从查询中提取疑似代码符号的标识符 token（CamelCase / snake_case）。
///
/// 中文与数字 token 会被过滤（汉字的 is_uppercase/is_lowercase 均为 false），
/// 仅保留形如 `handleTimeout`、`lru_cache`、`parseJSON` 的标识符，用于
/// 代码符号精确检索（见 `hybrid_search` 的符号命中注入）。
fn extract_symbol_tokens(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for t in query.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let t = t.trim();
        if t.len() < 2 || t.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let has_upper = t.chars().any(|c| c.is_uppercase());
        let has_lower = t.chars().any(|c| c.is_lowercase());
        if (has_upper && has_lower) || t.contains('_') {
            tokens.push(t.to_string());
        }
    }
    tokens
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
            chat_bm25_cache: Mutex::new(None),
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

        // ── 核心文件处理循环 ──
        let mut batch_chunks: Vec<DocumentChunk> = Vec::with_capacity(BATCH_CHUNK_LIMIT);
        let mut file_count = 0u32;
        let mut batch_file_count = 0u32;
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
                .to_string()
                .replace('\\', "/");

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
            batch_file_count += 1;

            // 以文件数为基准计算线性进度 5% → 95%
            let base_pct = 5 + (file_count * 90 / total.max(1));
            msg(
                base_pct.min(94),
                &format!("读取文件 {}/{} (已缓存 {} 个文本块)", file_count, total, batch_chunks.len()),
            );

            // 分批触发 Embedding
            if batch_chunks.len() < BATCH_CHUNK_LIMIT && i + 1 < total as usize {
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

            let vectors = self.embed_batch(&batch_chunks, Some(&embed_progress)).await?;

            set_progress(
                embed_base + embed_total_pct,
                format!("已完成 {}/{} 文件 ({} 文本块) - 写入数据库", file_count, total, total_chunks + batch_chunks.len() as u32),
            );

            store.add_chunks(&batch_chunks, &vectors).await?;
            bm25.add_documents(&batch_chunks)?;

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
                .filter_entry(|e| !(e.file_type().is_dir() && e.file_name() == ".mdgo"))
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
        intent: RetrievalIntent,
    ) -> Result<Vec<SearchHit>, String> {
        let store = self.get_lance_store(dir_path).await;
        let config = self.config_store.read();
        let alpha = compute_alpha(query, config.fusion_alpha);

        // 意图 → 文件扩展名白名单（元数据过滤）
        let allowed_exts = intent_allowed_exts(intent);
        let has_filter = allowed_exts.map(|e| !e.is_empty()).unwrap_or(false);

        // 候选池扩大到 top_k * 5；启用元数据过滤时扩大到 top_k * 10 补偿召回损失
        let vec_k = if has_filter { (top_k * 10).max(30) } else { (top_k * 5).max(15) };
        let vec_hits = match store.search_vectors(query_vector, vec_k).await {
            Ok(hits) => hits,
            Err(e) => {
                log::warn!("[indexer] 向量检索失败，本次查询退化为纯 BM25: {}", e);
                Vec::new()
            }
        };

        let bm25_k = if has_filter { (top_k * 10).max(30) } else { (top_k * 5).max(15) };
        let bm25_hits = match self.get_bm25_index(dir_path).await {
            Ok(idx) => match idx.search(query, bm25_k) {
                Ok(hits) => hits,
                Err(e) => {
                    log::warn!("[indexer] BM25 检索失败，本次查询退化为纯向量: {}", e);
                    Vec::new()
                }
            },
            Err(e) => {
                log::warn!("[indexer] 获取 BM25 索引失败: {}", e);
                Vec::new()
            }
        };

        // 元数据过滤（内存实现）
        let vec_hits = match allowed_exts {
            Some(exts) => filter_hits_by_ext(vec_hits, exts),
            None => vec_hits,
        };
        let bm25_hits = match allowed_exts {
            Some(exts) => filter_hits_by_ext(bm25_hits, exts),
            None => bm25_hits,
        };

        log::debug!("[indexer] hybrid_search query='{}' intent={:?} alpha={:.2} vec_k={} vec_hits={} bm25_k={} bm25_hits={}",
            query, intent, alpha, vec_k, vec_hits.len(), bm25_k, bm25_hits.len());

        let mut fused = fuse_hits(vec_hits, bm25_hits, alpha);
        log::debug!("[indexer] after score fuse: candidates={}", fused.len());

        // ── 代码符号命中注入（代码查询专用）──
        // 语义检索可能漏掉"符号定义"所在 chunk（符号名与语义向量距离远），
        // 此处从查询中提取标识符 token，按符号名精确/前缀匹配补召回，
        // 提升"XX 函数在哪个文件定义"类代码问答的召回率。
        if intent == RetrievalIntent::Code {
            let symbols = extract_symbol_tokens(query);
            let mut seen: std::collections::HashSet<(String, u32)> =
                fused.iter().map(|h| (h.doc_name.clone(), h.chunk_index)).collect();
            let mut added = 0usize;
            for symbol in symbols.into_iter().take(3) {
                match self.search_symbols(dir_path, &symbol, 5).await {
                    Ok(sym_hits) => {
                        for hit in sym_hits {
                            let key = (hit.doc_name.clone(), hit.chunk_index);
                            if seen.insert(key) {
                                fused.push(hit);
                                added += 1;
                            }
                        }
                    }
                    Err(e) => log::debug!("[indexer] 代码符号检索失败（忽略）: {}", e),
                }
            }
            if added > 0 {
                log::debug!("[indexer] 注入代码符号命中: added={}", added);
                fused.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
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

        // ── 代码符号匹配加分 ──
        // 当查询词与 chunk 的 symbol_name（函数名/类名等）匹配时额外加分，
        // 使代码检索时，函数/类定义所在 chunk 获得更高排名。
        // 精确匹配（符号名 == 查询词）：代码查询 0.5 分，普通查询 0.3 分。
        // 部分包含：代码查询 0.25 分，普通查询 0.15 分。
        // 文件概览 chunk（symbol_kind == "file"）匹配文件名时额外加分。
        let has_code_symbols = fused.iter().any(|h| h.symbol_name.is_some() || h.symbol_kind.as_deref() == Some("file"));
        if has_code_symbols && !query_tokens.is_empty() {
            let is_code = intent == RetrievalIntent::Code;
            let (exact_boost, partial_boost) = if is_code { (0.5, 0.25) } else { (0.3, 0.15) };

            for hit in fused.iter_mut() {
                let sym_name = match hit.symbol_name.as_ref() {
                    Some(n) => n,
                    None => {
                        // 文件概览 chunk：匹配 doc_name（即文件名）加分
                        if hit.symbol_kind.as_deref() == Some("file") {
                            let name_lower = hit.doc_name.to_lowercase();
                            let file_matches = query_tokens
                                .iter()
                                .filter(|t| name_lower.contains(*t))
                                .count();
                            if file_matches > 0 {
                                let boost = 0.2 * file_matches as f32;
                                hit.score = (hit.score + boost).min(1.0);
                            }
                        }
                        continue;
                    }
                };
                let sym_lower = sym_name.to_lowercase();
                let query_matches = query_tokens
                    .iter()
                    .filter(|t| sym_lower.contains(*t))
                    .count();
                if query_matches > 0 {
                    // 精确匹配（符号名与查询词完全相同） → 更高加分
                    let exact_match = query_tokens.iter().any(|t| sym_lower == *t);
                    let boost = if exact_match { exact_boost } else { partial_boost };
                    let boost = boost * query_matches as f32;
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

        let mut result: Vec<SearchHit> = fused.into_iter().take(top_k as usize).collect();

        // ── 上下文扩展（Post-Retrieval Context Window） ──
        // 为每个 top_k 结果的相邻 chunks 做上下文合并，填充 sentence_window。
        // 适用于 Markdown（按文档顺序）、OPML/FreeMind（DFS 顺序）所有文档类型。
        if !result.is_empty() {
            let ctx_window = Self::compute_context_window(query);
            for hit in result.iter_mut() {
                match store.fetch_chunks_in_range(&hit.doc_name, hit.chunk_index, ctx_window).await {
                    Ok(chunks) if chunks.len() > 1 => {
                        let mut merged = String::new();
                        let mut has_parent = false;
                        for (idx, text, path_json) in &chunks {
                            // OPML/FreeMind: 检测 path_json 是否为父节点（当前 chunk 路径的前缀）
                            if *idx != hit.chunk_index && !has_parent && path_json.is_some() && hit.path_json.is_some() {
                                if is_path_prefix(path_json.as_deref().unwrap(), hit.path_json.as_deref().unwrap()) {
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
                    _ => {} // 无扩展内容则保持原样
                }
            }
        }

        Ok(result)
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
                id: format!("chat:{}:{}", session.id, i),
                doc_name: session.id.clone(),
                chunk_index: i as u32,
                text: format!("[{}] {}", msg.role, msg.content),
                path_depth: None,
                path_json: None,
                sentence_window: None,
                symbol_name: None,
                symbol_kind: None,
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
        let query_embedding = tokio::task::spawn_blocking(move || utils::call_embedding_query(&query_string))
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

        // 4. 分数加权融合（对话搜索也使用同样的融合权重）
        let config = self.config_store.read();
        let alpha = compute_alpha(query, config.fusion_alpha);
        let fused = fuse_hits(vec_hits, bm25_hits, alpha);

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
                .to_string()
                .replace('\\', "/");

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
        progress(3, &format!("已发现 {} 个文件，正在检查已索引状态...", total));

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

        // 先读取 + 分块所有未索引文件，合并 DocumentChunk
        progress(15, &format!("正在读取 {} 个未索引文件...", unindexed_count));
        let cfg = self.config_store.read();
        let mut all_file_data: Vec<(String, Vec<DocumentChunk>)> = Vec::new(); // (rel_path, chunks)
        let mut total_new_chunks: u32 = 0;
        let mut file_count: u32 = 0;

        for (idx, (rel, abs)) in unindexed_paths.iter().enumerate() {
            let content = match read_file_content(Path::new(abs)) {
                Some(c) if c.len() >= 10 => c,
                _ => continue,
            };
            let ext = rel.rsplit('.').next().unwrap_or("txt");
            let splitter = chunk_splitter_factory().get_splitter(ext);
            let chunks = splitter.split(&content, cfg.chunk_size, cfg.chunk_overlap);
            if chunks.is_empty() {
                continue;
            }
            let doc_chunks = utils::build_document_chunks(rel, &chunks);
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
        let all_vectors = self.embed_batch(&all_chunks, Some(&embed_progress)).await?;

        // 分批写入 LanceDB + BM25
        progress(82, "正在写入数据库...");
        let bm25 = self.get_bm25_index(dir_path).await?;
        for batch_idx in (0..all_chunks.len()).step_by(BATCH_CHUNK_LIMIT) {
            let end = (batch_idx + BATCH_CHUNK_LIMIT).min(all_chunks.len());
            let batch_chunks = &all_chunks[batch_idx..end];
            let batch_vectors = &all_vectors[batch_idx..end];

            store.add_chunks(batch_chunks, batch_vectors).await.map_err(|e| {
                log::error!("[indexer] 增量索引 LanceDB 写入失败: {}", e);
                e
            })?;
            bm25.add_documents(batch_chunks).map_err(|e| {
                log::error!("[indexer] 增量索引 BM25 写入失败: {}", e);
                e
            })?;

            let write_pct = 82 + ((batch_idx + BATCH_CHUNK_LIMIT) * 13 / all_chunks.len().max(1)) as u8;
            progress(write_pct.min(95), &format!("写入数据库 {}/{} 文本块", end, all_chunks.len()));
        }

        // 批量更新元数据
        self.update_metadata_delta(dir_path, file_count as i32, total_new_chunks as i32, all_vectors.len() as i32).await;

        progress(100, &format!("增量索引完成: {} 文件, {} 文本块", file_count, total_new_chunks));
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

/// 向量/BM25 分数加权融合（convex combination），替代纯排名 RRF。
///
/// 融合分数 = α·vec + (1-α)·bm25，两路分数均已归一化到 [0,1]：
/// - 向量分数：余弦相似度 `1 - distance`（LanceDB 已给出）
/// - BM25 分数：按候选池最大值归一化（Bm25Index 已处理）
///
/// 双路命中取加权和；**单路命中直接使用该路自身分数（不折算）**——
/// 因为分数已归一化，折算会把单路命中阈值抬高（如纯向量需 ≥0.75 才过
/// min_score=0.3），系统性丢失"仅一路召回"的相关片段。融合分数始终落在
/// [0,1]，`min_score` 作为绝对阈值有确定语义。元数据（path_json /
/// sentence_window / symbol 等）优先取向量路结果，文本以向量路为基准、
/// BM25 路兜底。
fn fuse_hits(vec_hits: Vec<SearchHit>, bm25_hits: Vec<SearchHit>, alpha: f32) -> Vec<SearchHit> {
    use std::collections::HashMap;

    #[derive(Default)]
    struct Entry {
        score_vec: f32,
        score_bm25: f32,
        text: String,
        path_json: Option<String>,
        sentence_window: Option<String>,
        symbol_name: Option<String>,
        symbol_kind: Option<String>,
    }

    let mut score_map: HashMap<(String, u32), Entry> = HashMap::new();

    // ── 向量结果集 ──
    for hit in vec_hits {
        let key = (hit.doc_name.clone(), hit.chunk_index);
        let entry = score_map.entry(key).or_default();
        entry.score_vec = entry.score_vec.max(hit.score);
        entry.text = hit.text.clone();
        if entry.path_json.is_none() {
            entry.path_json = hit.path_json.clone();
        }
        if entry.sentence_window.is_none() {
            entry.sentence_window = hit.sentence_window.clone();
        }
        if entry.symbol_name.is_none() {
            entry.symbol_name = hit.symbol_name.clone();
            entry.symbol_kind = hit.symbol_kind.clone();
        }
    }

    // ── BM25 结果集 ──
    for hit in bm25_hits {
        let key = (hit.doc_name.clone(), hit.chunk_index);
        let entry = score_map.entry(key).or_default();
        entry.score_bm25 = entry.score_bm25.max(hit.score);
        // 仅当向量路未覆盖此 key 时才用 BM25 的文本
        if entry.text.is_empty() {
            entry.text = hit.text.clone();
        }
        if entry.symbol_name.is_none() {
            entry.symbol_name = hit.symbol_name.clone();
            entry.symbol_kind = hit.symbol_kind.clone();
        }
    }

    // 融合分数：双路命中取加权和；单路命中使用该路自身分数（避免阈值被抬高）
    let fused_score = |e: &Entry| {
        if e.score_vec > 0.0 && e.score_bm25 > 0.0 {
            alpha * e.score_vec + (1.0 - alpha) * e.score_bm25
        } else {
            e.score_vec.max(e.score_bm25)
        }
    };

    let mut entries: Vec<(String, u32, Entry)> = score_map
        .drain()
        .map(|((doc_name, chunk_index), e)| (doc_name, chunk_index, e))
        .collect();

    entries.sort_by(|a, b| {
        fused_score(&b.2)
            .partial_cmp(&fused_score(&a.2))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    entries
        .into_iter()
        .map(|(doc_name, chunk_index, e)| {
            let score = fused_score(&e).clamp(0.0, 1.0);
            SearchHit {
                text: e.text,
                doc_name,
                chunk_index,
                score,
                score_vec: e.score_vec.clamp(0.0, 1.0),
                score_bm25: e.score_bm25.clamp(0.0, 1.0),
                path_json: e.path_json,
                sentence_window: e.sentence_window,
                symbol_name: e.symbol_name,
                symbol_kind: e.symbol_kind,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_intent_detects_code() {
        assert_eq!(route_intent("LRUCache 是怎么实现的"), RetrievalIntent::Code);
        assert_eq!(route_intent("index_all 函数的逻辑"), RetrievalIntent::Code);
        assert_eq!(route_intent("fn main 在哪定义"), RetrievalIntent::Code);
    }

    #[test]
    fn route_intent_detects_outline_and_doc() {
        assert_eq!(route_intent("这个思维导图的大纲结构"), RetrievalIntent::Outline);
        assert_eq!(route_intent("README 文档说明"), RetrievalIntent::Document);
    }

    #[test]
    fn route_intent_defaults_to_general() {
        assert_eq!(route_intent("今天天气怎么样"), RetrievalIntent::General);
        // 动词性"说明"不再误路由为 Document（防止中文代码提问被过滤掉代码文件）
        assert_eq!(route_intent("请说明 fetch 的实现方式"), RetrievalIntent::General);
        // 泛化词"大纲"不再误路由为 Outline（普通文档查询不会被限制到 opml/mm）
        assert_eq!(route_intent("本文大纲是什么"), RetrievalIntent::General);
    }

    #[test]
    fn compute_alpha_handles_cjk_and_short_queries() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-5;
        // 短英文关键词 → 降低向量权重，依赖关键词精确匹配
        assert!(close(compute_alpha("fetch", 0.6), 0.4), "got {}", compute_alpha("fetch", 0.6));
        // 中等长度中文查询不再被误判为"短查询"，保持基准权重
        assert!(close(compute_alpha("索引配置如何热更新", 0.6), 0.6), "got {}", compute_alpha("索引配置如何热更新", 0.6));
        // 较长中文查询（≥14 字）略提高向量权重
        assert!(close(compute_alpha("如何在不重启的情况下热更新索引配置", 0.6), 0.7), "got {}", compute_alpha("如何在不重启的情况下热更新索引配置", 0.6));
        // 超长混合查询略提高向量权重
        assert!(
            close(
                compute_alpha("如何实现一个支持并发读写的缓存 并且能够自动清理过期条目 同时保证线程安全", 0.6),
                0.7
            ),
            "got {}",
            compute_alpha("如何实现一个支持并发读写的缓存 并且能够自动清理过期条目 同时保证线程安全", 0.6)
        );
    }

    #[test]
    fn fuse_hits_weights_both_sources() {
        let v = SearchHit {
            text: "t".into(),
            doc_name: "a.rs".into(),
            chunk_index: 0,
            score: 0.8,
            score_vec: 0.8,
            score_bm25: 0.0,
            path_json: None,
            sentence_window: None,
            symbol_name: None,
            symbol_kind: None,
        };
        let b = SearchHit {
            text: "t".into(),
            doc_name: "a.rs".into(),
            chunk_index: 0,
            score: 0.6,
            score_vec: 0.0,
            score_bm25: 0.6,
            path_json: None,
            sentence_window: None,
            symbol_name: None,
            symbol_kind: None,
        };
        // 双路命中：0.6*0.8 + 0.4*0.6 = 0.72
        let fused = fuse_hits(vec![v], vec![b], 0.6);
        assert_eq!(fused.len(), 1);
        assert!((fused[0].score - 0.72).abs() < 1e-5, "融合分数应为 0.72, got {}", fused[0].score);
    }

    #[test]
    fn fuse_hits_single_source_keeps_own_score() {
        // 仅向量命中：不再按 α 折算，保留自身分数（避免纯向量命中被 min_score 误过滤）
        let vec_only = SearchHit {
            text: "v".into(),
            doc_name: "a.md".into(),
            chunk_index: 0,
            score: 0.5,
            score_vec: 0.5,
            score_bm25: 0.0,
            path_json: None,
            sentence_window: None,
            symbol_name: None,
            symbol_kind: None,
        };
        let fused = fuse_hits(vec![vec_only], Vec::new(), 0.6);
        assert!(
            (fused[0].score - 0.5).abs() < 1e-5,
            "单路向量命中不应被折算, got {}",
            fused[0].score
        );

        // 仅 BM25 命中：同样保留自身分数
        let bm25_only = SearchHit {
            text: "b".into(),
            doc_name: "b.md".into(),
            chunk_index: 0,
            score: 0.6,
            score_vec: 0.0,
            score_bm25: 0.6,
            path_json: None,
            sentence_window: None,
            symbol_name: None,
            symbol_kind: None,
        };
        let fused = fuse_hits(Vec::new(), vec![bm25_only], 0.6);
        assert!(
            (fused[0].score - 0.6).abs() < 1e-5,
            "单路 BM25 命中不应被折算, got {}",
            fused[0].score
        );
    }

    #[test]
    fn filter_hits_by_ext_keeps_matching_only() {
        let mk = |doc: &str| SearchHit {
            text: "t".into(),
            doc_name: doc.into(),
            chunk_index: 0,
            score: 0.5,
            score_vec: 0.5,
            score_bm25: 0.0,
            path_json: None,
            sentence_window: None,
            symbol_name: None,
            symbol_kind: None,
        };
        let hits = vec![mk("src/a.rs"), mk("docs/readme.md"), mk("notes/outline.opml")];
        let filtered = filter_hits_by_ext(hits, &["rs"]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].doc_name, "src/a.rs");
    }
}