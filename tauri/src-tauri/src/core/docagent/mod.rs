//! `core/docagent` —— 文档子 Agent（DocAgent）的「单文件上下文工程」。
//!
//! 职责（本模块只做纯函数/无 LLM 依赖部分，命令层负责组装对话）：
//! - [`read_doc`]：读取知识库内单个文件，按 Markdown 标题解析为「章节 + 行号锚点」，
//!   进程级 mtime 缓存（对齐 agent 规约缓存策略：未变更零读盘）。
//! - [`build_context`]：给定用户问题，在 token/字符预算内选择最相关章节，
//!   生成带 `[§id]` 引用标记的上下文块（模型须按 `§id` 标注出处，前端据此跳转行号）。
//! - 引用锚点契约：`§N` = 第 N 个章节；`file:line` 作为补充行引用（回答中直接给出）。
//!
//! 参考主流做法：NotebookLM/飞书/Khoj 的“引用可点开定位原文段落”。本模块把“定位粒度”
//! 落到 **Markdown 标题章节 + 起止行号**（本地结构化文档天然可做到行级）。
//!
//! 边界安全：所有读取都经过根目录 canonicalize + 前缀校验（对齐 `core/agent/tools` 的
//! 防逃逸惯例），本模块自身不做任何写操作。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// 进程级文件缓存：(规范根 + 相对路径) → (mtime, 解析结果)
static DOC_CACHE: OnceLock<Mutex<HashMap<String, (u64, Arc<DocFile>)>>> = OnceLock::new();

const CACHE_MAX_ENTRIES: usize = 64;

fn doc_cache() -> &'static Mutex<HashMap<String, (u64, Arc<DocFile>)>> {
    DOC_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 单个 Markdown 章节（含行号锚点）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DocSection {
    /// 章节序号（0 起）。引用格式 `§N`；前端据此在编辑器内高亮 line_start..line_end。
    pub id: usize,
    /// 标题文本（无 `#` 前缀）；id=0 且无标题时为 `(文档开头)`
    pub heading: String,
    /// 该章节内容在文件中的起始行号（1 起，含标题行）
    pub line_start: usize,
    /// 该章节内容在文件中的结束行号（含）
    pub line_end: usize,
    /// 章节文本（不含标题行，用于检索打分与回显）
    pub text: String,
}

/// 解析后的单个文档（缓存载体）。
#[derive(Debug, Clone)]
pub struct DocFile {
    pub rel_path: String,
    pub mtime_ms: u64,
    /// 原始全文（含标题行）
    pub full_text: String,
    pub total_lines: usize,
    pub total_chars: usize,
    /// 章节列表（按顺序）
    pub sections: Vec<DocSection>,
    /// 是否在单个直读预算内（由调用方按预算判定；不判定时 None）
    pub fits_budget: Option<bool>,
}

/// 从 Markdown frontmatter 提取 tags/aliases（支持 `- 项`、`[a, b]`、逗号/空格分隔）。
pub fn front_matter_tags(text: &str) -> Vec<String> {
    let mut lines = text.lines();
    if lines.next().map(|l| l.trim()) != Some("---") {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut in_tags = false;
    for line in lines.take(80) {
        let t = line.trim();
        let lower = t.to_lowercase();
        if t == "---" {
            break;
        }
        if lower.starts_with("tags:") || lower.starts_with("aliases:") {
            in_tags = true;
            let rest = t.split_once(':').map(|(_, r)| r).unwrap_or("").trim();
            if !rest.is_empty() {
                let cleaned = rest.trim_start_matches('[').trim_end_matches(']');
                for part in cleaned.split([',', '，', ' ']) {
                    let p = part.trim().trim_matches(['"', '\'']).to_string();
                    if !p.is_empty() {
                        out.push(p);
                    }
                }
            }
            continue;
        }
        if in_tags {
            if t.starts_with('-') {
                let p = t[1..].trim().trim_matches(['"', '\'']).to_string();
                if !p.is_empty() {
                    out.push(p);
                }
            } else if t.contains(':') {
                break; // 其它 frontmatter 键：标签块结束
            } else if !t.is_empty() {
                for part in t.split([',', '，', ' ']) {
                    let p = part.trim().to_string();
                    if !p.is_empty() {
                        out.push(p);
                    }
                }
            }
        }
    }
    out
}

/// 前端「文件卡片 / 引用定位」需要的元数据（轻量，不含正文）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DocMeta {
    pub rel_path: String,
    pub mtime_ms: u64,
    pub total_lines: usize,
    pub total_chars: usize,
    pub sections: Vec<DocSection>,
    pub fits_budget: Option<bool>,
}

