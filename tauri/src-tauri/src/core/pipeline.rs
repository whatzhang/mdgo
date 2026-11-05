//! Pipeline 模块：将「文件 → 索引」拆分为独立可复用的阶段。
//!
//! ```text
//! File → document_stage → chunk_stage → embedding_stage → index_stage
//! ```
//!
//! 收益：
//! - `index_all` / `index_file` / `index_unindexed` / Watcher 批量索引共用同一管线，
//!   消除三处重复编排
//! - 未来扩展（OCR、图片理解、实体抽取、摘要）只需替换/插入 stage，不侵入核心
//!
//! 阶段说明：
//! - `read_document`：读取文件内容（PDF 提取 / UTF-8 文本）＝ document_stage
//! - `chunk_document`：按扩展名分块并组装 `DocumentChunk` ＝ chunk_stage
//! - `embed_chunks`：批量向量化（优先 `embedding_text`，退化 `text`）＝ embedding_stage
//! - `write_chunks`：写入 LanceDB + BM25 ＝ index_stage

use std::sync::atomic::{AtomicU64, Ordering};

use crate::core::db::bm25::Bm25Index;
use crate::core::db::chunk_splitter::ChunkSplitterFactory;
use crate::core::db::lance::{DocumentChunk, LanceStore};
use crate::core::db::token_budget::{self, ValidationReport};
use crate::core::db::utils;
use crate::core::db::utils::IgnoreMatcher;

/// 全局 ChunkSplitter 工厂（懒初始化，线程安全）
static CHUNK_SPLITTER_FACTORY: std::sync::OnceLock<ChunkSplitterFactory> =
    std::sync::OnceLock::new();
pub(crate) fn chunk_splitter_factory() -> &'static ChunkSplitterFactory {
    CHUNK_SPLITTER_FACTORY.get_or_init(ChunkSplitterFactory::new)
}

// ─── 文档装载层唯一入口（方案 §4.1：索引与预览共用）───
//
// 其余类型（`DocStatus`/`PageDiagnostic`/`PageSpan`/`ConverterInfo`/`MIN_DOC_BYTES`）
// 直接引用 `core::document::loader`，不经本模块再导出，避免产生"看似有消费者"的空转导出。

pub use crate::core::document::loader::{load_document, DocumentSource, SkipReason};

// ─── 跳过原因收集（N3 可观测性；镜像 budget_stats 的「基线差分」范式）───
//
// 与 TRUNCATED_/RESPLIT_ 同理：`index_file`/`index_files_batch`（watcher 路径，不持
// indexing_lock）也会记录跳过，硬清零会让 index_all 的窗口混入窗口外数据；
// 基线快照使窗口语义与并发无关。

/// 详情上限（方案 §5.3：`skipped_files` 最多 100 条，超出只计数）
pub const SKIP_DETAIL_LIMIT: usize = 100;

/// 单条跳过记录（前端「为什么这个文件没进库」面板消费）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkippedFile {
    pub rel_path: String,
    /// 机器可读码（`SkipReason::code()`）
    pub code: String,
    /// 中文说明（`SkipReason::message()`）
    pub reason: String,
    /// 需 OCR 的页号（仅 needs_ocr；1-indexed）
    #[serde(default)]
    pub pages: Vec<u32>,
}

static SKIP_TOTAL: AtomicU64 = AtomicU64::new(0);
static SKIP_TOTAL_BASE: AtomicU64 = AtomicU64::new(0);
static SKIP_DETAILS: std::sync::Mutex<Vec<SkippedFile>> = std::sync::Mutex::new(Vec::new());
static SKIP_DETAILS_BASE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// 重置跳过统计基线（索引窗口开始）
pub fn reset_skip_stats() {
    SKIP_TOTAL_BASE.store(SKIP_TOTAL.load(Ordering::Relaxed), Ordering::Relaxed);
    let len = SKIP_DETAILS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len();
    SKIP_DETAILS_BASE.store(len, Ordering::Relaxed);
}

