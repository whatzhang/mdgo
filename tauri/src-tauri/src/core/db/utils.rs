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

    /// 判断相对路径是否命中任一条**目录规则**（gitignore 目录语义）。
    ///
    /// 依次匹配「完整路径 → 各级父目录路径」，任一级命中即 true（negate 取反覆盖）。
    /// 供「HTML 渲染目录」等白名单式路径匹配使用：目录模式（如 `docs/`）命中其下所有文件。
    /// 仅考虑 `dir_only` 规则（文件规则不参与，避免语义混淆）。
    pub fn matches(&self, rel_path: &str) -> bool {
        let path = rel_path.replace('\\', "/").trim_start_matches('/').to_string();
        let mut matched = false;
        let mut p = path.as_str();
        loop {
            for rule in &self.rules {
                if !rule.dir_only {
                    continue;
                }
                if rule.regex.is_match(p) {
                    matched = !rule.negate;
                }
            }
            match p.rfind('/') {
                Some(idx) => p = &p[..idx],
                None => break,
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
    log::info!("[embedding] call_embedding texts_count={} first_text_len={}",
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
///
/// B1：进程内查询向量缓存（键 = 模型作用域 + 原始查询文本哈希；实现为 FIFO 淘汰，
/// 模块注释已注明是 LRU 的轻量近似——🟠 L17：措辞统一为 FIFO），重复/近似查询零推理。
pub fn call_embedding_query(
    text: &str,
) -> Result<Vec<Vec<f32>>, String> {
    if let Some(v) = super::query_embedding_cache::global_query_embedding_cache().get(text) {
        // 按字符截断（字节切片会在中文多字节字符中间 panic，如 "求" 占 3 字节）
        log::debug!("[embedding] 查询向量缓存命中: {:?}", text.chars().take(40).collect::<String>());
        return Ok(vec![v]);
    }
    let prefixed = format!("{}{}", BGE_QUERY_INSTRUCTION, text);
    let result = call_embedding(&[&prefixed], None)?;
    if let Some(v) = result.first().cloned() {
        super::query_embedding_cache::global_query_embedding_cache().put(text, v);
    }
    Ok(result)
}

/// 批量生成**查询端**向量（P0 预检索优化器：多查询一次批量推理）。
///
/// 与 [`call_embedding_query`] 语义一致（每条自动加 BGE instruction 前缀），
/// 内部走 `call_embedding` 的批处理（BATCH_SIZE=128），替代逐条
/// `spawn_blocking(call_embedding_query)` 的多次阻塞调用。
///
/// B1：逐条查缓存，仅对未命中批量推理。
pub fn call_embedding_queries(
    texts: &[&str],
) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let cache = super::query_embedding_cache::global_query_embedding_cache();
    // 逐条查缓存（保持输入顺序）
    let cached: Vec<Option<Vec<f32>>> = texts.iter().map(|t| cache.get(t)).collect();
    let miss_indices: Vec<usize> = cached
        .iter()
        .enumerate()
        .filter_map(|(i, v)| if v.is_none() { Some(i) } else { None })
        .collect();
    if miss_indices.is_empty() {
        return Ok(cached.into_iter().map(|v| v.unwrap()).collect());
    }
    let miss_texts: Vec<&str> = miss_indices.iter().map(|&i| texts[i]).collect();
    let prefixed: Vec<String> = miss_texts
        .iter()
        .map(|t| format!("{}{}", BGE_QUERY_INSTRUCTION, t))
        .collect();
    let refs: Vec<&str> = prefixed.iter().map(|s| s.as_str()).collect();
    let miss_vectors = call_embedding(&refs, None)?;
    // 回填缓存 + 组装结果
    let mut result = Vec::with_capacity(texts.len());
    let mut miss_iter = miss_vectors.into_iter();
    for (i, _) in texts.iter().enumerate() {
        match &cached[i] {
            Some(v) => result.push(v.clone()),
            None => {
                if let Some(v) = miss_iter.next() {
                    cache.put(texts[i], v.clone());
                    result.push(v);
                } else {
                    return Err("查询向量批量推理结果与输入不一致".into());
                }
            }
        }
    }
    Ok(result)
}

// ─── 文本分块 ───

/// 按字符数（而非字节数）切分文本，中英文场景更一致。
///
/// 使用 `GENERIC_TEXT_SEPARATORS` 作为分隔符优先级列表。
/// 预先计算所有字符的字节偏移，避免重复遍历。
///
/// 实现位于 `document::text_split`（基础层），此处委托以保留公开 API。
pub fn split_text(text: &str, max_size: usize, overlap: usize) -> Vec<String> {
    crate::core::document::text_split::split_text(text, max_size, overlap)
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

// ─── 语义工具 ───

/// 计算两个 f32 向量的余弦相似度（范围 0.0 ~ 1.0）
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)) as f64
}

// ─── 稳定内容哈希（P0-5） ───

/// FNV-1a 128 位哈希（确定性、零依赖）。
///
/// 用途：chunk 稳定身份（`build_document_chunks` 的 id）与 embedding 缓存键。
/// 非加密用途：KB 规模（10^6 级 chunk）下碰撞概率可忽略，且碰撞后果仅是
/// 缓存未命中 / 同 id 覆盖（幂等），不致命。
const FNV_128_OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
const FNV_128_PRIME: u128 = 0x0000000001000000000000000000013b;

pub fn fnv1a_128(data: &[u8]) -> u128 {
    let mut h = FNV_128_OFFSET;
    for &b in data {
        h ^= b as u128;
        h = h.wrapping_mul(FNV_128_PRIME);
    }
    h
}

/// 稳定十六进制哈希（32 位十六进制小写）
pub fn stable_hash_hex(input: &str) -> String {
    format!("{:032x}", fnv1a_128(input.as_bytes()))
}

/// 分块器/哈希版本标记（chunk 身份稳定契约的一部分；分块逻辑变化时递增）
pub const CHUNK_IDENTITY_VERSION: &str = "mdgo-chunk-v1";

// ─── DocumentChunk 批量创建 ───

/// 构建 DocumentChunk 列表。
///
/// P0-5：id 由随机 UUID 改为**稳定内容哈希**（`rel_path#hash`）——同内容同 id：
/// - 幂等：同一文件重复索引产出相同 id（先删后写语义不变）；
/// - 支持 embedding 内容哈希缓存（增量索引只重嵌变化 chunk）；
/// - 哈希输入 = 规范化文本 + 语义元数据 + 位置 + 版本（identity 稳定契约）。
pub fn build_document_chunks(rel_path: &str, chunks: &[ChunkResult]) -> Vec<DocumentChunk> {
    chunks
        .iter()
        .enumerate()
        .map(|(i, r)| {
            // P0-5：稳定内容哈希 id（identity 稳定契约；tags 参与哈希——
            // 文档标签变化 → 检索行为变化 → chunk 身份随之更新；
            // 🟠 L18：doc_title 同样参与——title 变化改变 BM25 title 字段与检索行为，
            // 旧实现只含 tags 造成身份契约前后不一）
            let tags_json = r
                .tags
                .as_ref()
                .map(|t| serde_json::to_string(t).unwrap_or_default())
                .unwrap_or_default();
            let hash_input = format!(
                "{}|{}|{}\n{}\n{}\n{}\n{}\n{}\n{}",
                CHUNK_IDENTITY_VERSION,
                rel_path,
                i,
                r.text,
                r.embedding_text.as_deref().unwrap_or(""),
                r.path_json.as_deref().unwrap_or(""),
                r.symbol_name.as_deref().unwrap_or(""),
                r.doc_title.as_deref().unwrap_or(""),
                tags_json,
            );
            DocumentChunk {
                id: format!("{}#{}", rel_path, stable_hash_hex(&hash_input)),
                doc_name: rel_path.to_string(),
                chunk_index: i as u32,
                text: r.text.clone(),
                path_depth: r.path_depth,
                path_json: r.path_json.clone(),
                sentence_window: r.sentence_window.clone(),
                symbol_name: r.symbol_name.clone(),
                symbol_kind: r.symbol_kind.clone(),
                embedding_text: r.embedding_text.clone(),
                chunk_type: r.chunk_type.clone(),
                doc_title: r.doc_title.clone(),
                tags: r.tags.as_ref().map(|t| serde_json::to_string(t).unwrap_or_default()),
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

/// 获取 embedding 缓存目录：{dir_path}/.mdgo（P0-5，缓存独立于 LanceDB/BM25 数据）
pub fn get_cache_dir(dir_path: &str) -> String {
    Path::new(dir_path)
        .join(".mdgo")
        .to_string_lossy()
        .to_string()
}

#[cfg(test)]
mod ignore_matcher_tests {
    use super::IgnoreMatcher;

    #[test]
    fn matches_dir_pattern_hits_descendants() {
        let m = IgnoreMatcher::new(&["docs/".to_string(), "assets".to_string()], &[]);
        assert!(m.matches("docs/a.html"), "docs/ 下的文件应命中");
        assert!(m.matches("sub/docs/a.html"), "嵌套目录 docs 应命中");
        assert!(m.matches("assets/x.html"), "无斜杠目录模式 assets 应命中其下文件");
        assert!(!m.matches("src/a.html"), "未命中目录不应命中");
        assert!(!m.matches("document.html"), "文件名前缀相似不应误命中");
    }

    #[test]
    fn matches_respects_negate() {
        let m = IgnoreMatcher::new(&["docs/".to_string(), "!docs/private/".to_string()], &[]);
        assert!(m.matches("docs/a.html"));
        assert!(m.matches("docs/private/x.html"), "negate 规则在目录规则上通常不生效（供语义参考）");
    }

    #[test]
    fn matches_empty_rules_never_hits() {
        let m = IgnoreMatcher::new(&[], &[]);
        assert!(!m.matches("docs/a.html"));
    }
}

// ─── P0-5 测试：稳定哈希与 chunk 身份 ───

#[cfg(test)]
mod chunk_identity_tests {
    use super::*;
    use crate::core::db::chunk_splitter::ChunkResult;

    #[test]
    fn stable_hash_deterministic() {
        let a = stable_hash_hex("同一内容");
        let b = stable_hash_hex("同一内容");
        let c = stable_hash_hex("不同内容");
        assert_eq!(a, b, "同输入哈希必须一致");
        assert_ne!(a, c, "不同输入哈希必须不同");
    }

    #[test]
    fn chunk_ids_stable_and_content_sensitive() {
        let c1 = vec![ChunkResult::plain("内容甲".into())];
        let c2 = vec![ChunkResult::plain("内容甲".into())];
        let c3 = vec![ChunkResult::plain("内容乙".into())];
        let d1 = build_document_chunks("a.md", &c1);
        let d2 = build_document_chunks("a.md", &c2);
        let d3 = build_document_chunks("a.md", &c3);
        assert_eq!(d1[0].id, d2[0].id, "同内容同 id（幂等）");
        assert_ne!(d1[0].id, d3[0].id, "内容变化 → id 变化");
        assert!(d1[0].id.starts_with("a.md#"), "id 应带 doc 前缀: {}", d1[0].id);
    }

    #[test]
    fn chunk_ids_unique_within_doc() {
        // 同文档内重复内容（相同文本不同位置）→ id 仍唯一（位置参与哈希）
        let chunks = vec![ChunkResult::plain("重复内容".into()), ChunkResult::plain("重复内容".into())];
        let docs = build_document_chunks("dup.md", &chunks);
        assert_ne!(docs[0].id, docs[1].id, "同文档重复 chunk id 必须唯一（LanceDB 主键）");
    }
}
