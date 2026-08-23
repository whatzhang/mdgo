use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

use super::utils;
use crate::core::document::chunk_engine::{Chunk, ChunkEngine, SemanticChunkEngine};
use crate::core::document::text_split::char_len;
use crate::core::document::{ComrakMarkdownParser, MarkdownParser};

// ─── ChunkSplitter 特质（策略模式） ───

/// 分块结果，包含文本和可选的结构化元数据。
#[derive(Debug, Clone)]
pub struct ChunkResult {
    pub text: String,
    /// 节点在树形结构中的深度（仅 OPML/FreeMind 文件有值）
    pub path_depth: Option<u32>,
    /// 节点路径的 JSON 数组（仅 OPML/FreeMind 文件有值），如 `["项目计划","第一阶段"]`
    pub path_json: Option<String>,
    /// 句子级 chunk 的上下文窗口文本（SentenceWindow 用），存储该句子周边的扩展上下文
    pub sentence_window: Option<String>,
    /// 符号名（仅代码文件有值），如函数名、类名
    pub symbol_name: Option<String>,
    /// 符号类型（仅代码文件有值），如 "function"、"class"、"method"
    pub symbol_kind: Option<String>,
    /// 向量化文本（AST 语义分块用）：与 `text` 分离，避免标题词稀释正文语义；
    /// `None` 表示向量化时直接使用 `text`。
    pub embedding_text: Option<String>,
    /// 分块类型（AST 语义分块用）：paragraph/code/table/list/quote/section 等
    pub chunk_type: Option<String>,
    /// 文档显式标题（P0-1：frontmatter `title`，缺失时检索侧回退文件名；BM25 title 字段优先使用）
    pub doc_title: Option<String>,
    /// 文档标签（P0-1：frontmatter `tags` + `aliases`；BM25 tags 字段）
    pub tags: Option<Vec<String>>,
}

impl ChunkResult {
    /// 创建一个无元数据的普通 chunk
    pub fn plain(text: String) -> Self {
        Self {
            text,
            path_depth: None,
            path_json: None,
            sentence_window: None,
            symbol_name: None,
            symbol_kind: None,
            embedding_text: None,
            chunk_type: None,
            doc_title: None,
            tags: None,
        }
    }

    /// 创建一个带代码符号元数据的 chunk
    pub fn code(text: String, symbol_name: Option<String>, symbol_kind: Option<String>) -> Self {
        Self {
            text,
            path_depth: None,
            path_json: None,
            sentence_window: None,
            symbol_name,
            symbol_kind,
            embedding_text: None,
            chunk_type: None,
            doc_title: None,
            tags: None,
        }
    }

    /// 创建一个文件概览 chunk（标记 symbol_kind = "file"）。
    /// 文件概览 chunk 包含 imports 摘要、所有定义的符号名等，用于"按文件名/用途搜索"。
    pub fn file_overview(text: String) -> Self {
        Self {
            text,
            path_depth: None,
            path_json: None,
            sentence_window: None,
            symbol_name: None,
            symbol_kind: Some("file".to_string()),
            embedding_text: None,
            chunk_type: None,
            doc_title: None,
            tags: None,
        }
    }
}

/// AST 语义 chunk → 对外分块结果。
///
/// 转换保持在 `db` 层（依赖 `document`），维持 `document` 为纯基础层的分层方向：
/// heading_path 映射到 path_depth / path_json。
impl From<Chunk> for ChunkResult {
    fn from(chunk: Chunk) -> Self {
        let path_depth = (!chunk.path.is_empty()).then_some(chunk.path.len() as u32);
        let path_json = (!chunk.path.is_empty())
            .then(|| serde_json::to_string(&chunk.path).unwrap_or_default());
        ChunkResult {
            text: chunk.text,
            path_depth,
            path_json,
            sentence_window: None,
            symbol_name: None,
            symbol_kind: None,
            embedding_text: Some(chunk.embedding_text),
            chunk_type: Some(chunk.chunk_type),
            doc_title: None,
            tags: None,
        }
    }
}

/// ChunkSplitter 特质：定义文本分割的统一接口。
///
/// 每种文件类型（Markdown、纯文本等）实现该特质，提供不同分割策略。
pub trait ChunkSplitter: Send + Sync {
    /// 将输入文本分割为若干文本块，每个块可能携带结构化元数据。
    fn split(&self, text: &str, max_size: usize, overlap: usize) -> Vec<ChunkResult>;
}

// ─── 纯文本文档分割器 ───

/// 纯文本文档分割器
///
/// 按句子边界（。！？等）切分，适合代码、配置、普通文本等文件。
/// P0-2：token 感知切分（一次 tokenize + 按 token 预算定位切分点）；
/// tokenizer 不可用时降级字符切分。
pub struct PlainTextChunkSplitter;

impl ChunkSplitter for PlainTextChunkSplitter {
    fn split(&self, text: &str, max_size: usize, overlap: usize) -> Vec<ChunkResult> {
        let counter = crate::core::document::token_budget::global_token_counter();
        let pieces = crate::core::document::text_split::split_text_token_aware(
            text,
            max_size,
            overlap,
            crate::core::document::text_split::GENERIC_TEXT_SEPARATORS,
            &*counter,
        )
        .unwrap_or_else(|| {
            // 🟠 M8 修复：tokenizer 不可用降级字符切分前，先把 token 语义的
            // max_size/overlap 按文本实际密度折算为字符预算（旧实现直接把 token
            // 值当字符数：英文 ≈4 字符/token，分片会放大到预算的 ~4 倍，
            // 先产出一批超限 chunk 再被 Validator 全量重切）。
            let (char_max, char_overlap) =
                crate::core::document::token_budget::char_budget_pair(text, max_size, overlap, &*counter);
            utils::split_text(text, char_max, char_overlap)
        });
        pieces.into_iter().map(ChunkResult::plain).collect()
    }
}

// ─── 代码语言感知分割器 ───

/// 代码语言感知分块器：按函数/类边界分割代码文件，并提取符号名。
///
/// # 原理（LangChain RecursiveCharacterTextSplitter 思路）
/// 使用语言特定的结构化分隔符（如 \nfn \nclass \ndef ）作为高优先级切分点，
/// 保证每个 chunk 尽可能保持在函数/类边界内。若单块超长，降级到通用分隔符。
///
/// # 符号提取
/// 每个 chunk 的首行若匹配函数/类定义模式，则提取符号名（如 "parseJSON"），
/// 填充到 `symbol_name` 和 `symbol_kind` 字段，用于后续 BM25 加权检索。
pub struct CodeAwareChunkSplitter {
    /// 语言特定分隔符列表（高优先级在前）
    separators: Vec<&'static str>,
    /// 语言特定符号提取正则（捕获组 1=符号名，组 2=符号类型）
    symbol_pattern: Option<Regex>,
}

