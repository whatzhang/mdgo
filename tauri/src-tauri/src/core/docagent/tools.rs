//! 宿主 A（停靠栏「小助手」）的**按需取文**工具集：只读、仅限当前关联文件。
//!
//! ## 为什么需要
//!
//! 宿主 A 走纯对话 `LoopAgent`（无工具），上下文由命令层按 token 预算**预选注入**：
//! 长文档只会注入相关度最高的若干章节，其余章节只以「未纳入本次上下文」清单出现。
//! 于是用户问「第 14 章讲了什么」时，若该章没被预算选中，模型只能回答
//! 「该章节未纳入上下文」——用户体感就是"它读不到文档"。
//!
//! 本模块给模型补上**只读取证**能力（等价于只读 grep / read）：
//! - [`DocOutlineTool`]：文档全量章节目录（§id + 行号区间 + 规模），先看清楚有什么；
//! - [`DocSearchTool`]：按关键词定位行（loci + 所在章节），等价 grep；
//! - [`DocReadSectionTool`]：按 §id / 标题 / 行号区间取回原文（带行号，可续读）。
//!
//! ## 安全边界
//!
//! 工具持有的是宿主传入的 `(dir_path, rel_path)`，**模型无法提供路径**：
//! 只能读它当前正在被问的那一个文件，不存在目录穿越/越权读取面。
//! 三个工具都只读，`concurrency_safe = true`（可并行），并有单次超时。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{estimate_tokens, read_doc, DocFile, DocSection};
use crate::core::r#loop::{
    HashMapToolRegistry, Tool, ToolError, ToolRegistry, ToolRunContext, ToolSpec,
};

/// 单次检索最多返回的命中行数。
const MAX_SEARCH_HITS: usize = 40;
/// `doc_search` 默认命中行数。
const DEFAULT_SEARCH_HITS: usize = 20;
/// 单次取文默认 token 预算（超出则截断并提示续读）。
const DEFAULT_READ_TOKENS: usize = 4000;
/// 单次取文硬上限。
const MAX_READ_TOKENS: usize = 8000;
/// 工具单次执行超时。
const TOOL_TIMEOUT_MS: u64 = 15_000;

fn readonly_spec(name: &str, description: &str, parameters: Value) -> ToolSpec {
    let mut spec = ToolSpec::new(name, description, parameters);
    spec.timeout_ms = Some(TOOL_TIMEOUT_MS);
    spec.concurrency_safe = true;
    spec
}

fn load(dir: &str, rel: &str) -> Result<Arc<DocFile>, ToolError> {
    read_doc(dir, rel).map_err(|e| ToolError::Failed(format!("读取当前文档失败: {e}")))
}

/// 章节 token 估算（与 `build_context` 口径一致）。
fn section_est(sec: &DocSection) -> usize {
    estimate_tokens(&sec.heading) + estimate_tokens(&sec.text) + 4
}

/// 在标题里找「编号」匹配：`14. 部署` / `14、部署` / `14 部署` / `第14章 部署`。
pub(crate) fn heading_matches_number(heading: &str, n: usize) -> bool {
    let h = heading.trim();
    if h.is_empty() {
        return false;
    }
    let pats = [
        format!("{n}."),
        format!("{n}、"),
        format!("{n}．"),
        format!("{n} "),
        format!("{n}　"),
        format!("第{n}章"),
        format!("第{n}节"),
        format!("第{n}部分"),
        format!("第{n}篇"),
        format!("第{n}条"),
    ];
    if pats.iter().any(|p| h.starts_with(p.as_str())) {
        return true;
    }
    // 「第 14 章 xxx」带空格形式
    let compact: String = h.chars().filter(|c| !c.is_whitespace()).collect();
    compact.starts_with(&format!("第{n}章"))
        || compact.starts_with(&format!("第{n}节"))
        || compact.starts_with(&format!("第{n}部分"))
}

/// 中文数字字符判定（注意：汉字码位不连续，`四/六/八` 不在 `一..九` 的码位区间内，
/// 必须逐字列出，不能用 `'一'..='九'` 区间）。
fn is_cn_num_char(c: char) -> bool {
    matches!(
        c,
        '一' | '二' | '三' | '四' | '五' | '六' | '七' | '八' | '九' | '十' | '百' | '零' | '两'
    )
}