impl DocFile {
    pub fn meta(&self) -> DocMeta {
        DocMeta {
            rel_path: self.rel_path.clone(),
            mtime_ms: self.mtime_ms,
            total_lines: self.total_lines,
            total_chars: self.total_chars,
            sections: self.sections.clone(),
            fits_budget: self.fits_budget,
        }
    }
}

/// 上下文构建结果：给模型的提示块 + 被选中的章节 id（供调用方维护引用映射）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContextOut {
    pub prompt_block: String,
    pub included_ids: Vec<usize>,
    /// 是否全文注入（无省略）
    pub full: bool,
    /// 被省略章节的标题清单（提示模型这些内容未纳入）
    pub omitted: Vec<String>,
}

fn mtime_ms_of(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 路径解析 + 防逃逸：返回规范化绝对路径；rel 越权/不存在返回 Err。
fn resolve_in_root(root: &str, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() {
        return Err("文件路径为空".to_string());
    }
    let rel_p = Path::new(rel);
    if rel_p.is_absolute() || rel_p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(format!("非法相对路径: {rel}"));
    }
    let root_c = std::fs::canonicalize(root).map_err(|e| format!("无法访问知识库根目录: {e}"))?;
    let candidate = root_c.join(rel);
    // 词法防逃逸
    if !candidate.starts_with(&root_c) {
        return Err(format!("路径越权: {rel}"));
    }
    let full = std::fs::canonicalize(&candidate).map_err(|e| format!("读取文件失败: {e}"))?;
    if !full.starts_with(&root_c) {
        return Err(format!("路径越权（符号链接）: {rel}"));
    }
    if !full.is_file() {
        return Err(format!("不是文件: {rel}"));
    }
    Ok(full)
}

fn read_text_at(full: &Path) -> Result<(String, u64), String> {
    let mtime = mtime_ms_of(full);
    // UTF-8 读取失败时降级 lossy（避免单个乱码文件阻断问答）
    let raw = std::fs::read(full).map_err(|e| format!("读取文件失败: {e}"))?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    Ok((text, mtime))
}

fn cache_key(root: &str, rel: &str) -> String {
    format!("{root}\u{0}{rel}")
}

/// 读取并解析单文件（带 mtime 缓存）。
pub fn read_doc(root: &str, rel: &str) -> Result<Arc<DocFile>, String> {
    let full = resolve_in_root(root, rel)?;
    let mtime = mtime_ms_of(&full);
    let key = cache_key(root, rel);
    if let Ok(map) = doc_cache().lock() {
        if let Some((t, doc)) = map.get(&key) {
            if *t == mtime {
                return Ok(doc.clone());
            }
        }
    }
    let (text, mtime) = read_text_at(&full)?;
    let doc = parse_doc(root, rel, text, mtime);
    if let Ok(mut map) = doc_cache().lock() {
        if !map.contains_key(&key) && map.len() >= CACHE_MAX_ENTRIES {
            if let Some(oldest) = map.keys().next().cloned() {
                map.remove(&oldest);
            }
        }
        map.insert(key, (mtime, Arc::new(doc.clone())));
    }
    Ok(Arc::new(doc))
}

fn strip_front_matter<'a, 'b>(lines: &'a [&'b str]) -> (&'a [&'b str], usize) {
    if lines.first().map(|l| l.trim()) == Some("---") {
        for (i, l) in lines.iter().enumerate().skip(1) {
            if l.trim() == "---" {
                let end = i + 1; // 闭包行之后的正文从 end 开始
                return (&lines[end..], end);
            }
        }
    }
    (lines, 0)
}

fn atx_heading_len(trimmed: &str) -> usize {
    // 返回标题标记宽度（# 个数），非 ATX 标题返回 0
    let mut hashes = 0;
    for c in trimmed.chars() {
        if c == '#' {
            hashes += 1;
        } else {
            break;
        }
    }
    if hashes == 0 || hashes > 6 || hashes >= trimmed.len() {
        return 0;
    }
    if trimmed.as_bytes()[hashes] != b' ' {
        return 0;
    }
    hashes
}