/// 所有代码语言的符号提取通用正则（匹配常见函数/类定义语法）。
///
/// D4 增强：
/// - 可选前缀扩展：`pub` / `export` / `export default` / `public` / `private` / `protected` / `async`
/// - 泛型签名后跟符号名：`impl<T> Foo`、`fn foo<T>(`（`impl\s+` 后允许 `<...>`）
/// - 符号名取关键字后第一个标识符（捕获组 1）
static CODE_SYMBOL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(
        r"(?m)^[\t ]*(?:(?:pub|export|export\s+default|public|private|protected|async)\s+)*?(?:fn|def|function|func|class|struct|enum|trait|interface|type|object)\s+(\w+)|^(?:impl)\s*<[^>]*>\s*(\w+)|^(?:impl)\s+(\w+)|^(?:const|let|var)\s+(\w+)\s*=\s*(?:async\s*)?(?:\(|function|class)|^[\t ]*(?:(?:pub|export|export\s+default|public|private|protected|async)\s+)*(?:const|let|var|static)\s+(\w+)\s*:\s*[^=\r\n]+?=",
    )
    .expect("CODE_SYMBOL_RE 编译失败")
});

/// 语言特定分隔符映射表
static CODE_LANG_SEPARATORS: std::sync::LazyLock<HashMap<&'static str, Vec<&'static str>>> =
    std::sync::LazyLock::new(|| {
        let mut m = HashMap::new();
        // Python
        m.insert("py", vec!["\nclass ", "\nasync def ", "\ndef ", "\n\n", "\n", " ", ""]);
        // Rust
        m.insert("rs", vec!["\nimpl ", "\ntrait ", "\nenum ", "\nstruct ", "\nfn ", "\n\n", "\n", " ", ""]);
        // Go
        m.insert("go", vec!["\nfunc ", "\ntype ", "\nstruct ", "\ninterface ", "\n\n", "\n", " ", ""]);
        // JS/TS
        m.insert("js", vec!["\nclass ", "\nfunction ", "\nexport ", "\nconst ", "\nlet ", "\nvar ", "\n\n", "\n", " ", ""]);
        m.insert("ts", vec!["\nclass ", "\nfunction ", "\nexport ", "\ninterface ", "\ntype ", "\nconst ", "\nlet ", "\nvar ", "\n\n", "\n", " ", ""]);
        m.insert("jsx", vec!["\nclass ", "\nfunction ", "\nexport ", "\nconst ", "\n\n", "\n", " ", ""]);
        m.insert("tsx", vec!["\nclass ", "\nfunction ", "\nexport ", "\ninterface ", "\ntype ", "\nconst ", "\n\n", "\n", " ", ""]);
        // Java
        m.insert("java", vec!["\nclass ", "\ninterface ", "\nenum ", "\npublic ", "\nprivate ", "\nprotected ", "\n\n", "\n", " ", ""]);
        // C/C++
        m.insert("c", vec!["\nstruct ", "\nenum ", "\nunion ", "\n\n", "\n", " ", ""]);
        m.insert("cpp", vec!["\nclass ", "\nstruct ", "\nenum ", "\nunion ", "\n\n", "\n", " ", ""]);
        m.insert("h", vec!["\nstruct ", "\nenum ", "\nclass ", "\n\n", "\n", " ", ""]);
        m.insert("hpp", vec!["\nclass ", "\nstruct ", "\nenum ", "\n\n", "\n", " ", ""]);
        // C#
        m.insert("cs", vec!["\nclass ", "\nstruct ", "\nenum ", "\ninterface ", "\npublic ", "\nprivate ", "\nprotected ", "\n\n", "\n", " ", ""]);
        // Swift
        m.insert("swift", vec!["\nclass ", "\nstruct ", "\nenum ", "\nfunc ", "\nvar ", "\nlet ", "\n\n", "\n", " ", ""]);
        // Kotlin
        m.insert("kt", vec!["\nclass ", "\nfun ", "\ninterface ", "\nobject ", "\n\n", "\n", " ", ""]);
        // PHP
        m.insert("php", vec!["\nclass ", "\nfunction ", "\n\n", "\n", " ", ""]);
        // Ruby
        m.insert("rb", vec!["\nclass ", "\ndef ", "\nmodule ", "\n\n", "\n", " ", ""]);
        // Shell
        m.insert("sh", vec!["\nfunction ", "\n\n", "\n", " ", ""]);
        m.insert("bash", vec!["\nfunction ", "\n\n", "\n", " ", ""]);
        m.insert("zsh", vec!["\nfunction ", "\n\n", "\n", " ", ""]);
        // Lua
        m.insert("lua", vec!["\nfunction ", "\n\n", "\n", " ", ""]);
        // SQL
        m.insert("sql", vec!["\nCREATE ", "\nALTER ", "\nDROP ", "\nINSERT ", "\nUPDATE ", "\nDELETE ", "\nSELECT ", "\n\n", "\n", " ", ""]);
        // R
        m.insert("r", vec!["\nfunction", "\nsetClass", "\nsetMethod", "\n\n", "\n", " ", ""]);
        // Scala
        m.insert("scala", vec!["\nclass ", "\nobject ", "\ntrait ", "\ndef ", "\n\n", "\n", " ", ""]);
        // Dart
        m.insert("dart", vec!["\nclass ", "\nvoid ", "\n\n", "\n", " ", ""]);
        m
    });

impl CodeAwareChunkSplitter {
    /// 根据文件扩展名创建对应的代码分块器
    pub fn for_extension(ext: &str) -> Self {
        let separators = CODE_LANG_SEPARATORS
            .get(ext)
            .cloned()
            .unwrap_or_else(|| vec!["\n\n", "\n", " ", ""]);
        let symbol_pattern = if CODE_LANG_SEPARATORS.contains_key(ext) {
            Some(CODE_SYMBOL_RE.clone())
        } else {
            None
        };
        Self { separators, symbol_pattern }
    }
}

impl ChunkSplitter for CodeAwareChunkSplitter {
    fn split(&self, text: &str, max_size: usize, overlap: usize) -> Vec<ChunkResult> {
        split_code_with_overview(text, self, max_size, overlap)
    }
}

/// 递归分隔符切分：从最高优先级分隔符开始尝试，超长块降级到下一级分隔符。
fn split_recursive_by_separators(text: &str, separators: &[&str], max_size: usize) -> Vec<String> {
    if char_len(text) <= max_size || separators.is_empty() {
        return vec![text.to_string()];
    }

    let sep = separators[0];
    let rest = &separators[1..];

    let mut parts = Vec::new();
    if sep.is_empty() {
        // 最后一级：按字符切分（避免 UTF-8 多字节字符被截断）
        let chars: Vec<char> = text.chars().collect();
        for chunk in chars.chunks(max_size) {
            parts.push(chunk.iter().collect::<String>());
        }
    } else {
        for part in text.split(sep) {
            if part.is_empty() { continue; }
            if char_len(part) <= max_size {
                parts.push(part.to_string());
            } else {
                // 超长，降级到下一级分隔符
                let sub_parts = split_recursive_by_separators(part, rest, max_size);
                parts.extend(sub_parts);
            }
        }
    }

    parts
}