/// 记录一条跳过（详情超上限只计数）
pub fn record_skip(rel_path: &str, reason: &SkipReason) {
    SKIP_TOTAL.fetch_add(1, Ordering::Relaxed);
    let mut guard = SKIP_DETAILS.lock().unwrap_or_else(|e| e.into_inner());
    if guard.len() < SKIP_DETAIL_LIMIT {
        let pages = match reason {
            SkipReason::NeedsOcr { pages, .. } => pages.clone(),
            _ => Vec::new(),
        };
        guard.push(SkippedFile {
            rel_path: rel_path.to_string(),
            code: reason.code().to_string(),
            reason: reason.message(),
            pages,
        });
    }
}

/// 读取窗口内的跳过统计：`(总数, 详情列表)`
pub fn skip_stats() -> (u32, Vec<SkippedFile>) {
    let total = SKIP_TOTAL
        .load(Ordering::Relaxed)
        .saturating_sub(SKIP_TOTAL_BASE.load(Ordering::Relaxed));
    let base = SKIP_DETAILS_BASE.load(Ordering::Relaxed);
    let guard = SKIP_DETAILS.lock().unwrap_or_else(|e| e.into_inner());
    let from = base.min(guard.len());
    (total as u32, guard[from..].to_vec())
}

// ─── 部分索引统计（Plan B v2 §5.3 / §7.2 F9）───
//
// 与 `skipped_files` **语义不同、必须分开显示**：
//   - `skipped_files`：整个文件没进库（PDF 全篇扫描件、加密、超限……）
//   - `partial_files`：文件**进了库**，但有若干页被跳过（Q8 决策）
// 混在一起会让用户以为"这个文件没被索引"，而实际上大部分内容是可检索的。
//
// 与 skip 同一套基线差分设计（见上方 skip 区块的说明）。

/// 页级诊断（`PageDiagnostic` 的对外可序列化形态）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PageDiagItem {
    pub page: u32,
    pub code: String,
    pub detail: String,
}

/// 部分索引记录（前端「哪些页没进库」面板消费）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PartialFile {
    pub rel_path: String,
    /// 被跳过的页号（1-indexed）
    pub skipped_pages: Vec<u32>,
    /// 文档总页数（已入库页 + 跳过页）
    pub page_count: u32,
    /// 页级诊断（为什么这几页没有）
    pub diagnostics: Vec<PageDiagItem>,
}

static PARTIAL_TOTAL: AtomicU64 = AtomicU64::new(0);
static PARTIAL_TOTAL_BASE: AtomicU64 = AtomicU64::new(0);
static PARTIAL_DETAILS: std::sync::Mutex<Vec<PartialFile>> = std::sync::Mutex::new(Vec::new());
static PARTIAL_DETAILS_BASE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// 重置部分索引统计基线（索引窗口开始）
pub fn reset_partial_stats() {
    PARTIAL_TOTAL_BASE.store(PARTIAL_TOTAL.load(Ordering::Relaxed), Ordering::Relaxed);
    let len = PARTIAL_DETAILS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len();
    PARTIAL_DETAILS_BASE.store(len, Ordering::Relaxed);
}

/// 记录一个部分索引文件（仅当确实有页被跳过时调用；详情超上限只计数）
pub fn record_partial(rel_path: &str, skipped_pages: &[u32], extracted_pages: u32, doc: &crate::core::document::loader::DocumentSource) {
    PARTIAL_TOTAL.fetch_add(1, Ordering::Relaxed);
    let mut guard = PARTIAL_DETAILS.lock().unwrap_or_else(|e| e.into_inner());
    if guard.len() >= SKIP_DETAIL_LIMIT {
        return;
    }
    guard.push(PartialFile {
        rel_path: rel_path.to_string(),
        skipped_pages: skipped_pages.to_vec(),
        page_count: extracted_pages + skipped_pages.len() as u32,
        diagnostics: doc
            .page_diagnostics
            .iter()
            .map(|d| PageDiagItem {
                page: d.page,
                code: d.code.to_string(),
                detail: d.detail.clone(),
            })
            .collect(),
    });
}

/// 读取窗口内的部分索引统计：`(总数, 详情列表)`
pub fn partial_stats() -> (u32, Vec<PartialFile>) {
    let total = PARTIAL_TOTAL
        .load(Ordering::Relaxed)
        .saturating_sub(PARTIAL_TOTAL_BASE.load(Ordering::Relaxed));
    let base = PARTIAL_DETAILS_BASE.load(Ordering::Relaxed);
    let guard = PARTIAL_DETAILS.lock().unwrap_or_else(|e| e.into_inner());
    let from = base.min(guard.len());
    (total as u32, guard[from..].to_vec())
}

