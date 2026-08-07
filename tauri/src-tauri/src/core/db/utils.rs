use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use regex::Regex;
use serde::Serialize;

use super::lance::DocumentChunk;
use super::chunk_splitter::ChunkResult;

// ─── 常量 ───

pub const KB_SUPPORTED_EXTS: &[&str] = &[
    "md", "txt", "pdf", "docx", "js", "ts", "jsx", "tsx", "py", "java", "go", "rs", "rb", "php",
    "c", "cpp", "h", "hpp", "cs", "swift", "kt", "scala", "r", "lua", "sh", "bash", "zsh", "ps1",
    "sql", "css", "scss", "less", "html", "htm", "xml", "json", "yaml", "yml", "toml", "ini",
    "cfg", "conf", "env", "gitignore", "dockerfile", "makefile", "opml", "mm",
];

/// 垃圾箱目录名（与前端 DELETED_DIR_NAME 一致）：该目录下的文件不参与索引与监听
pub const TRASH_DIR_NAME: &str = "mdgo_trash";

// ─── Gitignore 风格模式匹配（对齐 JS compileIgnorePatterns / testIgnore）───

/// 编译后的单条规则
struct IgnoreRule {
    regex: Regex,
    negate: bool,
    dir_only: bool,
}

/// Gitignore 风格模式匹配器
/// 对齐 JS: compileIgnorePatterns() + testIgnore()
pub struct IgnoreMatcher {
    rules: Vec<IgnoreRule>,
}

impl IgnoreMatcher {
    /// 从黑名单模式列表构建匹配器（对齐 JS DIR_BLACKLIST + FILE_BLACKLIST）
    ///
    /// - `dir_patterns`: 目录黑名单，模式以 `/` 结尾会被标记为仅匹配目录
    /// - `file_patterns`: 文件黑名单
    ///
    /// 模式语法（对齐 JS compileIgnorePatterns）：
    /// - `!` 前缀 = 取反（不跳过）
    /// - `/` 后缀 = 仅匹配目录
    /// - 含 `/` 的模式 = 匹配完整相对路径
    /// - 不含 `/` 的模式 = 仅匹配文件名
    /// - `**/` = 任意层目录
    /// - `*` = 匹配非 `/` 任意字符
    /// - `?` = 匹配单个非 `/` 字符
    pub fn new(dir_patterns: &[String], file_patterns: &[String]) -> Self {
        let mut rules = Vec::new();
        // 先处理目录黑名单（对齐 JS: compileIgnorePatterns(DIR_BLACKLIST)）
        for raw in dir_patterns {
            let raw = raw.trim();
            if raw.is_empty() || raw.starts_with('#') {
                continue;
            }
            let mut pattern = raw;
            let mut negate = false;

            // 处理 ! 取反
            if pattern.starts_with('!') {
                negate = true;
                pattern = &pattern[1..];
            }

            // 目录模式始终 dir_only = true（对齐 JS: DIR_RULES 的 dirOnly 来自 / 后缀）
            let dir_only = true;
            // 移除尾部 / 用于正则构建
            if pattern.ends_with('/') {
                pattern = &pattern[..pattern.len() - 1];
            }

            let anchored = pattern.starts_with('/');
            if anchored {
                pattern = &pattern[1..];
            }
            let has_slash = pattern.contains('/');

            // 将 gitignore 模式转换为正则
            if let Some(regex_str) = Self::compile_pattern(pattern, anchored, has_slash) {
                match Regex::new(&regex_str) {
                    Ok(re) => rules.push(IgnoreRule { regex: re, negate, dir_only }),
                    Err(e) => log::warn!("[IgnoreMatcher] 正则编译失败 ({}): {}", pattern, e),
                }
            }

            // 目录模式还需添加匹配其下所有文件的规则（对齐 JS: compileIgnorePatterns 中 dirOnly 的扩展规则）
            let file_pattern_str = if anchored {
                format!("{}/**", pattern)
            } else {
                format!("**/{}/**", pattern)
            };
            if let Some(regex_str) = Self::compile_pattern(&file_pattern_str, true, true) {
                match Regex::new(&regex_str) {
                    Ok(re) => rules.push(IgnoreRule { regex: re, negate, dir_only: false }),
                    Err(e) => log::warn!("[IgnoreMatcher] 正则编译失败 ({}): {}", file_pattern_str, e),
                }
            }
        }

        // 再处理文件黑名单（对齐 JS: compileIgnorePatterns(FILE_BLACKLIST)）
        for raw in file_patterns {
            let raw = raw.trim();
            if raw.is_empty() || raw.starts_with('#') {
                continue;
            }

            let mut pattern = raw;
            let mut negate = false;

            // 处理 ! 取反
            if pattern.starts_with('!') {
                negate = true;
                pattern = &pattern[1..];
            }

            // 文件黑名单不处理 / 后缀（不会标记为 dir_only）
            let anchored = pattern.starts_with('/');
            if anchored {
                pattern = &pattern[1..];
            }
            let has_slash = pattern.contains('/');

            if let Some(regex_str) = Self::compile_pattern(pattern, anchored, has_slash) {
                match Regex::new(&regex_str) {
                    Ok(re) => rules.push(IgnoreRule {
                        regex: re,
                        negate,
                        dir_only: false,
                    }),
                    Err(e) => log::warn!("[IgnoreMatcher] 正则编译失败 ({}): {}", pattern, e),
                }
            }
        }
        Self { rules }
    }

