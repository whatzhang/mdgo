//! 文档装载层（`DocumentLoader`）——Plan B v2 / Phase 0B。
//!
//! # 唯一入口契约（方案 §4.1）
//!
//! **索引与预览必须走同一入口**：`load_document()` 是文件 → [`DocumentSource`] 的
//! 唯一通路。`document_preview` 命令不得自行调用 anydoc / pdf-inspector。
//! 收益：① 预览与索引输出天然一致；② 预览不产生第二次解析（Phase 3 接入转换缓存后直接命中）；
//! ③ 新增格式只改注册表 + 本模块一处。
//!
//! # 为什么不是 `Option<String>`
//!
//! 改造前的 `pipeline::read_document() -> Option<String>` 有两个结构性缺陷：
//! ① **失败不可解释**（`None` 不带原因，`.docx` 只能打一条"非 UTF-8 编码"）；
//! ② **String 丢掉 provenance**（page/slide/chapter 无处安放）。
//! [`DocumentSource`] 携带**内容形态 + provenance 载体 + 诊断**，其中
//! `page_spans` 是 Phase 1 的挂载点（本轮为空）。
//!
//! # 偏移是构造出来的，不是反查出来的（方案 §4.3）
//!
//! Phase 1 起，PDF 的逐页内容会被**顺序拼接**并同步记录 `page_spans`
//! （每段的起始字节由拼接过程本身给出），因此页归属无需从 markdown 反查 PDF——
//! 这一点很关键：`pipeline::chunk_document` 在分块前还会做 frontmatter 剥离与
//! HTML 清洗（`pipeline.rs` 现状），任何"文件字节 → chunk 字节"的反查都会系统性错位。

use std::path::Path;

use crate::core::document::filekind::{self, Converter, DocumentForm};

/// 单文件装载的最小有效字节数。
///
/// 沿用改造前 `pipeline.rs` 的 `c.len() >= 10` 门槛，但**显式化为诊断**
/// （旧实现是静默 `continue`）。
pub const MIN_DOC_BYTES: usize = 10;

// ──────────────────────────── 诊断类型 ────────────────────────────

/// 跳过原因（对齐上游错误契约：anydoc `ConvertError` / pdf-inspector `PdfError`）。
///
/// 本类型是"什么没进库、为什么"的**唯一表达**，由 `Indexer` 汇总进 `IndexDiagnostics`。
///
/// `#[allow(dead_code)]`：`NeedsOcr`/`ResourceLimit`/`MissingPart` 在 Phase 1/2
/// 由 pdf-inspector / anydoc 的错误映射构造，本阶段保留完整契约以便上层一次写全匹配。
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SkipReason {
    /// 注册表未登记 / 无法用既有转换器处理
    Unsupported { ext: String },
    /// 文本文件不是合法 UTF-8（旧实现只打日志）
    NotUtf8,
    /// PDF 存在扫描/纯图片页（Phase 1 起由 pdf-inspector 精确给出页号）
    NeedsOcr { pages: Vec<u32>, page_count: u32 },
    /// 加密或受口令保护
    Encrypted,
    /// 结构不可用，提取不到有意义内容
    Malformed { detail: String },
    /// 触及安全上限（如上游 anydoc `max_asset_total_bytes`）
    ResourceLimit { limit: String },
    /// 产出所必需部件缺失（OOXML/OLE 结构损坏）
    MissingPart { part: String },
    /// 超过 `ConversionPolicy::max_file_bytes` 前置护栏
    TooLarge { size: u64, limit: u64 },
    /// 有效内容不足 `MIN_DOC_BYTES`
    TooSmall { size: usize },
    /// 提取成功但内容为空
    EmptyContent,
    /// 文件读取失败
    Io { detail: String },
}

impl SkipReason {
    /// 稳定机器可读码（日志聚合 / 前端分组用；与上游 `ConvertError::code()` 同名）
    pub fn code(&self) -> &'static str {
        match self {
            SkipReason::Unsupported { .. } => "unsupported",
            SkipReason::NotUtf8 => "not_utf8",
            SkipReason::NeedsOcr { .. } => "needs_ocr",
            SkipReason::Encrypted => "encrypted",
            SkipReason::Malformed { .. } => "malformed",
            SkipReason::ResourceLimit { .. } => "resource_limit",
            SkipReason::MissingPart { .. } => "missing_part",
            SkipReason::TooLarge { .. } => "too_large",
            SkipReason::TooSmall { .. } => "too_small",
            SkipReason::EmptyContent => "empty_content",
            SkipReason::Io { .. } => "io",
        }
    }

    /// 人类可读中文说明（UI 展示）
    pub fn message(&self) -> String {
        match self {
            SkipReason::Unsupported { ext } => {
                if ext.is_empty() {
                    "不支持的格式".to_string()
                } else {
                    format!("不支持的格式: .{}", ext)
                }
            }
            SkipReason::NotUtf8 => "非 UTF-8 编码的文本文件".to_string(),
            SkipReason::NeedsOcr { pages, page_count } => format!(
                "扫描件/图片页需 OCR（第 {} 页，共 {} 页）",
                pages
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
                page_count
            ),
            SkipReason::Encrypted => "文件已加密或受口令保护".to_string(),
            SkipReason::Malformed { detail } => format!("结构不可用: {}", detail),
            SkipReason::ResourceLimit { limit } => format!("触及安全上限: {}", limit),
            SkipReason::MissingPart { part } => format!("缺少必要部件: {}", part),
            SkipReason::TooLarge { size, limit } => {
                format!("文件过大（{} MB > 上限 {} MB）", size / 1024 / 1024, limit / 1024 / 1024)
            }
            SkipReason::TooSmall { size } => format!("有效内容不足（{} 字节 < {}）", size, MIN_DOC_BYTES),
            SkipReason::EmptyContent => "未提取到任何内容".to_string(),
            SkipReason::Io { detail } => format!("读取失败: {}", detail),
        }
    }
}

/// 文档级状态（与"统计值"分离；方案 §5.3）
///
/// `#[allow(dead_code)]`：`PartiallyIndexed` 在 Phase 1（PDF 部分索引，Q8 决策）启用。
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DocStatus {
    /// 完整入库
    Indexed,
    /// **部分入库**（Phase 1 起：可提取页入库，需 OCR 的页单独记录，Q8 决策）
    PartiallyIndexed { skipped_pages: Vec<u32> },
}

/// 页级诊断（Phase 1 起使用；`code` 对齐上游原因码）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageDiagnostic {
    pub page: u32,
    pub code: &'static str,
    pub detail: String,
}