/// 合并太小的块：相邻块合并直到达到 max_size * 0.4 或遇到不同符号
fn merge_small_chunks(chunks: &[String], max_size: usize, overlap: usize) -> Vec<String> {
    if chunks.len() <= 1 {
        return chunks.to_vec();
    }
    let min_size = (max_size as f64 * 0.4) as usize;
    let mut result = Vec::new();
    let mut buffer = String::new();

    for chunk in chunks {
        if buffer.is_empty() {
            buffer = chunk.clone();
        } else if char_len(&buffer) < min_size {
            buffer.push('\n');
            buffer.push_str(chunk);
        } else {
            result.push(std::mem::take(&mut buffer));
            buffer = chunk.clone();
        }
    }
    if !buffer.is_empty() {
        result.push(buffer);
    }

    // overlap 处理：若还有空间，在块间插入 overlap 字符
    // 🟠 L15：`> 0` 取代旧 `> 10` 门槛——调用方已把 token overlap 按文本密度折算为
    // 字符（英文低密度文本折算后可能 ≤10 字符），旧门槛会把有效 overlap 静默丢弃。
    if overlap > 0 && result.len() > 1 {
        let overlap_size = overlap.min(max_size / 4);
        if overlap_size > 0 {
            let mut merged = Vec::new();
            for i in 0..result.len() {
                if i == 0 {
                    merged.push(result[i].clone());
                } else {
                    let prev_tail: String = result[i - 1]
                        .chars()
                        .rev()
                        .take(overlap_size)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    let new_chunk = format!("{}\n{}", prev_tail, result[i]);
                    merged.push(new_chunk);
                }
            }
            return merged;
        }
    }

    result
}

/// 符号感知的合并：不合并两个都有独立符号名的相邻 chunk，
/// 保证每个函数/类定义保持独立 chunk 身份。
/// 仅将无符号的小 chunk 合并到前一个有符号的 chunk 中。
fn merge_small_chunks_symbol_aware(chunks: Vec<ChunkResult>, max_size: usize) -> Vec<ChunkResult> {
    if chunks.len() <= 1 {
        return chunks;
    }
    let min_size = (max_size as f64 * 0.4) as usize;
    let mut result: Vec<ChunkResult> = Vec::new();

    for chunk in chunks {
        if let Some(prev) = result.last_mut() {
            let prev_has_symbol = prev.symbol_name.is_some();
            let curr_has_symbol = chunk.symbol_name.is_some();
            let prev_too_small = char_len(&prev.text) < min_size;

            if prev_too_small && !curr_has_symbol {
                // 前一个 chunk 太小且当前无符号：合并到前一个
                let merged_text = format!("{}\n{}", prev.text, chunk.text);
                *prev = ChunkResult {
                    text: merged_text,
                    path_depth: prev.path_depth.or(chunk.path_depth),
                    path_json: prev.path_json.clone().or(chunk.path_json),
                    sentence_window: prev.sentence_window.clone().or(chunk.sentence_window),
                    symbol_name: prev.symbol_name.clone(),
                    symbol_kind: prev.symbol_kind.clone(),
                    embedding_text: prev.embedding_text.clone().or(chunk.embedding_text),
                    chunk_type: prev.chunk_type.clone().or(chunk.chunk_type),
                    doc_title: prev.doc_title.clone().or(chunk.doc_title),
                    tags: prev.tags.clone().or(chunk.tags),
                };
                continue;
            }
            if !curr_has_symbol && !prev_has_symbol && char_len(&prev.text) < min_size {
                // 两个都无符号且前一个太小：合并
                let merged_text = format!("{}\n{}", prev.text, chunk.text);
                *prev = ChunkResult {
                    text: merged_text,
                    path_depth: prev.path_depth.or(chunk.path_depth),
                    path_json: prev.path_json.clone().or(chunk.path_json),
                    sentence_window: prev.sentence_window.clone().or(chunk.sentence_window),
                    symbol_name: prev.symbol_name.clone(),
                    symbol_kind: prev.symbol_kind.clone(),
                    embedding_text: prev.embedding_text.clone().or(chunk.embedding_text),
                    chunk_type: prev.chunk_type.clone().or(chunk.chunk_type),
                    doc_title: prev.doc_title.clone().or(chunk.doc_title),
                    tags: prev.tags.clone().or(chunk.tags),
                };
                continue;
            }
        }
        result.push(chunk);
    }

    result
}

/// 从文本块首行提取符号名和类型
///
/// 检测逻辑：
/// - 优先从 chunk 首行提取关键字（因为分隔符切分后关键字一般在行首）
/// - 兼容 `.contains(" fn ")` 和 `.starts_with("fn ")`（不同切分场景）
/// - 也检测 `pub/async/pub async` 前缀（Rust 等语言）
fn extract_symbol_info(text: &str, re: &Regex) -> (Option<String>, Option<String>) {
    if let Some(caps) = re.captures(text) {
        // D4：多分支正则——取任一非空捕获组作为符号名
        let name = caps
            .iter()
            .skip(1)
            .find_map(|m| m.map(|m| m.as_str().to_string()));
        let kind = detect_symbol_kind(text);
        (name, kind)
    } else {
        (None, None)
    }
}

/// 根据文本内容检测符号类型
fn detect_symbol_kind(text: &str) -> Option<String> {
    // 首行检测（最可靠——切分后关键字在行首）
    let first_line = text.lines().next().unwrap_or(text);
    // 同时检测 contains 和 starts_with，覆盖不同切分场景
    let tests: &[(&str, &str)] = &[
        ("class", "class"),
        ("struct", "struct"),
        ("enum", "enum"),
        ("trait", "trait"),
        ("interface", "interface"),
        ("fn", "function"),
        ("def", "function"),
        ("function", "function"),
        ("func", "function"),
        ("type", "type"),
    ];
    for (kw, kind) in tests {
        if first_line.contains(&format!(" {} ", kw))
            || first_line.starts_with(&format!("{} ", kw))
            || first_line.starts_with(&format!("pub {} ", kw))
            || first_line.starts_with(&format!("async {} ", kw))
            || first_line.starts_with(&format!("pub async {} ", kw))
        {
            return Some(kind.to_string());
        }
    }
    Some("symbol".to_string())
}

/// 从代码文本中提取所有定义的符号名（函数、类、结构体等），去重并保持顺序。
fn extract_all_symbols(text: &str) -> Vec<String> {
    let re = &CODE_SYMBOL_RE;
    let mut seen = std::collections::HashSet::new();
    let mut symbols = Vec::new();
    for cap in re.captures_iter(text) {
        // D4：多分支正则——取任一非空捕获组
        if let Some(m) = cap.iter().skip(1).find_map(|m| m) {
            let name = m.as_str();
            if seen.insert(name.to_string()) {
                symbols.push(name.to_string());
            }
        }
    }
    symbols
}