    /// 将 gitignore 模式片段编译为正则表达式字符串
    /// 返回 None 表示模式为空
    fn compile_pattern(pattern: &str, anchored: bool, has_slash: bool) -> Option<String> {
        if pattern.is_empty() {
            return None;
        }
        let mut regex_str = String::new();
        let mut chars = pattern.chars().peekable();

        while let Some(ch) = chars.next() {
            match ch {
                '*' => {
                    if chars.peek() == Some(&'*') {
                        chars.next(); // 跳过第二个 *
                        if chars.peek() == Some(&'/') {
                            chars.next(); // 跳过 /
                            regex_str.push_str("(.+/)?");
                        } else {
                            regex_str.push_str(".*");
                        }
                    } else {
                        regex_str.push_str("[^/]*");
                    }
                }
                '?' => regex_str.push_str("[^/]"),
                '.' => regex_str.push_str("\\."),
                '[' => {
                    let mut class = String::from("[");
                    if chars.peek() == Some(&'!') {
                        chars.next();
                        class.push('^');
                    }
                    let mut first = true;
                    loop {
                        match chars.next() {
                            Some(']') if !first => {
                                class.push(']');
                                break;
                            }
                            Some(c) => {
                                class.push(c);
                                first = false;
                            }
                            None => break,
                        }
                    }
                    regex_str.push_str(&class);
                }
                '+' | '(' | ')' | '{' | '}' | '|' | '^' | '$' | '\\' => {
                    regex_str.push('\\');
                    regex_str.push(ch);
                }
                _ => regex_str.push(ch),
            }
        }

        // 匹配范围（对齐 JS）
        if anchored || has_slash {
            regex_str = format!("^{}$", regex_str);
        } else {
            regex_str = format!("(^|.*/){}$", regex_str);
        }

        Some(regex_str)
    }

    /// 测试是否应该跳过目录
    /// - `rel_path`: 相对于根目录的路径（如 `src/node_modules/foo`）
    pub fn should_skip_dir(&self, rel_path: &str) -> bool {
        let path = rel_path.replace('\\', "/").trim_end_matches('/').to_string();
        let mut matched = false;
        for rule in &self.rules {
            if !rule.dir_only {
                // 目录规则仅匹配 dir_only 模式（JS: rule.dirOnly check）
                continue;
            }
            if rule.regex.is_match(&path) {
                matched = !rule.negate;
            }
        }
        matched
    }

    /// 测试是否应该跳过文件
    /// - `rel_path`: 相对于根目录的路径（如 `src/index.js`）
    pub fn should_skip_file(&self, rel_path: &str) -> bool {
        let path = rel_path.replace('\\', "/").trim_end_matches('/').to_string();
        let mut matched = false;
        for rule in &self.rules {
            if rule.dir_only {
                continue;
            }
            if rule.regex.is_match(&path) {
                matched = !rule.negate;
            }
        }
        matched
    }

    /// 检查文件是否被跳过（含文件名级别的快速判断：隐藏文件、临时文件）
    /// 这是 KB 专用的组合检查
    pub fn is_kb_file_allowed(&self, file_name: &str, rel_path: &str) -> bool {
        // 先检查黑名单模式
        if self.should_skip_file(rel_path) {
            return false;
        }
        // 对齐 JS FILE_BLACKLIST + 额外临时文件检查
        if file_name.starts_with('.') {
            return false;
        }
        if file_name.ends_with('$') {
            return false;
        }
        let lower = file_name.to_lowercase();
        if lower.ends_with(".tmp") || lower.ends_with(".log") {
            return false;
        }
        if file_name.ends_with('~') || file_name.starts_with("~$") {
            return false;
        }
        true
    }

