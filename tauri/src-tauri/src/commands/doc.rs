//! `commands/doc` —— DocAgent（文档子 Agent）的前端可调入口（v0：元数据 + 上下文构建）。
//!
//! 仅提供无状态只读能力：返回单文件的结构化元数据（标题章节 + 行号锚点 + mtime）与
//! “问题 → 预算内章节切片”上下文块。真正的多轮问答（流式 LoopAgent）与
//! `doc_agent`/`parallel_doc_agent` 工具在接入命令层后于 `commands/llm.rs` 链路扩展。

use serde::Serialize;

use crate::core::docagent::{self, ContextOut, DocMeta};

/// 相关文档候选（P1-7/T1-7 语义相关提示）。
#[derive(Serialize)]
pub struct DocRelItem {
    pub rel_path: String,
    pub score: f32,
    pub lines: usize,
}

fn token_set(text: &str, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in text.chars().take(max) {
        if ch.is_ascii_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(&ch) {
            cur.push(ch);
        } else if !cur.is_empty() {
            out.push(cur.to_lowercase());
            cur.clear();
        }
    }
    if !cur.is_empty() {
        out.push(cur.to_lowercase());
    }
    out
}

fn score_overlap(a: &[String], b: &[String]) -> f32 {
    use std::collections::HashSet;
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let set_a: HashSet<&String> = a.iter().collect();
    let hit = b.iter().filter(|t| set_a.contains(t)).count();
    hit as f32 / b.len().min(200).max(1) as f32
}

/// 会话级资料圈选候选：列出指定目录（默认当前文件所在目录）内 md/txt/markdown 相对路径。
#[tauri::command]
pub async fn doc_dir_files(
    dir_path: String,
    file_path: String,
) -> Result<Vec<String>, String> {
    let folder = file_path
        .rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default();
    let dir_abs = std::fs::canonicalize(&dir_path).map_err(|e| format!("根目录无效: {e}"))?;
    let folder_abs = if folder.is_empty() {
        dir_abs.clone()
    } else {
        dir_abs.join(&folder)
    };
    let Ok(entries) = std::fs::read_dir(&folder_abs) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<String> = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !p.is_file() {
            continue;
        }
        // D8/N7：资料圈选口径由注册表统一决定（覆盖 PDF/Office/EPUB 等转换后形态）
        let rel = if folder.is_empty() {
            name
        } else {
            format!("{folder}/{name}")
        };
        if !crate::core::document::filekind::is_document_material(&rel) {
            continue;
        }
        out.push(rel);
    }
    out.sort();
    Ok(out)
}

/// #标签 目录内匹配：返回当前文件同目录中 frontmatter 含该标签的 md/txt（≤3，排除自身）。
#[tauri::command]
pub async fn doc_tag_files(
    dir_path: String,
    file_path: String,
    tag: String,
) -> Result<Vec<String>, String> {
    let tag = tag.trim().trim_start_matches('#').to_lowercase();
    if tag.is_empty() {
        return Ok(Vec::new());
    }
    let folder = file_path
        .rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default();
    let dir_abs = std::fs::canonicalize(&dir_path).map_err(|e| format!("根目录无效: {e}"))?;
    let folder_abs = if folder.is_empty() {
        dir_abs.clone()
    } else {
        dir_abs.join(&folder)
    };
    let Ok(entries) = std::fs::read_dir(&folder_abs) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<String> = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !p.is_file() {
            continue;
        }
        let rel = if folder.is_empty() {
            name.clone()
        } else {
            format!("{folder}/{name}")
        };
        // D8/N7：资料圈选口径由注册表统一决定（覆盖 PDF/Office/EPUB 等转换后形态），
        // 不再硬编码 md/txt/markdown——否则会出现"前端能看、DocAgent 圈不到"的分裂。
        if !crate::core::document::filekind::is_document_material(&rel) {
            continue;
        }
        if rel == file_path {
            continue;
        }
        let Ok(doc) = docagent::read_doc(&dir_path, &rel) else {
            continue;
        };
        let matched = docagent::front_matter_tags(&doc.full_text)
            .iter()
            .any(|t| t.to_lowercase() == tag);
        if matched {
            out.push(rel);
            if out.len() >= 3 {
                break;
            }
        }
    }
    Ok(out)
}