/// 从代码文本中提取文件概览信息，构建"文件概览 chunk"的文本内容。
///
/// 返回 None 表示无需概览（文件太小、无符号、无 imports）。
fn build_file_overview_text(text: &str) -> Option<String> {
    if char_len(text) < 80 {
        return None;
    }

    // 收集导入语句（前 30 行）
    let lines: Vec<&str> = text.lines().collect();
    let import_re = Regex::new(r"^\s*(?:import\s|from\s|use\s|#include|require\b|pub\s+use|using\b)").unwrap();
    let mut imports: Vec<&str> = Vec::new();
    for line in lines.iter().take(30) {
        if import_re.is_match(line) {
            let trimmed = line.trim();
            if !imports.contains(&trimmed) {
                imports.push(trimmed);
            }
        }
    }

    let symbols = extract_all_symbols(text);

    // 无符号也无 imports → 不需要单独概览（小文件或配置文件）
    if symbols.is_empty() && imports.is_empty() {
        return None;
    }

    let mut parts = Vec::new();

    if !symbols.is_empty() {
        let sym_str = symbols.iter().take(20).map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
        if symbols.len() > 20 {
            parts.push(format!("符号: {}... (共 {} 个)", sym_str, symbols.len()));
        } else {
            parts.push(format!("符号: {}", sym_str));
        }
    }

    if !imports.is_empty() {
        let imp_str = imports.iter().take(5).map(|s| s.to_string()).collect::<Vec<_>>().join("; ");
        if imports.len() > 5 {
            parts.push(format!("导入: {}... (共 {} 条)", imp_str, imports.len()));
        } else {
            parts.push(format!("导入: {}", imp_str));
        }
    }

    Some(parts.join(" | "))
}

/// 检测文本是否主要是注释内容（判断比例是否 > 50%）。
/// 支持：
/// - # 注释（Python、Shell、Ruby）
/// - // /* */ 注释（C、Rust、JS/TS）
/// -- 注释（SQL）
/// - """ ''' 文档字符串（Python）
fn is_comment_block(text: &str) -> bool {
    let total_chars = char_len(text);
    if total_chars < 5 {
        return false;
    }
    let comment_chars = text
        .chars()
        .filter(|&c| c == '#' || c == '/' || c == '*' || c == ' ' || c == '\n' || c == '\t' || c == '-' || c == '"' || c == '\'')
        .count();
    // 分子分母统一按字符计数（原实现分子用 chars、分母用 bytes，单位混算导致比例失真）
    comment_chars as f64 / total_chars as f64 > 0.5
}

/// 将相邻的注释块合并到后续的函数/类定义 chunk 中。
///
/// 例如：
/// ```python
/// # LRU Cache implementation
/// # This is a doc comment
/// class LRUCache:
/// ```
/// 当分隔符切分后，注释块和类定义被拆成两个 chunk。此函数检测相邻的
/// "无符号的注释块 + 有符号的代码块" 模式，将注释合并到代码块前。
fn merge_comment_into_func(chunks: Vec<ChunkResult>, max_size: usize) -> Vec<ChunkResult> {
    if chunks.len() <= 1 {
        return chunks;
    }
    let mut merged: Vec<ChunkResult> = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        if let Some(prev) = merged.last_mut() {
            // 当前 chunk 有符号（函数/类定义），前一个 chunk 是注释块
            if chunk.symbol_name.is_some()
                && prev.symbol_name.is_none()
                && prev.symbol_kind.as_deref() != Some("file")
                && char_len(&prev.text) < max_size / 2
                && is_comment_block(&prev.text)
            {
                let merged_text = format!("{}\n{}", prev.text, chunk.text);
                *prev = ChunkResult::code(
                    merged_text,
                    chunk.symbol_name.clone(),
                    chunk.symbol_kind.clone(),
                );
                continue;
            }
        }
        merged.push(chunk);
    }
    merged
}

/// 增强的代码分块：先生成符号感知的代码块，再为文件生成一个概览 chunk。
///
/// # 概览 chunk 生成规则
/// - 当文件包含 2 个以上符号或富含 imports 时，自动生成文件概览 chunk
/// - 概览 chunk 的 `symbol_kind = Some("file")`，便于检索时识别
/// - 概览 chunk 被插入到结果列表的最前面（chunk_index = 0）
fn split_code_with_overview(
    text: &str,
    splitter: &CodeAwareChunkSplitter,
    max_size: usize,
    overlap: usize,
) -> Vec<ChunkResult> {
    // P0-2：max_size/overlap 语义为 token（`chunk_size` 配置升级后），
    // 按文本实际 token 密度折算为字符预算供 char-based 递归切分（英文 ~4 字符/token，
    // 中文 ~1 字符/token）；密度波动由 TokenBudgetValidator 兜底最终裁决。
    let counter = crate::core::document::token_budget::global_token_counter();
    let (max_size, overlap) =
        crate::core::document::token_budget::char_budget_pair(text, max_size, overlap, &*counter);
    // 1. 使用语言特定分隔符做递归切分
    let raw_chunks = split_recursive_by_separators(text, &splitter.separators, max_size);
    // 2. 简单合并太小的原始块（无符号信息，仅基于长度）
    let chunks = merge_small_chunks(&raw_chunks, max_size, overlap);
    // 3. 提取符号
    let mut results: Vec<ChunkResult> = chunks
        .into_iter()
        .map(|t| {
            let (symbol_name, symbol_kind) = if let Some(ref re) = splitter.symbol_pattern {
                extract_symbol_info(&t, re)
            } else {
                (None, None)
            };
            ChunkResult::code(t, symbol_name, symbol_kind)
        })
        .collect();

    // 4. 符号感知合并：不合并两个有独立符号的 chunk，保证函数边界
    results = merge_small_chunks_symbol_aware(results, max_size);
    // 5. 注释块合并：将相邻的注释/文档合并到后续的函数/类定义 chunk 中
    results = merge_comment_into_func(results, max_size);

    // 6. 生成文件概览 chunk（如果有足够符号或 imports）
    if let Some(overview_text) = build_file_overview_text(text) {
        let overview = ChunkResult::file_overview(overview_text);
        results.insert(0, overview); // 插入最前面，chunk_index = 0
    }

    results
}

/// Markdown 分块器配置，支持业务灵活调整规则
#[derive(Debug, Clone)]
pub struct MarkdownSplitConfig {
    /// 开启 Setext 标题识别（=== / --- 二级标题）
    pub enable_setext_heading: bool,
    /// 拆分时正文最小预留 token 数，防止前缀占满空间
    pub min_body_reserve_tokens: usize,
}

impl Default for MarkdownSplitConfig {
    fn default() -> Self {
        Self {
            enable_setext_heading: true,
            min_body_reserve_tokens: 64,
        }
    }
}

// ─── Markdown 文档分割器（AST 语义分块版） ───

/// Markdown 文档分割器
///
/// **全 AST 方案**：Markdown → comrak 完整解析 → `DocumentNode` 文档树 →
/// `SemanticChunkEngine` 语义分块。
///
/// 与旧版（行切片 + 长度切分）相比的收益：
/// - 标题层级成为结构性父节点，heading_path（path_json）随之产生
/// - 段落 / 列表 / 表格 / 代码块作为整体参与分组，**不再中途截断语义单元**
/// - `text`（上下文文本）与 `embedding_text`（向量化文本）分离，标题不稀释向量
/// - 无标题文档同样按块边界分组（旧版退化为字符切分）
#[derive(Debug, Clone)]
pub struct MarkdownChunkSplitter {
    config: MarkdownSplitConfig,
}

