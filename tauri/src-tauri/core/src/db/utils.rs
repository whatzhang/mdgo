use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;

use super::lance::DocumentChunk;

// ─── 常量 ───

pub const KB_SUPPORTED_EXTS: &[&str] = &[
    "md", "txt", "pdf", "docx", "js", "ts", "jsx", "tsx", "py", "java", "go", "rs", "rb", "php",
    "c", "cpp", "h", "hpp", "cs", "swift", "kt", "scala", "r", "lua", "sh", "bash", "zsh", "ps1",
    "sql", "css", "scss", "less", "html", "htm", "xml", "json", "yaml", "yml", "toml", "ini",
    "cfg", "conf", "env", "gitignore", "dockerfile", "makefile",
];

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

/// 本地 BGE-Small-ZH 模型输出的向量维度（384）。
pub const LOCAL_EMBEDDING_DIMENSION: u32 = 384;

/// 启动时解析模型文件的实际路径（纯本地，零网络依赖）。
///
/// # 搜索优先级
/// 1. `MDGO_MODEL_DIR` 环境变量（用户/启动器手动指定）
/// 2. 可执行文件同目录 `models/bge-small-zh-v1.5/`（Windows NSIS 打包）
/// 3. macOS 资源目录 `<App.app>/Contents/Resources/models/bge-small-zh-v1.5/`
/// 4. 开发模式 `CARGO_MANIFEST_DIR/models/bge-small-zh-v1.5/`
fn resolve_model_dir() -> std::path::PathBuf {
    // 1. 环境变量
    if let Ok(dir) = std::env::var("MDGO_MODEL_DIR") {
        let p = std::path::PathBuf::from(&dir);
        if p.join("model.onnx").exists() {
            return p;
        }
    }

    // 2. 可执行文件同目录
    if let Ok(exe) = std::env::current_exe() {
        let candidates = [
            exe.parent().map(|p| p.join("models").join("bge-small-zh-v1.5")),
            // macOS: executable in MacOS/, resources in ../Resources/
            exe.parent()
                .and_then(|p| p.parent())
                .map(|p| p.join("Resources").join("models").join("bge-small-zh-v1.5")),
        ];
        for candidate in candidates.iter().flatten() {
            if candidate.join("model.onnx").exists() {
                return candidate.clone();
            }
        }
    }

    // 3. 开发模式：CARGO_MANIFEST_DIR/models/bge-small-zh-v1.5
    if cfg!(debug_assertions) {
        if let Ok(cargo_dir) = std::env::var("CARGO_MANIFEST_DIR")
            .or_else(|_| std::env::var("MDGO_CARGO_MANIFEST_DIR"))
        {
            let p = std::path::PathBuf::from(&cargo_dir)
                .join("models")
                .join("bge-small-zh-v1.5");
            if p.join("model.onnx").exists() {
                return p;
            }
        }
        // fallback: env!("CARGO_MANIFEST_DIR") 编译时注入（core crate 在 core/ 下，需退回父目录）
        #[cfg(debug_assertions)]
        {
            let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("models")
                .join("bge-small-zh-v1.5");
            if p.join("model.onnx").exists() {
                return p;
            }
        }
    }

    // 全部失败 → 返回默认路径并让调用者报错
    log::error!("[local_embedding] 模型文件未找到，请检查安装包完整性");
    std::path::PathBuf::from("models/bge-small-zh-v1.5")
}

/// 模型目录路径（惰性静态缓存，仅首次解析，终身复用）
static MODEL_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 获取模型目录路径（缓存版，首次调用后零开销）
pub fn get_model_dir() -> &'static Path {
    MODEL_DIR.get_or_init(|| resolve_model_dir())
}

/// 使用本地 BGE-Small-ZH 模型生成向量。
///
/// 向量维度：384（bge-small-zh-v1.5）。
///
/// # 并发模型
/// - `call_embedding_parallel` 内部使用 ONNX Runtime 批处理推理
/// - 模型路径通过 `OnceLock` 缓存，仅首次调用时解析
pub fn call_embedding(texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }

    let model_dir = get_model_dir();
    crate::embedding::call_embedding_parallel(texts, model_dir)
}

// ─── 文本分块（解决 C2：唯一版本）───

/// 按字符数（而非字节数）切分文本，中英文场景更一致。
///
/// 预先计算所有字符的字节偏移，避免重复遍历。
pub fn split_text(text: &str, max_size: usize, overlap: usize) -> Vec<String> {
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
    let separators = [
        "\n## ", "\n### ", "\n#### ", "\n---\n", "\n\n", "\n", ". ", "。", "！", "？", "，", " ",
    ];
    let mut start = 0usize;

    while start < total {
        let mut end = (start + max_size).min(total);

        if end < total {
            let mut best_sep_pos = start;
            // 使用预计算的字节偏移取窗口子串
            let window_start_byte = byte_offsets[start];
            let window_end_byte = byte_offsets[end];
            let window = &text[window_start_byte..window_end_byte];

            for sep in &separators {
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

// ─── DocumentChunk 批量创建 ───

pub fn build_document_chunks(rel_path: &str, chunks: &[String]) -> Vec<DocumentChunk> {
    chunks
        .iter()
        .enumerate()
        .map(|(i, text)| DocumentChunk {
            id: format!("{}:{}:{}", rel_path, i, uuid::Uuid::new_v4()),
            doc_name: rel_path.to_string(),
            chunk_index: i as u32,
            text: text.clone(),
        })
        .collect()
}

// ─── 路径工具 ───

/// 获取知识库数据目录：{dir_path}/.mdgo/data
///
/// 每个目录的索引数据独立存储在该目录下的 .mdgo/data 中，
/// 切换目录时自动加载对应的数据，无需依赖系统级应用数据目录。
pub fn get_data_dir(dir_path: &str) -> String {
    Path::new(dir_path)
        .join(".mdgo")
        .join("data")
        .join("lancedb")
        .to_string_lossy()
        .to_string()
}

/// 获取 BM25 索引目录：{dir_path}/.mdgo/data/bm25
pub fn get_bm25_dir(dir_path: &str) -> String {
    Path::new(dir_path)
        .join(".mdgo")
        .join("data")
        .join("bm25")
        .to_string_lossy()
        .to_string()
}

/// 获取对话 BM25 索引目录：{dir_path}/.mdgo/data/chat_bm25
///
/// 对话数据与文档数据分离存储，避免互相污染。
pub fn get_chat_bm25_dir(dir_path: &str) -> String {
    Path::new(dir_path)
        .join(".mdgo")
        .join("data")
        .join("chat_bm25")
        .to_string_lossy()
        .to_string()
}
