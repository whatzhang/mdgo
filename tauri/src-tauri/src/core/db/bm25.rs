use std::fs;
use std::path::Path;
use std::sync::Mutex;

use tantivy::collector::{Count, TopDocs};
use tantivy::query::{
    BooleanQuery, BoostQuery, Occur, Query, QueryParser, TermQuery,
};
use tantivy::schema::*;
use tantivy::tokenizer::{
    LowerCaser, RemoveLongFilter, TextAnalyzer, Token, TokenStream, Tokenizer,
};
use tantivy::{
    doc, Index, IndexReader, ReloadPolicy, Searcher, TantivyDocument, Term,
};

use super::lance::{DocumentChunk, SearchHit};

/// Jieba 中文分词器（替换简单 Bigram 2-gram）。
///
/// 使用 jieba-rs 进行真正的词法切分，"机器学习" → ["机器", "学习"] 而非 ["机器", "器学", "学习"]。
/// 英文/数字保持原有按字母数字 token 的处理。
/// Jieba 实例通过 OnceLock 全局缓存，首次使用时加载词典（约 1-2 秒），
/// 应用启动时通过 [warmup] 后台预热，避免阻塞首个检索/索引请求。
#[derive(Clone)]
struct JiebaTokenizer;

/// 全局 Jieba 实例（词典加载成本高，仅初始化一次）
static JIEBA: std::sync::OnceLock<jieba_rs::Jieba> = std::sync::OnceLock::new();

/// 预热 Jieba 中文分词器（线程安全幂等，可在应用启动时后台调用）。
pub fn warmup() {
    let _ = JIEBA.get_or_init(|| {
        log::info!("[bm25] 初始化 Jieba 中文分词...");
        jieba_rs::Jieba::new()
    });
}

struct JiebaTokenStream {
    tokens: Vec<(String, usize, usize)>, // (text, offset_from, offset_to)
    pos: usize,
    current_token: Token,
}

impl Tokenizer for JiebaTokenizer {
    type TokenStream<'a> = JiebaTokenStream;

    fn token_stream<'a>(&mut self, text: &'a str) -> JiebaTokenStream {
        let jieba = JIEBA.get_or_init(|| {
            log::info!("[bm25] 初始化 Jieba 中文分词...");
            jieba_rs::Jieba::new()
        });
        JiebaTokenStream {
            tokens: segment_text(text, jieba),
            pos: 0,
            current_token: Token::default(),
        }
    }
}

/// 对文本做 Jieba 中文分词 + 英文数字 token 化，返回 (text, byte_offset_from, byte_offset_to)。
fn segment_text(text: &str, jieba: &jieba_rs::Jieba) -> Vec<(String, usize, usize)> {
    let mut results = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // CJK 段：收集连续 CJK 字符，用 Jieba 分词
        if is_cjk(c) {
            let start = i;
            while i < chars.len() && is_cjk(chars[i]) {
                i += 1;
            }
            let cjk_text: String = chars[start..i].iter().collect();
            let cjk_base_offset = char_offset_to_byte(text, start); // CJK 段在原始文本中的字节起始
            // hmm=true 开启隐马尔可夫未知词识别：词典未收录的新词（人名、技术术语等）
            // 通过 Viterbi 动态规划切出，提升 BM25 对新词的召回；已收录词典词的切分不受影响
            for token in jieba.cut(&cjk_text, true) {
                let word = token.word;
                // 跳过空白分词结果
                if word.trim().is_empty() {
                    continue;
                }
                // 使用 jieba 返回的字节偏移（byte_start/byte_end 相对 cjk_text），
                // 叠加 CJK 段在原始文本中的起始偏移，避免指针算术（空词时可能产生无效偏移）
                let offset_from = cjk_base_offset + token.byte_start;
                let offset_to = cjk_base_offset + token.byte_end;
                results.push((word.to_string(), offset_from, offset_to));
            }
            continue;
        }
        // 英文/数字：收集连续的字母数字
        if c.is_alphanumeric() {
            let start = i;
            while i < chars.len() && chars[i].is_alphanumeric() {
                i += 1;
            }
            let token: String = chars[start..i].iter().collect();
            let offset_from = char_offset_to_byte(text, start);
            let offset_to = char_offset_to_byte(text, i);
            results.push((token, offset_from, offset_to));
            continue;
        }
        // 标点/空白：跳过
        i += 1;
    }
    results
}

/// 快速判断 CJK 字符
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF   // CJK Unified Ideographs
        | 0x3400..=0x4DBF   // CJK Unified Ideographs Extension A
        | 0x3040..=0x309F   // Hiragana
        | 0x30A0..=0x30FF   // Katakana
        | 0xAC00..=0xD7AF   // Hangul Syllables
    )
}

/// 将 char 索引转换为字节偏移
fn char_offset_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(text.len())
}