// ─── Token Budget 统计（P0-1 可观测性）───//
// chunk_document 是全部索引路径的唯一汇聚点；Validator 的截断/重切统计在此累加，
// 由 index_all / index_unindexed 在索引窗口内 reset/read（索引由 indexing_lock 串行化，
// 无并发窗口；watcher 批量路径不产出 KbIndexResult，无需读取）。

static TRUNCATED_CHUNKS: AtomicU64 = AtomicU64::new(0);
static RESPLIT_CHUNKS: AtomicU64 = AtomicU64::new(0);
// 🟠 L6：基线快照（reset 记录、读取返回窗口内增量）——index_file（watcher 路径，
// 不持 indexing_lock）也会经 chunk_document 累加全局计数；硬清零会让 index_all /
// index_unindexed 的窗口混入窗口外数据，快照差分使窗口语义与并发无关。
static TRUNCATED_BASE: AtomicU64 = AtomicU64::new(0);
static RESPLIT_BASE: AtomicU64 = AtomicU64::new(0);

/// 重置统计基线（索引窗口开始；embedding 层兜底截断同样按基线差分）
pub fn reset_budget_stats() {
    TRUNCATED_BASE.store(TRUNCATED_CHUNKS.load(Ordering::Relaxed), Ordering::Relaxed);
    RESPLIT_BASE.store(RESPLIT_CHUNKS.load(Ordering::Relaxed), Ordering::Relaxed);
    crate::core::embedding::reset_embedding_truncated_count();
}

/// 读取窗口内统计（当前值 − 基线；截断数含 embedding 层兜底截断）。
///
/// 🟠 L5：口径说明——`truncated` 可能对**同一原子块双计**：Validator 显式降级计 1 次
/// （`db/token_budget.rs`），原样通过后在 embedding 层又被截断计数 1 次
/// （`core/embedding.rs` 兜底）。健康态 0 不变；非零时数字偏大属已知口径，
/// 不影响"非 0 即需检查"的告警语义。
pub fn budget_stats() -> (u64, u64) {
    let truncated = TRUNCATED_CHUNKS
        .load(Ordering::Relaxed)
        .saturating_sub(TRUNCATED_BASE.load(Ordering::Relaxed))
        + crate::core::embedding::embedding_truncated_count();
    let resplit = RESPLIT_CHUNKS
        .load(Ordering::Relaxed)
        .saturating_sub(RESPLIT_BASE.load(Ordering::Relaxed));
    (truncated, resplit)
}

/// 汇总一次 Validator 报告进全局统计
fn accumulate_report(report: &ValidationReport) {
    TRUNCATED_CHUNKS.fetch_add(report.truncated_count as u64, Ordering::Relaxed);
    RESPLIT_CHUNKS.fetch_add(report.resplit_count as u64, Ordering::Relaxed);
}

// `read_document` 已由 `core::document::loader::load_document` 取代（Phase 0B / N8）。
//
// 旧签名 `-> Option<String>` 的两个结构性问题：
//   ① 失败不带原因（`.docx` 只能打一条"非 UTF-8 编码"日志）→ 现为 `SkipReason` + 跳过统计；
//   ② 返回裸 String，provenance（page 等）无处安放 → 现为 `DocumentSource`。
//
// 迁移对照（旧 → 新）：
//   pdf  → Converter::LegacyPdf（Phase 1 换 pdf-inspector）
//   其余 → Converter::Plain（UTF-8 直读，非 UTF-8 报 NotUtf8 而非静默跳过）