/// 章节引用语义：`§N` 与 `第N章` **不是一回事**，必须分开处理。
///
/// - [`SectionRef::Id`]：引用协议里的 `§N` = 第 N 个章节（按标题出现顺序分配）；
/// - [`SectionRef::Chapter`]：文档作者写的章号（`第14章` / `14. 标题` / 裸数字 `14`），
///   需要拿标题里的编号去匹配。
///
/// 长文档里两者经常不等（例如 `## 12. 里程碑` 可能是第 30 个章节），
/// 早期实现把两者混为一谈，导致「§12」被解析到标题含 12 的那一节而不是 §12。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SectionRef {
    Id(usize),
    Chapter(usize),
}

/// 把 `"§14"` / `"14"` / `"第14章"` / `"十四"` 解析为章节引用。
/// 带 `§` 前缀 → 章节序号；`第N` 或裸数字 → 章号（解析失败时由调用方退回序号）。
pub(crate) fn parse_section_ref(raw: &str) -> Option<SectionRef> {
    let t = raw.trim();
    let by_id = t.starts_with('§');
    let s = t.trim_start_matches('§').trim_start_matches('第');
    let n = {
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            digits.parse::<usize>().ok()
        } else {
            let cn: String = s.chars().take_while(|c| is_cn_num_char(*c)).collect();
            if cn.is_empty() {
                None
            } else {
                cn_number(&cn)
            }
        }
    }?;
    Some(if by_id { SectionRef::Id(n) } else { SectionRef::Chapter(n) })
}

/// 中文数字（支持 一~九十九 + 一百 档内的常见写法）。
fn cn_number(s: &str) -> Option<usize> {
    let digit = |c: char| -> Option<usize> {
        match c {
            '零' => Some(0),
            '一' => Some(1),
            '二' | '两' => Some(2),
            '三' => Some(3),
            '四' => Some(4),
            '五' => Some(5),
            '六' => Some(6),
            '七' => Some(7),
            '八' => Some(8),
            '九' => Some(9),
            _ => None,
        }
    };
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return None;
    }
    if let Some(i) = chars.iter().position(|&c| c == '百') {
        let head = chars[..i].iter().find_map(|&c| digit(c)).unwrap_or(1);
        let rest: String = chars[i + 1..].iter().collect();
        return Some(head * 100 + cn_number(&rest).unwrap_or(0));
    }
    if let Some(i) = chars.iter().position(|&c| c == '十') {
        let head = if i == 0 { 1 } else { chars[..i].iter().find_map(|&c| digit(c))? };
        let rest: String = chars[i + 1..].iter().collect();
        let tail = if rest.is_empty() { 0 } else { cn_number(&rest)? };
        return Some(head * 10 + tail);
    }
    chars.iter().find_map(|&c| digit(c))
}

/// 从问题中解析章节引用：`§N` → [`SectionRef::Id`]；`第N章/第N节` → [`SectionRef::Chapter`]。
pub(crate) fn query_section_refs(q: &str) -> Vec<SectionRef> {
    let mut out: Vec<SectionRef> = Vec::new();
    let chars: Vec<char> = q.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        // §N（引用协议：章节序号）
        if chars[i] == '§' {
            let mut j = i + 1;
            let mut num = String::new();
            while j < chars.len() && chars[j].is_ascii_digit() {
                num.push(chars[j]);
                j += 1;
            }
            if let Ok(n) = num.parse::<usize>() {
                let r = SectionRef::Id(n);
                if !out.contains(&r) {
                    out.push(r);
                }
            }
            i = j;
            continue;
        }
        // 第N章 / 第N节 / 第N部分（含中文数字）：章号语义
        if chars[i] == '第' {
            let rest: String = chars[i + 1..].iter().collect();
            let mut consumed = 1usize;
            let mut token = String::new();
            for c in rest.chars() {
                let ok = c.is_ascii_digit() || is_cn_num_char(c) || c.is_whitespace();
                if !ok {
                    break;
                }
                consumed += 1;
                if !c.is_whitespace() {
                    token.push(c);
                }
            }
            let after: String = chars.get(i + consumed).map(|c| c.to_string()).unwrap_or_default();
            let is_section_word = matches!(after.as_str(), "章" | "节" | "部" | "篇" | "条" | "讲");
            if !token.is_empty() && is_section_word {
                if let Some(SectionRef::Chapter(n)) = parse_section_ref(&token) {
                    let r = SectionRef::Chapter(n);
                    if !out.contains(&r) {
                        out.push(r);
                    }
                }
            }
            i += 1;
            continue;
        }
        i += 1;
    }
    out
}