/// 相关文档候选（词面重叠近似语义）：与当前文件同目录的 md/txt 按内容相似度排序。
#[tauri::command]
pub async fn doc_related(
    dir_path: String,
    file_path: String,
    limit: Option<u32>,
) -> Result<Vec<DocRelItem>, String> {
    let limit = limit.unwrap_or(3).clamp(1, 5) as usize;
    let cur = docagent::read_doc(&dir_path, &file_path)?;
    let cur_tokens = token_set(&cur.full_text, 120_000);
    let folder = file_path
        .rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default();

    let dir_abs = std::fs::canonicalize(&dir_path).map_err(|e| format!("根目录无效: {e}"))?;
    let folder_abs = if folder.is_empty() {
        dir_abs.clone()
    } else {
        dir_abs.join(&folder)
    };
    let Ok(entries) = std::fs::read_dir(&folder_abs) else {
        return Ok(Vec::new());
    };

    let mut scored: Vec<(String, f32, usize)> = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !p.is_file() {
            continue;
        }
        let rel = if folder.is_empty() {
            name.clone()
        } else {
            format!("{folder}/{name}")
        };
        // D8/N7：资料圈选口径由注册表统一决定（覆盖 PDF/Office/EPUB 等转换后形态），
        // 不再硬编码 md/txt/markdown——否则会出现"前端能看、DocAgent 圈不到"的分裂。
        if !crate::core::document::filekind::is_document_material(&rel) {
            continue;
        }
        if rel == file_path {
            continue;
        }
        let Ok(doc) = docagent::read_doc(&dir_path, &rel) else {
            continue;
        };
        let toks = token_set(&doc.full_text, 60_000);
        let score = score_overlap(&cur_tokens, &toks);
        if score > 0.0 {
            scored.push((rel, score, doc.total_lines));
        }
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Ok(scored
        .into_iter()
        .take(limit)
        .map(|(rel, score, lines)| DocRelItem {
            rel_path: rel,
            score,
            lines,
        })
        .collect())
}

/// 未显式给预算时使用的默认文档上下文预算（token）。前端通常按模型上下文窗口传入，
/// 此值仅作后端兜底，保证 `doc_build_context` 不因缺参返回空。
pub const DEFAULT_DOC_BUDGET_TOKENS: u32 = 16_000;

#[derive(Serialize)]
pub struct DocMetaPayload {
    pub meta: DocMeta,
    /// 单文件全文是否可在一个直读预算内放下（预算未提供时为 None）
    pub full_fits: Option<bool>,
}

/// 读取单个文档的结构化元数据（文件卡片 / TOC / 引用行号锚点用）。
#[tauri::command]
pub async fn doc_read_meta(
    dir_path: String,
    rel_path: String,
    budget_tokens: Option<u32>,
) -> Result<DocMetaPayload, String> {
    let doc = docagent::read_doc(&dir_path, &rel_path)?;
    let budget = budget_tokens.map(|b| b as usize);
    let full_fits = budget.map(|b| docagent::estimate_tokens(&doc.full_text) <= b);
    let mut meta = doc.meta();
    meta.fits_budget = full_fits;
    Ok(DocMetaPayload { meta, full_fits })
}

#[derive(Serialize)]
pub struct DocContextPayload {
    pub prompt_block: String,
    pub included_ids: Vec<usize>,
    pub full: bool,
    pub omitted: Vec<String>,
    pub meta: DocMeta,
}

/// 构建“问题 → 文档章节上下文”块（前端可先预览/调试；问答链路将其注入 system）。
#[tauri::command]
pub async fn doc_build_context(
    dir_path: String,
    rel_path: String,
    question: String,
    budget_tokens: Option<u32>,
) -> Result<DocContextPayload, String> {
    let doc = docagent::read_doc(&dir_path, &rel_path)?;
    let budget = budget_tokens
        .map(|b| b as usize)
        .unwrap_or(DEFAULT_DOC_BUDGET_TOKENS as usize);
    let ContextOut {
        prompt_block,
        included_ids,
        full,
        omitted,
    } = docagent::build_context(&doc, &question, budget);
    let mut meta = doc.meta();
    meta.fits_budget = Some(docagent::estimate_tokens(&doc.full_text) <= budget);
    Ok(DocContextPayload {
        prompt_block,
        included_ids,
        full,
        omitted,
        meta,
    })
}

// ─── 文档预览（Phase 0B：与索引共用 DocumentLoader，方案 §4.1 / §7.3）───

/// 预览正文上限（字符）。防止把 50MB 的转换结果整体送进 webview。
/// 超出部分截断并置 `truncated = true`（前端提示）。
const PREVIEW_TEXT_LIMIT: usize = 200_000;