    /// 检查目录是否被跳过（含隐藏目录检查）
    pub fn is_kb_dir_allowed(&self, dir_name: &str, rel_path: &str) -> bool {
        // 先检查黑名单模式
        if self.should_skip_dir(rel_path) {
            return false;
        }
        // 对齐 JS DIR_BLACKLIST: 隐藏目录 ($*/ 和 .*/)
        if dir_name.starts_with('.') || dir_name.ends_with('$') {
            return false;
        }
        true
    }
}

// ─── 进度事件结构 ───

#[derive(Debug, Serialize, Clone)]
pub struct KbProgress {
    pub percent: u8,
    pub message: String,
}

/// 本地 BGE 模型输出的向量维度（动态获取，等于模型的 hidden_size）。
///
/// 注意：首次调用时会触发模型初始化（可能触发远程下载）以确保返回正确的维度。
/// 模型不可用时返回错误，调用方应阻止建表/检索等后续操作。
pub fn get_local_embedding_dimension() -> Result<u32, String> {
    let model_dir = get_model_dir()?;
    crate::core::embedding::ensure_initialized(model_dir)?;
    Ok(crate::core::embedding::get_embedding_dimension() as u32)
}

/// 本地 BGE 模型名称（从模型目录名提取，如 bge-small-zh-v1.5）。
pub fn get_local_embedding_model_name() -> String {
    crate::core::embedding::get_model_name()
}

/// 模型目录路径（惰性缓存，仅首次解析成功，终身复用）
static MODEL_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 模型下载互斥锁：防止多线程首次加载时并发下载同一模型
static MODEL_DIR_LOCK: Mutex<()> = Mutex::new(());

/// 最近一次模型加载失败原因（成功时清空；供 UI 区分"下载中/加载失败"）
static MODEL_LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// 非阻塞检查模型是否已就绪（进程内路径缓存存在即视为就绪，零 IO）
pub fn is_model_ready() -> bool {
    MODEL_DIR.get().is_some()
}

/// 最近一次模型下载/加载失败原因（无失败则为 None）
pub fn model_load_error() -> Option<String> {
    MODEL_LAST_ERROR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// 确保模型可用：首次调用触发远程下载（下载 zip → SHA-256 校验 → 解压部署到缓存目录）。
///
/// 供启动时后台预下载与首次使用两种场景调用，内部加锁保证全局只下载一次。
/// 下载失败不缓存错误（记录到 MODEL_LAST_ERROR 供状态展示），下次调用自动重试。
pub fn ensure_model_ready() -> Result<&'static Path, String> {
    if let Some(p) = MODEL_DIR.get() {
        return Ok(p);
    }

    // 加锁防止并发重复下载，持锁期间其他线程等待
    let _guard = MODEL_DIR_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // 等待期间其他线程可能已完成下载
    if let Some(p) = MODEL_DIR.get() {
        return Ok(p);
    }
    let dir = match crate::core::model_download::ensure_model_downloaded() {
        Ok(d) => {
            *MODEL_LAST_ERROR
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            d
        }
        Err(e) => {
            *MODEL_LAST_ERROR
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(e.clone());
            return Err(e);
        }
    };
    let _ = MODEL_DIR.set(dir);
    Ok(MODEL_DIR.get().expect("刚 set 成功"))
}

/// 获取模型目录路径（缓存版，首次调用后零开销）。
///
/// 模型不随安装包内置，首次使用前自动后台下载部署（见 ensure_model_ready）。
pub fn get_model_dir() -> Result<&'static Path, String> {
    ensure_model_ready()
}

/// BGE 查询端指令前缀：为向量检索优化查询表示。
///
/// BGE 模型在检索任务中，对查询加 instruction 前缀可使向量更聚焦于检索意图，
/// 检索精度提升 3-5%（BGE 官方基准测试确认）。
const BGE_QUERY_INSTRUCTION: &str = "为这个句子生成表示以用于检索相关文章：";

/// 使用本地 BGE-Small-ZH 模型生成向量（文档端，不加指令前缀）。
///
/// # 并发模型
/// - `call_embedding_parallel` 内部使用 ONNX Runtime 批处理推理
/// - 模型路径通过 `OnceLock` 缓存，仅首次调用时解析
pub fn call_embedding(
    texts: &[&str],
    progress: Option<&(dyn Fn(usize, usize, &str) + Send + Sync)>,
) -> Result<Vec<Vec<f32>>, String> {
    log::debug!("[embedding] call_embedding texts_count={} first_text_len={}",
        texts.len(), texts.first().map(|t| t.len()).unwrap_or(0));
    if texts.is_empty() {
        return Ok(Vec::new());
    }

    let model_dir = get_model_dir()?;
    crate::core::embedding::call_embedding_parallel(texts, model_dir, progress)
}