/// chunk_stage：按**内容形态**选择分块器，分块并组装 `DocumentChunk`。
///
/// `src.form` 来自 [crate::core::document::filekind] 注册表（Phase 0B / C3）：
/// 不再按源扩展名硬编码判断，转换后的 PDF/Office 因此能复用 Markdown AST 分块。
///
/// `html_render_matcher`：可选「HTML 渲染目录」匹配器（gitignore 格式，来自
/// 设置 `htmlCodeShowBlacklist`）。语义：命中该目录的 HTML 作为**文档**语义分块
/// （`HtmlChunkSplitter`）；**未命中的 HTML 直接放弃（返回空，不索引）**——
/// 不识别为代码；`None`（未配置）时保持现状（全部 HTML 按文档分块，兼容旧行为）。
pub fn chunk_document(
    src: &DocumentSource,
    chunk_size: usize,
    chunk_overlap: usize,
    html_render_matcher: Option<&IgnoreMatcher>,
) -> Vec<DocumentChunk> {
    let rel_path = src.rel_path.as_str();
    let content = src.text.as_str();
    // 扩展名可能为空（Dockerfile/Makefile 这类纯文件名规则），此时分块器走 Plain
    let ext = crate::core::document::filekind::ext_of(rel_path).unwrap_or("");
    let is_html = src.form == crate::core::document::filekind::DocumentForm::Html;
    // 等价旧 `is_markdown_ext`：注册表 doc_like && form==Markdown（md/markdown/mdown/rst）
    let is_md = src.frontmatter;

    // P0-1：Markdown 类文件先解析 FrontMatter（tags/aliases/title 重新纳入检索）。
    // 元数据仅用于 BM25 title/tags 字段与 chunk 身份，不进入 embedding 文本。
    let mut fm_title: Option<String> = None;
    let mut fm_tags: Vec<String> = Vec::new();
    let body_owned;
    let body: &str = if is_md {
        let (meta, body) = crate::core::document::markdown::parse_frontmatter(content);
        if let Some(meta) = meta {
            fm_title = meta.title.filter(|t| !t.is_empty());
            let mut tags = meta.tags;
            tags.extend(meta.aliases);
            tags.sort();
            tags.dedup();
            fm_tags = tags;
        }
        body_owned = body;
        body_owned.as_str()
    } else {
        content
    };

    // v2：Mark 标注/备注 HTML 入库前清洗（仅 Markdown 类文件，解析前正则剥离标签保留文本）
    let cleaned_owned;
    let cleaned: &str = if is_md {
        cleaned_owned = crate::core::document::html_clean::strip_custom_html_tags(body);
        cleaned_owned.as_str()
    } else {
        body
    };

    // Phase 4：EPUB 富化会把图片写成 `![alt](mdgoasset://local/<文件名>)`——那是**渲染用**的
    // 本地资源地址。而 Markdown 分块的 chunk 文本取的是**源码行切片**（`document/markdown.rs`
    // 的 sourcepos 切片，不是 inline 纯文本），URL 因此会原样进入 BM25 文本与 embedding 输入：
    // 一本书几十张图就是几十段无意义词项，既污染关键词检索又白占 token 预算。
    // 这里把图片**目标地址清空**（`![alt]()`）：alt 文本（真正的检索价值）保留、
    // AST 形态与 `chunk_type` 不变、预览渲染不受影响（预览用的是原始 `src.text`）。
    //
    // 只作用于 `source_kind == "epub"`：普通 Markdown 的图片路径属于**用户自己的内容**，
    // 动它会改变既有索引文本并触发全量重建，收益与风险都不成比例，故保持原行为。
    let epub_owned;
    let cleaned: &str = if src.source_kind == "epub" {
        epub_owned = crate::core::document::epub::strip_image_destinations(cleaned);
        epub_owned.as_str()
    } else {
        cleaned
    };
    let splitter = if is_html {
        match html_render_matcher {
            // 已配置渲染目录：命中 → 文档分块；未命中 → 放弃该文件（不索引）
            Some(m) if !m.matches(rel_path) => return Vec::new(),
            _ => chunk_splitter_factory().get_splitter("html"),
        }
    } else {
        chunk_splitter_factory().get_splitter(ext)
    };
    // Phase 1：页 provenance 注入守卫。
    //
    // 上面的 frontmatter 剥离 / HTML 清洗会改变文本行号（`cleaned != content`），
    // 此时 loader 构造的「行 → 页」映射会**系统性错位**，因此只在文本**逐字节未变**时注入。
    // PDF 转换产物不走这两步（`is_md = false`，`cleaned` 与 `content` 是同一个切片），
    // 故恒成立；Markdown 家族虽然可能通过长度比较，但它们的 `line_page_map` 恒为空。
    // 这条守卫把"变换"与"provenance 映射"绑定在一起，避免未来新增预处理时静默错页。
    //
    // 用 `cleaned == content`（内容相等）而非 `len() == len()`（长度相等）：长度相等是
    // 前者的**弱化代理**，等长替换（如标签被替换成同长文本）会骗过它从而错页；正文已在
    // 内存中，一次 memcmp 的代价相对转换本身可忽略。
    //
    // ⚠ 第二道预检：上面的 frontmatter 剥离只是**本函数内**的变换；分块器内部
    // （`document::markdown::ComrakMarkdownParser::parse`）还会再做两步——
    //   · `\r\n → \n` 归一：**不改变行数**，而页映射是行号制，故安全；
    //   · `strip_frontmatter`：会**删掉开头若干行** → 行号整体前移 → 静默错页。
    // 触发条件极窄（PDF 转换产物首行恰为 `---` 且前 50 行内出现 `键: 值`），但后果是
    // 页码系统性偏移且无任何报错，所以这里用**同一个判定函数**预检：只有当剥离结果与
    // 输入逐字节一致（= 未发生剥离）时才注入页映射。
    let page_map: &[crate::core::document::loader::LineSpan] =
        if !src.line_page_map.is_empty()
            && cleaned == content
            && crate::core::document::markdown::parse_frontmatter(cleaned).1 == cleaned
        {
            &src.line_page_map
        } else {
            &[]
        };
    // 默认实现忽略 pages（非分页格式行为不变）
    let chunks = splitter.split_with_pages(cleaned, chunk_size, chunk_overlap, page_map);
    if chunks.is_empty() {
        return Vec::new();
    }

    // ── P0-1：Chunk Normalizer + Token Budget Validator（最终裁决）──
    // 所有索引路径的唯一强制点：规范化 → 预算校验 → 超限重切 / 显式降级。
    // 任何进入 embedding 的 embedding_text 都必须通过预算（硬上限 = 模型窗口 - 预留）。
    let normalized = token_budget::normalize_chunks(chunks);
    let validator = token_budget::TokenBudgetValidator::new(
        token_budget::budget_from_config(chunk_size, chunk_overlap),
        token_budget::global_token_counter(),
    );
    let (mut validated, report) = validator.validate(normalized);
    if report.degraded_token_count {
        log::warn!(
            "[pipeline] tokenizer 未就绪，分块预算降级为字符估算（{}）: {}",
            rel_path,
            report.chunks_in
        );
    }
    accumulate_report(&report);
    if report.truncated_count > 0 {
        log::warn!(
            "[pipeline] {} 个 chunk 超限且无法重切，已显式降级截断（健康态应为 0）: {}",
            report.truncated_count,
            rel_path
        );
    }

    // P0-1：注入 FrontMatter 元数据（doc_title / tags）——所有 chunk 共享文档级元数据
    if fm_title.is_some() || !fm_tags.is_empty() {
        for c in validated.iter_mut() {
            if c.doc_title.is_none() {
                c.doc_title = fm_title.clone();
            }
            if c.tags.is_none() && !fm_tags.is_empty() {
                c.tags = Some(fm_tags.clone());
            }
        }
    }

    utils::build_document_chunks(src, &validated)
}

