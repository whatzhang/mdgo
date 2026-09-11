//! 转换结果持久缓存（Plan B v2 / Phase 3）。
//!
//! # 目标
//!
//! **全量重建不再重复解析 PDF / Office**。pdf-inspector 单篇约 200ms、anydoc 单文件数 ms，
//! 单看一次不显眼，但 `kb_index` 每次全量重建都要重跑全部文件——配合 embedding 缓存
//! （`db/embedding_cache.rs`）后，本缓存让"二次重建"接近纯写库的成本。
//!
//! # 键设计（方案 §5.6，按评审意见从 `mtime+size` 改为**内容哈希**）
//!
//! ```text
//! PRIMARY KEY (source_hash, converter_id, converter_version, options_hash)
//! ```
//!
//! - `source_hash` = **SHA-256(文件字节)**。用内容哈希而非 `mtime+size` 的理由很直接：
//!   两个转换器入口本来就要求**整份字节在手**（`to_markdown_bytes` /
//!   `extract_pages_markdown_mem`），因此哈希成本相对转换几乎免费，没有理由接受
//!   "内容改回同尺寸 + mtime 被恢复"的误命中；
//! - `converter_id` + `converter_version` 进主键 → **转换器升级自然失效**，无需人工清缓存；
//! - `options_hash` 预留（当前转换无可调项）。
//!
//! # 与 `EmbeddingCache` 的关系
//!
//! 同构：`{dir}/.mdgo/*.sqlite`、`Mutex<Connection>`、`open_shared` 进程级复用、
//! 按 `created_at` 最旧裁剪、失败只告警不阻断索引。
//!
//! **但哈希算法刻意不同**：embedding 缓存继续用 FNV-1a 128（`db/utils::stable_hash_hex`）——
//! 那是"缓存正确性不依赖人工失效"的既有契约，换算法会让全部旧缓存失效；本缓存的
//! **来源身份**按方案要求用 SHA-256。
//!
//! # 与"唯一入口"契约的关系
//!
//! 本模块**不实现转换**：命中缓存则直接还原 [`DocumentSource`]，未命中则调用
//! `document::loader::load_document_bytes`。因此 `DocumentLoader` 仍是唯一转换点，
//! 缓存只是它前面的一层透明加速。

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::core::document::filekind::{self, Converter};
use crate::core::document::loader::{
    self, ConverterInfo, DocStatus, DocumentSource, LineSpan, PageDiagnostic, PageSpan, SkipReason,
};

/// 缓存最大条目数（PDF Markdown 体积远大于向量，取比 embedding 缓存更保守的值）
const CACHE_MAX_ENTRIES: usize = 20_000;

/// 转换选项指纹（当前转换器无可调项 → 常量；将来加选项时改这里）
const OPTIONS_HASH: &str = "default";

/// 可缓存的载荷（`DocumentSource` 中**由转换决定**的部分）。
///
/// 其余字段（`form`/`source_kind`/`frontmatter`）由注册表决定、与转换无关，
/// 因此不入缓存（注册表变化由 `REGISTRY_VERSION` 走版本失效）。
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CachedPayload {
    text: String,
    /// `(page, byte_start, byte_end)`
    #[serde(default)]
    page_spans: Vec<(u32, usize, usize)>,
    /// `(line_start, line_end, page)`
    #[serde(default)]
    line_page_map: Vec<(usize, usize, u32)>,
    /// 部分索引时被跳过的页（Q8）
    #[serde(default)]
    skipped_pages: Vec<u32>,
    /// `(page, code, detail)`
    #[serde(default)]
    diagnostics: Vec<(u32, String, String)>,
    #[serde(default)]
    warnings: Vec<String>,
}

impl CachedPayload {
    fn from_source(src: &DocumentSource) -> Self {
        Self {
            text: src.text.clone(),
            page_spans: src
                .page_spans
                .iter()
                .map(|s| (s.page, s.byte_start, s.byte_end))
                .collect(),
            line_page_map: src
                .line_page_map
                .iter()
                .map(|s| (s.line_start, s.line_end, s.page))
                .collect(),
            skipped_pages: match &src.doc_status {
                DocStatus::Indexed => Vec::new(),
                DocStatus::PartiallyIndexed { skipped_pages } => skipped_pages.clone(),
            },
            diagnostics: src
                .page_diagnostics
                .iter()
                .map(|d| (d.page, d.code.to_string(), d.detail.clone()))
                .collect(),
            warnings: src.warnings.clone(),
        }
    }