/// 转换器身份（替代裸 `&'static str`，评审 §五）
///
/// `#[allow(dead_code)]`：`PDF_INSPECTOR`/`ANYDOC` 在 Phase 1/2 启用；`label()`
/// 在 Phase 0C 落库到 `DocumentChunk.converter` 时启用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ConverterInfo {
    pub id: &'static str,
    pub version: &'static str,
}

#[allow(dead_code)]
impl ConverterInfo {
    /// 原生直读（无转换器）
    pub const NATIVE: Self = Self { id: "native", version: "1" };
    /// Phase 0A/0B 过渡：既有 `pdf-extract`（Phase 1 由 pdf-inspector 取代）
    pub const PDF_EXTRACT: Self = Self { id: "pdf-extract", version: "0.7" };
    /// Phase 1：pdf-inspector。版本与 Cargo.toml 的**精确锁定值**同步（`pdf-inspector = "=1.19.0"`）；
    /// 该字符串同时是转换缓存主键与 `kind_converters` 失效快照的一部分——改动即主动让旧结果失效。
    /// 依赖侧锁精确版本，正是为了让这个手写常量不可能被 `cargo update` 静默带偏。
    pub const PDF_INSPECTOR: Self = Self { id: "pdf-inspector", version: "1.19.0" };
    /// Phase 2：anydoc（版本与 Cargo.toml 的精确锁定值 `=0.2.4` 同步）
    pub const ANYDOC: Self = Self { id: "anydoc", version: "0.2.4" };

    /// `id@version` 形式（落库到 `DocumentChunk.converter`，供版本失效判定）
    pub fn label(&self) -> String {
        format!("{}@{}", self.id, self.version)
    }
}

impl std::fmt::Display for ConverterInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.id, self.version)
    }
}

/// 页码归属区间（**构造产物**，非反查；Phase 1 起填充）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageSpan {
    /// 1-indexed（对外统一口径）
    pub page: u32,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// 行区间 → 页码映射（**构造产物**）。
///
/// 与 [`PageSpan`] 的区别：字节区间供前端/缓存定位，**行区间供 AST 节点归属**
/// （`NodeMetadata.start_line/end_line` 就是行号，方案 §4.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineSpan {
    /// 1-based，半开区间 `[line_start, line_end)`
    pub line_start: usize,
    pub line_end: usize,
    /// 1-indexed
    pub page: u32,
}

impl LineSpan {
    /// 该行是否落在本区间
    pub fn contains_line(&self, line: usize) -> bool {
        line >= self.line_start && line < self.line_end
    }
}

/// 装载结果：内容 + 形态 + provenance 载体 + 诊断。
///
/// `#[allow(dead_code)]`：`source_kind`/`converter` 在 Phase 0C 落库；
/// `page_spans`/`doc_status`/`page_diagnostics`/`warnings` 在 Phase 1 填充并被消费。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DocumentSource {
    /// 相对路径（与 `DocumentChunk.doc_name` 一致）
    pub rel_path: String,
    /// 正文（Phase 1 起为转换后的 Markdown；本阶段为原始文本）
    pub text: String,
    /// 决定分块策略的形态（来自注册表）
    pub form: DocumentForm,
    /// 是否走 frontmatter 解析 + 自定义 HTML 清洗（等价于旧 `is_markdown_ext`）
    pub frontmatter: bool,
    /// 版本失效的粒度键（来自注册表能力位）
    pub source_kind: &'static str,
    /// 页码归属（Phase 1 起非空）
    pub page_spans: Vec<PageSpan>,
    /// 行区间 → 页码（Phase 1 起非空；供 AST 节点归属）
    pub line_page_map: Vec<LineSpan>,
    pub converter: ConverterInfo,
    pub doc_status: DocStatus,
    pub page_diagnostics: Vec<PageDiagnostic>,
    pub warnings: Vec<String>,
}