impl MarkdownChunkSplitter {
    pub fn new() -> Self {
        Self {
            config: MarkdownSplitConfig::default(),
        }
    }
}

impl Default for MarkdownChunkSplitter {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkSplitter for MarkdownChunkSplitter {
    fn split(&self, text: &str, max_chars: usize, overlap: usize) -> Vec<ChunkResult> {
        let config = &self.config;

        if text.trim().is_empty() {
            return Vec::new();
        }

        // 全 AST 解析：换行符归一化与 FrontMatter 剥离由 Document Parser 统一处理
        // （唯一 normalize 点，行号切片基于同一份文本，保持一致）。
        // 标题层级、代码块、表格、列表、引用均由 CommonMark/GFM 解析器保证，
        // 不再按行号做章节边界切片，语义单元（段落/列表项/代码行）不会被截断。
        let parser = ComrakMarkdownParser;
        let document = parser.parse(text, !config.enable_setext_heading);

        // P0-2：token 预算（max_chars/overlap 语义为 token）注入引擎
        let budget = crate::core::document::token_budget::budget_from_config(max_chars, overlap);
        let engine = SemanticChunkEngine::new(
            budget.target_tokens,
            budget.overlap_tokens,
            config.min_body_reserve_tokens,
            budget.prefix_max_tokens,
            crate::core::document::token_budget::global_token_counter(),
        );
        engine
            .build(&document)
            .into_iter()
            .map(ChunkResult::from)
            .collect()
    }
}

/// HTML 分块器：scraper 解析 → 语义 AST（DocumentNode）→ SemanticChunkEngine 语义分块。
///
/// 使 HTML 从 PlainText 无结构分块升级为与 Markdown 一致的语义边界分块
/// （标题层级 + 段落/代码/表格/列表/引用边界，chunk_type 复用既有取值）。
/// 解析失败降级为纯文本分块（不阻断索引）。
#[derive(Debug, Clone)]
pub struct HtmlChunkSplitter;

impl HtmlChunkSplitter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for HtmlChunkSplitter {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkSplitter for HtmlChunkSplitter {
    fn split(&self, text: &str, max_chars: usize, overlap: usize) -> Vec<ChunkResult> {
        if text.trim().is_empty() {
            return Vec::new();
        }
        let parser = crate::core::document::html_ast::HtmlDocumentParser;
        let document = match parser.parse(text) {
            Ok(doc) => doc,
            Err(_) => {
                // 解析失败降级为纯文本分块
                return PlainTextChunkSplitter.split(text, max_chars, overlap);
            }
        };
        let config = MarkdownSplitConfig::default();
        // P0-2：token 预算注入引擎
        let budget = crate::core::document::token_budget::budget_from_config(max_chars, overlap);
        let engine = SemanticChunkEngine::new(
            budget.target_tokens,
            budget.overlap_tokens,
            config.min_body_reserve_tokens,
            budget.prefix_max_tokens,
            crate::core::document::token_budget::global_token_counter(),
        );
        engine
            .build(&document)
            .into_iter()
            .map(ChunkResult::from)
            .collect()
    }
}

// ─── 树形处理器常量 ───/// 短叶子节点最大字符数
const SHORT_LEAF_MAX_CHARS: usize = 8;
/// 路径前缀最大保留级数
const PATH_MAX_LEVELS: usize = 3;
/// 路径前缀最大字符数（超过则截断）
const PATH_MAX_CHARS: usize = 50;

// ─── TreeNode 特质（树形节点统一访问接口） ───

/// 树形节点特质：提供统一访问接口，供 TreeProcessor 使用。
///
/// 适用于 OPML、FreeMind 等树形大纲格式的节点访问。
/// 遵循接口隔离原则，每种格式的节点只需实现本特质的三个方法。
trait TreeNode: Sized {
    fn text(&self) -> &str;
    fn note(&self) -> &str;
    fn children(&self) -> &[Self];
}

// ─── 宏：消除 OPML / FreeMind 的 TreeNode 和 ChunkSplitter 重复实现 ───

/// 为树形节点类型实现 TreeNode trait
macro_rules! impl_tree_node {
    ($node_type:ty) => {
        impl TreeNode for $node_type {
            fn text(&self) -> &str { &self.text }
            fn note(&self) -> &str { &self.note }
            fn children(&self) -> &[Self] { &self.children }
        }
    };
}

/// 为树形格式分割器实现 ChunkSplitter trait（解析 + TreeProcessor 遍历）
macro_rules! impl_tree_chunk_splitter {
    ($splitter_type:ty, $parse_fn:expr) => {
        impl ChunkSplitter for $splitter_type {
            fn split(&self, text: &str, max_size: usize, overlap: usize) -> Vec<ChunkResult> {
                let nodes = $parse_fn(text);
                if nodes.is_empty() {
                    return utils::split_text_char_based(text, max_size, overlap)
                        .into_iter().map(ChunkResult::plain).collect();
                }
                // P0-2：max_size/overlap 语义为 token → 按密度折算字符预算（TreeProcessor 为 char-based）
                let counter = crate::core::document::token_budget::global_token_counter();
                let (max_size, overlap) =
                    crate::core::document::token_budget::char_budget_pair(text, max_size, overlap, &*counter);
                let mut result = Vec::new();
                for root_node in &nodes {
                    TreeProcessor::process_node(root_node, &[], max_size, overlap, &mut result);
                }
                result
            }
        }
    };
}

// ─── TreeProcessor（树形处理公共逻辑） ───

/// 树形大纲文档的通用 chunk 处理引擎。
///
/// 封装了所有与节点类型无关的树形遍历逻辑：
/// - DFS 递归遍历
/// - 路径上下文前缀构建
/// - 短叶子兄弟聚合
/// - 空容器跳过
/// - 超长内容二次切分
/// - HTML note 清洗
///
/// 遵循单一职责原则：只处理树形遍历和 chunk 生成，不关心具体 XML 格式。
/// 遵循开闭原则：新增格式时只需实现 TreeNode，无需修改本处理器。
struct TreeProcessor;

impl TreeProcessor {
    // ─── HTML 清洗 ───

    /// 清洗 HTML 标签，返回纯文本。
    ///
    /// 块级标签转为换行，其余标签直接剥离，解码 HTML 实体，压缩空白行。
    fn clean_html(raw: &str) -> String {
        // Phase 1: 块级标签转换行 —— 用单次正则替换减少中间分配
        static BLOCK_RE: OnceLock<Regex> = OnceLock::new();
        let re = BLOCK_RE.get_or_init(|| {
            Regex::new(r"</?(?:p|br\s*/?|div|li)>").unwrap()
        });
        let s = re.replace_all(raw, |caps: &regex::Captures| {
            match &caps[0] {
                "<p>" | "<br>" | "<br/>" | "<br />" | "<div>" | "</li>" => "\n",
                _ => "",
            }
        });
        // Phase 2: 剥离所有剩余 HTML 标签
        static HTML_TAG_RE: OnceLock<Regex> = OnceLock::new();
        let re = HTML_TAG_RE.get_or_init(|| Regex::new(r"<[^>]+>").unwrap());
        let s = re.replace_all(&s, "");
        // Phase 3: 解码 HTML 实体
        let s = s
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&nbsp;", " ")
            .replace("&quot;", "\"");
        // Phase 4: 压缩空白行
        let s: Vec<&str> = s.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
        s.join("\n")
    }