impl JiebaTokenStream {
    fn fill_current_token(&mut self) {
        if let Some((text, offset_from, offset_to)) = self.tokens.get(self.pos) {
            self.current_token = Token {
                offset_from: *offset_from,
                offset_to: *offset_to,
                position: self.pos,
                text: text.clone(),
                ..Default::default()
            };
        }
    }
}

impl TokenStream for JiebaTokenStream {
    fn advance(&mut self) -> bool {
        if self.pos >= self.tokens.len() {
            return false;
        }
        self.fill_current_token();
        self.pos += 1;
        true
    }

    fn token(&self) -> &Token {
        &self.current_token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.current_token
    }
}

/// 构建中文 + 英文混合的文本分词器（Jieba 分词 + 小写化 + 长词过滤）
fn chinese_text_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(JiebaTokenizer)
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .build()
}

/// 标识符分词器：把 `字母/数字/_` 连续段视为一个 token。
///
/// 用于 `symbol_name` 字段（Field BM25 升级）：
/// - `lru_cache` → 单个 token，而非 jieba 切出的 ["lru", "cache"]
/// - `parseJSON` → 单个 token（经 LowerCaser 后为 `parsejson`），
///   代码问答"parseJSON 在哪里定义"可直接命中
#[derive(Clone)]
struct IdentifierTokenizer;

struct IdentifierTokenStream {
    tokens: Vec<(String, usize, usize)>, // (text, offset_from, offset_to)
    pos: usize,
    current_token: Token,
}

impl Tokenizer for IdentifierTokenizer {
    type TokenStream<'a> = IdentifierTokenStream;

    fn token_stream<'a>(&mut self, text: &'a str) -> IdentifierTokenStream {
        IdentifierTokenStream {
            tokens: segment_identifiers(text),
            pos: 0,
            current_token: Token::default(),
        }
    }
}

/// 将文本按 `字母/数字/_` 连续段切分为标识符 token。
///
/// CJK 表意文字（汉字等）属于 `is_alphanumeric()` 但**语义上每个字是独立词元**：
/// 若与 ASCII 拼进同一 token（如 `文档介绍`），BM25 单字查询"介绍"将无法命中。
/// 因此 CJK 字符各自成为独立单字 token，ASCII 标识符仍保持整体（C2）。
fn segment_identifiers(text: &str) -> Vec<(String, usize, usize)> {
    let mut tokens = Vec::new();
    let mut current: Option<(String, usize)> = None; // (token 文本, 起始字节)
    for (i, c) in text.char_indices() {
        if c.is_ascii_alphanumeric() || c == '_' {
            match &mut current {
                Some((s, _)) => s.push(c),
                None => current = Some((c.to_string(), i)),
            }
        } else {
            // 分隔符（含中文标点、/、. 等）：结束当前 ASCII token
            if let Some((s, start)) = current.take() {
                tokens.push((s, start, i));
            }
            // CJK 等非 ASCII 字母：独立单字 token，保证单字/双字查询可命中
            if c.is_alphabetic() {
                tokens.push((c.to_string(), i, i + c.len_utf8()));
            }
        }
    }
    if let Some((s, start)) = current.take() {
        tokens.push((s, start, text.len()));
    }
    tokens
}

impl IdentifierTokenStream {
    fn fill_current_token(&mut self) {
        if let Some((text, offset_from, offset_to)) = self.tokens.get(self.pos) {
            self.current_token = Token {
                offset_from: *offset_from,
                offset_to: *offset_to,
                position: self.pos,
                text: text.clone(),
                ..Default::default()
            };
        }
    }
}

impl TokenStream for IdentifierTokenStream {
    fn advance(&mut self) -> bool {
        if self.pos >= self.tokens.len() {
            return false;
        }
        self.fill_current_token();
        self.pos += 1;
        true
    }

    fn token(&self) -> &Token {
        &self.current_token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.current_token
    }
}

/// 构建标识符分词器（小写化，便于大小写不敏感匹配）
fn identifier_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(IdentifierTokenizer)
        .filter(RemoveLongFilter::limit(120))
        .filter(LowerCaser)
        .build()
}

/// BM25 索引 schema 版本号（v4：text 字段仅索引纯正文 + 新增 display_text 展示字段）。
///
/// tantivy 的 schema 变更后旧索引目录无法复用，开发阶段直接删除重建；
/// `open()` 读取标记文件内容，与当前版本不符即自动重建，避免旧目录报错阻断启动。
/// 标记文件名与版本号保持一致（C1）。
const SCHEMA_VERSION: &str = "4";
const SCHEMA_MARKER: &str = ".schema_v4";

/// Windows 下 tantivy commit/merge 偶发 `PermissionDenied`（Defender 实时扫描刚创建的
/// segment 文件；或上一 commit 残留的后台 merge 与本次 commit 竞争 `.managed.json` 重命名）。
/// 失败为瞬时且非破坏性，按退避重试即可恢复。COMMIT_MAX_ATTEMPTS 含首次尝试。
const COMMIT_MAX_ATTEMPTS: usize = 3;
const COMMIT_RETRY_BACKOFF_MS: u64 = 200;