/// 使用本地 BGE-Small-ZH 模型生成**查询端**向量（自动加 BGE instruction 前缀）。
///
/// 与文档端（不加前缀）配合使用，可提升检索精度 3-5%。
pub fn call_embedding_query(
    text: &str,
) -> Result<Vec<Vec<f32>>, String> {
    let prefixed = format!("{}{}", BGE_QUERY_INSTRUCTION, text);
    call_embedding(&[&prefixed], None)
}

// ─── 文本分块 ───

/// 通用文本分隔符（按优先级从高到低）
pub const GENERIC_TEXT_SEPARATORS: &[&str] = &[
    "\n\n", "\n", ". ", "。", "！", "？", "，", " ",
];

/// Markdown 专用分隔符（含标题模式，仅用于结构化 Markdown 文本）
#[allow(dead_code)]
pub const MARKDOWN_TEXT_SEPARATORS: &[&str] = &[
    "\n## ", "\n### ", "\n#### ", "\n---\n", "\n\n", "\n", ". ", "。", "！", "？", "，", " ",
];

/// 按字符数（而非字节数）切分文本，中英文场景更一致。
///
/// 使用 `GENERIC_TEXT_SEPARATORS` 作为分隔符优先级列表。
/// 预先计算所有字符的字节偏移，避免重复遍历。
pub fn split_text(text: &str, max_size: usize, overlap: usize) -> Vec<String> {
    split_text_with_separators(text, max_size, overlap, GENERIC_TEXT_SEPARATORS)
}

/// 使用自定义分隔符优先级列表进行文本切分。
///
/// 按优先级从高到低尝试在每个窗口内寻找分隔符位置切分，
/// 保证块内语义完整性。max_size 为单块最大字符数，
/// overlap 为前后块重叠字符数。
pub fn split_text_with_separators(
    text: &str,
    max_size: usize,
    overlap: usize,
    separators: &[&str],
) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();

    if total <= max_size {
        return vec![text.to_string()];
    }

    // 预计算每个字符位置的字节偏移（一次性 O(n)，避免循环内重复计算）
    let byte_offsets: Vec<usize> = std::iter::once(0)
        .chain(text.char_indices().skip(1).map(|(i, _)| i))
        .chain(std::iter::once(text.len()))
        .collect();

    let mut chunks = Vec::new();
    let mut start = 0usize;

    while start < total {
        let mut end = (start + max_size).min(total);

        if end < total {
            let mut best_sep_pos = start;
            // 使用预计算的字节偏移取窗口子串
            let window_start_byte = byte_offsets[start];
            let window_end_byte = byte_offsets[end];
            let window = &text[window_start_byte..window_end_byte];

            for sep in separators {
                if let Some(rel_byte) = window.rfind(sep) {
                    let sep_end_byte = rel_byte + sep.len();
                    let sep_char_count = window[..sep_end_byte].chars().count();
                    let candidate = start + sep_char_count;
                    if candidate > best_sep_pos
                        && candidate - start < (max_size as f64 * 1.5) as usize
                    {
                        best_sep_pos = candidate;
                    }
                }
            }
            if best_sep_pos > start {
                end = best_sep_pos;
            }
        }

        let start_byte = byte_offsets[start];
        let end_byte = byte_offsets[end];
        chunks.push(text[start_byte..end_byte].to_string());

        let next_start = if end > overlap { end - overlap } else { end };
        // 防止无限循环：确保每次迭代都有进展
        if next_start <= start {
            start = end;
        } else {
            start = next_start;
        }
    }

    chunks.retain(|c| !c.trim().is_empty());
    chunks
}

/// 基于 Unicode chars 切割文本，严格按字符计数，避免多字节字符截断乱码。
///
/// 与 `split_text` 不同，此函数不做任何分隔符智能切分，仅按固定字符数滑动窗口分割。
/// max_chars：单块最大字符数量；overlap：前后块重叠字符数。
pub fn split_text_char_based(text: &str, max_chars: usize, overlap: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    if total == 0 || max_chars == 0 {
        return chunks;
    }

    let mut start = 0;
    while start < total {
        let end = (start + max_chars).min(total);
        let chunk: String = chars[start..end].iter().collect();
        chunks.push(chunk);
        // 滑动窗口重叠，防止上下文断裂
        let next = end.saturating_sub(overlap);
        if next <= start {
            break;
        }
        start = next;
    }
    chunks
}

// ─── 句子分割与语义工具 ───