/// embedding_stage：批量向量化。
///
/// 向量化文本优先取 `chunk.embedding_text`（AST 语义分块的紧凑标题路径 + 正文），
/// 无则退化用 `chunk.text`（代码/OPML 等仍按原始文本向量化）。
///
/// P0-5：内容哈希缓存——`cache_dir` 非空时启用（传 `utils::get_cache_dir(dir_path)`），
/// 命中缓存的 chunk 跳过推理，只对变化 chunk 调用 embedding。
pub async fn embed_chunks(
    chunks: &[DocumentChunk],
    progress: Option<&(dyn Fn(usize, usize, &str) + Send + Sync)>,
    cache_dir: &str,
) -> Result<Vec<Vec<f32>>, String> {
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    log::info!(
        "[pipeline] 【Embedding 批量处理】 开始，共 {} 个文本块",
        chunks.len()
    );

    let texts: Vec<String> = chunks
        .iter()
        .map(|c| {
            c.embedding_text
                .clone()
                .unwrap_or_else(|| c.text.clone())
        })
        .collect();

    // ── P0-5：缓存键（model|dim|content_hash）→ 命中跳过推理 ──
    let mut cache = None;
    if !cache_dir.is_empty() {
        // 🟠 L13：按目录复用连接（open_shared），避免每批次 open+create_dir_all
        match crate::core::db::embedding_cache::EmbeddingCache::open_shared(cache_dir) {
            Ok(c) => cache = Some(c),
            Err(e) => log::warn!("[pipeline] embedding 缓存不可用（降级全量推理）: {}", e),
        }
    }
    let keys: Vec<Option<String>> = match &cache {
        Some(c) => texts
            .iter()
            .map(|t| Some(c.key(&crate::core::db::embedding_cache::EmbeddingCache::content_hash(t))))
            .collect(),
        None => vec![None; texts.len()],
    };
    let cached: HashMap<String, Vec<f32>> = match &cache {
        Some(c) => {
            let present: Vec<String> = keys.iter().flatten().cloned().collect();
            c.get_many(&present).unwrap_or_default()
        }
        None => HashMap::new(),
    };

    // 仅对未命中文本推理（保持 miss 顺序与 miss_indices 对应）
    let mut miss_indices: Vec<usize> = Vec::new();
    let mut miss_texts: Vec<&str> = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        let hit = k.as_ref().map(|k| cached.contains_key(k)).unwrap_or(false);
        if !hit {
            miss_indices.push(i);
            miss_texts.push(&texts[i]);
        }
    }

    let mut new_vectors: Vec<Vec<f32>> = Vec::new();
    if !miss_texts.is_empty() {
        log::info!(
            "[pipeline] 【Embedding】 缓存命中 {} 条，需推理 {} 条",
            texts.len() - miss_texts.len(),
            miss_texts.len()
        );
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<(usize, usize, String)>();

        // 启动阻塞任务进行嵌入（闭包持有 miss 文本的 owned 副本，避免借用逃逸）
        let miss_texts_owned: Vec<String> = miss_texts.iter().map(|s| s.to_string()).collect();
        let mut handle = tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = miss_texts_owned.iter().map(|s| s.as_str()).collect();
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
                        Err(e) => Err(format!("Embedding 任务执行失败, error={}", e)),
                    });
                }
            }
        }
        new_vectors = result.unwrap()?;

        // 写缓存（失败不影响本次索引）
        if let Some(c) = &cache {
            // 🔴 修复：必须按 miss 下标配对（keys 覆盖全部 texts，new_vectors 只含未命中——
            // 旧实现 keys.zip(new_vectors) 会把「命中 key」配错向量并覆盖正确缓存条目）
            let entries = cache_entries_from_misses(&keys, &miss_indices, &new_vectors);
            if let Err(e) = c.put_many(&entries) {
                log::warn!("[pipeline] embedding 缓存写入失败（不影响本次索引）: {}", e);
            }
        }
    } else {
        log::info!("[pipeline] 【Embedding】 全部 {} 条命中缓存，跳过推理", texts.len());
    }

    // 按原输入顺序组装结果（命中取缓存，未命中取新向量）
    let mut miss_iter = new_vectors.into_iter();
    let mut result = Vec::with_capacity(texts.len());
    for (i, _) in texts.iter().enumerate() {
        match &keys[i] {
            Some(k) => {
                if let Some(v) = cached.get(k) {
                    result.push(v.clone());
                } else if let Some(v) = miss_iter.next() {
                    result.push(v);
                } else {
                    return Err("embedding 结果与输入不一致（缓存与推理错位）".into());
                }
            }
            None => {
                if let Some(v) = miss_iter.next() {
                    result.push(v);
                } else {
                    return Err("embedding 结果与输入不一致（推理数量不足）".into());
                }
            }
        }
    }

    log::info!(
        "[pipeline] 【Embedding 批量处理】 完成，共 {} 个向量",
        result.len()
    );
    Ok(result)
}