/// 展示用：`(第 a–b 行)`。
fn range_label(sec: &DocSection) -> String {
    format!("（第 {}–{} 行）", sec.line_start, sec.line_end)
}

// ─────────────────────────── doc_outline ───────────────────────────

/// 文档章节目录：让模型先看清"这份文档有哪些章节、分别在第几行"。
pub struct DocOutlineTool {
    dir: String,
    rel: String,
}

#[async_trait]
impl Tool for DocOutlineTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            readonly_spec(
                "doc_outline",
                "列出当前关联文档的全部章节目录（§序号、标题、行号区间、规模）。\
                 当你要回答的问题涉及上下文里没出现的内容（尤其是「第 N 章/第 N 节」）时，\
                 先调用本工具确认章节是否存在、行号区间是多少，再用 doc_read_section 取正文，\
                 不要直接回答「未纳入上下文」。无参数。",
                json!({ "type": "object", "additionalProperties": false, "properties": {} }),
            )
        })
    }

    async fn execute(&self, _args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let doc = load(&self.dir, &self.rel)?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let mut out = format!(
            "【文档目录】{}（共 {} 行 / {} 字符，{} 个章节）\n",
            doc.rel_path,
            doc.total_lines,
            doc.total_chars,
            doc.sections.len()
        );
        for sec in &doc.sections {
            out.push_str(&format!(
                "§{} {} {} 约 {} tokens\n",
                sec.id,
                sec.heading,
                range_label(sec),
                section_est(sec)
            ));
        }
        out.push_str("\n提示：用 doc_read_section(section=§号) 取回任意章节全文；用 doc_search(query=关键词) 定位具体内容。\n");
        Ok(json!(out))
    }
}

// ─────────────────────────── doc_search ───────────────────────────

/// 关键词检索（等价只读 grep）：返回命中行号 + 所在章节。
pub struct DocSearchTool {
    dir: String,
    rel: String,
}

#[async_trait]
impl Tool for DocSearchTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            readonly_spec(
                "doc_search",
                "在当前关联文档里按关键词检索（等价 grep -i，不区分大小写），返回命中行号与所在章节，\
                 用于定位「某内容出现在文档哪里」。命中后可用 doc_read_section(section=§号 或 start_line/end_line) 读取上下文。",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "query": { "type": "string", "description": "检索关键词（中文/英文均可，按子串匹配）" },
                        "limit": { "type": "integer", "description": "最多返回命中行数（默认 20，上限 40）" }
                    },
                    "required": ["query"]
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 query 参数".into()))?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, MAX_SEARCH_HITS))
            .unwrap_or(DEFAULT_SEARCH_HITS);
        let doc = load(&self.dir, &self.rel)?;

        let needle = query.to_lowercase();
        let mut hits: Vec<(usize, Option<usize>, String, String)> = Vec::new();
        for (idx, line) in doc.full_text.lines().enumerate() {
            if ctx.cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            if line.to_lowercase().contains(&needle) {
                let line_no = idx + 1;
                let sec = doc
                    .sections
                    .iter()
                    .find(|s| line_no >= s.line_start && line_no <= s.line_end);
                let heading = sec.map(|s| format!("§{} {}", s.id, s.heading)).unwrap_or_default();
                let sec_id = sec.map(|s| s.id);
                hits.push((line_no, sec_id, heading, line.trim().to_string()));
                if hits.len() >= limit {
                    break;
                }
            }
        }
        if hits.is_empty() {
            return Ok(json!(format!(
                "【检索】关键字「{query}」在 {} 中未命中（共 {} 行）。可换用更短的关键词，或用 doc_outline 先看章节标题。",
                doc.rel_path, doc.total_lines
            )));
        }
        let mut out = format!(
            "【检索】关键字「{query}」命中 {} 行（最多显示 {} 行）：\n",
            hits.len(),
            limit
        );
        for (line_no, _sec_id, heading, text) in &hits {
            if heading.is_empty() {
                out.push_str(&format!("L{line_no}: {text}\n"));
            } else {
                out.push_str(&format!("L{line_no} {heading}: {text}\n"));
            }
        }
        out.push_str("\n提示：用 doc_read_section(start_line=行号, end_line=行号) 读取命中处的完整上下文。\n");
        Ok(json!(out))
    }
}