/// 将文本分割为句子列表，保留句尾标点。
///
/// 支持中英文混合文本：
/// - 中文边界：。！？……
/// - 英文边界：. ! ?（句点后需有空白或结尾）
/// - 换行符视为句子边界
/// - 短片段（≤3 字符且无中文）合并到前一句，减少英文缩写误切
#[allow(dead_code)]
pub fn split_sentences(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return vec![];
    }

    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"[^。！？!?\n]+[。！？!?\n]?|……+[^……]*……?").unwrap()
    });

    let sentences: Vec<String> = re.find_iter(text)
        .map(|m| m.as_str().trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // 合并可能被误切的英文缩写（如 "Mr." "Dr." "U.S."）
    let mut merged: Vec<String> = Vec::new();
    for s in sentences {
        if let Some(last) = merged.last_mut() {
            if s.chars().count() <= 3 && !s.contains(|c: char| c >= '\u{4e00}') {
                last.push(' ');
                last.push_str(&s);
                continue;
            }
        }
        merged.push(s);
    }

    if merged.is_empty() {
        merged.push(text.to_string());
    }
    merged
}

/// 计算两个 f32 向量的余弦相似度（范围 0.0 ~ 1.0）
#[allow(dead_code)]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)) as f64
}

// ─── DocumentChunk 批量创建 ───

pub fn build_document_chunks(rel_path: &str, chunks: &[ChunkResult]) -> Vec<DocumentChunk> {
    chunks
        .iter()
        .enumerate()
        .map(|(i, r)| {
            DocumentChunk {
                id: format!("{}:{}:{}", rel_path, i, uuid::Uuid::new_v4()),
                doc_name: rel_path.to_string(),
                chunk_index: i as u32,
                text: r.text.clone(),
                path_depth: r.path_depth,
                path_json: r.path_json.clone(),
                sentence_window: r.sentence_window.clone(),
                symbol_name: r.symbol_name.clone(),
                symbol_kind: r.symbol_kind.clone(),
            }
        })
        .collect()
}

// ─── 路径工具 ───

/// 获取知识库数据目录：{dir_path}/.mdgo
///
/// 每个目录的索引数据独立存储在该目录下的 .mdgo 中，
/// 切换目录时自动加载对应的数据，无需依赖系统级应用数据目录。
pub fn get_data_dir(dir_path: &str) -> String {
    Path::new(dir_path)
        .join(".mdgo")
        .join("lancedb")
        .to_string_lossy()
        .to_string()
}

/// 获取 BM25 索引目录：{dir_path}/.mdgo/bm25
pub fn get_bm25_dir(dir_path: &str) -> String {
    Path::new(dir_path)
        .join(".mdgo")
        .join("bm25")
        .to_string_lossy()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与前端默认黑名单保持一致（index.html DIR_BLACKLIST / FILE_BLACKLIST）
    fn default_ignore_dirs() -> Vec<String> {
        [
            ".*/",
            "$*/",
            "assets/",
            "node_modules/",
            "vendor/",
            "dist/",
            "build/",
            "target/",
            "__pycache__/",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn default_ignore_files() -> Vec<String> {
        [".*", "$*", "*.tmp", "*.log", "!.gitignore"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn test_ignore_matcher_matches_frontend_semantics() {
        let matcher = IgnoreMatcher::new(&default_ignore_dirs(), &default_ignore_files());

        // 目录规则：匹配任意层级的黑名单目录
        assert!(matcher.should_skip_dir("node_modules"));
        assert!(matcher.should_skip_dir(".git"));
        assert!(matcher.should_skip_dir("assets"));
        assert!(matcher.should_skip_dir("code/assets"));
        assert!(matcher.should_skip_dir("dist"));
        assert!(matcher.should_skip_dir("__pycache__"));
        assert!(!matcher.should_skip_dir("src"));
        assert!(!matcher.should_skip_dir("myassets"));
        assert!(!matcher.should_skip_dir("node_modules_x"));

        // 文件规则
        assert!(matcher.should_skip_file(".env"));
        assert!(!matcher.should_skip_file(".gitignore")); // !.gitignore 取反
        assert!(matcher.should_skip_file("a.tmp"));
        assert!(matcher.should_skip_file("x/y/z.log"));
        assert!(!matcher.should_skip_file("readme.md"));

        // 目录模式自动生成的 "目录下所有文件" 规则
        assert!(matcher.should_skip_file("node_modules/pkg/index.js"));
        assert!(matcher.should_skip_file("assets/img/logo.png"));
        assert!(!matcher.should_skip_file("src/main.rs"));
    }
}