/// 依据 doc_name 提取文件名主干（title 字段）：`src/tools/parse_json.rs` → `parse_json`
fn file_title(doc_name: &str) -> String {
    Path::new(doc_name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| doc_name.to_string())
}

/// 从 path_json（JSON 数组，如 `["Kubernetes","Network","Calico"]`）提取标题层级文本
/// （heading 字段）：解析失败或为空时返回空串。
fn heading_text(path_json: &Option<String>) -> String {
    let arr: Vec<String> = match path_json.as_deref().map(serde_json::from_str) {
        Some(Ok(v)) => v,
        _ => return String::new(),
    };
    arr.join(" ")
}

/// 提取 chunk 的纯正文（供 text 字段索引，B5）。
///
/// AST 语义分块产物的 `embedding_text` = 紧凑标题路径（= heading_text(path_json)）+ 正文，
/// 剥离该前缀即得纯正文；无路径或无法剥离时回退到 `chunk.text`（保持原行为，
/// 避免 text 字段重复索引标题词导致 heading 字段 boost 失效）。
fn body_text(chunk: &DocumentChunk) -> String {
    if let Some(prefix) = chunk
        .path_json
        .as_deref()
        .and_then(|pj| serde_json::from_str::<Vec<String>>(pj).ok())
        .filter(|arr| !arr.is_empty())
        .map(|arr| arr.join(" "))
    {
        if let Some(emb) = chunk.embedding_text.as_deref() {
            let mark = format!("{}\n", prefix);
            if let Some(rest) = emb.strip_prefix(&mark) {
                return rest.to_string();
            }
        }
    }
    chunk.text.clone()
}

pub struct Bm25Index {
    index_path: String,
    reader: Mutex<Option<IndexReader>>,
    write_lock: Mutex<()>,
}

impl Bm25Index {
    fn schema() -> Schema {
        let mut builder = Schema::builder();
        builder.add_text_field("doc_id", STRING | STORED);
        // 完整相对路径：doc_name 保持 STRING（delete_document 依赖整串 term 精确删除），
        // file_path 为分词字段（标识符分词，下划线整体匹配）用于路径模糊检索
        builder.add_text_field("doc_name", STRING | STORED);
        let file_path_options = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("identifier")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored();
        builder.add_text_field("file_path", file_path_options);
        builder.add_u64_field("chunk_index", STORED);
        // 正文（jieba 分词），权重 1.0。
        // v4 起仅索引**纯正文**（剥离标题前缀），标题词只经 heading 字段（boost 2.5）加权，
        // 消除 text + heading 双重计数导致的标题词过度加权（B5）
        let text_options = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("jieba_chinese")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored();
        builder.add_text_field("text", text_options);
        // 展示文本（仅存储）：上下文文本（含 Markdown 标题渲染），供检索结果展示/回传 LLM
        builder.add_text_field("display_text", STORED);
        // 文件名主干（jieba 分词），权重 3.0（文档主题的最强单点信号）
        let title_options = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("jieba_chinese")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored();
        builder.add_text_field("title", title_options);
        // 标题层级路径（jieba 分词），权重 2.5（heading_path → path_json）
        let heading_options = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("jieba_chinese")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored();
        builder.add_text_field("heading", heading_options);
        // 代码符号名（标识符分词，整体匹配），权重 2.0
        let symbol_options = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("identifier")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored();
        builder.add_text_field("symbol_name", symbol_options);
        // 代码符号类型（仅存储）
        builder.add_text_field("symbol_kind", STORED);
        // 分块类型（仅存储，供前端/未来 GraphRAG 使用）
        builder.add_text_field("chunk_type", STORED);
        builder.build()
    }

    /// 在 Index 上注册分词器（text/heading/title 用 jieba，symbol_name 用 identifier）
    fn register_tokenizers(index: &Index) {
        index
            .tokenizers()
            .register("jieba_chinese", chinese_text_analyzer());
        index
            .tokenizers()
            .register("identifier", identifier_analyzer());
    }

    /// 打开已有索引。
    ///
    /// schema 版本不匹配（开发阶段升级）时自动删除重建，不做数据迁移。
    pub fn open(path: &str) -> Result<Self, String> {
        if !Path::new(path).exists() {
            return Err(format!("BM25 索引目录不存在: {}", path));
        }
        // 标记缺失或版本不符（schema 变更）→ 重建
        let marker_path = Path::new(path).join(SCHEMA_MARKER);
        let version = fs::read_to_string(&marker_path).unwrap_or_default();
        if version.trim() != SCHEMA_VERSION {
            log::warn!(
                "[bm25] 检测到 schema 版本不匹配（期望 {}，实际 {:?}），自动重建",
                SCHEMA_VERSION,
                version
            );
            // 旧目录删除失败时中止重建：在残留文件上 Index::create_in_dir 可能产生
            // 半新半旧的不一致索引（C1）
            let p = Path::new(path);
            fs::remove_dir_all(p)
                .map_err(|e| format!("删除旧 BM25 索引目录失败（无法重建）: {}", e))?;
            return Self::create(path);
        }
        Ok(Self {
            index_path: path.to_string(),
            reader: Mutex::new(None),
            write_lock: Mutex::new(()),
        })
    }