// ─────────────────────── doc_read_section ───────────────────────

/// 按 §序号 / 标题关键词 / 行号区间取回原文（带行号，可截断续读）。
pub struct DocReadSectionTool {
    dir: String,
    rel: String,
}

/// 解析 `section` 参数：`"§14"` → 序号；数字 / `"14"` / `"第14章"` / `"十四"` → 章号。
fn arg_section_ref(args: &Value) -> Option<SectionRef> {
    match args.get("section") {
        Some(Value::Number(n)) => n.as_u64().map(|v| SectionRef::Chapter(v as usize)),
        Some(Value::String(s)) => parse_section_ref(s),
        _ => None,
    }
}

#[async_trait]
impl Tool for DocReadSectionTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            readonly_spec(
                "doc_read_section",
                "读取当前关联文档的指定内容（带真实行号，便于按 [§N] 与行号引用）。三种定位方式任选其一：\
                 1) section：§序号或章号（如 14 / \"§14\" / \"第14章\"），标题里的编号也会匹配；\
                 2) heading：标题关键词（子串匹配，如 \"部署\"）；\
                 3) start_line + end_line：直接给行号区间（配合 doc_search 的命中行号使用）。\
                 返回内容超过预算会截断并提示续读行号。",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "section": { "description": "§序号或章号（数字，或 \"§14\"/\"第14章\" 这类字符串）" },
                        "heading": { "type": "string", "description": "标题关键词（子串匹配）" },
                        "start_line": { "type": "integer", "description": "起始行号（1 起，含）" },
                        "end_line": { "type": "integer", "description": "结束行号（1 起，含）" },
                        "max_tokens": { "type": "integer", "description": "本次取文 token 预算（默认 4000，上限 8000）" }
                    }
                }),
            )
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let doc = load(&self.dir, &self.rel)?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let max_tokens = args
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(256, MAX_READ_TOKENS))
            .unwrap_or(DEFAULT_READ_TOKENS);
        let heading_q = args
            .get("heading")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let start_line = args.get("start_line").and_then(|v| v.as_u64()).map(|v| v as usize);
        let end_line = args.get("end_line").and_then(|v| v.as_u64()).map(|v| v as usize);
        let section_ref = arg_section_ref(&args);

        // 1) 行号区间优先（最精确）
        let (range, title) = if let Some(s) = start_line {
            let s = s.max(1);
            let e = end_line.unwrap_or_else(|| (s + 120).min(doc.total_lines)).max(s);
            (Some((s, e.min(doc.total_lines))), format!("第 {s}–{} 行", e.min(doc.total_lines)))
        } else if let Some(sec) = resolve_section(&doc, section_ref, heading_q.as_deref()) {
            (Some((sec.line_start, sec.line_end)), format!("§{} {}", sec.id, sec.heading))
        } else {
            (None, String::new())
        };

        let Some((start, end)) = range else {
            let hint = match section_ref {
                Some(SectionRef::Id(n)) => format!("未找到 §{n}（章节序号不存在）"),
                Some(SectionRef::Chapter(n)) => {
                    format!("未找到编号为 {n} 的章节（标题编号与 §序号都未命中）")
                }
                None => match heading_q.as_deref() {
                    Some(h) => format!("未找到标题包含「{h}」的章节"),
                    None => "缺少定位参数：请给 section / heading / start_line 之一".to_string(),
                },
            };
            return Ok(json!(format!(
                "【取文失败】{hint}。当前文档共 {} 章，可先用 doc_outline 查看章节目录。",
                doc.sections.len()
            )));
        };

        let lines: Vec<&str> = doc.full_text.lines().collect();
        let mut out = format!(
            "【文档取文】{} · {}（第 {}–{} 行）\n",
            doc.rel_path, title, start, end
        );
        let mut used = 0usize;
        let mut last_line = start.saturating_sub(1);
        for line_no in start..=end {
            let Some(line) = lines.get(line_no - 1) else { break };
            let est = estimate_tokens(line) + 1;
            if used + est > max_tokens {
                break;
            }
            used += est;
            last_line = line_no;
            out.push_str(&format!("{line_no}| {line}\n"));
        }
        if last_line < end {
            out.push_str(&format!(
                "\n（内容超预算已截断：本次显示到第 {last_line} 行；如需后续内容，请再次调用 doc_read_section(start_line={}, end_line={}) 续读）\n",
                last_line + 1,
                end
            ));
        }
        out.push_str("\n引用该内容时请标注对应的 [§N] 与行号。\n");
        Ok(json!(out))
    }
}