impl DocumentSource {
    /// **仅测试用**：按相对路径从纯文本构造等价来源。
    ///
    /// 生产路径必须走 [`load_document`]（唯一入口契约）；本构造器只服务于
    /// `chunk_document` 的单元测试，避免测试为了造一个 `DocumentSource` 而落盘。
    #[cfg(test)]
    pub fn for_test(rel_path: &str, text: &str) -> Self {
        let kind = filekind::lookup(rel_path);
        Self {
            rel_path: rel_path.to_string(),
            text: text.to_string(),
            form: kind.map(|k| k.form).unwrap_or(DocumentForm::Plain),
            frontmatter: kind.map(|k| k.caps.doc_like && k.form == DocumentForm::Markdown).unwrap_or(false),
            source_kind: kind.map(|k| k.caps.source_kind).unwrap_or("text"),
            page_spans: Vec::new(),
            line_page_map: Vec::new(),
            converter: ConverterInfo::NATIVE,
            doc_status: DocStatus::Indexed,
            page_diagnostics: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// 该文档是否走 frontmatter/HTML 清洗路径
    #[allow(dead_code)]
    pub fn has_frontmatter(&self) -> bool {
        self.frontmatter
    }
}

// ──────────────────────────── 装载实现 ────────────────────────────

/// **唯一入口**：文件 → [`DocumentSource`]。
///
/// `rel_path` 必须是用 `/` 分隔的相对路径（与索引侧 `doc_name` 同一口径）。
///
/// 实现：读入字节后交给 [`load_document_bytes`]——转换缓存（Phase 3）也走同一个函数，
/// 从而做到"**读一次文件**"（缓存需要内容哈希，若分成两次读会放大 I/O）。
pub fn load_document(abs_path: &Path, rel_path: &str) -> Result<DocumentSource, SkipReason> {
    // **前置**尺寸护栏：必须在 `fs::read` 之前。`load_document_bytes` 里那道同样的检查
    // 要等字节已经进内存才跑得到，此时"避免把 2GB 读进内存"已不成立（Rust 分配失败是
    // abort，不可捕获）。两条路径共用同一上限，口径见 `precheck_size`。
    precheck_size(abs_path, rel_path)?;
    let bytes = std::fs::read(abs_path).map_err(|e| SkipReason::Io { detail: e.to_string() })?;
    load_document_bytes(abs_path, rel_path, &bytes)
}

/// 尺寸前置护栏（**读盘前**调用）。
///
/// `load_document_bytes` 内部保留同一判断，用于「字节由调用方提供」的路径
/// （转换缓存先读盘算哈希，然后带着字节进来）。两处共用 `ConversionPolicy::max_file_bytes`。
fn precheck_size(abs_path: &Path, rel_path: &str) -> Result<(), SkipReason> {
    let Some(kind) = filekind::lookup(rel_path) else {
        return Ok(()); // 未登记格式的拒绝理由由字节路径给出（Unsupported）
    };
    if let Ok(meta) = std::fs::metadata(abs_path) {
        if meta.len() > kind.policy.max_file_bytes {
            return Err(SkipReason::TooLarge {
                size: meta.len(),
                limit: kind.policy.max_file_bytes,
            });
        }
    }
    Ok(())
}

/// 装载（字节已由调用方读入）。
///
/// 由 [`load_document`] 与 `db::conversion_cache` 共用：前者负责读盘，后者先用同一份
/// 字节算内容哈希查缓存，未命中再调本函数——**转换逻辑只有这一处**。
pub fn load_document_bytes(
    abs_path: &Path,
    rel_path: &str,
    bytes: &[u8],
) -> Result<DocumentSource, SkipReason> {
    let kind = filekind::lookup(rel_path).ok_or_else(|| SkipReason::Unsupported {
        ext: filekind::ext_of(rel_path).unwrap_or("").to_string(),
    })?;

    if !kind.caps.searchable {
        return Err(SkipReason::Unsupported {
            ext: filekind::ext_of(rel_path).unwrap_or("").to_string(),
        });
    }

    // 尺寸护栏（字节已由调用方提供时的那一道；读盘路径由 `load_document` 在读盘前先挡）
    if let Ok(meta) = std::fs::metadata(abs_path) {
        if meta.len() > kind.policy.max_file_bytes {
            return Err(SkipReason::TooLarge {
                size: meta.len(),
                limit: kind.policy.max_file_bytes,
            });
        }
    }

    let (text, converter) = match kind.converter {
        Converter::Plain => {
            // 二进制容器绝不允许走 UTF-8 直读（Phase 0B 守卫；Phase 1/2 的
            // pdf/docx 由各自转换器接管，不会落到这里）
            if kind.caps.binary {
                return Err(SkipReason::Unsupported {
                    ext: filekind::ext_of(rel_path).unwrap_or("").to_string(),
                });
            }
            let text = std::str::from_utf8(bytes)
                .map_err(|_| SkipReason::NotUtf8)?
                .to_string();
            (text, ConverterInfo::NATIVE)
        }
        Converter::LegacyPdf => (convert_legacy_pdf(abs_path)?, ConverterInfo::PDF_EXTRACT),
        Converter::PdfInspector => {
            let pdf = convert_pdf_inspector(bytes)?;
            return Ok(DocumentSource {
                rel_path: rel_path.to_string(),
                text: pdf.text,
                form: kind.form,
                frontmatter: false, // PDF 转换产物是 Markdown，但没有 frontmatter 概念
                source_kind: kind.caps.source_kind,
                page_spans: pdf.page_spans,
                line_page_map: pdf.line_page_map,
                converter: ConverterInfo::PDF_INSPECTOR,
                doc_status: pdf.doc_status,
                page_diagnostics: pdf.diagnostics,
                warnings: pdf.warnings,
            });
        }
        Converter::AnyDoc => {
            let text = convert_anydoc(bytes, rel_path)?;
            return Ok(DocumentSource {
                rel_path: rel_path.to_string(),
                text,
                form: kind.form,
                frontmatter: false, // Office 无 frontmatter 概念
                source_kind: kind.caps.source_kind,
                page_spans: Vec::new(),
                line_page_map: Vec::new(),
                converter: ConverterInfo::ANYDOC,
                doc_status: DocStatus::Indexed,
                page_diagnostics: Vec::new(),
                warnings: Vec::new(),
            });
        }
    };

    if text.trim().is_empty() {
        return Err(SkipReason::EmptyContent);
    }
    if text.len() < MIN_DOC_BYTES {
        return Err(SkipReason::TooSmall { size: text.len() });
    }

    Ok(DocumentSource {
        rel_path: rel_path.to_string(),
        text,
        form: kind.form,
        // frontmatter 解析仅对 Markdown 家族中登记为 `doc_like` 的扩展名生效
        // （与旧 `is_markdown_ext` 语义一致：md/markdown/mdown/rst 生效，mdx 不生效）
        frontmatter: kind.caps.doc_like && kind.form == DocumentForm::Markdown,
        source_kind: kind.caps.source_kind,
        page_spans: Vec::new(),
        line_page_map: Vec::new(),
        converter,
        doc_status: DocStatus::Indexed,
        page_diagnostics: Vec::new(),
        warnings: Vec::new(),
    })
}

/// Phase 0A/0B 过渡：既有 `pdf-extract` 分支（Phase 1 由 pdf-inspector 取代）。
#[cfg(feature = "pdf-extract")]
fn convert_legacy_pdf(path: &Path) -> Result<String, SkipReason> {
    match pdf_extract::extract_text(path) {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                // 扫描件/图片型 PDF 的典型表现：提取为空
                Err(SkipReason::EmptyContent)
            } else {
                Ok(trimmed.to_string())
            }
        }
        Err(e) => {
            let msg = e.to_string();
            let lower = msg.to_lowercase();
            if lower.contains("encrypt") || lower.contains("password") {
                Err(SkipReason::Encrypted)
            } else {
                Err(SkipReason::Malformed { detail: msg })
            }
        }
    }
}

#[cfg(not(feature = "pdf-extract"))]
fn convert_legacy_pdf(_path: &Path) -> Result<String, SkipReason> {
    Err(SkipReason::Unsupported { ext: "pdf".to_string() })
}

// ──────────────────────────── Phase 1：pdf-inspector ────────────────────────────

/// [`convert_pdf_inspector`] 的输出（正文 + provenance + 诊断）
#[derive(Debug)]
struct PdfOutcome {
    text: String,
    page_spans: Vec<PageSpan>,
    line_page_map: Vec<LineSpan>,
    doc_status: DocStatus,
    diagnostics: Vec<PageDiagnostic>,
    warnings: Vec<String>,
}

