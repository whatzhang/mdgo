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

/// 中文 Bigram 分词器
///
/// 对中文字符做 2-gram 切分，英文/数字使用 SimpleTokenizer。
/// 无需额外依赖，对中文搜索效果有显著提升。
#[derive(Clone)]
struct ChineseBigramTokenizer;

struct ChineseBigramTokenStream<'a> {
    text: &'a str,
    chars: Vec<char>,
    pos: usize,
    current_token: Token,
}

impl Tokenizer for ChineseBigramTokenizer {
    type TokenStream<'a> = ChineseBigramTokenStream<'a>;

    fn token_stream<'a>(&mut self, text: &'a str) -> Self::TokenStream<'a> {
        let chars: Vec<char> = text.chars().collect();
        ChineseBigramTokenStream {
            text,
            chars,
            pos: 0,
            current_token: Token::default(),
        }
    }
}

impl<'a> TokenStream for ChineseBigramTokenStream<'a> {
    fn advance(&mut self) -> bool {
        while self.pos < self.chars.len() {
            let c = self.chars[self.pos];

            // 中文字符：bigram 切分
            if Self::is_cjk(c) {
                if self.pos + 1 < self.chars.len() && Self::is_cjk(self.chars[self.pos + 1]) {
                    let text: String = self.chars[self.pos..self.pos + 2].iter().collect();
                    let offset_from = self.text.char_indices().nth(self.pos).map(|(i, _)| i).unwrap_or(0);
                    let offset_to = self.text.char_indices().nth(self.pos + 2).map(|(i, _)| i).unwrap_or(self.text.len());
                    self.current_token = Token {
                        offset_from,
                        offset_to,
                        position: self.pos,
                        text,
                        ..Default::default()
                    };
                    self.pos += 1;
                    return true;
                } else {
                    // 单个中文字符（最后一个）
                    let text: String = c.to_string();
                    let offset_from = self.text.char_indices().nth(self.pos).map(|(i, _)| i).unwrap_or(0);
                    let offset_to = self.text.char_indices().nth(self.pos + 1).map(|(i, _)| i).unwrap_or(self.text.len());
                    self.current_token = Token {
                        offset_from,
                        offset_to,
                        position: self.pos,
                        text,
                        ..Default::default()
                    };
                    self.pos += 1;
                    return true;
                }
            }

            // 英文/数字：收集连续的字母数字
            if c.is_alphanumeric() {
                let start = self.pos;
                while self.pos < self.chars.len() && self.chars[self.pos].is_alphanumeric() {
                    self.pos += 1;
                }
                let text: String = self.chars[start..self.pos].iter().collect();
                let offset_from = self.text.char_indices().nth(start).map(|(i, _)| i).unwrap_or(0);
                let offset_to = self.text.char_indices().nth(self.pos).map(|(i, _)| i).unwrap_or(self.text.len());
                self.current_token = Token {
                    offset_from,
                    offset_to,
                    position: start,
                    text,
                    ..Default::default()
                };
                return true;
            }

            // 标点/空白：跳过
            self.pos += 1;
        }
        false
    }

    fn token(&self) -> &Token {
        &self.current_token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.current_token
    }
}

impl<'a> ChineseBigramTokenStream<'a> {
    fn is_cjk(c: char) -> bool {
        matches!(c as u32,
            0x4E00..=0x9FFF   // CJK Unified Ideographs
            | 0x3400..=0x4DBF   // CJK Unified Ideographs Extension A
            | 0x3040..=0x309F   // Hiragana
            | 0x30A0..=0x30FF   // Katakana
            | 0xAC00..=0xD7AF   // Hangul Syllables
        )
    }
}

/// 构建中文 + 英文混合的文本分词器
fn chinese_text_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(ChineseBigramTokenizer)
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
        // text 字段使用中文 bigram 分词器（索引时用，需在 Index 上注册同名 tokenizer）
        let text_options = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("chinese_bigram")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored();
        builder.add_text_field("text", text_options);
        builder.build()
    }

    /// 在 Index 上注册中文分词器（用于 "text" 字段）
    fn register_tokenizers(index: &Index) {
        index
            .tokenizers()
            .register("chinese_bigram", chinese_text_analyzer());
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
        let mut reader_guard = self.reader.lock().unwrap();
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
        let mut reader_guard = self.reader.lock().unwrap();
        *reader_guard = None;
    }

    /// 批量建立全文索引（解决 M3：减少 writer 创建次数）
    pub fn add_documents(&self, chunks: &[DocumentChunk]) -> Result<(), String> {
        if chunks.is_empty() {
            return Ok(());
        }

        let index = self.open_index()?;
        let schema = Self::schema();
        let doc_id_field = schema.get_field("doc_id").unwrap();
        let doc_name_field = schema.get_field("doc_name").unwrap();
        let chunk_index_field = schema.get_field("chunk_index").unwrap();
        let text_field = schema.get_field("text").unwrap();

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
                ))
                .map_err(|e| format!("添加文档到 BM25 失败: {}", e))?;
        }

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

        let reader = self.get_reader()?;
        let searcher = reader.searcher();

        let query_parser = QueryParser::for_index(&index, vec![text_field]);
        let escaped_query = escape_query(query_str);
        let query = query_parser
            .parse_query(&escaped_query)
            .map_err(|e| format!("解析 BM25 查询失败: {}", e))?;

        let collector = TopDocs::with_limit(top_k as usize).order_by_score();
        let top_docs = searcher
            .search(&query, &collector)
            .map_err(|e| format!("BM25 检索失败: {}", e))?;

        let mut hits = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            let doc = searcher
                .doc::<TantivyDocument>(doc_address)
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

            hits.push(SearchHit {
                text,
                doc_name,
                chunk_index,
                score: (score as f32 / 10.0).min(1.0),
            });
        }

        Ok(hits)
    }

    /// 删除指定文档的所有块（解决 C3：通过 doc_name 精确删除）
    pub fn delete_document(&self, doc_name: &str) -> Result<(), String> {
        let index = self.open_index()?;
        let schema = Self::schema();
        let doc_name_field = schema.get_field("doc_name").unwrap();

        let mut writer: tantivy::IndexWriter<TantivyDocument> = index
            .writer(50_000_000)
            .map_err(|e| format!("创建 BM25 writer 失败: {}", e))?;

        // Tantivy 使用 term 删除，返回删除的文档数（u64）
        let term = tantivy::Term::from_field_text(doc_name_field, doc_name);
        let _deleted = writer
            .delete_term(term);

        writer
            .commit()
            .map_err(|e| format!("BM25 提交删除失败: {}", e))?;

        self.invalidate_reader();
        Ok(())
    }

    /// 清空索引
    ///
    /// 直接删除整个目录再重建，比逐文件删除快得多。
    pub fn clear(&self) -> Result<(), String> {
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