    /// 还原为 `DocumentSource`（形态/来源键来自注册表，转换器身份由调用方给出）
    fn into_source(self, rel_path: &str, kind: &filekind::FileKind, converter: ConverterInfo) -> DocumentSource {
        DocumentSource {
            rel_path: rel_path.to_string(),
            text: self.text,
            form: kind.form,
            frontmatter: kind.caps.doc_like && kind.form == filekind::DocumentForm::Markdown,
            source_kind: kind.caps.source_kind,
            page_spans: self
                .page_spans
                .into_iter()
                .map(|(page, byte_start, byte_end)| PageSpan { page, byte_start, byte_end })
                .collect(),
            line_page_map: self
                .line_page_map
                .into_iter()
                .map(|(line_start, line_end, page)| LineSpan { line_start, line_end, page })
                .collect(),
            converter,
            doc_status: if self.skipped_pages.is_empty() {
                DocStatus::Indexed
            } else {
                DocStatus::PartiallyIndexed { skipped_pages: self.skipped_pages }
            },
            page_diagnostics: self
                .diagnostics
                .into_iter()
                .map(|(page, code, detail)| PageDiagnostic {
                    page,
                    // code 由本模块从 &'static str 序列化而来；未知值退化为 "needs_ocr"
                    code: match code.as_str() {
                        "needs_ocr" => "needs_ocr",
                        "suspected_garbled_text" => "suspected_garbled_text",
                        "vector_text" => "vector_text",
                        "no_text" => "no_text",
                        _ => "needs_ocr",
                    },
                    detail,
                })
                .collect(),
            warnings: self.warnings,
        }
    }
}

/// 转换结果缓存
pub struct ConversionCache {
    conn: Mutex<Connection>,
}

impl ConversionCache {
    /// 打开（或创建）缓存；`dir` 不存在时自动创建
    pub fn open(dir: &str) -> Result<Self, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("创建转换缓存目录失败 ({}): {}", dir, e))?;
        let path = Path::new(dir).join("conversion_cache.sqlite");
        let conn = Connection::open(&path)
            .map_err(|e| format!("打开转换缓存失败 ({}): {}", path.display(), e))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS conversion_cache (
                source_hash       TEXT NOT NULL,
                converter_id      TEXT NOT NULL,
                converter_version TEXT NOT NULL,
                options_hash      TEXT NOT NULL,
                payload           BLOB NOT NULL,
                created_at        INTEGER NOT NULL,
                PRIMARY KEY (source_hash, converter_id, converter_version, options_hash)
            );
            CREATE INDEX IF NOT EXISTS idx_conversion_cache_created
                ON conversion_cache(created_at);",
        )
        .map_err(|e| format!("初始化转换缓存表失败: {}", e))?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// 按目录复用的进程级缓存连接（与 `EmbeddingCache::open_shared` 同范式）
    pub fn open_shared(dir: &str) -> Result<Arc<Self>, String> {
        use std::collections::HashMap as Map;
        use std::sync::OnceLock;
        static REGISTRY: OnceLock<Mutex<Map<String, Arc<ConversionCache>>>> = OnceLock::new();
        let registry = REGISTRY.get_or_init(|| Mutex::new(Map::new()));
        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = guard.get(dir) {
            return Ok(Arc::clone(c));
        }
        let cache = Arc::new(Self::open(dir)?);
        guard.insert(dir.to_string(), Arc::clone(&cache));
        Ok(cache)
    }

    /// 内容哈希：SHA-256(文件字节) 十六进制（小写，64 字符）
    pub fn source_hash(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        // sha2 0.11 的 `finalize()` 返回 hybrid-array（未实现 LowerHex），故手动编码
        let mut out = String::with_capacity(64);
        for b in digest.iter() {
            out.push_str(&format!("{:02x}", b));
        }
        out
    }

    /// 组装主键四元组
    fn key_parts(bytes: &[u8], converter: ConverterInfo) -> (String, String, String, String) {
        (
            Self::source_hash(bytes),
            converter.id.to_string(),
            converter.version.to_string(),
            OPTIONS_HASH.to_string(),
        )
    }

    fn get(&self, key: &(String, String, String, String)) -> Option<CachedPayload> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn
            .prepare_cached(
                "SELECT payload FROM conversion_cache
                 WHERE source_hash=?1 AND converter_id=?2 AND converter_version=?3 AND options_hash=?4",
            )
            .ok()?;
        let blob: Vec<u8> = stmt
            .query_row(
                rusqlite::params![key.0, key.1, key.2, key.3],
                |row| row.get(0),
            )
            .ok()?;
        serde_json::from_slice(&blob).ok()
    }

    fn put(&self, key: &(String, String, String, String), payload: &CachedPayload) {
        let Ok(blob) = serde_json::to_vec(payload) else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let res = conn.execute(
            "INSERT OR REPLACE INTO conversion_cache
             (source_hash, converter_id, converter_version, options_hash, payload, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![key.0, key.1, key.2, key.3, blob, now],
        );
        if let Err(e) = res {
            log::warn!("[conversion_cache] 写入失败（不影响本次索引）: {}", e);
            return;
        }
        // 超限按 created_at 最旧裁剪（与 embedding 缓存同策略）
        let _ = conn.execute(
            "DELETE FROM conversion_cache WHERE rowid IN (
                SELECT rowid FROM conversion_cache ORDER BY created_at ASC
                LIMIT (SELECT CASE WHEN COUNT(*) > ?1 THEN COUNT(*) - ?1 ELSE 0 END
                       FROM conversion_cache)
             )",
            rusqlite::params![CACHE_MAX_ENTRIES as i64],
        );
    }

    /// **缓存优先装载**：命中直接还原，未命中调用 [`loader::load_document_bytes`] 并写回。
    ///
    /// 只对**需要转换**的来源启用（`PdfInspector` / `AnyDoc` / `LegacyPdf`）：
    /// 纯文本直读本身极快，哈希反而增加开销。
    pub fn load_or_convert(&self, abs_path: &Path, rel_path: &str) -> Result<DocumentSource, SkipReason> {
        let kind = match filekind::lookup(rel_path) {
            Some(k) => k,
            None => return loader::load_document(abs_path, rel_path),
        };
        let converter = match kind.converter {
            Converter::PdfInspector => ConverterInfo::PDF_INSPECTOR,
            Converter::AnyDoc => ConverterInfo::ANYDOC,
            Converter::LegacyPdf => ConverterInfo::PDF_EXTRACT,
            // 直读路径不缓存
            Converter::Plain => return loader::load_document(abs_path, rel_path),
        };

        let bytes = match std::fs::read(abs_path) {
            Ok(b) => b,
            Err(e) => return Err(SkipReason::Io { detail: e.to_string() }),
        };
        let key = Self::key_parts(&bytes, converter);
        if let Some(payload) = self.get(&key) {
            log::debug!("[conversion_cache] 命中: {} ({})", rel_path, converter);
            return Ok(payload.into_source(rel_path, kind, converter));
        }
        // 未命中：走唯一转换点（用已读入的字节，避免二次读盘）
        let src = loader::load_document_bytes(abs_path, rel_path, &bytes)?;
        self.put(&key, &CachedPayload::from_source(&src));
        Ok(src)
    }

    /// 当前缓存条目数（测试/诊断用）
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row("SELECT COUNT(*) FROM conversion_cache", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize
    }
}