/// **PDF 主转换路径**（Phase 1 / C1）：pdf-inspector 逐页 Markdown + 页码 provenance。
///
/// 设计要点：
/// - **预分类**（`detect_pdf_mem`，~10–50ms）：整篇扫描件/图片型直接 `NeedsOcr`，
///   省掉一次全量提取；
/// - **逐页提取**（`extract_pages_markdown_mem`）：页码口径按方案 §3.2 表归一——
///   入参 `pages` 与返回 `PageMarkdown.page` 都是 **0-indexed**，对外统一 **+1**；
/// - **拼接 + 记账**：`page_spans`/`line_page_map` 由拼接过程**构造**（方案 §4.3），
///   不是从 markdown 反查 PDF；
/// - **部分索引**（Q8）：可提取页入库，需 OCR 的页写入 `PartiallyIndexed`，
///   不做"任一页需 OCR 就整篇拒绝"（上游 anydoc 的语义对个人知识库过于严格）。
fn convert_pdf_inspector(bytes: &[u8]) -> Result<PdfOutcome, SkipReason> {
    use pdf_inspector::{detect_pdf_mem, extract_pages_markdown_mem, PdfType};

    let det = detect_pdf_mem(bytes).map_err(map_pdf_error)?;
    let page_count = det.page_count;

    if matches!(det.pdf_type, PdfType::Scanned | PdfType::ImageBased) {
        return Err(SkipReason::NeedsOcr {
            pages: (1..=page_count).collect(),
            page_count,
        });
    }

    let ex = extract_pages_markdown_mem(bytes, None).map_err(map_pdf_error)?;
    let mut out = assemble_pdf_pages(&ex.pages, page_count)?;

    // 文档级警告：依赖 detect/extract 的顶层结论，不属于逐页装配
    if det.has_encoding_issues {
        out.warnings
            .push("检测到字体编码问题，部分文本可能乱码（可考虑 OCR）".to_string());
    }
    if ex.is_complex {
        out.warnings.push(format!(
            "版面较复杂（表格页 {} / 多栏页 {}），已按阅读顺序重组",
            ex.pages_with_tables.len(),
            ex.pages_with_columns.len()
        ));
    }
    Ok(out)
}

/// 逐页结果 → 正文 + 页 provenance（**纯函数**：不碰 PDF 字节，故可直接单测）。
///
/// 为什么单独拆出来：页码归属正确率是 §8.4 第一层的硬指标，而「部分页需 OCR」
/// （Q8 决策）这条分支在真实样本上很难构造——真实文件要么整篇是文本页、要么整篇
/// 是扫描件（验收 harness 里两个扫描件都落在"整篇需 OCR"）。做成纯函数后可以
/// 直接喂合成的 `PageMarkdown` 覆盖三条分支：全成功 / 中间页需 OCR / 全失败。
fn assemble_pdf_pages(
    pages: &[pdf_inspector::PageMarkdown],
    page_count: u32,
) -> Result<PdfOutcome, SkipReason> {
    let mut text = String::new();
    let mut page_spans: Vec<PageSpan> = Vec::new();
    let mut line_page_map: Vec<LineSpan> = Vec::new();
    let mut diagnostics: Vec<PageDiagnostic> = Vec::new();
    let mut skipped_pages: Vec<u32> = Vec::new();
    // 1-based 行游标（半开区间语义：下一行文本将落在 cur_line）
    let mut cur_line: usize = 1;

    for pm in pages {
        let page_1 = pm.page + 1;
        // 上游在 needs_ocr 为真时已把该页 markdown 清空（Phase 1 侦察结论）
        if pm.needs_ocr || pm.markdown.trim().is_empty() {
            diagnostics.push(PageDiagnostic {
                page: page_1,
                code: "needs_ocr",
                detail: pm
                    .ocr_reason
                    .clone()
                    .unwrap_or_else(|| "该页无可提取文本".to_string()),
            });
            skipped_pages.push(page_1);
            continue;
        }
        let body = pm.markdown.trim_end();
        if !text.is_empty() {
            // 页间分隔：追加 "\n\n"。
            //
            // ⚠ 只 +1 行，不是 +2：上一页正文经 `trim_end` 后**不带尾换行**，因此
            // 第一个 '\n' 只是"结束上一页最后一行"（不产生新行），只有第二个 '\n'
            // 才真正多出一行。故下一页正文首行 = 上一页 `line_end` + 1。
            // （曾误写成 +2：每跨一页多算 1 行，第 N 页偏移 +(N−1)，页码引用系统性错位。）
            text.push_str("\n\n");
            cur_line += 1;
        }
        let byte_start = text.len();
        let line_start = cur_line;
        text.push_str(body);
        // body 含 N 个 '\n' ⇒ 占 N+1 行
        let line_end = line_start + body.matches('\n').count() + 1;
        cur_line = line_end;

        page_spans.push(PageSpan {
            page: page_1,
            byte_start,
            byte_end: text.len(),
        });
        line_page_map.push(LineSpan {
            line_start,
            line_end,
            page: page_1,
        });
    }

    if text.trim().is_empty() {
        // 一页都提不出来：按"整篇需 OCR"报，而不是 EmptyContent
        //（后者会误导用户去查文件是否损坏）
        return Err(SkipReason::NeedsOcr {
            pages: (1..=page_count).collect(),
            page_count,
        });
    }

    Ok(PdfOutcome {
        text,
        page_spans,
        line_page_map,
        doc_status: if skipped_pages.is_empty() {
            DocStatus::Indexed
        } else {
            DocStatus::PartiallyIndexed { skipped_pages }
        },
        diagnostics,
        // 文档级警告由调用方（`convert_pdf_inspector`）追加
        warnings: Vec::new(),
    })
}

/// pdf-inspector 错误 → mdgo 跳过原因（`PdfError` 非 `#[non_exhaustive]`，可穷尽匹配）
fn map_pdf_error(e: pdf_inspector::PdfError) -> SkipReason {
    use pdf_inspector::PdfError;
    match e {
        PdfError::Encrypted => SkipReason::Encrypted,
        PdfError::Io(err) => SkipReason::Io { detail: err.to_string() },
        PdfError::NotAPdf(d) => SkipReason::Malformed {
            detail: format!("不是 PDF: {}", d),
        },
        PdfError::InvalidStructure => SkipReason::Malformed {
            detail: "PDF 结构无效".to_string(),
        },
        PdfError::Parse(d) => SkipReason::Malformed { detail: d },
    }
}

// ──────────────────────────── Phase 2：anydoc（Office/ODF/RTF/EPUB） ────────────────────────────