    // ─── 路径工具 ───

    /// 构建带截断的路径前缀字符串。
    ///
    /// - 跳过空 text 节点
    /// - 最多保留最近 `PATH_MAX_LEVELS` 级
    /// - 总长度限制 `PATH_MAX_CHARS` 字符
    fn build_path_prefix(path: &[String]) -> String {
        let meaningful: Vec<&str> = path.iter().filter(|s| !s.is_empty()).map(|s| s.as_str()).collect();
        let joined = if meaningful.len() > PATH_MAX_LEVELS {
            meaningful[meaningful.len() - PATH_MAX_LEVELS..].join(" > ")
        } else {
            meaningful.join(" > ")
        };
        if joined.chars().count() > PATH_MAX_CHARS {
            let truncated: String = joined.chars().take(PATH_MAX_CHARS - 3).collect();
            format!("{}...", truncated)
        } else {
            joined
        }
    }

    // ─── 节点判定 ───

    /// 是否为短叶子节点：无子节点、text ≤ `SHORT_LEAF_MAX_CHARS` 字符、note 为空。
    fn is_short_leaf<N: TreeNode>(node: &N) -> bool {
        node.children().is_empty() && node.text().chars().count() <= SHORT_LEAF_MAX_CHARS && node.note().is_empty()
    }

    /// 是否为空容器节点：仅有 children、自身 text 和 note 均为空。
    fn is_empty_container<N: TreeNode>(node: &N) -> bool {
        node.text().is_empty() && node.note().is_empty() && !node.children().is_empty()
    }

    // ─── 内容构建 ───

    /// 构建节点正文。
    fn build_content<N: TreeNode>(node: &N) -> String {
        if !node.note().is_empty() {
            node.note().to_string()
        } else if !node.children().is_empty() {
            let summary: Vec<String> = node
                .children()
                .iter()
                .filter(|c| !c.text().is_empty())
                .map(|c| format!("- {}", c.text()))
                .collect();
            if summary.is_empty() {
                node.text().to_string()
            } else if node.text().is_empty() {
                summary.join("\n")
            } else {
                format!("{}\n{}", node.text(), summary.join("\n"))
            }
        } else {
            node.text().to_string()
        }
    }

    // ─── 元数据 ───

    /// 从路径数组生成 path_depth 和 path_json。
    fn path_to_metadata(path: &[String]) -> (Option<u32>, Option<String>) {
        let depth = path.iter().filter(|s| !s.is_empty()).count() as u32;
        let path_depth = if depth > 0 { Some(depth) } else { None };
        let meaningful: Vec<&str> = path.iter().filter(|s| !s.is_empty()).map(|s| s.as_str()).collect();
        let path_json = if meaningful.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&meaningful).unwrap_or_default())
        };
        (path_depth, path_json)
    }

    // ─── Chunk 生成 ───

    /// 创建一个带元数据的 ChunkResult 并加入结果。
    ///
    /// 内部自动拼接【上下文: 路径前缀】前缀、校验长度、超长时二次切分。
    fn push_chunk(
        body: &str,
        path: &[String],
        max_size: usize,
        overlap: usize,
        result: &mut Vec<ChunkResult>,
    ) {
        let prefix_str = Self::build_path_prefix(path);
        let prefix_line = if prefix_str.is_empty() {
            String::new()
        } else {
            format!("【上下文: {}】\n", prefix_str)
        };
        let combined = format!("{}{}", prefix_line, body);
        let (path_depth, path_json) = Self::path_to_metadata(path);
        let char_count = combined.chars().count();

        if char_count <= max_size.saturating_mul(6) / 5 {
            result.push(ChunkResult {
                text: combined,
                path_depth,
                path_json,
                sentence_window: None,
                symbol_name: None,
                symbol_kind: None,
                embedding_text: None,
                chunk_type: None,
                doc_title: None,
                tags: None,
            });
            return;
        }

        let prefix_char_count = prefix_line.chars().count();
        let sub_max = if max_size > prefix_char_count {
            max_size - prefix_char_count
        } else {
            max_size
        };
        for sub in utils::split_text(body, sub_max, overlap) {
            result.push(ChunkResult {
                text: format!("{}{}", prefix_line, sub),
                path_depth,
                path_json: path_json.clone(),
                sentence_window: None,
                symbol_name: None,
                symbol_kind: None,
                embedding_text: None,
                chunk_type: None,
                doc_title: None,
                tags: None,
            });
        }
    }

    /// 直接创建 ChunkResult，不校验长度（用于兄弟合并缓冲区刷新）。
    fn push_chunk_unchecked(
        body: &str,
        path: &[String],
        result: &mut Vec<ChunkResult>,
    ) {
        let prefix_str = Self::build_path_prefix(path);
        let combined = if prefix_str.is_empty() {
            body.to_string()
        } else {
            format!("【上下文: {}】\n{}", prefix_str, body)
        };
        let (path_depth, path_json) = Self::path_to_metadata(path);
        result.push(ChunkResult {
            text: combined,
            path_depth,
            path_json,
            sentence_window: None,
            symbol_name: None,
            symbol_kind: None,
            embedding_text: None,
            chunk_type: None,
            doc_title: None,
            tags: None,
        });
    }

    // ─── DFS 遍历 ───

    /// 处理单个节点及其子节点。
    fn process_node<N: TreeNode>(
        node: &N,
        path: &[String],
        max_size: usize,
        overlap: usize,
        result: &mut Vec<ChunkResult>,
    ) {
        if Self::is_empty_container(node) {
            Self::process_children(node.children(), path, max_size, overlap, result);
            return;
        }

        let mut current_path = path.to_vec();
        if !node.text().is_empty() {
            current_path.push(node.text().to_string());
        }

        let content = Self::build_content(node);
        if !content.is_empty() {
            Self::push_chunk(&content, &current_path, max_size, overlap, result);
        }

        Self::process_children(node.children(), &current_path, max_size, overlap, result);
    }

    /// 遍历子节点列表，带兄弟短叶子聚合和大小上限检查。
    fn process_children<N: TreeNode>(
        children: &[N],
        parent_path: &[String],
        max_size: usize,
        overlap: usize,
        result: &mut Vec<ChunkResult>,
    ) {
        let mut buf: Vec<String> = Vec::new();
        let mut buf_chars: usize = 0;

        for child in children {
            if Self::is_empty_container(child) {
                Self::flush_sibling_buf(&mut buf, &mut buf_chars, parent_path, result);
                Self::process_node(child, parent_path, max_size, overlap, result);
                continue;
            }

            if Self::is_short_leaf(child) {
                let text = child.text().to_string();
                let added = text.chars().count() + 2; // "- " overhead
                // 如果加入后超出上限，先 flush 再继续
                if max_size > 0 && buf_chars > 0 && buf_chars + added > max_size.saturating_mul(6) / 5 {
                    Self::flush_sibling_buf(&mut buf, &mut buf_chars, parent_path, result);
                }
                buf.push(text);
                buf_chars += added;
                continue;
            }

            Self::flush_sibling_buf(&mut buf, &mut buf_chars, parent_path, result);
            Self::process_node(child, parent_path, max_size, overlap, result);
        }

        Self::flush_sibling_buf(&mut buf, &mut buf_chars, parent_path, result);
    }

    /// 刷新兄弟合并缓冲区。
    fn flush_sibling_buf(
        buf: &mut Vec<String>,
        buf_chars: &mut usize,
        parent_path: &[String],
        result: &mut Vec<ChunkResult>,
    ) {
        if buf.is_empty() {
            return;
        }
        let merged = format!("- {}", buf.join("\n- "));
        Self::push_chunk_unchecked(&merged, parent_path, result);
        buf.clear();
        *buf_chars = 0;
    }
}