/// 文档预览载荷。
#[derive(Serialize)]
pub struct DocumentPreview {
    /// 转换/读取后的正文（Phase 1 起为转换器产出的 Markdown）
    pub text: String,
    /// 内容形态（markdown/html/tree/code/plain）——前端据此选渲染器
    pub form: String,
    /// 版本失效粒度键（pdf/office/markdown/code/text/data）
    pub source_kind: String,
    /// 转换器身份（`id@version`）
    pub converter: String,
    /// 成功为 None；失败时为 `SkipReason::code()`（如 unsupported/not_utf8/too_large）
    pub skip_code: Option<String>,
    /// 对应的中文说明
    pub skip_reason: Option<String>,
    /// 页码归属 `[page, byte_start, byte_end]`（Phase 1 起非空）
    pub page_spans: Vec<[u64; 3]>,
    pub warnings: Vec<String>,
    /// 正文是否被预览上限截断
    pub truncated: bool,
    /// 正文总字节数（截断前）
    pub bytes: usize,
}

/// 预览侧装载：**索引与预览必须走同一条通路**（方案 §4.1 / §7.3：
/// `DocumentLoader → ConversionCache`）。
///
/// 给出 `dir_path` 时用转换缓存（目录可由 `get_cache_dir` 推断）：刚索引过的 PDF/Office
/// 打开预览会直接命中，不再二次全量解析；缺省时退化为直接装载（无从推断缓存目录）。
/// 缓存不可用只降级、不影响正确性——转换逻辑仍只有 loader 一处（唯一入口契约不破）。
fn load_preview_source(
    abs: &std::path::Path,
    rel_path: &str,
    dir_path: Option<&str>,
) -> Result<crate::core::document::loader::DocumentSource, crate::core::document::loader::SkipReason>
{
    use crate::core::document::loader as loader;
    let Some(dir) = dir_path else {
        return loader::load_document(abs, rel_path);
    };
    match crate::core::db::conversion_cache::ConversionCache::open_shared(
        &crate::core::db::utils::get_cache_dir(dir),
    ) {
        Ok(cache) => cache.load_or_convert(abs, rel_path),
        Err(e) => {
            log::debug!("[doc] 预览转换缓存不可用（回退直接装载）: {}", e);
            loader::load_document(abs, rel_path)
        }
    }
}

/// 预览一个文件：**必须经 `DocumentLoader`**（唯一入口契约）。
///
/// `dir_path` 可选：提供时用于把绝对路径折算成相对路径（与索引侧 `doc_name` 同口径，
/// 保证预览与索引看到同一份 `FileKind`）；缺省时退化为按文件名匹配。
///
/// 设计要点（方案 §4.1 / §7.2.1）：
/// - 前端**只传路径**，不要把文件内容读进 JS 再传回（大 `.pptx` 会双倍搬运）；
/// - 与索引共用同一装载实现，预览与入库结果天然一致；
/// - Phase 3 接入转换缓存后，预览与索引都不再重复解析。
#[tauri::command]
pub async fn document_preview(
    path: String,
    dir_path: Option<String>,
) -> Result<DocumentPreview, String> {
    let abs = std::path::PathBuf::from(&path);
    if !abs.is_file() {
        return Err(format!("不是文件: {}", path));
    }
    let rel_path = match dir_path.as_deref() {
        Some(dir) => {
            let d = std::path::Path::new(dir);
            abs.strip_prefix(d)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| {
                    abs.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                })
        }
        None => abs
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
    };

    match load_preview_source(&abs, &rel_path, dir_path.as_deref()) {
        Ok(src) => {
            let total_bytes = src.text.len();
            let truncated = src.text.chars().count() > PREVIEW_TEXT_LIMIT;
            let text: String = if truncated {
                src.text.chars().take(PREVIEW_TEXT_LIMIT).collect()
            } else {
                src.text.clone()
            };
            Ok(DocumentPreview {
                text,
                form: src.form.as_str().to_string(),
                source_kind: src.source_kind.to_string(),
                converter: src.converter.label(),
                skip_code: None,
                skip_reason: None,
                page_spans: src
                    .page_spans
                    .iter()
                    .map(|s| [s.page as u64, s.byte_start as u64, s.byte_end as u64])
                    .collect(),
                warnings: src.warnings.clone(),
                truncated,
                bytes: total_bytes,
            })
        }
        Err(reason) => {
            // 失败也返回结构化结果（而非 Err），前端据此显示"为什么打不开"
            Ok(DocumentPreview {
                text: String::new(),
                form: "plain".to_string(),
                source_kind: String::new(),
                converter: String::new(),
                skip_code: Some(reason.code().to_string()),
                skip_reason: Some(reason.message()),
                page_spans: Vec::new(),
                warnings: Vec::new(),
                truncated: false,
                bytes: 0,
            })
        }
    }
}