/// **Office / ODF / RTF / EPUB 主转换路径**（Phase 2 / C2）：anydoc → GitHub-Flavored Markdown。
///
/// 关键点（上游源码核验，方案 §3.1）：
/// - **探测顺序：先内容、后扩展名**（`from_bytes().or_else(from_path())`）。加密 OOXML 在
///   内容探测阶段返回 `None`，若只靠内容嗅探会退化成 `Unsupported`，丢掉"加密"这一更有
///   价值的诊断（FastGPT 的生产代码同样用这个组合）；
/// - anydoc **没有 options 结构体、没有 feature flag**：图片在 Markdown 里以 alt text
///   呈现（字节只在 `to_document().assets`，本版不需要资产入库）；
/// - `ConvertError` 是 `#[non_exhaustive]` → 映射必须保留 catch-all 且**不得 panic**
///   （见 [`map_anydoc_error`]）。
fn convert_anydoc(bytes: &[u8], rel_path: &str) -> Result<String, SkipReason> {
    use anydoc::Format;

    // CSV 无内容签名，必须显式命名格式；本注册表把 csv 交给 Plain 直读，
    // 因此此处 format 为 None 只会出现在"既无签名也认不出扩展名"的情况。
    let format = Format::from_bytes(bytes).or_else(|| Format::from_path(Path::new(rel_path)));
    let ext = filekind::ext_of(rel_path).unwrap_or("");

    let md = match anydoc::to_markdown_bytes(bytes, format) {
        Ok(md) => md,
        Err(e) => {
            log::debug!("[loader] anydoc 转换失败 ({}): {}", rel_path, e);
            return Err(map_anydoc_error(e, ext));
        }
    };
    if md.trim().is_empty() {
        return Err(SkipReason::EmptyContent);
    }
    Ok(md)
}

/// anydoc 错误 → mdgo 跳过原因（`ConvertError` 为 `#[non_exhaustive]`，必须有 catch-all）
fn map_anydoc_error(e: anydoc::ConvertError, ext: &str) -> SkipReason {
    use anydoc::ConvertError as E;
    match e {
        // `Unsupported(String)` 携带的是诊断消息（非扩展名）→ 扩展名从 rel_path 取，
        // 消息只进日志（`SkipReason` 只保留机器可读码 + 中文说明）。
        E::Unsupported(detail) => {
            log::debug!("[loader] anydoc 不支持该内容: {}", detail);
            SkipReason::Unsupported { ext: ext.to_string() }
        }
        E::NeedsOcr { pages, page_count } => SkipReason::NeedsOcr { pages, page_count },
        E::Malformed { part, detail } => SkipReason::Malformed {
            detail: match part {
                Some(p) => format!("{}: {}", p, detail),
                None => detail,
            },
        },
        E::Encrypted => SkipReason::Encrypted,
        E::ResourceLimit { limit, detail } => SkipReason::ResourceLimit {
            limit: format!("{}: {}", limit, detail),
        },
        E::MissingPart { part } => SkipReason::MissingPart { part },
        E::Io(err) => SkipReason::Io { detail: err.to_string() },
        // 上游新增变体（non_exhaustive）→ 视为结构不可用，绝不 panic
        other => SkipReason::Malformed {
            detail: format!("anydoc 未知错误: {:?}", other),
        },
    }
}

// ──────────────────────────── 测试 ────────────────────────────