/// 章节解析优先级：`heading` 关键词 → 按引用语义定位。
///
/// - `§N`（[`SectionRef::Id`]）：直接取章节序号 N，标题编号仅作兜底；
/// - `第14章` / 裸数字（[`SectionRef::Chapter`]）：先用标题里的编号匹配，再退回章节序号 N。
fn resolve_section<'a>(
    doc: &'a DocFile,
    section_ref: Option<SectionRef>,
    heading_q: Option<&str>,
) -> Option<&'a DocSection> {
    if let Some(h) = heading_q {
        let hl = h.to_lowercase();
        if let Some(s) = doc.sections.iter().find(|s| s.heading.to_lowercase().contains(&hl)) {
            return Some(s);
        }
    }
    match section_ref {
        Some(SectionRef::Id(n)) => doc
            .sections
            .iter()
            .find(|s| s.id == n)
            .or_else(|| doc.sections.iter().find(|s| heading_matches_number(&s.heading, n))),
        Some(SectionRef::Chapter(n)) => doc
            .sections
            .iter()
            .find(|s| heading_matches_number(&s.heading, n))
            .or_else(|| doc.sections.iter().find(|s| s.id == n)),
        None => None,
    }
}

/// 构造宿主 A 的只读工具注册表（绑定到「当前关联文件」，模型无法指定路径）。
pub fn doc_file_tools(dir: &str, rel: &str) -> Arc<dyn ToolRegistry> {
    let mut reg = HashMapToolRegistry::new();
    reg.register(Arc::new(DocOutlineTool { dir: dir.to_string(), rel: rel.to_string() }));
    reg.register(Arc::new(DocSearchTool { dir: dir.to_string(), rel: rel.to_string() }));
    reg.register(Arc::new(DocReadSectionTool { dir: dir.to_string(), rel: rel.to_string() }));
    Arc::new(reg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_refs_digits_and_cjk() {
        assert_eq!(parse_section_ref("§14"), Some(SectionRef::Id(14)));
        assert_eq!(parse_section_ref("第14章"), Some(SectionRef::Chapter(14)));
        assert_eq!(parse_section_ref("14"), Some(SectionRef::Chapter(14)));
        assert_eq!(parse_section_ref("第十四章"), Some(SectionRef::Chapter(14)));
        assert_eq!(parse_section_ref("第二十一章"), Some(SectionRef::Chapter(21)));
        assert_eq!(parse_section_ref("部署"), None);
    }

    #[test]
    fn query_refs_extraction() {
        assert_eq!(query_section_refs("第14章节讲了什么？"), vec![SectionRef::Chapter(14)]);
        assert_eq!(query_section_refs("请看 §7 的内容"), vec![SectionRef::Id(7)]);
        assert_eq!(query_section_refs("第十四章的核心结论"), vec![SectionRef::Chapter(14)]);
        assert!(query_section_refs("总结全文").is_empty());
    }

    #[test]
    fn heading_number_match() {
        assert!(heading_matches_number("14. 部署与发布", 14));
        assert!(heading_matches_number("第14章 部署", 14));
        assert!(heading_matches_number("14、部署", 14));
        assert!(!heading_matches_number("部署", 14));
    }
}