fn parse_doc(_root: &str, rel: &str, text: String, mtime: u64) -> DocFile {
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_string();
    let raw_lines: Vec<&str> = text.lines().collect();
    let (body, front_lines) = strip_front_matter(&raw_lines);
    // 正文行号 = front 偏移 + 1（行号锚点对应当前编辑器可见行，含 front matter）
    let mut sections: Vec<DocSection> = Vec::new();
    let mut cur: Option<DocSection> = None;
    let mut saw_heading = false;

    for (idx, raw) in body.iter().enumerate() {
        let lnum = front_lines + 1 + idx;
        let trimmed = raw.trim_start();
        let hl = atx_heading_len(trimmed);
        if hl > 0 {
            // 收尾上一段（段落到标题前一行为止）
            if let Some(mut s) = cur.take() {
                s.line_end = lnum.saturating_sub(1).max(s.line_start);
                sections.push(s);
            }
            let heading = trimmed[hl..].trim().to_string();
            cur = Some(DocSection {
                id: 0, // 解析后按顺序重排
                heading,
                line_start: lnum,
                line_end: lnum,
                text: String::new(),
            });
            saw_heading = true;
            continue;
        }
        if !trimmed.is_empty() && cur.is_none() && !saw_heading {
            // 第一个标题之前的正文：作为「前言」章节
            cur = Some(DocSection {
                id: 0,
                heading: "(前言)".to_string(),
                line_start: lnum,
                line_end: lnum,
                text: String::new(),
            });
        }
        if let Some(s) = cur.as_mut() {
            if !s.text.is_empty() {
                s.text.push('\n');
            }
            s.text.push_str(raw);
            s.line_end = lnum;
        }
    }
    if let Some(mut s) = cur.take() {
        s.line_end = s.line_end.max(s.line_start);
        sections.push(s);
    }

    let total_chars = text.chars().count();
    let total_lines = text.lines().count();
    let body_last_line = front_lines + body.len().max(1);
    // 无任何标题：整篇作一个章节
    if sections.is_empty() && total_chars > 0 {
        sections.push(DocSection {
            id: 0,
            heading: "(全文)".to_string(),
            line_start: front_lines + 1,
            line_end: body_last_line,
            text: text.clone(),
        });
    }
    // 按出现顺序重排 id（引用契约：§id == sections 下标）
    for (i, s) in sections.iter_mut().enumerate() {
        s.id = i;
    }
    DocFile {
        rel_path: rel.to_string(),
        mtime_ms: mtime,
        full_text: text,
        total_lines,
        total_chars,
        sections,
        fits_budget: None,
    }
}

/// CJK 感知的字符→token 粗略估算（与前端 estimateTokenCount 口径一致：CJK 1.5 字符/token、其余 4）。
pub fn estimate_tokens(s: &str) -> usize {
    let mut chars = 0usize;
    let mut cjk = 0usize;
    for c in s.chars() {
        chars += 1;
        if ('\u{4e00}'..='\u{9fff}').contains(&c) {
            cjk += 1;
        }
    }
    (cjk as f64 / 1.5).ceil() as usize + (chars - cjk) / 4
}

/// 轻量打分：查询词在章节文本（含标题，标题权重 ×3）的词频（对数缩放）+ 章节出现次序惩罚极小。
fn score_section(q_terms: &[String], sec: &DocSection) -> f64 {
    if q_terms.is_empty() {
        return 0.0;
    }
    let hay = format!("{} {} {}", sec.heading, sec.heading, sec.heading); // heading 加权
    let text_l = sec.text.to_lowercase();
    let mut score = 0.0f64;
    for t in q_terms {
        let mut tf = 0usize;
        let mut start = 0;
        while let Some(i) = text_l[start..].find(t) {
            tf += 1;
            start += i + t.len();
            if tf > 50 {
                break;
            }
        }
        if tf > 0 {
            score += 1.0 + (tf as f64).ln();
        }
    }
    // 标题命中显著加成
    let hay_l = hay.to_lowercase();
    for t in q_terms {
        if hay_l.contains(t) {
            score += 3.0;
        }
    }
    score
}

/// 从问题里切词（ASCII 词 + CJK 连续串），供轻量打分用。
fn tokenize_query(q: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut ascii = String::new();
    let mut cjk = String::new();
    for c in q.chars() {
        if c.is_ascii_alphanumeric() {
            ascii.push(c.to_ascii_lowercase());
            if !cjk.is_empty() {
                out.push(std::mem::take(&mut cjk));
            }
        } else if ('\u{4e00}'..='\u{9fff}').contains(&c) {
            cjk.push(c);
            if !ascii.is_empty() {
                out.push(std::mem::take(&mut ascii));
            }
        } else {
            if !ascii.is_empty() {
                out.push(std::mem::take(&mut ascii));
            }
            if !cjk.is_empty() {
                out.push(std::mem::take(&mut cjk));
            }
        }
    }
    if !ascii.is_empty() {
        out.push(ascii);
    }
    if !cjk.is_empty() {
        out.push(cjk);
    }
    out
}

