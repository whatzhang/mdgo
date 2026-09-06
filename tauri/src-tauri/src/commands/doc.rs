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
        let ext = p
            .extension()
            .and_then(|x| x.to_str())
            .map(|x| x.to_lowercase())
            .unwrap_or_default();
        if ext != "md" && ext != "txt" && ext != "markdown" {
            continue;
        }
        out.push(if folder.is_empty() {
            name
        } else {
            format!("{folder}/{name}")
        });
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
        let ext = p
            .extension()
            .and_then(|x| x.to_str())
            .map(|x| x.to_lowercase())
            .unwrap_or_default();
        if ext != "md" && ext != "txt" && ext != "markdown" {
            continue;
        }
        let rel = if folder.is_empty() {
            name.clone()
        } else {
            format!("{folder}/{name}")
        };
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
        let ext = p
            .extension()
            .and_then(|x| x.to_str())
            .map(|x| x.to_lowercase())
            .unwrap_or_default();
        if ext != "md" && ext != "txt" && ext != "markdown" {
            continue;
        }
        let rel = if folder.is_empty() {
            name.clone()
        } else {
            format!("{folder}/{name}")
        };
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
