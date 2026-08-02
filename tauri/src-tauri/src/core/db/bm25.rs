use std::fs;
use std::path::Path;
use std::sync::Mutex;

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::*;
use tantivy::tokenizer::{
    LowerCaser, RemoveLongFilter, TextAnalyzer, Token, TokenStream, Tokenizer,
};
use tantivy::{doc, Index, IndexReader, ReloadPolicy, TantivyDocument};

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
            for token in jieba.cut(&cjk_text, false) {
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

pub struct Bm25Index {
    index_path: String,
    reader: Mutex<Option<IndexReader>>,
}

impl Bm25Index {
    fn schema() -> Schema {
        let mut builder = Schema::builder();
        builder.add_text_field("doc_id", STRING | STORED);
        builder.add_text_field("doc_name", STRING | STORED);
        builder.add_u64_field("chunk_index", STORED);
        // text 字段使用 Jieba 中文分词器（索引时用，需在 Index 上注册同名 tokenizer）
        let text_options = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("jieba_chinese")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored();
        builder.add_text_field("text", text_options);
        // 代码符号字段（仅存储，不索引——符号加权在 RRF 融合后做）
        builder.add_text_field("symbol_name", STORED);
        builder.add_text_field("symbol_kind", STORED);
        builder.build()
    }

    /// 在 Index 上注册中文分词器（用于 "text" 字段）
    fn register_tokenizers(index: &Index) {
        index
            .tokenizers()
            .register("jieba_chinese", chinese_text_analyzer());
    }

    /// 打开已有索引
    pub fn open(path: &str) -> Result<Self, String> {
        if !Path::new(path).exists() {
            return Err(format!("BM25 索引目录不存在: {}", path));
        }
        Ok(Self {
            index_path: path.to_string(),
            reader: Mutex::new(None),
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
        Ok(Self {
            index_path: path.to_string(),
            reader: Mutex::new(None),
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
        // 写入前先释放旧 reader 的 mmap 句柄，避免 Windows 下 commit 阶段删除
        // 旧 segment 文件时被占用；commit 偶发失败（文件锁瞬时冲突）时重试一次
        self.invalidate_reader();
        match self.try_add_documents(chunks) {
            Ok(()) => Ok(()),
            Err(e) => {
                log::warn!("[bm25] 提交失败，释放句柄后重试: {}", e);
                self.invalidate_reader();
                std::thread::sleep(std::time::Duration::from_millis(200));
                self.try_add_documents(chunks)
                    .map_err(|e2| format!("BM25 提交失败(重试后仍失败): {}", e2))
            }
        }
    }

    /// 单次写入+提交（失败由 add_documents 决定是否重试）
    fn try_add_documents(&self, chunks: &[DocumentChunk]) -> Result<(), String> {
        let index = self.open_index()?;
        let schema = Self::schema();
        let doc_id_field = schema.get_field("doc_id").unwrap();
        let doc_name_field = schema.get_field("doc_name").unwrap();
        let chunk_index_field = schema.get_field("chunk_index").unwrap();
        let text_field = schema.get_field("text").unwrap();
        let symbol_name_field = schema.get_field("symbol_name").unwrap();
        let symbol_kind_field = schema.get_field("symbol_kind").unwrap();

        let mut writer = index
            .writer(50_000_000)
            .map_err(|e| format!("创建 BM25 writer 失败: {}", e))?;

        for chunk in chunks {
            writer
                .add_document(doc!(
                    doc_id_field => chunk.id.as_str(),
                    doc_name_field => chunk.doc_name.as_str(),
                    chunk_index_field => chunk.chunk_index as u64,
                    text_field => chunk.text.as_str(),
                    symbol_name_field => chunk.symbol_name.as_deref().unwrap_or(""),
                    symbol_kind_field => chunk.symbol_kind.as_deref().unwrap_or(""),
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

    /// 关键词检索（BM25 分数）
    pub fn search(&self, query_str: &str, top_k: u32) -> Result<Vec<SearchHit>, String> {
        let index = self.open_index()?;
        let schema = Self::schema();
        let text_field = schema.get_field("text").unwrap();
        let doc_name_field = schema.get_field("doc_name").unwrap();
        let chunk_index_field = schema.get_field("chunk_index").unwrap();
        let symbol_name_field = schema.get_field("symbol_name").unwrap();
        let symbol_kind_field = schema.get_field("symbol_kind").unwrap();

        let reader = self.get_reader()?;
        let searcher = reader.searcher();

        let mut query_parser = QueryParser::for_index(&index, vec![text_field, doc_name_field]);
        query_parser.set_field_boost(doc_name_field, 2.0);
        let escaped_query = escape_query(query_str);
        let query = query_parser
            .parse_query(&escaped_query)
            .map_err(|e| format!("解析 BM25 查询失败: {}", e))?;

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
                .get_first(text_field)
                .and_then(|v| v.as_str())
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
        // 与 add_documents 相同：先释放 reader 句柄，commit 失败重试一次
        self.invalidate_reader();
        match self.try_delete_document(doc_name) {
            Ok(n) => Ok(n),
            Err(e) => {
                log::warn!("[bm25] 删除提交失败，释放句柄后重试: {}", e);
                self.invalidate_reader();
                std::thread::sleep(std::time::Duration::from_millis(200));
                self.try_delete_document(doc_name)
                    .map_err(|e2| format!("BM25 提交删除失败(重试后仍失败): {}", e2))
            }
        }
    }

    /// 单次删除+提交（失败由 delete_document 决定是否重试）
    fn try_delete_document(&self, doc_name: &str) -> Result<usize, String> {
        let index = self.open_index()?;
        let schema = Self::schema();
        let doc_name_field = schema.get_field("doc_name").unwrap();

        let mut writer: tantivy::IndexWriter<TantivyDocument> = index
            .writer(50_000_000)
            .map_err(|e| format!("创建 BM25 writer 失败: {}", e))?;

        // Tantivy 使用 term 删除，返回删除的文档数
        let term = tantivy::Term::from_field_text(doc_name_field, doc_name);
        let deleted_count = writer.delete_term(term);
        log::debug!("[bm25] 删除文档 '{}': 删除了 {} 条", doc_name, deleted_count);

        writer
            .commit()
            .map_err(|e| format!("BM25 提交删除失败: {}", e))?;

        self.invalidate_reader();
        Ok(deleted_count as usize)
    }

    /// 清空索引
    ///
    /// 直接删除整个目录再重建，比逐文件删除快得多。
    pub fn clear(&self) -> Result<(), String> {
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
        self.invalidate_reader();
        Ok(())
    }
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