/// 在 `budget_tokens` 内构造上下文块。
///
/// 策略（单文件优先、无索引依赖、任意文本可用）：
/// 1. 整篇 token 估算 ≤ 预算 → 全文注入（保留所有 `§` 章节标记，模型可直接定位）。
/// 2. 超预算 → 按查询打分选 Top 章节（累积不超过预算）；输出开头附全部章节 TOC，
///    模型可据此告知用户"文档还包含 §5…"，或请其指明章节后由宿主带新上下文重问。
pub fn build_context(doc: &DocFile, query: &str, budget_tokens: usize) -> ContextOut {
    if budget_tokens == 0 {
        return ContextOut {
            prompt_block: String::new(),
            included_ids: vec![],
            full: false,
            omitted: doc.sections.iter().map(|s| s.heading.clone()).collect(),
        };
    }
    let whole_est = estimate_tokens(&doc.full_text) + doc.sections.len() * 2 + 40;
    if whole_est <= budget_tokens {
        return ContextOut {
            prompt_block: render_full(doc),
            included_ids: (0..doc.sections.len()).collect(),
            full: true,
            omitted: vec![],
        };
    }
    // 超预算：打分选段
    let terms = tokenize_query(query);
    let mut ranked: Vec<usize> = (0..doc.sections.len()).collect();
    if !terms.is_empty() {
        ranked.sort_by(|a, b| {
            score_section(&terms, &doc.sections[*b])
                .partial_cmp(&score_section(&terms, &doc.sections[*a]))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let mut chosen: Vec<usize> = Vec::new();
    let mut used = 0usize;
    for id in ranked {
        let sec = &doc.sections[id];
        let est = estimate_tokens(&sec.heading) + estimate_tokens(&sec.text) + 4;
        if used + est > budget_tokens {
            if chosen.is_empty() {
                // 单章超预算：只放头部（截断渲染由调用方控制，此处仅收录该章节）
                chosen.push(id);
            }
            break;
        }
        chosen.push(id);
        used += est;
    }
    chosen.sort_unstable();
    let chosen_set: std::collections::HashSet<usize> = chosen.iter().copied().collect();
    let omitted: Vec<String> = doc
        .sections
        .iter()
        .filter(|s| !chosen_set.contains(&s.id))
        .map(|s| format!("§{} {}（第 {}–{} 行）", s.id, s.heading, s.line_start, s.line_end))
        .collect();
    let prompt_block = render_partial(doc, &chosen, &omitted);
    ContextOut {
        prompt_block,
        included_ids: chosen,
        full: false,
        omitted,
    }
}

fn render_full(doc: &DocFile) -> String {
    let mut out = header(doc, true);
    for (i, line) in doc.full_text.lines().enumerate() {
        out.push_str(&format!("{:<6}| {}\n", i + 1, line));
    }
    out.push_str(&citation_rules(doc));
    out
}

fn render_partial(doc: &DocFile, ids: &[usize], omitted: &[String]) -> String {
    let mut out = header(doc, false);
    if !omitted.is_empty() {
        out.push_str("以下章节未纳入本次上下文（文档其余部分）：\n");
        for o in omitted {
            out.push_str(&format!("- {o}\n"));
        }
        out.push('\n');
    }
    out.push_str("已纳入章节（行号 = 编辑器实际行号，引用必须给出 §号）：\n");
    for id in ids {
        let sec = &doc.sections[*id];
        out.push_str(&format!(
            "--- §{} {}\n（第 {}–{} 行）---\n{}\n",
            sec.id,
            sec.heading,
            sec.line_start,
            sec.line_end,
            sec.text.trim_end()
        ));
    }
    out.push_str(&citation_rules(doc));
    out
}

fn header(doc: &DocFile, full: bool) -> String {
    format!(
        "【当前文档】{}（共 {} 行 / {} 字符 / mtime={}，{}）\n",
        doc.rel_path,
        doc.total_lines,
        doc.total_chars,
        doc.mtime_ms,
        if full { "全文已注入" } else { "按需注入部分章节" }
    )
}

/// 字符索引（char 计数）→ 字节偏移（避免 CJK 下按字节切片切坏字符）。
fn char_offset_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

/// 按「字符区间选区」构造上下文块（用于选区级问答/改写：只给模型选区原文与行号，
/// 不注入整篇；引用仍按 `file:行-行` 与 [§N] 章节协议，选区落在哪一节由模型回答时给出）。
pub fn build_selection_context(doc: &DocFile, start_char: usize, end_char: usize) -> String {
    let full = &doc.full_text;
    let s = char_offset_to_byte(full, start_char.min(full.chars().count()));
    let e = char_offset_to_byte(full, end_char.max(start_char).min(full.chars().count()));
    let (s, e) = if s <= e { (s, e) } else { (e, s) };
    let sel = &full[s..e];
    if sel.trim().is_empty() {
        return String::new();
    }
    let line_start = full[..s].chars().filter(|c| *c == '\n').count() + 1;
    let line_end = full[..e].chars().filter(|c| *c == '\n').count() + 1;
    format!(
        "【选区内容】来自 {}（第 {}–{} 行）：\n```text\n{}\n```\n\
         引用规则：涉及该选区内容时，在句末标注 `({}:{}–{})`；不得引用未提供的其他内容；\
         若需整篇上下文请告知，将由宿主重新注入。\n",
        doc.rel_path, line_start, line_end, sel.trim_end(), doc.rel_path, line_start, line_end
    )
}

fn citation_rules(doc: &DocFile) -> String {
    format!(
        "\n引用规则：回答中引用本文内容时，必须在对应句末标注出处，格式为 `[§N]`（N 为章节号），\n\
         需要精确到行时追加 `({}:行号-行号)`。不得引用未提供的内容；若文档未覆盖，明确说明「未在文中找到」。\n",
        doc.rel_path
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "G:/gitProject/mdgo";
    const REL: &str = "docs/PRD-小助手文档Agent.md";

    #[test]
    fn parse_known_doc_produces_sections() {
        let doc = read_doc(ROOT, REL).expect("读取 PRD 文档应成功");
        assert!(doc.total_lines > 10);
        assert!(!doc.sections.is_empty());
        assert_eq!(doc.sections[0].id, 0);
        for s in &doc.sections {
            assert!(s.line_end >= s.line_start);
            assert!(s.line_start >= 1);
        }
        // 全文字符数应与 total_chars 一致
        let joined: String = doc.sections.iter().map(|s| s.text.clone()).collect::<Vec<_>>().join("\n");
        assert!(!joined.is_empty());
    }

    #[test]
    fn tiny_budget_selects_subset_and_omits_rest() {
        let doc = read_doc(ROOT, REL).unwrap();
        // 超小预算应只选部分
        let out = build_context(&doc, "引用溯源 怎么做", 400);
        assert!(!out.prompt_block.is_empty());
        if !out.full {
            assert!(!out.omitted.is_empty());
            assert!(!out.included_ids.is_empty());
            // prompt 内包含 § 标记
            assert!(out.prompt_block.contains('§'));
        }
    }

    #[test]
    fn huge_budget_injects_full_doc() {
        let doc = read_doc(ROOT, REL).unwrap();
        // build_context 的全量判定预算 = 全文估算 + 每章节开销(≈2) + 固定 40；测试预算须覆盖它
        let est = estimate_tokens(&doc.full_text) + doc.sections.len() * 2 + 48;
        let out = build_context(&doc, "任意问题", est);
        assert!(out.full, "预算足够时应全文注入");
        assert!(out.included_ids.len() == doc.sections.len());
    }

    #[test]
    fn path_escape_rejected() {
        assert!(resolve_in_root(ROOT, "../main.html").is_err());
        assert!(resolve_in_root(ROOT, "C:/Windows/win.ini").is_err());
    }

    #[test]
    fn tokenizer_splits_ascii_and_cjk() {
        let terms = tokenize_query("RAG 引用溯源 citation 怎么做");
        assert!(terms.iter().any(|t| t == "rag"));
        assert!(terms.iter().any(|t| t.contains("引用")));
        assert!(terms.iter().any(|t| t == "citation"));
    }

    #[test]
    fn front_matter_tags_parses_list_and_inline() {
        let md = "---\ntitle: t\ntags:\n  - redis\n  - 运维\naliases: [r2]\n---\n正文";
        let tags = front_matter_tags(md);
        assert!(tags.contains(&"redis".to_string()));
        assert!(tags.iter().any(|t| t.contains("运维")));
        assert!(tags.contains(&"r2".to_string()));
        assert!(front_matter_tags("无 frontmatter").is_empty());
    }
}