/// 组装缓存回填条目：只写「未命中 key ↔ 本次推理向量」对（按 miss 下标对齐）。
///
/// `keys` 覆盖全部文本（缓存启用时全为 `Some`），`new_vectors` 只含未命中文本的向量
/// （顺序与 `miss_indices` 一一对应）；按位置 zip 会把「命中 key」配到其他文本的向量上，
/// 覆盖正确缓存条目（🔴-1 回归测试见 `tests::cache_entries_from_misses_aligns_by_miss_index`）。
fn cache_entries_from_misses(
    keys: &[Option<String>],
    miss_indices: &[usize],
    new_vectors: &[Vec<f32>],
) -> Vec<(String, Vec<f32>)> {
    miss_indices
        .iter()
        .zip(new_vectors.iter())
        .filter_map(|(&i, v)| keys[i].clone().map(|k| (k, v.clone())))
        .collect()
}

/// index_stage：写入 LanceDB + BM25。
pub async fn write_chunks(
    store: &LanceStore,
    bm25: &Bm25Index,
    chunks: &[DocumentChunk],
    vectors: &[Vec<f32>],
) -> Result<(), String> {
    if chunks.is_empty() {
        return Ok(());
    }
    store.add_chunks(chunks, vectors).await?;
    bm25.add_documents(chunks)?;
    Ok(())
}

