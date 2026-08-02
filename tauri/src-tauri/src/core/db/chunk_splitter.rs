use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

use super::utils;

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
pub struct PlainTextChunkSplitter;

impl ChunkSplitter for PlainTextChunkSplitter {
    fn split(&self, text: &str, max_size: usize, overlap: usize) -> Vec<ChunkResult> {
        utils::split_text(text, max_size, overlap)
            .into_iter()
            .map(ChunkResult::plain)
            .collect()
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

/// 所有代码语言的符号提取通用正则（匹配常见函数/类定义语法）
static CODE_SYMBOL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?m)^[\t ]*(?:(?:pub\s+)?(?:async\s+)?(?:fn|def|function|func|async function|class|struct|enum|trait|interface|type|object|impl)\s+)(\w+)")
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
    if text.len() <= max_size || separators.is_empty() {
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
            if part.len() <= max_size && rest.is_empty() {
                parts.push(part.to_string());
            } else if part.len() <= max_size {
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
        } else if buffer.len() < min_size {
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
    if overlap > 0 && result.len() > 1 {
        let overlap_size = overlap.min(max_size / 4);
        if overlap_size > 10 {
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
            let prev_too_small = prev.text.len() < min_size;

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
                };
                continue;
            }
            if !curr_has_symbol && !prev_has_symbol && prev.text.len() < min_size {
                // 两个都无符号且前一个太小：合并
                let merged_text = format!("{}\n{}", prev.text, chunk.text);
                *prev = ChunkResult {
                    text: merged_text,
                    path_depth: prev.path_depth.or(chunk.path_depth),
                    path_json: prev.path_json.clone().or(chunk.path_json),
                    sentence_window: prev.sentence_window.clone().or(chunk.sentence_window),
                    symbol_name: prev.symbol_name.clone(),
                    symbol_kind: prev.symbol_kind.clone(),
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
        let name = caps.get(1).map(|m| m.as_str().to_string());
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
    let re = Regex::new(r"(?m)^[\t ]*(?:(?:pub\s+)?(?:async\s+)?(?:fn|def|function|func|async function|class|struct|enum|trait|interface|type|object|impl)\s+)(\w+)")
        .expect("extract_all_symbols regex 编译失败");
    let mut seen = std::collections::HashSet::new();
    let mut symbols = Vec::new();
    for cap in re.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let name = m.as_str();
            if seen.insert(name) {
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
    if text.len() < 80 {
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
    if text.len() < 5 {
        return false;
    }
    let comment_chars = text
        .chars()
        .filter(|&c| c == '#' || c == '/' || c == '*' || c == ' ' || c == '\n' || c == '\t' || c == '-' || c == '"' || c == '\'')
        .count();
    comment_chars as f64 / text.len() as f64 > 0.5
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
                && prev.text.len() < max_size / 2
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
    /// 是否携带完整父级标题链路作为上下文前缀
    pub full_parent_context: bool,
    /// 开启 Setext 标题识别（=== / --- 二级标题）
    pub enable_setext_heading: bool,
    /// 单章节宽松上限系数，字符数超过则二次拆分
    pub oversize_factor: f32,
    /// 拆分时正文最小预留字符数，防止前缀占满空间
    pub min_body_reserve_chars: usize,
}

impl Default for MarkdownSplitConfig {
    fn default() -> Self {
        Self {
            full_parent_context: true,
            enable_setext_heading: true,
            oversize_factor: 1.25,
            min_body_reserve_chars: 50,
        }
    }
}

/// 章节标题栈节点，缓存完整标题前缀避免重复拼接
#[derive(Debug)]
struct HeadingNode {
    level: usize,
    cached_prefix: String,
}

/// 行解析状态机：区分普通文本 / 代码块
#[derive(Debug, Default)]
enum ParseState {
    #[default]
    Normal,
    /// 代码块，存储起始反引号数量（3/4等）
    CodeBlock(usize),
}

// ─── Markdown 文档分割器（增强版） ───

/// Markdown 文档分割器
///
/// 按标题层级（# ~ ######）划分段落，每个 chunk 注入父级标题路径作为前缀。
/// 超过 max_size 的长段落回退到 split_text 切分。
///
/// 增强特性：
/// - 代码块状态机屏蔽代码块内 # 标题误识别
/// - 全文本长度统一使用 chars().count() 字符计数
/// - 安全无溢出可用正文长度计算
/// - 标题文本强制 trim()
/// - Setext 二级标题支持
/// - 列表/引用行屏蔽标题匹配
/// - Windows \r\n 换行兼容
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

    #[allow(dead_code)]
    pub fn with_config(config: MarkdownSplitConfig) -> Self {
        Self { config }
    }

    // 1. ATX 标题正则：兼容 #标题 / # 标题，自动剔除首尾空白
    fn atx_heading_re() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new(r"^(#{1,6})\s*(.+?)\s*$").unwrap())
    }

    // 2. 代码块起始/结束正则：匹配 ``` / ````
    fn code_block_re() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new(r"^(`{3,})").unwrap())
    }

    // 3. Setext 二级标题分隔线：=== / ---
    fn setext_line_re() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new(r"^(=+|-+)\s*$").unwrap())
    }

    // 4. 列表/引用前缀：行首 > / - / * / 数字. ，这类行不识别 ATX 标题
    fn list_quote_prefix_re() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new(r"^(\s*>|\s*[-*]\s|\s*\d+\.)").unwrap())
    }

    /// 构建当前标题栈完整上下文前缀
    fn build_stack_prefix(stack: &[HeadingNode], full_context: bool) -> String {
        let mut buf = String::new();
        if !full_context && !stack.is_empty() {
            // 仅保留最近一级标题，减少 token 占用
            let last = stack.last().unwrap();
            buf.push_str(&last.cached_prefix);
            return buf;
        }
        for node in stack {
            buf.push_str(&node.cached_prefix);
        }
        buf
    }

    /// 内部封装章节入块逻辑，统一空过滤、字符长度校验、安全拆分
    fn push_section(
        config: &MarkdownSplitConfig,
        result: &mut Vec<ChunkResult>,
        stack: &[HeadingNode],
        lines: &[&str],
        start: usize,
        end: usize,
        max_chars: usize,
        overlap: usize,
    ) {
        if end <= start {
            return;
        }
        // 拼接章节正文
        let body_raw = lines[start..end].join("\n");
        let body_trim = body_raw.trim();
        if body_trim.is_empty() {
            return; // 过滤纯空白章节
        }

        // 拼接标题上下文前缀
        let prefix = Self::build_stack_prefix(stack, config.full_parent_context);
        let combined = format!("{}{}", prefix, body_raw);
        let combined_char_count = combined.chars().count();
        let max_single_chars = (max_chars as f32 * config.oversize_factor) as usize;

        if combined_char_count <= max_single_chars {
            result.push(ChunkResult::plain(combined));
            return;
        }

        // 超长章节：二次拆分，安全计算可用正文字符数，杜绝下溢
        let prefix_char_count = prefix.chars().count();
        let min_reserve = config.min_body_reserve_chars;
        let available_body_chars = max_chars.saturating_sub(prefix_char_count).max(min_reserve);

        let body_chunks = utils::split_text_char_based(&body_raw, available_body_chars, overlap);
        for chunk in body_chunks {
            result.push(ChunkResult::plain(format!("{}{}", prefix, chunk)));
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
        let mut result = Vec::new();
        result.reserve(text.chars().count() / max_chars.max(1) + 10); // 预分配容量

        // 预处理：统一换行符，消除 Windows \r\n
        let uniform_text = text.replace("\r\n", "\n");
        let lines: Vec<&str> = uniform_text.lines().collect();
        let atx_re = Self::atx_heading_re();
        let code_re = Self::code_block_re();
        let setext_re = Self::setext_line_re();
        let list_quote_re = Self::list_quote_prefix_re();

        // 全局状态
        let mut parse_state = ParseState::default();
        let mut heading_stack: Vec<HeadingNode> = Vec::new();
        let mut section_start = 0usize;
        let mut setext_candidate: Option<(usize, String)> = None;

        // 无 # 且关闭 Setext 时，降级为分隔符感知的段落分割（保留段落结构）
        let has_hash = uniform_text.contains('#');
        if !has_hash && !config.enable_setext_heading {
            return utils::split_text_with_separators(
                &uniform_text, max_chars, overlap, &["\n\n", "\n", ". ", "。"]
            ).into_iter().map(ChunkResult::plain).collect();
        }

        for (line_idx, line) in lines.iter().enumerate() {
            // 1. 处理代码块状态切换
            if let Some(caps) = code_re.captures(line) {
                let backtick_count = caps.get(1).unwrap().as_str().chars().count();
                match &parse_state {
                    ParseState::Normal => parse_state = ParseState::CodeBlock(backtick_count),
                    ParseState::CodeBlock(open_count) if open_count == &backtick_count => {
                        parse_state = ParseState::Normal;
                    }
                    ParseState::CodeBlock(_) => {}
                }
                setext_candidate = None;
                continue;
            }

            // 代码块内完全跳过标题解析
            if matches!(parse_state, ParseState::CodeBlock(_)) {
                setext_candidate = None;
                continue;
            }

            // 2. Setext 标题逻辑（=== / ---）
            if config.enable_setext_heading {
                if setext_re.is_match(line) {
                    if let Some((prev_idx, title)) = setext_candidate.take() {
                        // --- 仅在候选标题较短（≤100 字符）时视为 H2，
                        // 避免长段落前的 --- 分割线被误识别为 Setext 标题
                        let is_h1 = line.starts_with('=');
                        if !is_h1 && title.chars().count() > 100 {
                            // --- 且候选文本过长 → 视为 HR（分割线），跳过
                            setext_candidate = None;
                            continue;
                        }
                        // 先保存上一段落
                        if !heading_stack.is_empty() {
                            Self::push_section(
                                config,
                                &mut result,
                                &heading_stack,
                                &lines,
                                section_start,
                                prev_idx,
                                max_chars,
                                overlap,
                            );
                        }
                        // 压入标题栈（=== 为 H1，--- 为 H2）
                        let setext_level = if is_h1 { 1 } else { 2 };
                        while let Some(top) = heading_stack.last() {
                            if top.level >= setext_level {
                                heading_stack.pop();
                            } else {
                                break;
                            }
                        }
                        let tag = "#".repeat(setext_level);
                        let prefix = format!("{} {}\n", tag, title);
                        heading_stack.push(HeadingNode {
                            level: setext_level,
                            cached_prefix: prefix,
                        });
                        section_start = line_idx + 1;
                    }
                    continue;
                }
                // 记录可能作为 Setext 标题的上一行文本
                // 排除列表/引用行中的 ---/===（应被视为 <hr> 而非标题）
                if !line.trim_start().is_empty() && !list_quote_re.is_match(line) {
                    setext_candidate = Some((line_idx, line.trim_start().to_string()));
                } else {
                    setext_candidate = None;
                }
            }

            // 3. 跳过列表/引用行，不识别 ATX 标题
            if list_quote_re.is_match(line) {
                continue;
            }

            // 4. ATX # 标题匹配
            if let Some(caps) = atx_re.captures(line) {
                // 先输出当前未闭合章节
                if !heading_stack.is_empty() {
                    Self::push_section(
                        config,
                        &mut result,
                        &heading_stack,
                        &lines,
                        section_start,
                        line_idx,
                        max_chars,
                        overlap,
                    );
                }

                // 解析标题层级+文本，强制 trim 清除空白
                let level = caps.get(1).unwrap().as_str().chars().count();
                let raw_text = caps.get(2).unwrap().as_str();
                let heading_text = raw_text.trim().to_string();
                if heading_text.is_empty() {
                    section_start = line_idx + 1;
                    continue;
                }

                // 栈弹出：清除同级、更高级标题
                while let Some(top) = heading_stack.last() {
                    if top.level >= level {
                        heading_stack.pop();
                    } else {
                        break;
                    }
                }

                // 缓存标题前缀，避免重复拼接
                let tag = "#".repeat(level);
                let cached_prefix = format!("{} {}\n", tag, heading_text);
                heading_stack.push(HeadingNode {
                    level,
                    cached_prefix,
                });
                section_start = line_idx + 1;
                setext_candidate = None;
            }
        }

        // 处理文档末尾剩余章节
        if !heading_stack.is_empty() {
            Self::push_section(
                config,
                &mut result,
                &heading_stack,
                &lines,
                section_start,
                lines.len(),
                max_chars,
                overlap,
            );
        }

        // 兜底：未识别任何标题，降级纯文本分割
        if result.is_empty() {
            return utils::split_text_char_based(&uniform_text, max_chars, overlap)
                .into_iter().map(ChunkResult::plain).collect();
        }

        result
    }
}