// ──────────────────────────── 测试 ────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> String {
        let d = std::env::temp_dir().join(format!("mdgo_conv_cache_{}", name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("建临时目录");
        d.to_string_lossy().to_string()
    }

    /// 内容哈希：同内容同值、异内容异值、且为 64 位小写十六进制（SHA-256）
    #[test]
    fn source_hash_is_sha256_hex() {
        let a = ConversionCache::source_hash(b"hello");
        let b = ConversionCache::source_hash(b"hello");
        let c = ConversionCache::source_hash(b"hello!");
        assert_eq!(a, b, "同内容必须同哈希");
        assert_ne!(a, c, "内容变化必须改变哈希");
        assert_eq!(a.len(), 64, "SHA-256 十六进制长度应为 64");
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));
    }

    /// 主键含转换器身份 → 转换器升级自然失效（无需人工清缓存）
    #[test]
    fn key_includes_converter_identity() {
        let bytes = b"same bytes";
        let k1 = ConversionCache::key_parts(bytes, ConverterInfo::PDF_EXTRACT);
        let k2 = ConversionCache::key_parts(bytes, ConverterInfo::PDF_INSPECTOR);
        assert_ne!(k1, k2, "换转换器必须换键");
        assert_eq!(k1.0, k2.0, "source_hash 部分相同（同一份字节）");
        assert_eq!(k1.1, "pdf-extract");
        assert_eq!(k2.1, "pdf-inspector");
    }

    /// 命中路径：写入 → 读出 → 还原为等价 DocumentSource（含页 provenance）
    #[test]
    fn payload_round_trip_preserves_provenance() {
        let dir = tmp_dir("roundtrip");
        let cache = ConversionCache::open(&dir).expect("打开缓存");
        let bytes = b"content-for-roundtrip";
        let key = ConversionCache::key_parts(bytes, ConverterInfo::PDF_INSPECTOR);

        let payload = CachedPayload {
            text: "第一页内容\n\n第二页内容".to_string(),
            page_spans: vec![(1, 0, 12), (2, 14, 26)],
            line_page_map: vec![(1, 2, 1), (3, 4, 2)],
            skipped_pages: vec![3],
            diagnostics: vec![(3, "needs_ocr".to_string(), "该页无可提取文本".to_string())],
            warnings: vec!["版面较复杂".to_string()],
        };
        cache.put(&key, &payload);

        let got = cache.get(&key).expect("应命中");
        let kind = filekind::registry().lookup_ext("pdf").expect("pdf 在册");
        let src = got.into_source("docs/a.pdf", kind, ConverterInfo::PDF_INSPECTOR);

        assert_eq!(src.text, payload.text);
        assert_eq!(src.page_spans.len(), 2);
        assert_eq!(src.page_spans[1].page, 2, "页号必须保持 1-indexed");
        assert_eq!(src.line_page_map.len(), 2);
        assert!(src.line_page_map[0].contains_line(1));
        assert_eq!(
            src.doc_status,
            DocStatus::PartiallyIndexed { skipped_pages: vec![3] },
            "部分索引状态必须还原"
        );
        assert_eq!(src.page_diagnostics.len(), 1);
        assert_eq!(src.page_diagnostics[0].code, "needs_ocr");
        assert_eq!(src.converter, ConverterInfo::PDF_INSPECTOR);
        assert_eq!(src.source_kind, "pdf");
        assert_eq!(src.form, filekind::DocumentForm::Markdown);
    }

    /// 未命中返回 None（键不匹配的任何维度）
    #[test]
    fn miss_when_any_key_part_differs() {
        let dir = tmp_dir("miss");
        let cache = ConversionCache::open(&dir).expect("打开缓存");
        let key = ConversionCache::key_parts(b"x", ConverterInfo::ANYDOC);
        cache.put(
            &key,
            &CachedPayload {
                text: "t".into(),
                page_spans: vec![],
                line_page_map: vec![],
                skipped_pages: vec![],
                diagnostics: vec![],
                warnings: vec![],
            },
        );
        assert!(cache.get(&key).is_some());
        // 换内容
        let other = ConversionCache::key_parts(b"y", ConverterInfo::ANYDOC);
        assert!(cache.get(&other).is_none(), "内容不同不应命中");
        // 换转换器
        let other_conv = ConversionCache::key_parts(b"x", ConverterInfo::PDF_INSPECTOR);
        assert!(cache.get(&other_conv).is_none(), "转换器不同不应命中");
    }

    /// **端到端**：真实 PDF 首次转换写缓存，二次命中且结果一致（不重复解析）
    #[test]
    fn pdf_conversion_is_cached_and_equivalent() {
        use crate::core::document::loader::MIN_DOC_BYTES;
        let dir = tmp_dir("pdf");
        let cache = ConversionCache::open(&dir).expect("打开缓存");

        // 复用 loader 测试里的最小 PDF 构造器（同 crate 测试可见）
        let bytes = crate::core::document::loader::minimal_pdf("Cache me once");
        let file = std::path::Path::new(&dir).join("cached.pdf");
        std::fs::write(&file, &bytes).expect("写 PDF");
        assert!(bytes.len() > MIN_DOC_BYTES);

        let first = cache.load_or_convert(&file, "docs/cached.pdf").expect("首次转换");
        assert_eq!(cache.len(), 1, "首次应写入一条缓存");
        assert!(first.text.contains("Cache me once"));

        let second = cache.load_or_convert(&file, "docs/cached.pdf").expect("二次读取");
        assert_eq!(cache.len(), 1, "二次不应新增条目（命中）");
        assert_eq!(second.text, first.text, "命中结果必须与首次一致");
        assert_eq!(second.converter, first.converter);
        assert_eq!(
            second.page_spans.len(),
            first.page_spans.len(),
            "页 provenance 必须随缓存还原"
        );
        assert_eq!(second.doc_status, first.doc_status);
    }

    /// 未登记 / 直读来源不进缓存（避免无收益的哈希开销）
    #[test]
    fn plain_text_sources_are_not_cached() {
        let dir = tmp_dir("plain");
        let cache = ConversionCache::open(&dir).expect("打开缓存");
        let file = std::path::Path::new(&dir).join("a.md");
        std::fs::write(&file, b"# title\n\nbody paragraph content\n").expect("写 md");
        let src = cache.load_or_convert(&file, "docs/a.md").expect("装载");
        assert_eq!(src.form, filekind::DocumentForm::Markdown);
        assert_eq!(cache.len(), 0, "直读来源不应进入转换缓存");
    }
}