    /// 创建新索引（若目录不存在则创建）
    pub fn create(path: &str) -> Result<Self, String> {
        let p = Path::new(path);
        if !p.exists() {
            fs::create_dir_all(p).map_err(|e| format!("创建 BM25 索引目录失败: {}", e))?;
        }
        let schema = Self::schema();
        let index = Index::create_in_dir(path, schema)
            .map_err(|e| format!("创建 BM25 索引失败: {}", e))?;
        Self::register_tokenizers(&index);
        // 写入 schema 版本标记
        fs::write(p.join(SCHEMA_MARKER), SCHEMA_VERSION)
            .map_err(|e| format!("写入 BM25 schema 版本标记失败: {}", e))?;
        Ok(Self {
            index_path: path.to_string(),
            reader: Mutex::new(None),
            write_lock: Mutex::new(()),
        })
    }

    /// 获取 Index 实例（已注册中文分词器）
    fn open_index(&self) -> Result<Index, String> {
        let index = Index::open_in_dir(&self.index_path)
            .map_err(|e| format!("打开 BM25 索引失败: {}", e))?;
        Self::register_tokenizers(&index);
        Ok(index)
    }

    /// 获取缓存的 reader（首次调用时创建，写入后需手动清理缓存）
    fn get_reader(&self) -> Result<IndexReader, String> {
        let mut reader_guard = self.reader.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref reader) = *reader_guard {
            return Ok(reader.clone());
        }
        let index = self.open_index()?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|e| format!("创建 BM25 reader 失败: {}", e))?;
        *reader_guard = Some(reader.clone());
        Ok(reader)
    }

    /// 写入数据后调用，使下一次查询时重建 reader
    fn invalidate_reader(&self) {
        let mut reader_guard = self.reader.lock().unwrap_or_else(|e| e.into_inner());
        *reader_guard = None;
    }

    /// 批量建立全文索引（解决 M3：减少 writer 创建次数）
    pub fn add_documents(&self, chunks: &[DocumentChunk]) -> Result<(), String> {
        if chunks.is_empty() {
            return Ok(());
        }
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.invalidate_reader();
        let mut last_err = String::new();
        for attempt in 0..COMMIT_MAX_ATTEMPTS {
            match self.try_add_documents(chunks) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last_err = e;
                    if attempt + 1 >= COMMIT_MAX_ATTEMPTS {
                        break;
                    }
                    log::warn!(
                        "[bm25] 提交失败（第 {} 次），释放句柄后重试: {}",
                        attempt + 1,
                        last_err
                    );
                    self.invalidate_reader();
                    std::thread::sleep(std::time::Duration::from_millis(
                        COMMIT_RETRY_BACKOFF_MS * (1 << attempt),
                    ));
                }
            }
        }
        Err(format!(
            "BM25 提交失败(重试 {} 次后仍失败): {}",
            COMMIT_MAX_ATTEMPTS, last_err
        ))
    }

    /// 单次写入+提交（失败由 add_documents 决定是否重试）
    fn try_add_documents(&self, chunks: &[DocumentChunk]) -> Result<(), String> {
        let index = self.open_index()?;
        let schema = Self::schema();
        let doc_id_field = schema.get_field("doc_id").unwrap();
        let doc_name_field = schema.get_field("doc_name").unwrap();
        let file_path_field = schema.get_field("file_path").unwrap();
        let chunk_index_field = schema.get_field("chunk_index").unwrap();
        let text_field = schema.get_field("text").unwrap();
        let display_text_field = schema.get_field("display_text").unwrap();
        let title_field = schema.get_field("title").unwrap();
        let heading_field = schema.get_field("heading").unwrap();
        let symbol_name_field = schema.get_field("symbol_name").unwrap();
        let symbol_kind_field = schema.get_field("symbol_kind").unwrap();
        let chunk_type_field = schema.get_field("chunk_type").unwrap();

        let mut writer = index
            .writer(50_000_000)
            .map_err(|e| format!("创建 BM25 writer 失败: {}", e))?;

        for chunk in chunks {
            // text 仅索引纯正文（B5），展示文本单独存 display_text
            let body = body_text(chunk);
            writer
                .add_document(doc!(
                    doc_id_field => chunk.id.as_str(),
                    doc_name_field => chunk.doc_name.as_str(),
                    file_path_field => chunk.doc_name.as_str(),
                    chunk_index_field => chunk.chunk_index as u64,
                    text_field => body.as_str(),
                    display_text_field => chunk.text.as_str(),
                    title_field => file_title(&chunk.doc_name).as_str(),
                    heading_field => heading_text(&chunk.path_json).as_str(),
                    symbol_name_field => chunk.symbol_name.as_deref().unwrap_or(""),
                    symbol_kind_field => chunk.symbol_kind.as_deref().unwrap_or(""),
                    chunk_type_field => chunk.chunk_type.as_deref().unwrap_or(""),
                ))
                .map_err(|e| format!("添加文档到 BM25 失败: {}", e))?;
        }

        // 防御性重注册：确保 commit 时 tantivy writer 能获取到 jieba_chinese 分词器
        Self::register_tokenizers(&index);

        writer
            .commit()
            .map_err(|e| format!("BM25 提交失败: {}", e))?;

        self.invalidate_reader();
        Ok(())
    }

    /// 关键词检索（BM25 分数，**minimum_should_match 严格语义**，供混合检索管线使用）。
    ///
    /// 解决"查询出许多不相关文档"的核心缺陷：QueryParser 默认词间 OR，
    /// 长查询只命中一个词的低相关文档也会被召回。本方法改为：
    /// - 查询词经 jieba（中文）+ 标识符（英文/下划线）分词、停用词过滤
    /// - 每个词在 text/title/heading/symbol/file_path 字段任一命中即算该词命中（保留 Field Boost）
    /// - 词间必须满足 `msm_ratio` 比例的词命中（默认 0.6）才进入候选
    /// - 查询词过少（≤1）或分词为空时退化为宽松 OR（QueryParser），避免收窄过度
    pub fn search_with_plan(
        &self,
        query_str: &str,
        top_k: u32,
        msm_ratio: f32,
    ) -> Result<Vec<SearchHit>, String> {
        let index = self.open_index()?;
        let reader = self.get_reader()?;
        let searcher = reader.searcher();

        let terms = segment_query_terms(query_str);
        let query: Box<dyn Query> = if terms.len() <= 1 {
            // 单/零词：退化 OR，避免 msm 导致"仅有的一个词"反而被过滤
            let parser = Self::build_query_parser(&index);
            parser
                .parse_query(&escape_query(query_str))
                .map_err(|e| format!("解析 BM25 查询失败: {}", e))?
        } else {
            Self::build_msm_query(&terms, msm_ratio)
        };

        Self::collect_hits(&searcher, query, top_k)
    }

    /// 构建带 Field Boost 的 QueryParser（title > heading > symbol > file_path > text）。
    fn build_query_parser(index: &Index) -> QueryParser {
        let schema = Self::schema();
        let text_field = schema.get_field("text").unwrap();
        let title_field = schema.get_field("title").unwrap();
        let heading_field = schema.get_field("heading").unwrap();
        let symbol_name_field = schema.get_field("symbol_name").unwrap();
        let file_path_field = schema.get_field("file_path").unwrap();

        let mut parser = QueryParser::for_index(
            index,
            vec![
                text_field,
                title_field,
                heading_field,
                symbol_name_field,
                file_path_field,
            ],
        );
        parser.set_field_boost(text_field, 1.0);
        parser.set_field_boost(title_field, 3.0);
        parser.set_field_boost(heading_field, 2.5);
        parser.set_field_boost(symbol_name_field, 2.0);
        parser.set_field_boost(file_path_field, 1.0);
        parser
    }

    /// 构建 minimum_should_match 语义的 BooleanQuery。
    ///
    /// 结构：`OR(词1, 词2, ..., 词n)`，其中每个词 = `OR(text/title/heading/symbol/path 的 TermQuery)`
    /// 且任一字段命中即算该词命中；词间最低命中数 = `ceil(n * msm_ratio)`。
    fn build_msm_query(terms: &[String], msm_ratio: f32) -> Box<dyn Query> {
        let schema = Self::schema();
        let fields_with_boost: Vec<(Field, f32)> = vec![
            (schema.get_field("text").unwrap(), 1.0),
            (schema.get_field("title").unwrap(), 3.0),
            (schema.get_field("heading").unwrap(), 2.5),
            (schema.get_field("symbol_name").unwrap(), 2.0),
            (schema.get_field("file_path").unwrap(), 1.0),
        ];

        // 词间最低命中数（1..=n，避免 msm_ratio=0 时仍要求 0 个词导致全召回）
        let min_should = ((terms.len() as f32) * msm_ratio.max(0.0)).ceil().max(1.0) as u32;

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(terms.len());
        for term in terms {
            let mut sub: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(fields_with_boost.len());
            for (field, boost) in &fields_with_boost {
                let tq = TermQuery::new(
                    Term::from_field_text(*field, term),
                    IndexRecordOption::WithFreqsAndPositions,
                );
                sub.push((Occur::Should, Box::new(BoostQuery::new(Box::new(tq), *boost))));
            }
            clauses.push((Occur::Should, Box::new(BooleanQuery::new(sub))));
        }

        log::info!(
            "[bm25] msm 查询构建: terms={:?} min_should={}",
            terms,
            min_should
        );
        let mut bq = BooleanQuery::new(clauses);
        bq.set_minimum_number_should_match(min_should as usize);
        Box::new(bq)
    }

    /// 执行查询并收集结果（公共逻辑：读取 doc 字段 + BM25 分数归一化到 [0,1]）。
    fn collect_hits(
        searcher: &Searcher,
        query: Box<dyn Query>,
        top_k: u32,
    ) -> Result<Vec<SearchHit>, String> {
        let schema = Self::schema();
        let text_field = schema.get_field("text").unwrap();
        let display_text_field = schema.get_field("display_text").unwrap();
        let doc_name_field = schema.get_field("doc_name").unwrap();
        let chunk_index_field = schema.get_field("chunk_index").unwrap();
        let symbol_name_field = schema.get_field("symbol_name").unwrap();
        let symbol_kind_field = schema.get_field("symbol_kind").unwrap();
        let chunk_type_field = schema.get_field("chunk_type").unwrap();

        let collector = TopDocs::with_limit(top_k as usize).order_by_score();
        let top_docs = searcher
            .search(&query, &collector)
            .map_err(|e| format!("BM25 检索失败: {}", e))?;

        let mut hits = Vec::with_capacity(top_docs.len());
        let mut raw_scores: Vec<f32> = Vec::with_capacity(top_docs.len());

        // 第一遍：收集原始分数和文档数据
        for (score, doc_address) in &top_docs {
            let doc = searcher
                .doc::<TantivyDocument>(*doc_address)
                .map_err(|e| format!("读取 BM25 文档失败: {}", e))?;

            let doc_name = doc
                .get_first(doc_name_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let text = doc
                .get_first(display_text_field)
                .and_then(|v| v.as_str())
                .or_else(|| {
                    // 旧索引（无 display_text）回退到 text 字段，避免读到纯正文而丢失标题上下文
                    doc.get_first(text_field).and_then(|v| v.as_str())
                })
                .unwrap_or("")
                .to_string();
            let chunk_index = doc
                .get_first(chunk_index_field)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let symbol_name = doc
                .get_first(symbol_name_field)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let symbol_kind = doc
                .get_first(symbol_kind_field)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let chunk_type = doc
                .get_first(chunk_type_field)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());

            raw_scores.push(*score as f32);
            hits.push(SearchHit {
                text,
                doc_name,
                chunk_index,
                score: 0.0, // 占位，第二遍归一化后填入
                score_vec: 0.0,
                score_bm25: 0.0,
                path_json: None,
                sentence_window: None,
                symbol_name,
                symbol_kind,
                chunk_type,
                score_rerank: None,
                query_sources: Vec::new(),
            });
        }

        // 第二遍：将 BM25 分数归一化到 [0, 1]，基于最高分动态缩放
        let max_score = raw_scores.iter().cloned().fold(0.0f32, f32::max);
        let normalize_factor = if max_score > 0.0 { 1.0 / max_score } else { 0.0 };
        for (i, hit) in hits.iter_mut().enumerate() {
            let normalized = (raw_scores[i] * normalize_factor).min(1.0);
            hit.score = normalized;
            hit.score_bm25 = normalized;
        }

        Ok(hits)
    }

    /// 删除指定文档的所有块（解决 C3：通过 doc_name 精确删除）
    ///
    /// 返回实际删除的文档（chunk）数，调用方可据此判断是否真的删除了数据，
    /// 避免对从未索引的文档误更新元数据。
    pub fn delete_document(&self, doc_name: &str) -> Result<usize, String> {
        // 与 add_documents 相同：串行化写入 + 先释放 reader 句柄，commit 失败按退避重试
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.invalidate_reader();
        let mut last_err = String::new();
        for attempt in 0..COMMIT_MAX_ATTEMPTS {
            match self.try_delete_document(doc_name) {
                Ok(n) => return Ok(n),
                Err(e) => {
                    last_err = e;
                    if attempt + 1 >= COMMIT_MAX_ATTEMPTS {
                        break;
                    }
                    log::warn!(
                        "[bm25] 删除提交失败（第 {} 次），释放句柄后重试: {}",
                        attempt + 1,
                        last_err
                    );
                    self.invalidate_reader();
                    std::thread::sleep(std::time::Duration::from_millis(
                        COMMIT_RETRY_BACKOFF_MS * (1 << attempt),
                    ));
                }
            }
        }
        Err(format!(
            "BM25 提交删除失败(重试 {} 次后仍失败): {}",
            COMMIT_MAX_ATTEMPTS, last_err
        ))
    }

    /// 单次删除+提交（失败由 delete_document 决定是否重试）
    fn try_delete_document(&self, doc_name: &str) -> Result<usize, String> {
        let index = self.open_index()?;
        let schema = Self::schema();
        let doc_name_field = schema.get_field("doc_name").unwrap();
        let term = tantivy::Term::from_field_text(doc_name_field, doc_name);

        // 先统计匹配文档数——`writer.delete_term()` 返回的是 **Opstamp（操作戳）**
        // 而非删除文档数（tantivy 语义）。原实现误把操作戳当「删除条数」返回，
        // 调用方（remove_file）据此把 chunk_delta 扣成整个索引的操作戳数量，
        // 导致「删除一个文件 → chunk_count/vector_count 元数据被清零、整个知识库
        // 显示被清空」。用 Count collector 在删除前统计 doc_name 精确匹配的真实数量。
        let term_query = TermQuery::new(term.clone(), IndexRecordOption::Basic);
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|e| format!("打开 BM25 读取器失败: {}", e))?;
        let searcher = reader.searcher();
        let matched = searcher
            .search(&term_query, &Count)
            .map_err(|e| format!("统计 BM25 待删除文档数失败: {}", e))?;

        let mut writer: tantivy::IndexWriter<TantivyDocument> = index
            .writer(50_000_000)
            .map_err(|e| format!("创建 BM25 writer 失败: {}", e))?;

        // Tantivy 使用 term 删除（doc_name 精确匹配，仅删除该文档的 chunk）
        writer.delete_term(term);
        writer
            .commit()
            .map_err(|e| format!("BM25 提交删除失败: {}", e))?;

        self.invalidate_reader();
        if matched > 0 {
            log::info!("[bm25] 删除文档 '{}': 删除了 {} 条", doc_name, matched);
        }
        Ok(matched)
    }

    /// 清空索引
    ///
    /// 直接删除整个目录再重建，比逐文件删除快得多。
    pub fn clear(&self) -> Result<(), String> {
        // 串行化写入：防止与 add_documents / delete_document 并发操作同一索引目录
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        // 先释放 reader 的 mmap 句柄，避免 Windows 下 remove_dir_all 删除被占用文件
        self.invalidate_reader();
        let path = Path::new(&self.index_path);
        if path.exists() {
            fs::remove_dir_all(path).map_err(|e| format!("删除 BM25 目录失败: {}", e))?;
        }
        fs::create_dir_all(path).map_err(|e| format!("重建 BM25 目录失败: {}", e))?;
        let schema = Self::schema();
        Index::create_in_dir(&self.index_path, schema)
            .map_err(|e| format!("重建 BM25 索引失败: {}", e))?;
        // 重建后写回 schema 版本标记（与 create 保持一致）
        fs::write(path.join(SCHEMA_MARKER), SCHEMA_VERSION)
            .map_err(|e| format!("写入 BM25 schema 版本标记失败: {}", e))?;
        self.invalidate_reader();
        Ok(())
    }
}