// ─── 树形处理器常量 ───

/// 短叶子节点最大字符数
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

        if char_count <= max_size * 6 / 5 {
            result.push(ChunkResult { text: combined, path_depth, path_json, sentence_window: None, symbol_name: None, symbol_kind: None });
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
        result.push(ChunkResult { text: combined, path_depth, path_json, sentence_window: None, symbol_name: None, symbol_kind: None });
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
                if max_size > 0 && buf_chars > 0 && buf_chars + added > max_size * 6 / 5 {
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

// ─── SemanticChunkSplitter（语义分块） ───

/// 语义分块器：通过 sentence-level embedding 相似度找到语义边界进行切分。
///
/// # 算法（Greg Kamradt 方法）
/// 1. 将文本分割为句子
/// 2. 用滑动窗口（默认 5 句）分组句子，对每组生成 embedding
/// 3. 计算相邻组的余弦相似度
/// 4. 在相似度低于阈值百分位数的位置切分
/// 5. 合并过小的 chunk（< 30% max_size）
///
/// # 退化策略
/// 若 embedding 调用失败（如模型未就绪），自动回退到字符级分块。
#[allow(dead_code)]
pub struct SemanticChunkSplitter {
    /// 滑动窗口大小（句子数），默认 5
    pub window_size: usize,
    /// 相似度阈值百分位数（0.0~1.0），默认 0.9
    pub threshold_percentile: f64,
}

impl Default for SemanticChunkSplitter {
    fn default() -> Self {
        Self {
            window_size: 5,
            threshold_percentile: 0.9,
        }
    }
}

impl ChunkSplitter for SemanticChunkSplitter {
    fn split(&self, text: &str, max_size: usize, overlap: usize) -> Vec<ChunkResult> {
        // 1. Split into sentences
        let sentences = utils::split_sentences(text);
        if sentences.len() <= 1 {
            return vec![ChunkResult::plain(text.to_string())];
        }

        // 2. Group sentences into overlapping windows of window_size
        let groups: Vec<String> = if sentences.len() <= self.window_size {
            vec![sentences.join("")]
        } else {
            sentences.windows(self.window_size)
                .map(|w| w.join(""))
                .collect()
        };

        // 3. Compute embedding for each group
        let refs: Vec<&str> = groups.iter().map(|s| s.as_str()).collect();
        let embeddings = match utils::call_embedding(&refs, None) {
            Ok(e) => e,
            Err(_) => {
                // Fallback to character-based splitting
                log::warn!("[semantic_chunk] embedding call failed, falling back to char-based split");
                return utils::split_text_char_based(text, max_size, overlap)
                    .into_iter().map(ChunkResult::plain).collect();
            }
        };

        // 4. Calculate cosine similarities between adjacent groups
        let similarities: Vec<f64> = embeddings.windows(2)
            .map(|w| utils::cosine_similarity(&w[0], &w[1]))
            .collect();

        // 5. Find threshold — break where similarity drops below percentile
        let threshold = if similarities.is_empty() {
            0.0
        } else {
            let mut sorted = similarities.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let idx = ((sorted.len() as f64) * self.threshold_percentile).floor() as usize;
            let idx = idx.min(sorted.len().saturating_sub(1));
            sorted[idx]
        };

        // 6. Build chunks at semantic breakpoints
        let mut chunk_sentences: Vec<Vec<String>> = Vec::new();
        let mut current: Vec<String> = Vec::new();
        current.push(sentences[0].clone());

        for i in 0..similarities.len() {
            let sentence_idx = i + 1;
            if sentence_idx < sentences.len() {
                if similarities[i] < threshold {
                    if !current.is_empty() {
                        chunk_sentences.push(std::mem::take(&mut current));
                    }
                }
                current.push(sentences[sentence_idx].clone());
            }
        }
        if !current.is_empty() {
            chunk_sentences.push(current);
        }

        // 7. Convert to ChunkResult
        let results: Vec<ChunkResult> = chunk_sentences.into_iter()
            .map(|group| ChunkResult::plain(group.join("")))
            .collect();

        // 8. Merge chunks that are too small (< 30% of max_size)
        let min_size = (max_size as f64 * 0.3).max(1.0) as usize;
        if results.len() > 1 {
            let mut merged: Vec<ChunkResult> = Vec::new();
            for chunk in results.into_iter() {
                if let Some(last) = merged.last_mut() {
                    if last.text.chars().count() < min_size {
                        last.text.push('\n');
                        last.text.push_str(&chunk.text);
                        continue;
                    }
                }
                merged.push(chunk);
            }
            merged
        } else {
            results
        }
    }
}

// ─── SentenceWindowChunkSplitter（句子窗口分块） ───

/// 句子窗口分块器：句子级细粒度分块 + 上下文窗口元数据。
///
/// # 原理（LlamaIndex SentenceWindowNodeParser 思路）
/// - 每个句子作为独立 chunk（被 embedding 和检索）
/// - chunk 的 `sentence_window` 字段存储该句子周边的扩展上下文
/// - 检索到该 chunk 后，后处理阶段可用窗口文本代替原句传给 LLM
///
/// # 使用场景
/// 对召回精度要求高（精确匹配句子）、同时对上下文完整性有要求的场景。
/// 相比传统固定大小分块，句子窗口在检索阶段更精确，在生成阶段更完整。
#[allow(dead_code)]
pub struct SentenceWindowChunkSplitter {
    /// 上下文窗口大小（前后句子数），默认 2（即前后各 2 句，共 5 句窗口）
    pub context_window: usize,
}

impl Default for SentenceWindowChunkSplitter {
    fn default() -> Self {
        Self { context_window: 2 }
    }
}

impl ChunkSplitter for SentenceWindowChunkSplitter {
    fn split(&self, text: &str, _max_size: usize, _overlap: usize) -> Vec<ChunkResult> {
        let sentences = utils::split_sentences(text);
        if sentences.is_empty() || sentences.len() <= 1 {
            return vec![ChunkResult::plain(text.to_string())];
        }

        let n = sentences.len();
        let window = self.context_window;
        let mut results = Vec::with_capacity(n);

        for i in 0..n {
            let sentence_text = sentences[i].clone();

            // Build context window: window sentences before + after
            let window_start = i.saturating_sub(window);
            let window_end = (i + window + 1).min(n);
            let window_text = sentences[window_start..window_end].join("");

            results.push(ChunkResult {
                text: sentence_text,
                path_depth: None,
                path_json: None,
                sentence_window: Some(window_text),
                symbol_name: None,
                symbol_kind: None,
            });
        }

        results
    }
}

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

        // OPML 类型
        factory.exact.insert("opml", opml);

        // FreeMind 类型
        factory.exact.insert("mm", freemind);

        // 代码文件类型（有语言特定分隔符的扩展名使用 CodeAwareChunkSplitter）
        // 其余非 Markdown/OPML/FreeMind 类型使用 PlainTextChunkSplitter
        for ext in utils::KB_SUPPORTED_EXTS {
            if ext == &"md" || ext == &"mdx" || ext == &"opml" || ext == &"mm" {
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