// ─── P0-1 测试：FrontMatter 元数据注入 ───

#[cfg(test)]
mod tests {
    use super::*;

    /// **前端契约**：`SkippedFile` / `PartialFile` 的 JSON 字段名是前端 `main.html`
    /// 直接读取的（`rel_path` / `reason` / `pages` / `skipped_pages` / `page_count` /
    /// `diagnostics[].detail`）。它们没有 `rename_all`，改字段名等于静默打断 UI——
    /// 这里把键名钉死，改名必须同步改前端。
    #[test]
    fn diagnostic_dtos_json_keys_are_frontend_contract() {
        let skipped = SkippedFile {
            rel_path: "scan.pdf".into(),
            code: "needs_ocr".into(),
            reason: "扫描件".into(),
            pages: vec![1, 2],
        };
        let sj: serde_json::Value = serde_json::to_value(&skipped).unwrap();
        assert_eq!(sj["rel_path"], "scan.pdf");
        assert_eq!(sj["code"], "needs_ocr");
        assert_eq!(sj["reason"], "扫描件");
        assert_eq!(sj["pages"], serde_json::json!([1, 2]));

        let partial = PartialFile {
            rel_path: "mixed.pdf".into(),
            skipped_pages: vec![4],
            page_count: 6,
            diagnostics: vec![PageDiagItem {
                page: 4,
                code: "needs_ocr".into(),
                detail: "该页无可提取文本".into(),
            }],
        };
        let pj: serde_json::Value = serde_json::to_value(&partial).unwrap();
        assert_eq!(pj["rel_path"], "mixed.pdf");
        assert_eq!(pj["skipped_pages"], serde_json::json!([4]));
        assert_eq!(pj["page_count"], 6);
        assert_eq!(pj["diagnostics"][0]["page"], 4);
        assert_eq!(pj["diagnostics"][0]["detail"], "该页无可提取文本");

        // 反序列化（IndexMeta 持久化路径）必须等价往返
        let back: PartialFile = serde_json::from_value(pj).unwrap();
        assert_eq!(back.skipped_pages, vec![4]);
        assert_eq!(back.page_count, 6);
        let back_s: SkippedFile = serde_json::from_value(sj).unwrap();
        assert_eq!(back_s.pages, vec![1, 2]);
    }