/// **测试用**：生成一个最小但**结构合法**的单页文本 PDF（含正确的 xref 偏移）。
///
/// 放在模块级（而非测试模块内）是为了让 `db::conversion_cache` 的测试也能复用，
/// 避免在仓库里塞二进制夹具。
#[cfg(test)]
pub(crate) fn minimal_pdf(text: &str) -> Vec<u8> {
    let content = format!("BT /F1 24 Tf 72 700 Td ({}) Tj ET", text);
    let objs = [
        "<</Type/Catalog/Pages 2 0 R>>".to_string(),
        "<</Type/Pages/Kids[3 0 R]/Count 1>>".to_string(),
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>"
            .to_string(),
        format!("<</Length {}>>\nstream\n{}\nendstream", content.len(), content),
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_string(),
    ];

    let mut out = String::from("%PDF-1.4\n");
    let mut offsets: Vec<usize> = Vec::new();
    for (i, body) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj{}\nendobj\n", i + 1, body));
    }
    let xref_at = out.len();
    out.push_str(&format!("xref\n0 {}\n", objs.len() + 1));
    out.push_str("0000000000 65535 f \n");
    for off in &offsets {
        out.push_str(&format!("{:010} 00000 n \n", off));
    }
    out.push_str(&format!(
        "trailer<</Size {}/Root 1 0 R>>\nstartxref\n{}\n%%EOF\n",
        objs.len() + 1,
        xref_at
    ));
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(name: &str, content: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("mdgo_loader_tests");
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let p = dir.join(name);
        std::fs::write(&p, content).expect("写临时文件");
        p
    }

    /// 构造一页抽取结果（`assemble_pdf_pages` 的输入）
    fn page_md(page_0: u32, markdown: &str, needs_ocr: bool) -> pdf_inspector::PageMarkdown {
        pdf_inspector::PageMarkdown {
            page: page_0,
            markdown: markdown.to_string(),
            needs_ocr,
            ocr_reason: needs_ocr.then(|| "扫描件".to_string()),
        }
    }

    /// **与实现无关**的真值：由正文 + 字节偏移反推某页占用的真实行区间。
    ///
    /// 为什么不直接断言"下一页 line_start == 上一页 line_end + k"：那种写法只是把
    /// 实现里的游标算术**抄一遍**，实现错了测试就跟着错（本文件曾因此把 `+2` 的
    /// off-by-one 固化成"契约"）。这里改为从 `text` 自身数换行符，与 `assemble_pdf_pages`
    /// 的游标完全解耦。
    ///
    /// 约定：1-based 半开区间 `[line_start, line_end)`，行号 = 该行之前出现过的换行数 + 1。
    fn true_line_span(text: &str, byte_start: usize, byte_end: usize) -> (usize, usize) {
        let nl = |b: usize| text[..b].matches('\n').count();
        // 首行 = 起始字节之前的换行数 + 1
        let start = nl(byte_start) + 1;
        // 正文以非换行符结尾（trim_end）⇒ 该页占用的行数 = 页内换行数 + 1，
        // 折算成"结束行"即 `byte_end` 之前换行数 + 2
        let end = nl(byte_end) + 2;
        (start, end)
    }

    /// 页码归属正确率（§8.4 第一层硬指标）：行区间必须与正文**逐页对齐**。
    ///
    /// 同时覆盖字节侧（gap 恰为 "\n\n"）与行号侧（真值比对），后者是本测试的重点。
    #[test]
    fn pdf_assembly_builds_exact_page_spans() {
        let pages = vec![
            page_md(0, "# P1 title\nP1 body", false),
            page_md(1, "P2 body", false),
            page_md(2, "# P3 title\nP3 body", false),
        ];
        let out = assemble_pdf_pages(&pages, 3).expect("应装配成功");
        assert_eq!(out.doc_status, DocStatus::Indexed);
        assert!(out.diagnostics.is_empty());
        assert_eq!(out.page_spans.len(), 3);
        assert_eq!(out.line_page_map.len(), 3);

        // 逐页：字节区间内容非空、不跨页，且行区间必须等于由正文反推的真值
        for (i, (span, ls)) in out.page_spans.iter().zip(out.line_page_map.iter()).enumerate() {
            assert_eq!(span.page, (i + 1) as u32, "页码必须 1-indexed 且递增");
            let slice = &out.text[span.byte_start..span.byte_end];
            assert_eq!(slice, pages[i].markdown.trim_end(), "字节区间必须精确覆盖该页正文");

            let (want_start, want_end) = true_line_span(&out.text, span.byte_start, span.byte_end);
            assert_eq!(
                (ls.line_start, ls.line_end),
                (want_start, want_end),
                "第 {} 页行区间错位：实现给出 [{},{})，按正文真值应为 [{},{})",
                span.page,
                ls.line_start,
                ls.line_end,
                want_start,
                want_end
            );
            // 行区间取出的文本必须等于该页正文（把行号换成字符串再比一次，最直观）
            let by_lines: Vec<&str> = out
                .text
                .lines()
                .skip(ls.line_start - 1)
                .take(ls.line_end - ls.line_start)
                .collect();
            assert_eq!(
                by_lines.join("\n"),
                slice,
                "第 {} 页行区间取出的文本与该页正文不一致（行号漂移）",
                span.page
            );
        }
        // 相邻页区间之间恰好是 2 个分隔换行
        for w in out.page_spans.windows(2) {
            let gap = &out.text[w[0].byte_end..w[1].byte_start];
            assert_eq!(gap, "\n\n", "页间分隔必须是两个换行");
        }
        assert_eq!(out.line_page_map[0].line_start, 1, "首页必须从第 1 行开始");
    }

    /// Q8 关键分支：**部分页需 OCR** 时必须 PartiallyIndexed 并列出页码，
    /// 而不是"任一页需 OCR 就整篇拒绝"（上游 anydoc 的严格语义）。
    /// 真实样本难以构造（要么全是文本页、要么整篇扫描件），故在此直接覆盖。
    #[test]
    fn pdf_assembly_partially_indexes_when_some_pages_need_ocr() {
        let pages = vec![
            page_md(0, "# P1 title\nP1 body", false),
            page_md(1, "", true),
            page_md(2, "P3 body", false),
        ];
        let out = assemble_pdf_pages(&pages, 3).expect("有可提取页就不应整篇拒绝");

        assert_eq!(
            out.doc_status,
            DocStatus::PartiallyIndexed { skipped_pages: vec![2] },
            "第 2 页需 OCR，必须记为 PartiallyIndexed 且页码 1-indexed"
        );
        assert_eq!(out.page_spans.len(), 2, "只应产出可提取页的页区间");
        assert_eq!(
            out.page_spans.iter().map(|s| s.page).collect::<Vec<_>>(),
            vec![1, 3],
            "跳过页不得产生页区间，但后续页页码必须保持原始页码"
        );
        assert!(!out.text.contains("P2"), "被跳过页的内容不得混入正文");
        assert_eq!(out.diagnostics.len(), 1);
        assert_eq!(out.diagnostics[0].page, 2);
        assert_eq!(out.diagnostics[0].code, "needs_ocr");
        assert_eq!(out.diagnostics[0].detail, "扫描件");

        // 跳过页不得让后续页的行号错位：仍按正文真值逐页校验
        for (span, ls) in out.page_spans.iter().zip(out.line_page_map.iter()) {
            let (want_start, want_end) = true_line_span(&out.text, span.byte_start, span.byte_end);
            assert_eq!(
                (ls.line_start, ls.line_end),
                (want_start, want_end),
                "跳过第 2 页后第 {} 页行区间错位",
                span.page
            );
            let by_lines: Vec<&str> = out
                .text
                .lines()
                .skip(ls.line_start - 1)
                .take(ls.line_end - ls.line_start)
                .collect();
            assert_eq!(by_lines.join("\n"), &out.text[span.byte_start..span.byte_end]);
        }
    }

    /// 一页都提不出来 → 报「整篇需 OCR」而非 EmptyContent
    /// （后者会误导用户去查文件是否损坏）。
    #[test]
    fn pdf_assembly_reports_whole_doc_ocr_when_no_page_extractable() {
        let pages = vec![page_md(0, "", true), page_md(1, "   ", false)];
        match assemble_pdf_pages(&pages, 2) {
            Err(SkipReason::NeedsOcr { pages, page_count }) => {
                assert_eq!(pages, vec![1, 2], "应覆盖文档全部页");
                assert_eq!(page_count, 2);
            }
            other => panic!("应报整篇需 OCR，实际: {:?}", other),
        }
    }

    #[test]
    fn loads_plain_text_with_registry_form() {
        let p = write_tmp("a.md", b"# Title\n\nBody paragraph content.");
        let src = load_document(&p, "notes/a.md").expect("应装载成功");
        assert_eq!(src.form, DocumentForm::Markdown);
        assert!(src.frontmatter, "md 应走 frontmatter 路径");
        assert_eq!(src.source_kind, "markdown");
        assert_eq!(src.converter, ConverterInfo::NATIVE);
        assert_eq!(src.doc_status, DocStatus::Indexed);
        assert!(src.page_spans.is_empty(), "Phase 0B 尚无 provenance");
        assert!(src.text.contains("# Title"));
    }

    /// mdx 旧行为：走 Markdown 分块器但**不**解析 frontmatter（`is_markdown_ext("mdx") == false`）
    #[test]
    fn mdx_keeps_legacy_no_frontmatter_behavior() {
        let p = write_tmp("b.mdx", b"---\ntitle: x\n---\n\n# Body content here.");
        let src = load_document(&p, "docs/b.mdx").expect("应装载成功");
        assert_eq!(src.form, DocumentForm::Markdown, "mdx 仍走 Markdown 分块器");
        assert!(!src.frontmatter, "mdx 不应解析 frontmatter（保持改造前行为）");
    }

    #[test]
    fn unregistered_extension_is_reported_not_silently_skipped() {
        // Phase 2 起 `.docx` 已登记（AnyDoc），故用真正未登记的扩展名验证"不静默跳过"
        let p = write_tmp("c.xyz", b"binary\x00\x01payload that is not utf8");
        let err = load_document(&p, "docs/c.xyz").expect_err("未登记扩展名应报错");
        assert_eq!(err.code(), "unsupported");
        assert!(err.message().contains(".xyz"), "错误信息应指出扩展名: {}", err.message());
    }

    #[test]
    fn non_utf8_is_reported_as_not_utf8() {
        // 0xFF 0xFE 不是合法 UTF-8
        let p = write_tmp("d.txt", &[0xFF, 0xFE, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        let err = load_document(&p, "docs/d.txt").expect_err("非 UTF-8 应报错");
        assert_eq!(err.code(), "not_utf8");
    }

    #[test]
    fn too_small_content_is_reported() {
        let p = write_tmp("e.txt", b"hi");
        let err = load_document(&p, "docs/e.txt").expect_err("内容过短应报错");
        assert_eq!(err.code(), "too_small");
    }

    #[test]
    fn empty_content_is_reported() {
        let p = write_tmp("f.txt", b"   \n\t  ");
        let err = load_document(&p, "docs/f.txt").expect_err("空白应报错");
        assert_eq!(err.code(), "empty_content");
    }

    #[test]
    fn filename_only_kinds_load_via_registry() {
        let p = write_tmp("Dockerfile", b"FROM rust:1.88\nRUN cargo build --release\n");
        let src = load_document(&p, "deploy/Dockerfile").expect("Dockerfile 应可装载（D2）");
        assert_eq!(src.form, DocumentForm::Plain);
        assert_eq!(src.source_kind, "text");
    }

    #[test]
    fn skip_reason_codes_are_stable() {
        // 机器可读码必须稳定（前端分组/日志聚合依赖）
        let cases = [
            (SkipReason::Unsupported { ext: "x".into() }, "unsupported"),
            (SkipReason::NotUtf8, "not_utf8"),
            (SkipReason::NeedsOcr { pages: vec![3], page_count: 10 }, "needs_ocr"),
            (SkipReason::Encrypted, "encrypted"),
            (SkipReason::Malformed { detail: "d".into() }, "malformed"),
            (SkipReason::ResourceLimit { limit: "l".into() }, "resource_limit"),
            (SkipReason::MissingPart { part: "p".into() }, "missing_part"),
            (SkipReason::TooLarge { size: 1, limit: 0 }, "too_large"),
            (SkipReason::TooSmall { size: 1 }, "too_small"),
            (SkipReason::EmptyContent, "empty_content"),
            (SkipReason::Io { detail: "d".into() }, "io"),
        ];
        for (reason, code) in cases {
            assert_eq!(reason.code(), code, "{:?} 的 code 不稳定", reason);
            assert!(!reason.message().is_empty(), "{:?} 缺少中文说明", reason);
        }
    }

    #[test]
    fn converter_label_is_id_at_version() {
        assert_eq!(ConverterInfo::NATIVE.label(), "native@1");
        assert_eq!(ConverterInfo::PDF_EXTRACT.label(), "pdf-extract@0.7");
        assert_eq!(ConverterInfo::ANYDOC.label(), "anydoc@0.2.4");
        assert_eq!(format!("{}", ConverterInfo::PDF_INSPECTOR), "pdf-inspector@1.19.0");
    }

    #[test]
    fn too_large_is_pre_guarded() {
        // 构造一个超过策略上限的文件名 → 用极小上限的 kind 不可得，故直接验证策略语义
        let p = write_tmp("g.txt", b"0123456789abcdef");
        let src = load_document(&p, "docs/g.txt").expect("小文件正常装载");
        assert!(src.text.len() >= MIN_DOC_BYTES);
    }

    // ─── Phase 1：PDF（pdf-inspector）端到端 ───

    /// PDF 走 pdf-inspector：正文提取 + 页码 provenance + 转换器身份
    #[test]
    fn pdf_uses_pdf_inspector_with_page_provenance() {
        let bytes = minimal_pdf("Hello mdgo PDF");
        let p = write_tmp("phase1-text.pdf", &bytes);
        let src = load_document(&p, "docs/phase1-text.pdf").expect("文本型 PDF 应可装载");

        assert_eq!(src.form, DocumentForm::Markdown, "PDF 转换产物按 Markdown 形态分块");
        assert!(!src.frontmatter, "PDF 不做 frontmatter 解析（保证行号与 AST 对齐）");
        assert_eq!(src.source_kind, "pdf");
        assert_eq!(src.converter, ConverterInfo::PDF_INSPECTOR);
        assert_eq!(src.converter.label(), "pdf-inspector@1.19.0");
        assert!(src.text.contains("Hello mdgo PDF"), "正文应含提取文本: {:?}", src.text);

        // provenance：字节区间与行区间**都由拼接构造**，且页号为对外 1-indexed
        assert_eq!(src.page_spans.len(), 1, "单页 PDF 应有 1 个字节区间");
        assert_eq!(src.page_spans[0].page, 1, "对外页号必须 1-indexed");
        assert_eq!(src.page_spans[0].byte_start, 0);
        assert_eq!(src.page_spans[0].byte_end, src.text.len());
        assert_eq!(src.line_page_map.len(), 1);
        assert!(src.line_page_map[0].contains_line(1), "第 1 行应落在第 1 页区间");
        assert!(!src.line_page_map[0].contains_line(0), "行号 1-based，0 不属于任何页");

        assert_eq!(src.doc_status, DocStatus::Indexed);
        assert!(src.page_diagnostics.is_empty());
    }

    /// 加密/非 PDF 的错误映射（`PdfError` 变体穷尽覆盖）
    #[test]
    fn pdf_error_mapping_is_exhaustive() {
        use pdf_inspector::PdfError;
        assert_eq!(map_pdf_error(PdfError::Encrypted), SkipReason::Encrypted);
        assert_eq!(
            map_pdf_error(PdfError::InvalidStructure).code(),
            "malformed"
        );
        assert_eq!(
            map_pdf_error(PdfError::NotAPdf("x".into())).code(),
            "malformed"
        );
        assert_eq!(map_pdf_error(PdfError::Parse("bad".into())).code(), "malformed");
        let io = map_pdf_error(PdfError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "missing",
        )));
        assert_eq!(io.code(), "io");
    }

    /// 非 PDF 内容被当作 PDF 装载 → 报 malformed 而非 panic
    #[test]
    fn garbage_pdf_is_reported_not_panicking() {
        let p = write_tmp("notreally.pdf", b"%PDF-1.4\nthis is not a real pdf body at all\n");
        match load_document(&p, "docs/notreally.pdf") {
            Ok(src) => panic!("损坏 PDF 不应成功装载: {:?}", src.doc_status),
            Err(e) => assert!(
                matches!(e.code(), "malformed" | "io" | "needs_ocr"),
                "应为可解释错误，实际 {}",
                e.code()
            ),
        }
    }

    /// **Phase 1 端到端**：PDF → 装载 → 分块，chunk 必须携带页码 provenance。
    ///
    /// 这是「跨页语义 chunk + page provenance」的验收点：分块走 Markdown AST 引擎
    /// （form=Markdown），页码来自 loader 构造的行→页映射，二者在同一 chunk 上汇合。
    #[test]
    fn pdf_chunks_carry_page_provenance() {
        let bytes = minimal_pdf("Hello mdgo PDF provenance");
        let p = write_tmp("phase1-chunks.pdf", &bytes);
        let src = load_document(&p, "docs/phase1-chunks.pdf").expect("装载");
        assert_eq!(src.form, DocumentForm::Markdown);
        assert!(!src.line_page_map.is_empty(), "loader 应构造行→页映射");

        let chunks = crate::core::pipeline::chunk_document(&src, 448, 56, None);
        assert!(!chunks.is_empty(), "PDF 应产出 chunk");

        // `chunk_document` 直接返回落库结构（DocumentChunk），因此这里同时验证
        // ① 页码 provenance ② 索引侧字段（source_kind / converter）都已就位
        for c in &chunks {
            assert_eq!(
                c.page_start,
                Some(1),
                "单页 PDF 的每个 chunk 都应归属第 1 页；实际 {:?}（type={:?}）",
                c.page_start,
                c.chunk_type
            );
            assert_eq!(c.page_end, Some(1));
            let spans = c.source_spans.as_deref().unwrap_or("");
            assert!(
                spans.contains("\"page\":1"),
                "chunk 应带页归属明细: {:?}",
                spans
            );
            // provenance 不得污染正文
            assert!(!c.text.contains("line_start"), "provenance 不应进入 chunk 正文");
            assert_eq!(c.source_kind.as_deref(), Some("pdf"));
            assert_eq!(c.converter.as_deref(), Some("pdf-inspector@1.19.0"));
        }
    }

    /// 非分页来源（txt）不得出现页码（避免把行号误当页号）
    #[test]
    fn non_paginated_source_has_no_pages() {
        let text = "# 标题\n\n正文段落内容足够长以产出 chunk。\n";
        let p = write_tmp("phase1-plain.md", text.as_bytes());
        let src = load_document(&p, "docs/phase1-plain.md").expect("装载");
        assert!(src.line_page_map.is_empty(), "md 无页信息");
        let chunks = crate::core::pipeline::chunk_document(&src, 448, 56, None);
        assert!(!chunks.is_empty());
        assert!(
            chunks.iter().all(|c| c.page_start.is_none() && c.page_end.is_none()),
            "非分页来源不应有页码"
        );
    }

    // ─── Phase 2：Office/ODF/RTF/EPUB（anydoc）端到端 ───

    /// **Phase 2 端到端**：RTF → anydoc → Markdown → AST 分块。
    ///
    /// 选 RTF 做样本的原因：它是**纯文本**格式（`{\rtf1…}`），因此测试无需二进制夹具，
    /// 但走的是与 `.docx/.xlsx/.pptx` 完全相同的 `Converter::AnyDoc` 通路。
    #[test]
    fn rtf_uses_anydoc_and_reaches_markdown_chunking() {
        let rtf = r"{\rtf1\ansi\deff0 {\b Hello} mdgo RTF conversion pipeline.}";
        let p = write_tmp("phase2-doc.rtf", rtf.as_bytes());
        let src = load_document(&p, "docs/phase2-doc.rtf").expect("RTF 应可转换");

        assert_eq!(src.form, DocumentForm::Markdown, "anydoc 产物按 Markdown 形态分块");
        assert_eq!(src.converter, ConverterInfo::ANYDOC);
        assert_eq!(src.converter.label(), "anydoc@0.2.4");
        assert_eq!(src.source_kind, "office");
        assert!(!src.frontmatter, "Office 不做 frontmatter 解析");
        assert!(
            src.text.contains("Hello") && src.text.contains("mdgo RTF"),
            "应保留正文文本: {:?}",
            src.text
        );
        assert!(src.page_spans.is_empty(), "Office 无页 provenance（slide 非目标 R1）");
        assert!(src.line_page_map.is_empty());
        assert_eq!(src.doc_status, DocStatus::Indexed);

        // 索引侧：走同一个 chunk_document，源类型/转换器身份必须落库
        let chunks = crate::core::pipeline::chunk_document(&src, 448, 56, None);
        assert!(!chunks.is_empty(), "应产出 chunk");
        assert!(chunks.iter().all(|c| c.page_start.is_none() && c.page_end.is_none()));
        assert_eq!(chunks[0].source_kind.as_deref(), Some("office"));
        assert_eq!(chunks[0].converter.as_deref(), Some("anydoc@0.2.4"));
    }

    /// 损坏的 OOXML（不是合法 ZIP）→ 可解释错误，绝不 panic
    #[test]
    fn malformed_office_is_reported_not_panicking() {
        let p = write_tmp("phase2-broken.docx", b"this is definitely not a zip container at all");
        match load_document(&p, "docs/phase2-broken.docx") {
            Ok(src) => panic!("损坏 docx 不应成功装载: {:?}", src.doc_status),
            Err(e) => assert!(
                matches!(e.code(), "unsupported" | "malformed" | "missing_part" | "not_utf8"),
                "应为可解释错误，实际 {}",
                e.code()
            ),
        }
    }

    /// 加密 OOXML 的探测特征：内容无签名 → 必须靠扩展名回落才能得到有意义诊断
    #[test]
    fn office_format_detection_falls_back_to_extension() {
        use anydoc::Format;
        // 纯文本内容探测不出任何格式 → 交给扩展名
        let bytes = b"plain bytes with no container signature";
        assert!(Format::from_bytes(bytes).is_none(), "无签名内容应探测失败");
        assert_eq!(Format::from_path(Path::new("a.docx")), Some(Format::Docx));
        assert_eq!(Format::from_path(Path::new("a.rtf")), Some(Format::Rtf));
        assert_eq!(Format::from_path(Path::new("a.wps")), None, "wps 上游不支持（Q3）");
    }
}