// ─── OPML 文档分割器 ───

/// OPML 文档分割器
///
/// OPML（Outline Processor Markup Language）是一种用于表示大纲结构的 XML 格式，
/// 常见于 RSS 订阅列表、播客订阅等场景。
///
// ─── OPML 文档分割器（树形层级感知） ───

/// 树形层级感知的 OPML 分块器。
///
/// 使用 roxmltree 将 OPML 解析为 OutlineNode 树，然后通过 TreeProcessor 进行 DFS 递归遍历：
/// - 构建祖先上下文路径前缀 `【上下文: A > B > C】`
/// - 兄弟短节点聚合（连续短文本叶子合并）
/// - 空容器节点跳过（仅起层级组织作用的父节点）
/// - 路径前缀截断（最多 3 级 / 50 字符）
/// - 父节点和子节点各自生成独立 chunk
///
/// 无法解析时回退到纯文本字符级分割。
pub struct OpmlChunkSplitter;

/// OPML 大纲节点
struct OutlineNode {
    text: String,
    note: String,
    children: Vec<OutlineNode>,
}

impl_tree_node!(OutlineNode);

impl OpmlChunkSplitter {
    /// 使用 roxmltree 解析 OPML XML 为 OutlineNode 树
    fn parse_opml(xml: &str) -> Vec<OutlineNode> {
        let doc = match roxmltree::Document::parse(xml) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };

        if let Some(body) = doc.root().descendants().find(|n| n.has_tag_name("body")) {
            body.children()
                .filter(|n| n.has_tag_name("outline"))
                .map(Self::parse_outline_node)
                .collect()
        } else {
            Vec::new()
        }
    }

    fn parse_outline_node(elem: roxmltree::Node) -> OutlineNode {
        let text = elem
            .attribute("text")
            .or_else(|| elem.attribute("TEXT"))
            .or_else(|| elem.attribute("title"))
            .unwrap_or("")
            .trim()
            .to_string();

        let note = elem
            .attribute("_note")
            .or_else(|| elem.attribute("note"))
            .or_else(|| elem.attribute("NOTE"))
            .map(TreeProcessor::clean_html)
            .unwrap_or_default();

        let children: Vec<OutlineNode> = elem
            .children()
            .filter(|c| c.has_tag_name("outline"))
            .map(Self::parse_outline_node)
            .collect();

        OutlineNode { text, note, children }
    }
}

impl_tree_chunk_splitter!(OpmlChunkSplitter, OpmlChunkSplitter::parse_opml);

// ─── FreeMind 文档分割器 ───

/// FreeMind 文档分割器
///
/// FreeMind（.mm）是一种用于表示思维导图的 XML 格式。
/// 节点使用 `<node TEXT="...">` 标签，支持 `<richcontent TYPE="NOTE">` 富文本注释。
///
/// 解析策略与 OPML 一致：解析为树形结构后复用 TreeProcessor 进行 DFS 遍历和 chunk 生成。
/// 无法解析时回退到纯文本字符级分割。
pub struct FreeMindChunkSplitter;

/// FreeMind 大纲节点
struct FreeMindNode {
    text: String,
    note: String,
    children: Vec<FreeMindNode>,
}

impl_tree_node!(FreeMindNode);

impl FreeMindChunkSplitter {
    /// 使用 roxmltree 解析 FreeMind XML 为 FreeMindNode 树
    fn parse_freemind(xml: &str) -> Vec<FreeMindNode> {
        let doc = match roxmltree::Document::parse(xml) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };

        if let Some(map) = doc.root().descendants().find(|n| n.has_tag_name("map")) {
            map.children()
                .filter(|n| n.has_tag_name("node"))
                .map(Self::parse_node)
                .collect()
        } else {
            Vec::new()
        }
    }

    fn parse_node(elem: roxmltree::Node) -> FreeMindNode {
        let text = elem
            .attribute("TEXT")
            .or_else(|| elem.attribute("text"))
            .unwrap_or("")
            .trim()
            .to_string();

        // 提取注释：优先取 NOTE 属性（纯文本），回退到 richcontent HTML 并清洗
        let note = elem
            .attribute("NOTE")
            .or_else(|| elem.attribute("note"))
            .map(|n| n.to_string())
            .or_else(|| Self::extract_richcontent_note(elem).map(|n| TreeProcessor::clean_html(&n)))
            .unwrap_or_default();

        let children: Vec<FreeMindNode> = elem
            .children()
            .filter(|c| c.has_tag_name("node"))
            .map(Self::parse_node)
            .collect();

        FreeMindNode { text, note, children }
    }

    /// 从 FreeMind 节点的 `<richcontent TYPE="NOTE">` 中提取 HTML 正文。
    ///
    /// 使用 `descendants()` 遍历所有嵌套层级，确保深层 HTML 内容不被遗漏。
    /// 块级元素插入换行标记，由调用方 `clean_html` 统一清洗。
    fn extract_richcontent_note(elem: roxmltree::Node) -> Option<String> {
        let rich = elem.children().find(|c| {
            c.has_tag_name("richcontent") && c.attribute("TYPE") == Some("NOTE")
        })?;
        let body = rich.descendants().find(|n| n.has_tag_name("body"))?;
        let parts: Vec<String> = body
            .descendants()
            .filter_map(|n| {
                if n.is_text() {
                    n.text().map(|t| t.to_string())
                } else if n.is_element() {
                    match n.tag_name().name() {
                        "p" | "div" | "br" | "tr" => Some("\n".to_string()),
                        "li" => Some("\n- ".to_string()),
                        "td" | "th" => Some("\t".to_string()),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .collect();
        let result = parts.join("").trim().to_string();
        if result.is_empty() { None } else { Some(result) }
    }
}

impl_tree_chunk_splitter!(FreeMindChunkSplitter, FreeMindChunkSplitter::parse_freemind);

// ─── ChunkSplitterFactory（工厂模式） ───

/// 文件扩展名到分割器的映射注册表
type ExtensionMap = HashMap<&'static str, Box<dyn ChunkSplitter + Send + Sync>>;

/// ChunkSplitter 工厂
///
/// 根据文件扩展名返回对应的 ChunkSplitter 实现。
/// 支持运行时注册新的扩展名-分割器对，遵循开闭原则。 
pub struct ChunkSplitterFactory {
    /// 精确扩展名匹配
    exact: ExtensionMap,
    /// 后缀匹配（如 .md 匹配 .mdx）
    suffix: Vec<(&'static str, Box<dyn ChunkSplitter + Send + Sync>)>,
}

impl ChunkSplitterFactory {
    /// 创建默认工厂，注册所有内置分割器
    pub fn new() -> Self {
        let mut factory = Self {
            exact: HashMap::new(),
            suffix: Vec::new(),
        };

        // 注册内置分割器（复用配置实例避免重复堆分配）
        let md_splitter = MarkdownChunkSplitter::new();
        let opml = Box::new(OpmlChunkSplitter);
        let freemind = Box::new(FreeMindChunkSplitter);

        // Markdown 类型
        factory.exact.insert("md", Box::new(md_splitter.clone()));
        factory.exact.insert("mdx", Box::new(md_splitter.clone()));
        factory.suffix.push(("md", Box::new(md_splitter)));

        // HTML 类型（scraper → 语义 AST → 语义分块，P2）
        let html_splitter = Box::new(HtmlChunkSplitter::new());
        factory.exact.insert("html", html_splitter.clone());
        factory.exact.insert("htm", html_splitter);

        // OPML 类型
        factory.exact.insert("opml", opml);

        // FreeMind 类型
        factory.exact.insert("mm", freemind);

        // 代码文件类型（有语言特定分隔符的扩展名使用 CodeAwareChunkSplitter）
        // 其余非 Markdown/OPML/FreeMind/HTML 类型使用 PlainTextChunkSplitter
        for ext in utils::KB_SUPPORTED_EXTS {
            if ext == &"md" || ext == &"mdx" || ext == &"opml" || ext == &"mm"
                || ext == &"html" || ext == &"htm"
            {
                continue;
            }
            if CODE_LANG_SEPARATORS.contains_key(ext) {
                factory.exact.insert(ext, Box::new(CodeAwareChunkSplitter::for_extension(ext)));
            } else {
                factory.exact.insert(ext, Box::new(PlainTextChunkSplitter));
            }
        }

        factory
    }

    /// 根据文件扩展名获取对应的分割器
    ///
    /// 匹配规则：
    /// 1. 精确匹配 `exact` 表
    /// 2. 后缀匹配 `suffix` 表（如 "md" 匹配 "mdx"）
    /// 3. 都不匹配则返回纯文本分割器
    pub fn get_splitter(&self, extension: &str) -> &dyn ChunkSplitter {
        let ext_lower = extension.to_lowercase();

        // 精确匹配
        if let Some(splitter) = self.exact.get(ext_lower.as_str()) {
            return splitter.as_ref();
        }

        // 后缀匹配（如 "md" 前缀匹配 "mdx"；避免误匹配 .cmd、.3md 等）
        for (suffix, splitter) in &self.suffix {
            if ext_lower == *suffix || ext_lower.starts_with(suffix) {
                return splitter.as_ref();
            }
        }

        // 默认：纯文本分割器
        &PLAIN_TEXT_SPLITTER
    }

}

impl Default for ChunkSplitterFactory {
    fn default() -> Self {
        Self::new()
    }
}

// 确保分割器可作静态变量
static PLAIN_TEXT_SPLITTER: PlainTextChunkSplitter = PlainTextChunkSplitter;

// ─── P0-3 测试：工厂路由（I-7 类型策略） ───

#[cfg(test)]
mod factory_tests {
    use super::*;

    #[test]
    fn factory_routes_by_extension() {
        let f = ChunkSplitterFactory::new();
        let md = "# 标题\n\n正文内容段落。\n\n## 子标题\n\n子节内容。\n";

        // Markdown → AST 语义分块（带标题路径 path_json）
        let md_chunks = f.get_splitter("md").split(md, 448, 56);
        assert!(!md_chunks.is_empty());
        assert!(
            md_chunks.iter().any(|c| c.path_json.is_some()),
            "Markdown 应走 AST 语义分块（带 heading 路径）"
        );

        // 纯文本 / 未知扩展名 → 无路径
        for ext in ["txt", "xyz", "log"] {
            let chunks = f.get_splitter(ext).split(md, 448, 56);
            assert!(
                chunks.iter().all(|c| c.path_json.is_none()),
                "{} 应走纯文本分块（无路径）",
                ext
            );
        }

        // 代码 → 符号感知（Rust fn 提取 symbol_name）
        let code = "fn main() {\n    let x = 1;\n}\n\nfn helper() {\n    let y = 2;\n}\n";
        let code_chunks = f.get_splitter("rs").split(code, 448, 56);
        assert!(!code_chunks.is_empty());
        assert!(
            code_chunks.iter().any(|c| c.symbol_name.is_some()),
            "Rust 代码应提取符号名"
        );

        // HTML → AST 语义分块
        let html = "<html><body><h1>标题</h1><p>正文段落</p></body></html>";
        let html_chunks = f.get_splitter("html").split(html, 448, 56);
        assert!(!html_chunks.is_empty(), "HTML 应产出 chunk");

        // OPML 树形 → path_depth 有值
        let opml = r#"<?xml version="1.0"?><opml version="2.0"><body>
            <outline text="项目"><outline text="第一阶段"/></outline>
        </body></opml>"#;
        let opml_chunks = f.get_splitter("opml").split(opml, 448, 56);
        assert!(
            opml_chunks.iter().any(|c| c.path_depth.is_some()),
            "OPML 应走树形分块（带路径深度）"
        );
    }

    /// 未知扩展名不 panic，且兜底纯文本
    #[test]
    fn factory_unknown_ext_falls_back() {
        let f = ChunkSplitterFactory::new();
        let chunks = f.get_splitter("totally-unknown-ext").split("一段普通文本。", 448, 56);
        assert!(!chunks.is_empty());
    }

    /// 🟠 P0-2 回归：类型标注的常量声明（`pub const SCHEMA_VERSION: &str = "5"`）
    /// 必须提取出符号名（旧正则 `const\s+(\w+)\s*=` 被 `: &str` 隔断而漏提）。
    #[test]
    fn typed_const_symbol_extraction() {
        // 类型标注形式（Rust 常量/静态）
        let (name, kind) = extract_symbol_info(
            "pub const SCHEMA_VERSION: &str = \"5\";",
            &CODE_SYMBOL_RE,
        );
        assert_eq!(name.as_deref(), Some("SCHEMA_VERSION"), "类型标注 const 应提取符号名");
        assert!(kind.is_some(), "应识别符号类型");

        // 无可见性前缀的类型标注 const
        let (name2, _) = extract_symbol_info(
            "const DEFAULT_LIMIT: usize = 100;",
            &CODE_SYMBOL_RE,
        );
        assert_eq!(name2.as_deref(), Some("DEFAULT_LIMIT"), "无前缀类型标注 const 应提取");

        // 带可见性前缀的静态变量
        let (name3, _) = extract_symbol_info(
            "static MAX_CHUNKS_PER_DOC: usize = 3;",
            &CODE_SYMBOL_RE,
        );
        assert_eq!(name3.as_deref(), Some("MAX_CHUNKS_PER_DOC"));
    }
}
