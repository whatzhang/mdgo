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

use std::path::Path;
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

// ─── Token Budget 统计（P0-1 可观测性）───
//
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

/// document_stage：读取文件内容。
///
/// PDF 走 pdf-extract 提取；其余按 UTF-8 读取。失败返回 None（由调用方跳过）。
pub fn read_document(path: &Path) -> Option<String> {
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
                    log::warn!(
                        "[pipeline] 跳过 PDF 文件 {}: 未提取到文本内容",
                        path.display()
                    );
                    None
                } else {
                    Some(trimmed.to_string())
                }
            }
            Err(e) => {
                log::warn!(
                    "[pipeline] 跳过 PDF 文件 {}: 提取文本失败: {}",
                    path.display(),
                    e
                );
                None
            }
        };
    }

    // 非 PDF 文件（或 pdf-extract 未启用）：读取 UTF-8 文本
    match std::fs::read_to_string(path) {
        Ok(c) => Some(c),
        Err(e) => {
            log::warn!(
                "[pipeline] 跳过文件 {}: {}",
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

/// chunk_stage：按扩展名选择分块器，分块并组装 `DocumentChunk`。
///
/// `html_render_matcher`：可选「HTML 渲染目录」匹配器（gitignore 格式，来自
/// 设置 `htmlCodeShowBlacklist`）。语义：命中该目录的 HTML 作为**文档**语义分块
/// （`HtmlChunkSplitter`）；**未命中的 HTML 直接放弃（返回空，不索引）**——
/// 不识别为代码；`None`（未配置）时保持现状（全部 HTML 按文档分块，兼容旧行为）。
pub fn chunk_document(
    rel_path: &str,
    content: &str,
    chunk_size: usize,
    chunk_overlap: usize,
    html_render_matcher: Option<&IgnoreMatcher>,
) -> Vec<DocumentChunk> {
    let ext = rel_path.rsplit('.').next().unwrap_or("txt");
    // D7：扩展名大小写不敏感（index.HTML 与 index.html 同语义，不绕过 html 渲染目录分支）
    let is_html = ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm");
    let is_md = crate::core::document::html_clean::is_markdown_ext(ext);

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
    let splitter = if is_html {
        match html_render_matcher {
            // 已配置渲染目录：命中 → 文档分块；未命中 → 放弃该文件（不索引）
            Some(m) if !m.matches(rel_path) => return Vec::new(),
            _ => chunk_splitter_factory().get_splitter("html"),
        }
    } else {
        chunk_splitter_factory().get_splitter(ext)
    };
    let chunks = splitter.split(cleaned, chunk_size, chunk_overlap);
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

    utils::build_document_chunks(rel_path, &validated)
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

    /// V1 闭环：markdown frontmatter → doc_title/tags 注入所有 chunk（BM25 title/tags 字段消费）
    #[test]
    fn chunk_document_injects_frontmatter_metadata() {
        let md = "---\ntitle: Redis 连接池手册\ntags:\n  - redis\n  - 运维\naliases:\n  - Redis Pool\n---\n# 正文\n连接池配置说明内容段落。";
        let chunks = chunk_document("notes/redis.md", md, 448, 56, None);
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
        let chunks = chunk_document("notes/plain.md", md, 448, 56, None);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|c| c.doc_title.is_none() && c.tags.is_none()));
    }

    /// 非 markdown 文件（代码/纯文本）：不解析 frontmatter，字段为空
    #[test]
    fn chunk_document_non_markdown_no_metadata() {
        let code = "fn main() {\n    let x = 1;\n}\n";
        let chunks = chunk_document("src/main.rs", code, 448, 56, None);
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