/// 查询词切分：jieba 中文切词 + 英文/数字 token 化 + 停用词过滤 + 去重。
///
/// 供 [`Bm25Index::search_with_plan`] 使用，与索引侧分词器对齐：
/// - CJK 文本 → jieba 词（与 text/title/heading 字段一致）
/// - ASCII 连续字母数字 → 单个 token（与 text 字段及 symbol 字段小写化一致）
/// - 过滤中英文常见停用词，避免"的/是/the/and"等虚词占据 msm 命中配额
///
/// 下划线标识符（如 `lru_cache`）会被拆为 `lru`/`cache`：符号名整体匹配
/// 由符号路召回（`QueryPlan.symbols`）兜底，此处保持与 text 字段切分一致。
fn segment_query_terms(query: &str) -> Vec<String> {
    let jieba = JIEBA.get_or_init(|| {
        log::info!("[bm25] 初始化 Jieba 中文分词...");
        jieba_rs::Jieba::new()
    });
    let mut terms: Vec<String> = Vec::new();
    for (text, _, _) in segment_text(query, jieba) {
        let t = text.trim();
        if t.is_empty() || is_stopword(t) {
            continue;
        }
        // 索引侧分词器（chinese_text_analyzer / identifier_analyzer）均挂 LowerCaser，
        // TermQuery 必须用小写词才能命中（如 "ParseJSON" → "parsejson"）；CJK 词不受影响。
        let lower = t.to_lowercase();
        if !terms.iter().any(|s| s == &lower) {
            terms.push(lower);
        }
    }
    terms
}

