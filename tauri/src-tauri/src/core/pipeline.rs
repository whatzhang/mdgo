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

use crate::core::db::bm25::Bm25Index;
use crate::core::db::chunk_splitter::ChunkSplitterFactory;
use crate::core::db::lance::{DocumentChunk, LanceStore};
use crate::core::db::utils;

/// 全局 ChunkSplitter 工厂（懒初始化，线程安全）
static CHUNK_SPLITTER_FACTORY: std::sync::OnceLock<ChunkSplitterFactory> =
    std::sync::OnceLock::new();
pub(crate) fn chunk_splitter_factory() -> &'static ChunkSplitterFactory {
    CHUNK_SPLITTER_FACTORY.get_or_init(ChunkSplitterFactory::new)
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
pub fn chunk_document(
    rel_path: &str,
    content: &str,
    chunk_size: usize,
    chunk_overlap: usize,
) -> Vec<DocumentChunk> {
    let ext = rel_path.rsplit('.').next().unwrap_or("txt");
    let splitter = chunk_splitter_factory().get_splitter(ext);
    let chunks = splitter.split(content, chunk_size, chunk_overlap);
    if chunks.is_empty() {
        return Vec::new();
    }
    utils::build_document_chunks(rel_path, &chunks)
}

/// embedding_stage：批量向量化。
///
/// 向量化文本优先取 `chunk.embedding_text`（AST 语义分块的紧凑标题路径 + 正文），
/// 无则退化用 `chunk.text`（代码/OPML 等仍按原始文本向量化）。
pub async fn embed_chunks(
    chunks: &[DocumentChunk],
    progress: Option<&(dyn Fn(usize, usize, &str) + Send + Sync)>,
) -> Result<Vec<Vec<f32>>, String> {
    use tokio::sync::mpsc;

    log::debug!(
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
                    Err(e) => Err(format!("Embedding 任务执行失败, error={}", e)),
                });
            }
        }
    }

    let all_vectors = result.unwrap()?;
    log::debug!(
        "[pipeline] 【Embedding 批量处理】 完成，共 {} 个向量",
        all_vectors.len()
    );
    Ok(all_vectors)
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