    /// 部分索引采集器：只记确实有页被跳过的文件，并保留页号
    #[test]
    fn record_partial_collects_skipped_pages() {
        reset_partial_stats();
        let src = DocumentSource::for_test("mixed.pdf", "正文");
        record_partial("mixed.pdf", &[3], 2, &src);
        let (total, details) = partial_stats();
        assert_eq!(total, 1, "应记 1 个部分索引文件");
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].rel_path, "mixed.pdf");
        assert_eq!(details[0].skipped_pages, vec![3]);
        assert_eq!(
            details[0].page_count, 3,
            "总页数 = 已提取页(2) + 跳过页(1)"
        );
        // 基线差分：重置后窗口内无新增
        reset_partial_stats();
        let (total2, details2) = partial_stats();
        assert_eq!(total2, 0, "重置基线后窗口内应无记录");
        assert!(details2.is_empty());
    }

    /// V1 闭环：markdown frontmatter → doc_title/tags 注入所有 chunk（BM25 title/tags 字段消费）
    #[test]
    fn chunk_document_injects_frontmatter_metadata() {
        let md = "---\ntitle: Redis 连接池手册\ntags:\n  - redis\n  - 运维\naliases:\n  - Redis Pool\n---\n# 正文\n连接池配置说明内容段落。";
        let chunks = chunk_document(&DocumentSource::for_test("notes/redis.md", md), 448, 56, None);
        assert!(!chunks.is_empty(), "应产出 chunk");
        for c in &chunks {
            assert_eq!(
                c.doc_title.as_deref(),
                Some("Redis 连接池手册"),
                "frontmatter title 应注入"
            );
            let tags: Vec<String> =
                serde_json::from_str(c.tags.as_deref().unwrap_or("[]")).unwrap_or_default();
            assert!(
                tags.contains(&"redis".to_string()) && tags.contains(&"Redis Pool".to_string()),
                "tags + aliases 应注入: {:?}",
                tags
            );
        }
    }

    /// 无 frontmatter 的 markdown：doc_title/tags 为空，不报错
    #[test]
    fn chunk_document_without_frontmatter_ok() {
        let md = "# 普通文档\n\n没有 frontmatter 的正文内容段落。";
        let chunks = chunk_document(&DocumentSource::for_test("notes/plain.md", md), 448, 56, None);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|c| c.doc_title.is_none() && c.tags.is_none()));
    }

    /// 非 markdown 文件（代码/纯文本）：不解析 frontmatter，字段为空
    #[test]
    fn chunk_document_non_markdown_no_metadata() {
        let code = "fn main() {\n    let x = 1;\n}\n";
        let chunks = chunk_document(&DocumentSource::for_test("src/main.rs", code), 448, 56, None);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|c| c.doc_title.is_none() && c.tags.is_none()));
    }

    /// 🔴-1 回归：缓存回填必须按 miss 下标对齐——「命中 key」不得被配到其他文本的向量上
    /// （旧实现 keys.zip(new_vectors) 在混合命中批次下覆盖正确缓存条目，污染向量库）。
    #[test]
    fn cache_entries_from_misses_aligns_by_miss_index() {
        // 模拟部分命中：texts = [A(命中), B(未命中), C(命中), D(未命中)]
        let keys: Vec<Option<String>> = vec![
            Some("kA".into()),
            Some("kB".into()),
            Some("kC".into()),
            Some("kD".into()),
        ];
        let miss_indices = vec![1usize, 3];
        let new_vectors = vec![vec![1.0], vec![3.0]]; // B、D 的推理向量

        let entries = cache_entries_from_misses(&keys, &miss_indices, &new_vectors);

        // 必须为 (kB→B 向量) 与 (kD→D 向量)；不得出现 (kA→B 向量) 等错配
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "kB");
        assert_eq!(entries[0].1, vec![1.0]);
        assert_eq!(entries[1].0, "kD");
        assert_eq!(entries[1].1, vec![3.0]);
    }

    /// 🔴-1 补充：全部命中 / 全部未命中两个极端批次也正确（zip 错位仅混合批次暴露）
    #[test]
    fn cache_entries_from_misses_all_hit_or_all_miss() {
        // 全部命中：无推理向量，无回填条目
        let keys_all_hit: Vec<Option<String>> = vec![Some("kA".into()), Some("kB".into())];
        assert!(
            cache_entries_from_misses(&keys_all_hit, &[], &[]).is_empty(),
            "全命中不产生回填"
        );
        // 全部未命中：按顺序配对
        let keys_all_miss: Vec<Option<String>> = vec![Some("kA".into()), Some("kB".into())];
        let entries = cache_entries_from_misses(&keys_all_miss, &[0, 1], &[vec![1.0], vec![2.0]]);
        assert_eq!(entries[0], ("kA".to_string(), vec![1.0]));
        assert_eq!(entries[1], ("kB".to_string(), vec![2.0]));
    }
}