/// 中英文常见停用词（保守清单：仅过滤无实义的虚词，避免误伤技术词汇）。
fn is_stopword(term: &str) -> bool {
    const STOPWORDS: &[&str] = &[
        // 中文虚词
        "的", "了", "是", "在", "和", "与", "及", "或", "并", "这", "那", "而", "之", "也", "都",
        "就", "很", "又", "被", "把", "对", "为", "于", "以", "其", "如", "若",
        // 英文虚词（小写化后比较）
        "a", "an", "the", "and", "or", "of", "to", "in", "on", "for", "is", "are", "was", "were",
        "be", "been", "with", "as", "at", "by", "it", "its", "this", "that", "these", "those",
        "from", "into", "about", "which", "what", "how", "when", "where", "why", "who",
        "can", "could", "will", "would", "should", "may", "might", "must", "not", "no", "yes",
    ];
    STOPWORDS.contains(&term.to_lowercase().as_str())
}

/// 转义 Tantivy QueryParser 特殊字符，防止用户输入含特殊符号时解析失败。
fn escape_query(input: &str) -> String {
    let special_chars = [
        '+', '-', '&', '|', '!', '(', ')', '{', '}', '[', ']', '^', '"', '~', '*', '?', ':', '\\',
        '/',
    ];
    let mut output = String::with_capacity(input.len());
    for c in input.chars() {
        if special_chars.contains(&c) {
            output.push('\\');
        }
        output.push(c);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &str, doc_name: &str, idx: u32, text: &str) -> DocumentChunk {
        DocumentChunk {
            id: id.to_string(),
            doc_name: doc_name.to_string(),
            chunk_index: idx,
            text: text.to_string(),
            path_depth: None,
            path_json: None,
            sentence_window: None,
            symbol_name: None,
            symbol_kind: None,
            embedding_text: None,
            chunk_type: None,
        }
    }

    /// P1 回归：`delete_document` 必须返回**真实删除的文档（chunk）数**，
    /// 而非 `writer.delete_term()` 的 Opstamp（操作戳）——原实现把操作戳当
    /// 「删除条数」返回，导致单文件删除时 chunk_delta 元数据被扣成 0、
    /// 整个知识库显示被清空。
    #[test]
    fn delete_document_returns_real_count_not_opstamp() {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let idx = Bm25Index::create(&dir.path().to_string_lossy()).expect("创建 BM25 索引失败");

        // 两个文档：a.txt 3 个 chunk、b.txt 2 个 chunk
        let chunks = vec![
            chunk("a1", "a.txt", 0, "Alpha 第一段内容"),
            chunk("a2", "a.txt", 1, "Alpha 第二段内容"),
            chunk("a3", "a.txt", 2, "Alpha 第三段内容"),
            chunk("b1", "b.txt", 0, "Beta 唯一段"),
            chunk("b2", "b.txt", 1, "Beta 第二段"),
        ];
        idx.add_documents(&chunks).expect("写入 BM25 失败");

        // 删除 a.txt：必须只返回 3（a.txt 的 chunk 数），绝不是索引操作戳（≥5）
        let deleted = idx.delete_document("a.txt").expect("删除 a.txt 失败");
        assert_eq!(deleted, 3, "delete_document 应返回该文档的真实 chunk 数，而非操作戳");

        // b.txt 不受影响：仍可精确删除，返回 2
        let deleted_b = idx.delete_document("b.txt").expect("删除 b.txt 失败");
        assert_eq!(deleted_b, 2, "b.txt 的 chunk 应完整保留并可精确删除");

        // 已删除/不存在的文档返回 0（幂等，不污染元数据）
        let again = idx.delete_document("a.txt").expect("重复删除应成功");
        assert_eq!(again, 0, "已删除文档再次删除应返回 0");
        let missing = idx.delete_document("never-existed.txt").expect("删除不存在文档应成功");
        assert_eq!(missing, 0, "不存在文档删除应返回 0");
    }

    /// 删除只影响目标文档：同库其他文档的检索仍命中（doc_name 精确匹配语义）。
    #[test]
    fn delete_document_leaves_other_docs_searchable() {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let idx = Bm25Index::create(&dir.path().to_string_lossy()).expect("创建 BM25 索引失败");
        idx.add_documents(&[
            chunk("x1", "keep.txt", 0, "保留文档内容 独特词汇甲"),
            chunk("y1", "drop.txt", 0, "被删文档内容 独特词汇乙"),
        ])
        .expect("写入 BM25 失败");

        assert_eq!(idx.delete_document("drop.txt").unwrap(), 1);
        // 删除后 keep.txt 仍可检索
        let hits = idx
            .search_with_plan("独特词汇甲", 5, 0.6)
            .expect("检索失败");
        assert!(
            hits.iter().any(|h| h.doc_name == "keep.txt"),
            "删除 drop.txt 后 keep.txt 必须仍可检索（不能被连带删除）"
        );
        assert!(
            !hits.iter().any(|h| h.doc_name == "drop.txt"),
            "drop.txt 的 chunk 必须已被删除"
        );
    }
}

