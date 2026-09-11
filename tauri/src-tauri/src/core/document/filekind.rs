//! 文件类型注册表（**全库唯一格式清单**）——Plan B v2 / Phase 0A。
//!
//! # 为什么需要这一层
//!
//! 改造前格式清单散落五处并已产生漂移（方案 §1.3 D7）：
//! `db/utils.rs::KB_SUPPORTED_EXTS`、`db/chunk_splitter.rs` 工厂注册循环、
//! `document/html_clean.rs::is_markdown_ext`、`indexer.rs::classify_ext`、
//! `search/query_plan.rs::{CODE_EXTENSIONS, intent_allowed_exts}`。
//! 本模块把这些**声明**收敛为单一来源，其余模块只做**派生消费**。
//!
//! # 职责边界（评审 §十二）
//!
//! 注册表只声明**中性事实**：如何识别文件、用哪个转换器、按哪种形态分块、
//! 索引统计分类、一组**能力位**（`FileCapabilities`）。
//! **不声明检索策略**——`RetrievalIntent → 扩展名集合` 的推导由
//! `search/query_plan.rs` 从能力位自行完成。
//!
//! # 与 `db::chunk_splitter::CODE_LANG_SEPARATORS` 的关系
//!
//! [`CODE_LANGS`] 是对"**有语言感知分块器**的扩展名集合"的唯一声明；
//! `chunk_splitter.rs` 的 `CODE_LANG_SEPARATORS` 提供各语言**分隔符表**（分块实现细节）。
//! 两者一致性由 `chunk_splitter.rs` 侧的对齐测试守护。
//!
//! # Phase 0A 行为等价约束
//!
//! 本阶段转换器只有 [`Converter::Plain`] 与 [`Converter::LegacyPdf`]（后者沿用既有
//! `pdf-extract` 分支，Phase 1 由 [`Converter::PdfInspector`] 取代）。
//! **登记的扩展名集合必须是"旧白名单 + 有意新增"的并集**：旧白名单中的每一项
//! 都必须在册（`pdf` 走 LegacyPdf，`env`/`gitignore` 因隐藏文件策略删除），
//! 否则会造成"升级后原本可索引的文件消失"的静默回归。

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// 注册表语义版本。
///
/// **任何改变 chunk 产物的注册表编辑都必须递增它**：例如某格式的 `form` 改变
/// （Plain → Markdown）、`converter` 换代、能力位变化导致白名单/路由变化。
/// 该值参与 `IndexerConfig::chunk_params_version()`，从而让**旧索引被识别为 stale**
/// （全局粒度）；而**单个转换器**升级只影响对应 `source_kind`（见 `KbStatus.stale_kinds`）。
pub const REGISTRY_VERSION: &str = "3";

// ──────────────────────────── 类型定义 ────────────────────────────

/// 文件识别方式。
///
/// `FileName` 的存在是 D2 的修复：`Dockerfile` / `Makefile` 这类文件名经
/// `Path::extension()` 得到 `None`，只按扩展名匹配**永远不可能命中**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matcher {
    /// 扩展名（**不带点**，大小写不敏感）
    Ext(&'static str),
    /// 精确文件名（大小写不敏感），用于无扩展名的约定文件名
    FileName(&'static str),
}

/// 读取/转换方式。
///
/// `#[allow(dead_code)]`：`AnyDoc`（Phase 2）与 `PdfInspector`（Phase 1）在 0A 尚未接入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Converter {
    /// UTF-8 直读
    Plain,
    /// **过渡态**：沿用既有 `pdf-extract` 分支（`pipeline.rs:90`），Phase 1 由 `PdfInspector` 取代
    LegacyPdf,
    /// anydoc 转换（Phase 2）
    AnyDoc,
    /// pdf-inspector 提取 + 逐页 provenance（Phase 1）
    PdfInspector,
}

/// 决定**分块策略**的内容形态。
///
/// 与 [`FileCapabilities::is_code`] 的区别：`form` 决定"用哪个 splitter"，
/// `is_code` 决定"检索上算不算代码文件"。例如 `bat`/`cc`/`vue` 无语言分隔符表
/// → `form = Plain`，但 `is_code = true`（参与 Code 意图白名单，修 D4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentForm {
    /// comrak AST 语义分块
    Markdown,
    /// scraper → AST 语义分块
    Html,
    /// OPML / FreeMind 树形分块
    Tree,
    /// 代码语言感知分块
    Code,
    /// 通用 token 感知分块
    Plain,
}

impl DocumentForm {
    /// 稳定字符串名（跨 IPC/落库用；前端据此选择渲染器）
    pub fn as_str(&self) -> &'static str {
        match self {
            DocumentForm::Markdown => "markdown",
            DocumentForm::Html => "html",
            DocumentForm::Tree => "tree",
            DocumentForm::Code => "code",
            DocumentForm::Plain => "plain",
        }
    }
}

/// 中性能力位（**不含任何检索策略**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileCapabilities {
    /// 是否参与检索索引
    pub searchable: bool,
    /// 代码类文件（Code 意图白名单来源，修 D4）
    pub is_code: bool,
    /// 结构化来源（表格/大纲/配置等）
    pub is_structured: bool,
    /// 能提供 page provenance（Phase 1 起为 pdf）
    pub paginated: bool,
    /// 二进制容器：**禁止 UTF-8 直读**（Phase 0B 起作为守卫依据）
    pub binary: bool,
    /// 文档类（Document 意图白名单来源；保持既有语义）
    pub doc_like: bool,
    /// 大纲/思维导图（Outline 意图白名单来源）
    pub outline: bool,
    /// 版本失效的粒度键（方案 §5.5）
    pub source_kind: &'static str,
}

/// 转换资源策略（**与 FileKind 解耦**，评审 §十三）。
///
/// 上游自身已有硬上限（anydoc `MAX_TOTAL_BYTES` 512MiB / `MAX_ENTRY_BYTES` 128MiB /
/// `MAX_ASSET_TOTAL_BYTES` 128MiB，私有常量不可配），因此本策略是**前置护栏**
/// （快速跳过超大文件、防止转换挂死），不是真正的内容边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConversionPolicy {
    /// 单文件上限（字节）
    pub max_file_bytes: u64,
    /// 解压膨胀上限（预留，Phase 2 对 OOXML/OLE 生效）
    pub max_expansion: u64,
    /// 内嵌资源上限（预留）
    pub max_asset_bytes: u64,
    /// 单文件转换超时（毫秒）
    pub timeout_ms: u64,
}

impl ConversionPolicy {
    /// 默认：200MB / 512MiB / 128MiB / 30s
    pub const DEFAULT: Self = Self {
        max_file_bytes: 200 * 1024 * 1024,
        max_expansion: 512 * 1024 * 1024,
        max_asset_bytes: 128 * 1024 * 1024,
        timeout_ms: 30_000,
    };

    /// 二进制文档容器（PDF/Office）：给更宽的超时
    pub const BINARY_DOC: Self = Self {
        max_file_bytes: 200 * 1024 * 1024,
        max_expansion: 512 * 1024 * 1024,
        max_asset_bytes: 128 * 1024 * 1024,
        timeout_ms: 120_000,
    };
}

/// 一条格式声明。
///
/// `#[allow(dead_code)]`：`converter` / `form` / `policy` 在 Phase 0A 尚未被消费
/// （转换器在 0B 接入、策略在 0B/2 生效）。本阶段一次性落地完整字段，
/// 是为了避免"分阶段追加字段"再次把格式声明拆散到多个模块。
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct FileKind {
    pub matcher: Matcher,
    pub converter: Converter,
    pub form: DocumentForm,
    /// 索引统计分类（对齐既有 `classify_ext` 取值：Markdown / 代码 / 数据 / 其他）
    pub category: &'static str,
    pub caps: FileCapabilities,
    pub policy: ConversionPolicy,
}

// ──────────────────────────── 扩展名集合声明 ────────────────────────────

/// 有**语言感知分块器**的代码扩展名（与 `db::chunk_splitter::CODE_LANG_SEPARATORS` 键一致）。
///
/// ⚠ 该集合与分隔符表的一致性由 `chunk_splitter.rs::code_lang_table_matches_registry`
/// 测试守护——`dart` 曾因被误归入"无分隔符表"分组而被该测试拦下。
pub const CODE_LANGS: &[&str] = &[
    "py", "rs", "go", "js", "ts", "jsx", "tsx", "java", "c", "cpp", "h", "hpp", "cs", "swift",
    "kt", "php", "rb", "sh", "bash", "zsh", "lua", "sql", "r", "scala", "dart",
];

/// 参与 Code 检索意图、但**没有**语言分隔符表（`form = Plain`）的代码类扩展名。
///
/// `bat` / `cc` 来自既有 `CODE_EXTENSIONS`；其余是前端 `_EXT_TYPE_MAP` 已识别、
/// 但改造前不在任何白名单的代码类格式（修 D4）。
pub const EXTRA_CODE_EXTS: &[&str] =
    &["bat", "cc", "cmd", "vue", "svelte", "astro", "gradle", "graphql", "gql", "proto"];

/// 既有 `CODE_EXTENSIONS`（改造前的 Code 意图白名单）——**仅用于回归测试与分类口径**。
///
/// 保留它以证明"新白名单 = 旧白名单 + 有意新增"，避免 D4 修复时误删旧成员。
#[allow(dead_code)]
pub const LEGACY_CODE_EXTENSIONS: &[&str] = &[
    "py", "js", "ts", "rs", "go", "java", "lua", "sh", "bat", "sql", "yaml", "yml", "toml",
    "conf", "c", "cpp", "cc", "h", "hpp", "rb", "php",
];

/// 文档类扩展名（Document 检索意图白名单来源）。
///
/// **刻意与既有 `intent_allowed_exts(Document)` 完全一致**：PDF/HTML/Office 是否纳入
/// Document 意图属**检索策略决策**，留到 Phase 1/2 拿到 A/B 数据后改能力位。
pub const DOC_LIKE_EXTS: &[&str] = &["md", "markdown", "mdown", "rst", "txt"];

/// 大纲/思维导图扩展名（Outline 检索意图白名单来源）。
pub const OUTLINE_EXTS: &[&str] = &["opml", "mm"];

/// 无扩展名的约定文件名（D2 修复）。
///
/// **`.env` / `.gitignore` 刻意不在此列**：它们是隐藏文件，被
/// `IgnoreMatcher::is_kb_file_allowed`（`db/utils.rs:273`，对齐前端 FILE_BLACKLIST）
/// 在设计上排除；登记它们只会重建"永不命中"的死配置。旧 `KB_SUPPORTED_EXTS` 中的
/// `env` / `gitignore` 两项据此**删除**。同理 `*.log` 被同一函数排除，故 `log` 不登记。
pub const DOC_FILE_NAMES: &[&str] = &["dockerfile", "makefile", "gnumakefile"];

// ──────────────────────────── 注册表构建 ────────────────────────────

/// 注册表（含索引，构建一次后只读）。
#[allow(dead_code)]
pub struct Registry {
    kinds: Vec<FileKind>,
    by_ext: HashMap<&'static str, usize>,
    by_name: HashMap<&'static str, usize>,
    all_exts: Vec<&'static str>,
    code_exts: Vec<&'static str>,
    doc_like_exts: Vec<&'static str>,
    outline_exts: Vec<&'static str>,
    paginated_exts: Vec<&'static str>,
    /// 白名单快速判定（`scan_directory` 每文件调用，必须 O(1)）
    ext_set: HashSet<&'static str>,
    name_set: HashSet<&'static str>,
    /// 纯 `FileName` 规则条数（一致性断言用）
    file_name_rules: usize,
}

fn caps(
    source_kind: &'static str,
    is_code: bool,
    doc_like: bool,
    outline: bool,
    is_structured: bool,
    paginated: bool,
    binary: bool,
) -> FileCapabilities {
    FileCapabilities {
        searchable: true,
        is_code,
        is_structured,
        paginated,
        binary,
        doc_like,
        outline,
        source_kind,
    }
}

/// 构建注册表。**新增/调整格式只改这里。**
fn build() -> Registry {
    let mut kinds: Vec<FileKind> = Vec::new();
    macro_rules! push {
        ($m:expr, $c:expr, $f:expr, $cat:expr, $caps:expr, $pol:expr) => {
            kinds.push(FileKind {
                matcher: $m,
                converter: $c,
                form: $f,
                category: $cat,
                caps: $caps,
                policy: $pol,
            })
        };
    }

    // ── Markdown 家族（含改造前白名单缺失的 markdown/mdown/rst）──
    for &ext in &["md", "mdx", "markdown", "mdown", "rst"] {
        let doc_like = DOC_LIKE_EXTS.contains(&ext);
        push!(
            Matcher::Ext(ext),
            Converter::Plain,
            DocumentForm::Markdown,
            "Markdown",
            caps("markdown", false, doc_like, false, false, false, false),
            ConversionPolicy::DEFAULT
        );
    }

    // ── HTML ──
    for &ext in &["html", "htm"] {
        push!(
            Matcher::Ext(ext),
            Converter::Plain,
            DocumentForm::Html,
            "其他",
            caps("markdown", false, false, false, false, false, false),
            ConversionPolicy::DEFAULT
        );
    }

    // ── 大纲 / 思维导图 ──
    for &ext in OUTLINE_EXTS {
        push!(
            Matcher::Ext(ext),
            Converter::Plain,
            DocumentForm::Tree,
            "其他",
            caps("data", false, false, true, true, false, false),
            ConversionPolicy::DEFAULT
        );
    }

    // ── 纯文本 ──
    push!(
        Matcher::Ext("txt"),
        Converter::Plain,
        DocumentForm::Plain,
        "其他",
        caps("text", false, true, false, false, false, false),
        ConversionPolicy::DEFAULT
    );

    // ── 代码：有语言分隔符表 ──
    for &ext in CODE_LANGS {
        push!(
            Matcher::Ext(ext),
            Converter::Plain,
            DocumentForm::Code,
            "代码",
            caps("code", true, false, false, false, false, false),
            ConversionPolicy::DEFAULT
        );
    }

    // ── 代码：无语言分隔符表（form=Plain，仍属 Code 意图，修 D4）──
    for &ext in EXTRA_CODE_EXTS {
        push!(
            Matcher::Ext(ext),
            Converter::Plain,
            DocumentForm::Plain,
            "代码",
            caps("code", true, false, false, false, false, false),
            ConversionPolicy::DEFAULT
        );
    }

    // ── 配置文件：旧 `classify_ext` 把它们算作"代码"（当时在 CODE_EXTENSIONS 内）→ 保持同口径 ──
    for &ext in &["yaml", "yml", "toml", "conf"] {
        push!(
            Matcher::Ext(ext),
            Converter::Plain,
            DocumentForm::Plain,
            "代码",
            caps("data", true, false, false, true, false, false),
            ConversionPolicy::DEFAULT
        );
    }

    // ── 其他结构化文本：旧实现里不在 CODE_EXTENSIONS → 分类为"其他"（保持同口径，
    //    口径本身是否合理属后续独立议题，本阶段不做检索侧行为变更）──
    for &ext in &["json", "xml", "ini", "cfg", "properties"] {
        push!(
            Matcher::Ext(ext),
            Converter::Plain,
            DocumentForm::Plain,
            "其他",
            caps("data", false, false, false, true, false, false),
            ConversionPolicy::DEFAULT
        );
    }

    // ── 表格数据 ──
    for &ext in &["csv", "tsv", "jsonl"] {
        push!(
            Matcher::Ext(ext),
            Converter::Plain,
            DocumentForm::Plain,
            "数据",
            caps("data", false, false, false, true, false, false),
            ConversionPolicy::DEFAULT
        );
    }

    // ── 既有白名单中的样式/脚本类（保持可索引，避免静默回归）──
    push!(
        Matcher::Ext("ps1"),
        Converter::Plain,
        DocumentForm::Plain,
        "其他",
        caps("code", true, false, false, false, false, false),
        ConversionPolicy::DEFAULT
    );
    for &ext in &["css", "scss", "less"] {
        push!(
            Matcher::Ext(ext),
            Converter::Plain,
            DocumentForm::Plain,
            "其他",
            caps("markdown", false, false, false, false, false, false),
            ConversionPolicy::DEFAULT
        );
    }

    // ── PDF（Phase 1 / C1）：pdf-inspector 逐页 Markdown + 页码 provenance ──
    //
    // form = `Markdown`：转换产物是 Markdown，因此**复用既有 comrak AST 语义分块**（C3）。
    // frontmatter 派生为 false（pdf 非 doc_like）→ 不做 frontmatter 解析与 HTML 清洗，
    // 这一点同时保证 loader 构造的行区间与 AST 节点行号**严格对齐**（方案 §4.3）。
    push!(
        Matcher::Ext("pdf"),
        Converter::PdfInspector,
        DocumentForm::Markdown,
        "其他",
        caps("pdf", false, false, false, false, true, true),
        ConversionPolicy::BINARY_DOC
    );

    // ── 无扩展名约定文件（D2）──
    for &name in DOC_FILE_NAMES {
        push!(
            Matcher::FileName(name),
            Converter::Plain,
            DocumentForm::Plain,
            "其他",
            caps("text", false, false, false, false, false, false),
            ConversionPolicy::DEFAULT
        );
    }
    // 保留扩展名形式：`api.dockerfile` / `x.makefile` 这类带点命名真实存在
    for &ext in &["dockerfile", "makefile"] {
        push!(
            Matcher::Ext(ext),
            Converter::Plain,
            DocumentForm::Plain,
            "其他",
            caps("text", false, false, false, false, false, false),
            ConversionPolicy::DEFAULT
        );
    }

    // ── Office / ODF / RTF / EPUB（Phase 2 / C2）：anydoc → Markdown ──
    //
    // 统一语义：
    // - `form = Markdown`：转换产物是 Markdown → 复用 comrak AST 语义分块（C3）；
    // - `binary = true`：**禁止 UTF-8 直读**（Phase 0B 的守卫依据）；
    // - `doc_like = false`：不解析 frontmatter（Office 无该概念），因此也不会做 HTML 清洗
    //   → 行号与 AST 保持一致（本阶段这些格式不产出页映射）；
    // - `category = "其他"`：保持统计图既有词表不变（前端另有 `_EXT_TYPE_MAP` 分类）。
    for &ext in &[
        // Word（含旧版 OLE `.doc` 与宏文档 `.docm`）
        "doc", "docx", "docm",
        // PowerPoint（含旧版 OLE `.ppt` 与全部 OOXML 变体）
        "ppt", "pps", "pot", "pptx", "pptm", "ppsx", "ppsm",
        // Excel（含旧版 OLE `.xls` 与二进制工作簿 `.xlsb`）
        "xls", "xlsx", "xlsm", "xlsb",
        // OpenDocument
        "odt", "ods", "odp",
        // RTF / EPUB
        "rtf", "epub",
    ] {
        // 电子表格类标记为结构化（供未来"列名/表头进 metadata"判断）
        let sheet = matches!(ext, "xls" | "xlsx" | "xlsm" | "xlsb" | "ods");
        push!(
            Matcher::Ext(ext),
            Converter::AnyDoc,
            DocumentForm::Markdown,
            "其他",
            caps("office", false, false, false, sheet, false, true),
            ConversionPolicy::BINARY_DOC
        );
    }

    build_index(kinds)
}

fn build_index(kinds: Vec<FileKind>) -> Registry {
    let mut by_ext = HashMap::new();
    let mut by_name = HashMap::new();
    let mut all_exts = Vec::new();
    let mut ext_set = HashSet::new();
    let mut name_set = HashSet::new();
    let mut code_exts = Vec::new();
    let mut doc_like_exts = Vec::new();
    let mut outline_exts = Vec::new();
    let mut paginated_exts = Vec::new();
    let mut file_name_rules = 0usize;

    for (idx, k) in kinds.iter().enumerate() {
        match k.matcher {
            Matcher::Ext(e) => {
                assert!(by_ext.insert(e, idx).is_none(), "[filekind] 扩展名重复登记: {}", e);
                if ext_set.insert(e) {
                    all_exts.push(e);
                }
            }
            Matcher::FileName(n) => {
                assert!(by_name.insert(n, idx).is_none(), "[filekind] 文件名重复登记: {}", n);
                name_set.insert(n);
                file_name_rules += 1;
            }
        }
        if let Matcher::Ext(e) = k.matcher {
            if k.caps.is_code {
                code_exts.push(e);
            }
            if k.caps.doc_like {
                doc_like_exts.push(e);
            }
            if k.caps.outline {
                outline_exts.push(e);
            }
            if k.caps.paginated {
                paginated_exts.push(e);
            }
        }
    }

    Registry {
        kinds,
        by_ext,
        by_name,
        all_exts,
        code_exts,
        doc_like_exts,
        outline_exts,
        paginated_exts,
        ext_set,
        name_set,
        file_name_rules,
    }
}

/// 全局注册表（构建一次，只读）。
pub fn registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(build)
}

// ──────────────────────────── 派生查询 ────────────────────────────

impl Registry {
    /// 全部已登记 `FileKind`
    #[allow(dead_code)]
    pub fn kinds(&self) -> &[FileKind] {
        &self.kinds
    }

    /// 全部已登记扩展名（不含点）——**替代旧 `KB_SUPPORTED_EXTS`**
    pub fn all_exts(&self) -> &[&'static str] {
        &self.all_exts
    }

    /// 无扩展名的约定文件名（小写）（Phase 1/2 消费）
    #[allow(dead_code)]
    pub fn file_names(&self) -> &HashSet<&'static str> {
        &self.name_set
    }

    /// 纯文件名规则条数（用于一致性断言）
    #[allow(dead_code)]
    pub fn file_name_rule_count(&self) -> usize {
        self.file_name_rules
    }

    /// Code 意图白名单（**替代旧 `CODE_EXTENSIONS`**）
    pub fn code_exts(&self) -> &[&'static str] {
        &self.code_exts
    }

    /// Document 意图白名单（保持既有语义）
    pub fn doc_like_exts(&self) -> &[&'static str] {
        &self.doc_like_exts
    }

    /// Outline 意图白名单
    pub fn outline_exts(&self) -> &[&'static str] {
        &self.outline_exts
    }

    /// 能提供 page provenance 的扩展名（Phase 1 的 chunk/page 元数据消费）
    #[allow(dead_code)]
    pub fn paginated_exts(&self) -> &[&'static str] {
        &self.paginated_exts
    }

    /// 按扩展名查（不含点，大小写不敏感）
    pub fn lookup_ext(&self, ext: &str) -> Option<&FileKind> {
        let lower = ext.to_ascii_lowercase();
        self.by_ext.get(lower.as_str()).map(|i| &self.kinds[*i])
    }

    /// 按文件名查（大小写不敏感；仅 `FileName` 规则）
    pub fn lookup_file_name(&self, file_name: &str) -> Option<&FileKind> {
        let lower = file_name.to_ascii_lowercase();
        self.by_name.get(lower.as_str()).map(|i| &self.kinds[*i])
    }

    /// 扩展名是否在白名单内
    pub fn is_indexable_ext(&self, ext: &str) -> bool {
        self.ext_set.contains(ext.to_ascii_lowercase().as_str())
    }

    /// 文件名是否命中 `FileName` 规则
    pub fn is_indexable_file_name(&self, file_name: &str) -> bool {
        self.name_set.contains(file_name.to_ascii_lowercase().as_str())
    }

    /// 相对路径是否可索引：**先文件名、后扩展名**（D2）
    pub fn is_indexable(&self, rel_path: &str) -> bool {
        if self.is_indexable_file_name(file_name_of(rel_path)) {
            return true;
        }
        match ext_of(rel_path) {
            Some(ext) => self.is_indexable_ext(ext),
            None => false,
        }
    }

    /// 相对路径 → `FileKind`（先文件名、后扩展名）
    pub fn lookup(&self, rel_path: &str) -> Option<&FileKind> {
        if let Some(k) = self.lookup_file_name(file_name_of(rel_path)) {
            return Some(k);
        }
        ext_of(rel_path).and_then(|e| self.lookup_ext(e))
    }

    /// 索引统计分类（替代 `indexer::classify_ext`）
    pub fn category(&self, rel_path: &str) -> &'static str {
        self.lookup(rel_path).map(|k| k.category).unwrap_or("其他")
    }
}

/// 取路径的文件名（兼容 `/` 与 `\`）
pub fn file_name_of(rel_path: &str) -> &str {
    rel_path.rsplit(['/', '\\']).next().unwrap_or(rel_path)
}

/// 取扩展名（不含点）。
///
/// 与 `Path::extension()` 语义一致：**以点开头且无其他点的文件名返回 `None`**
/// （`.env` / `.gitignore` 因此没有扩展名——这正是 D2 的成因）。
pub fn ext_of(rel_path: &str) -> Option<&str> {
    let name = file_name_of(rel_path);
    let idx = name.rfind('.')?;
    if idx == 0 || idx + 1 >= name.len() {
        return None;
    }
    Some(&name[idx + 1..])
}

// ──────────────────────────── 便捷函数 ────────────────────────────

/// 是否可索引（相对路径）
pub fn is_indexable(rel_path: &str) -> bool {
    registry().is_indexable(rel_path)
}

/// 该扩展名是否可索引
///
/// （Phase 0B 起由 `DocumentLoader` / `scan_directory` 替换路径使用；
/// 本阶段仅测试消费，故 `allow(dead_code)`）
#[allow(dead_code)]
pub fn is_indexable_ext(ext: &str) -> bool {
    registry().is_indexable_ext(ext)
}

/// 该文件名是否命中 `FileName` 规则（Phase 0B 消费）
#[allow(dead_code)]
pub fn is_indexable_file_name(file_name: &str) -> bool {
    registry().is_indexable_file_name(file_name)
}

/// 相对路径 → `FileKind`（Phase 0B 的 `DocumentLoader` 消费）
#[allow(dead_code)]
pub fn lookup(rel_path: &str) -> Option<&'static FileKind> {
    registry().lookup(rel_path)
}

/// 相对路径 → 索引统计分类（Phase 0B 消费；当前调用点走 `category_of_ext`）
#[allow(dead_code)]
pub fn category(rel_path: &str) -> &'static str {
    registry().category(rel_path)
}

/// 是否"文档类资料"（**DocAgent 圈选口径**，D8/N7）。
///
/// 与 [`is_indexable`] 的区别：索引白名单包含代码/配置类文件，但它们不作为
/// DocAgent 的"资料"候选。本判据 = Markdown 形态（含转换后的 PDF / Office / EPUB）
/// 或纯文本 —— 恰好覆盖用户会拿去问答的那批文件。
pub fn is_document_material(rel_path: &str) -> bool {
    match registry().lookup(rel_path) {
        Some(k) => k.form == DocumentForm::Markdown || k.matcher == Matcher::Ext("txt"),
        None => false,
    }
}

/// 扩展名 → 索引统计分类（未登记时 `"其他"`）
pub fn category_of_ext(ext: &str) -> &'static str {
    registry().lookup_ext(ext).map(|k| k.category).unwrap_or("其他")
}

// ──────────────────────────── 测试 ────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_builds_without_duplicate_matchers() {
        let r = registry();
        assert!(r.kinds().len() >= 50, "注册表规模异常: {}", r.kinds().len());
        assert_eq!(
            r.all_exts().len(),
            r.kinds().len() - r.file_name_rule_count(),
            "扩展名数量应为 kinds 数减去纯文件名规则数"
        );
    }

    /// **Phase 0A 核心回归**：旧白名单每一项都必须在册（否则是静默回归）
    #[test]
    fn legacy_whitelist_is_fully_preserved() {
        // 旧 KB_SUPPORTED_EXTS 去掉三处**有意**变更后的期望集合：
        //   - `docx`：旧登记是 D1 幽灵条目（无提取器，永远解析失败）→ Phase 2 随 AnyDoc 登记
        //   - `env` / `gitignore`：隐藏文件策略使其不可达 → 不再登记
        const LEGACY: &[&str] = &[
            "md", "txt", "pdf", "js", "ts", "jsx", "tsx", "py", "java", "go", "rs", "rb", "php",
            "c", "cpp", "h", "hpp", "cs", "swift", "kt", "scala", "r", "lua", "sh", "bash", "zsh",
            "ps1", "sql", "css", "scss", "less", "html", "htm", "xml", "json", "yaml", "yml",
            "toml", "ini", "cfg", "conf", "dockerfile", "makefile", "opml", "mm",
        ];
        let r = registry();
        for ext in LEGACY {
            assert!(r.is_indexable_ext(ext), "旧白名单扩展名 {} 丢失（静默回归）", ext);
        }
        // 有意删除的两项：隐藏文件策略使其不可达
        assert!(!r.is_indexable_ext("env"), "env 不可达（隐藏文件策略）");
        assert!(!r.is_indexable_ext("gitignore"), "gitignore 不可达（隐藏文件策略）");
        // docx 在 Phase 2 随 AnyDoc 登记（旧登记是 D1 幽灵条目）
        assert!(r.is_indexable_ext("docx"), "docx 应在 Phase 2 登记");
    }

    /// D2 修复：无扩展名约定文件名必须命中
    #[test]
    fn filename_matchers_fix_dead_entries() {
        let r = registry();
        for name in ["Dockerfile", "Makefile", "GNUmakefile", "dockerfile", "makefile"] {
            assert!(r.is_indexable_file_name(name), "{} 应被文件名规则命中", name);
            assert!(r.is_indexable(&format!("sub/dir/{}", name)), "{} 相对路径应可索引", name);
        }
        assert!(r.is_indexable("deploy/api.dockerfile"), "带点约定命名仍走扩展名规则");
    }

    /// 隐藏文件按既有策略不索引，且确实没有扩展名
    #[test]
    fn dotfiles_are_not_indexable_and_have_no_ext() {
        let r = registry();
        assert!(!r.is_indexable(".env"), ".env 属隐藏文件，按既有策略不索引");
        assert!(!r.is_indexable(".gitignore"));
        assert_eq!(ext_of(".env"), None);
        assert_eq!(ext_of(".gitignore"), None);
        assert_eq!(ext_of("a/.env"), None);
        assert!(r.lookup_ext("env").is_none(), "env 不应登记（不可达）");
        assert!(r.lookup_ext("gitignore").is_none(), "gitignore 不应登记（不可达）");
        assert!(r.lookup_ext("log").is_none(), "*.log 被黑名单排除，登记即死配置");
    }

    #[test]
    fn ext_of_matches_path_semantics() {
        assert_eq!(ext_of("a/b/c.md"), Some("md"));
        assert_eq!(ext_of("a\\b\\c.MD"), Some("MD"));
        assert_eq!(ext_of("README"), None);
        assert_eq!(ext_of("archive.tar.gz"), Some("gz"));
        assert_eq!(ext_of("trailing."), None);
        assert_eq!(file_name_of("a/b/c.md"), "c.md");
        assert_eq!(file_name_of("c.md"), "c.md");
        assert_eq!(file_name_of("a\\b"), "b");
    }

    /// D4 修复：Code 意图白名单覆盖所有语言分块器扩展名，且不丢旧成员
    #[test]
    fn code_intent_covers_all_language_splitter_exts() {
        let r = registry();
        for lang in CODE_LANGS {
            assert!(r.code_exts().contains(lang), "{} 有语言分块器但不在 Code 白名单", lang);
        }
        for old in LEGACY_CODE_EXTENSIONS {
            assert!(r.code_exts().contains(old), "旧 CODE_EXTENSIONS 成员 {} 丢失", old);
        }
        // 改造前遗漏、但可被索引的扩展名现在必须在白名单内
        for ext in ["jsx", "tsx", "cs", "swift", "kt", "bash", "zsh", "r", "scala", "ps1"] {
            assert!(r.code_exts().contains(&ext), "{} 应在 Code 意图白名单（D4）", ext);
        }
        // 新增代码类格式
        for ext in EXTRA_CODE_EXTS {
            assert!(r.code_exts().contains(ext), "{} 应在 Code 意图白名单", ext);
        }
    }

    /// Document / Outline 意图白名单**保持既有语义**
    #[test]
    fn intent_whitelists_preserve_legacy_semantics() {
        let r = registry();
        let mut doc = r.doc_like_exts().to_vec();
        doc.sort_unstable();
        assert_eq!(doc, vec!["markdown", "md", "mdown", "rst", "txt"]);
        let mut outline = r.outline_exts().to_vec();
        outline.sort_unstable();
        assert_eq!(outline, vec!["mm", "opml"]);
    }

    /// 分类口径与改造前 `classify_ext` 一致——**除了 D4 修复带来的有意变更**
    ///
    /// 旧口径把 `jsx/tsx/cs/swift/kt/bash/zsh/r/scala` 分到"其他"，尽管它们一直由
    /// **代码语言感知分块器**处理（`CODE_LANG_SEPARATORS` 里有它们）——这正是 D4 漂移的
    /// 直接体现（`classify_ext` 用的是 `CODE_EXTENSIONS`，而工厂用的是分隔符表）。
    /// 本版让统计分类跟随 `form = Code`，属**有意的可见变更**（类型分布图会变化）。
    #[test]
    fn category_matches_legacy_classify_ext_except_documented_d4_delta() {
        const D4_CATEGORY_FIXED: &[&str] =
            &["jsx", "tsx", "cs", "swift", "kt", "bash", "zsh", "r", "scala"];
        fn legacy(ext: &str) -> &'static str {
            match ext {
                "md" | "markdown" | "mdown" | "rst" => "Markdown",
                "csv" | "tsv" | "jsonl" | "parquet" | "arrow" | "feather" => "数据",
                e if LEGACY_CODE_EXTENSIONS.contains(&e) => "代码",
                _ => "其他",
            }
        }
        const LEGACY_INDEXABLE: &[&str] = &[
            "md", "txt", "pdf", "js", "ts", "jsx", "tsx", "py", "java", "go", "rs", "rb", "php",
            "c", "cpp", "h", "hpp", "cs", "swift", "kt", "scala", "r", "lua", "sh", "bash", "zsh",
            "ps1", "sql", "css", "scss", "less", "html", "htm", "xml", "json", "yaml", "yml",
            "toml", "ini", "cfg", "conf",
        ];
        for ext in LEGACY_INDEXABLE {
            if D4_CATEGORY_FIXED.contains(ext) {
                assert_eq!(
                    category_of_ext(ext),
                    "代码",
                    "{} 一直由代码语言分块器处理，D4 修复后应归入代码",
                    ext
                );
            } else {
                assert_eq!(
                    category_of_ext(ext),
                    legacy(ext),
                    "扩展名 {} 的分类与旧 classify_ext 不一致（统计图口径会变）",
                    ext
                );
            }
        }
        // 未登记的格式一律"其他"（与旧实现一致）
        for ext in ["unknown", "parquet", "arrow"] {
            assert_eq!(category_of_ext(ext), "其他");
        }
    }

    /// Phase 2：Office/ODF/RTF/EPUB 必须以 AnyDoc + Markdown 形态登记
    #[test]
    fn phase2_formats_use_anydoc_markdown_form() {
        let r = registry();
        for ext in [
            "doc", "docx", "docm", "ppt", "pps", "pot", "pptx", "pptm", "ppsx", "ppsm", "xls",
            "xlsx", "xlsm", "xlsb", "odt", "ods", "odp", "rtf", "epub",
        ] {
            let k = r.lookup_ext(ext).unwrap_or_else(|| panic!("{} 应在册", ext));
            assert_eq!(k.converter, Converter::AnyDoc, "{} 应走 anydoc", ext);
            assert_eq!(k.form, DocumentForm::Markdown, "{} 转换产物按 Markdown 分块", ext);
            assert!(k.caps.binary, "{} 必须标记 binary（禁止 UTF-8 直读）", ext);
            assert_eq!(k.caps.source_kind, "office");
            assert!(!k.caps.doc_like, "{} 不应解析 frontmatter", ext);
            assert!(!k.caps.paginated, "{} 无页 provenance（slide 非目标 R1）", ext);
        }
        // 电子表格标记为结构化
        for ext in ["xls", "xlsx", "xlsm", "xlsb", "ods"] {
            assert!(r.lookup_ext(ext).expect("在册").caps.is_structured, "{} 应为结构化", ext);
        }
    }

    // ── 跨层一致性守卫 ──
    //
    // D 类缺陷的根因是「同一份格式清单散落在 5 处，各自漂移」。后端已收敛为单一
    // 注册表，但按 Q1 决策前端（`main.html` / `support.js`）仍各自持有一份。
    // 一旦漂移就会出现两种真实故障：
    //   - 「后端能索引、前端点开乱码」：注册表在册但 `CONVERTED_DOC_EXT_SET` 漏登记
    //     → 落到 `else` 兜底 → `Blob.text()` 按 UTF-8 解 ZIP/OLE 二进制；
    //   - 「前端能点开、后端拒收」：前端登记了注册表没有的格式 → loader 返回 unsupported。
    // 这里在 `cargo test` 阶段把三处钉死。文件缺失（非完整仓库检出）时跳过。

    /// 从 `const NAME = new Set([...]);` 提取单引号字符串集合
    fn js_set(src: &str, name: &str) -> std::collections::BTreeSet<String> {
        let anchor = format!("const {name} = new Set([");
        let start = src.find(&anchor).unwrap_or_else(|| panic!("未找到 {name}"));
        let rest = &src[start + anchor.len()..];
        let end = rest.find("])").unwrap_or_else(|| panic!("{name} 未闭合"));
        let body: String = rest[..end]
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        body.split('\'')
            .skip(1)
            .step_by(2)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 从 `const NAME = /\.(a|b|c)$/i;` 提取分支集合
    fn js_regex_alternation(src: &str, name: &str) -> std::collections::BTreeSet<String> {
        let anchor = format!("const {name} = /");
        let start = src.find(&anchor).unwrap_or_else(|| panic!("未找到 {name}"));
        let rest = &src[start + anchor.len()..];
        let open = rest.find('(').unwrap_or_else(|| panic!("{name} 缺少分组"));
        let close = rest[open..].find(')').expect("正则分组未闭合") + open;
        rest[open + 1..close].split('|').map(|s| s.to_string()).collect()
    }

    /// 取 `const NAME = { ... };` 的对象字面量块
    fn js_object_block<'a>(src: &'a str, name: &str) -> &'a str {
        let anchor = format!("const {name} = {{");
        let start = src.find(&anchor).unwrap_or_else(|| panic!("未找到 {name}"));
        let rest = &src[start..];
        match rest.find("\n        };") {
            Some(end) => &rest[..end],
            None => rest,
        }
    }

    #[test]
    fn frontend_ext_tables_match_registry() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..");
        let html_path = root.join("main.html");
        let sup_path = root.join("css_js").join("modules").join("support.js");
        if !html_path.is_file() || !sup_path.is_file() {
            eprintln!("[skip] 未找到 main.html / support.js，跳过跨层一致性检查");
            return;
        }
        let html = std::fs::read_to_string(&html_path).expect("读取 main.html");
        let sup = std::fs::read_to_string(&sup_path).expect("读取 support.js");

        // 唯一真源：注册表里所有 Office 类扩展名（由 capabilities 派生，不另抄一份名单）
        let office: std::collections::BTreeSet<String> = registry()
            .kinds()
            .iter()
            .filter(|k| k.caps.source_kind == "office")
            .filter_map(|k| match k.matcher {
                Matcher::Ext(e) => Some(e.to_string()),
                Matcher::FileName(_) => None,
            })
            .collect();
        assert!(!office.is_empty(), "注册表应登记 Office 类格式");

        let doc_set = js_set(&html, "DOC_EXT_SET");
        let converted = js_set(&html, "CONVERTED_DOC_EXT_SET");

        // 1) 需转换预览集合必须被 `DOC_EXT_SET` 覆盖（否则 checkFileExt 直接拒开）
        for ext in &converted {
            assert!(
                doc_set.contains(ext),
                "{} 在 CONVERTED_DOC_EXT_SET 却不在 DOC_EXT_SET → checkFileExt 会拒开",
                ext
            );
        }
        // 2) 需转换预览 = Office 全集 − 有原生渲染器的表格（xls/xlsx 走 SheetJS）
        let expect: std::collections::BTreeSet<String> = office
            .iter()
            .filter(|e| !matches!(e.as_str(), "xls" | "xlsx"))
            .cloned()
            .collect();
        assert_eq!(
            converted, expect,
            "CONVERTED_DOC_EXT_SET（main.html）与注册表不一致"
        );
        // 3) support.js 的正则必须与其一致（supportsEdit / 只读预览语义）
        assert_eq!(
            js_regex_alternation(&sup, "CONVERTED_DOC_RE"),
            expect,
            "CONVERTED_DOC_RE（support.js）与 CONVERTED_DOC_EXT_SET 不一致"
        );
        // 4) 类型分布图必须有每一个 Office 扩展名的分组键（否则统计归类丢失）
        let map_block = js_object_block(&html, "_EXT_TYPE_MAP");
        for ext in &office {
            assert!(
                map_block.contains(&format!("'{ext}':")),
                "_EXT_TYPE_MAP 缺少 {} → 类型分布图归类丢失",
                ext
            );
        }

        // 5) **更强的整体不变式**：后端能索引的扩展名，前端必须能打开。
        //
        // 前端 `checkFileExt` 只放行 `DOC_EXT_SET ∪ CODE_EXT_SET ∪ 视频/图片/RAW`；
        // 后端注册表却在册更多扩展名（例如 `markdown`/`mdown`/`mdx`/`tsv`/`cfg`）。
        // 二者不一致时用户会看到"检索结果里能搜到、点开却提示文件类型不支持"——
        // 这正是 §7.2 F1「否则 checkFileExt 直接拦下」要防的坑，只是漏在了 Markdown
        // 家族与数据文本上。这里把它升级为**全域**不变式，而不是只盯 Office 名单。
        //
        // 只解析注册表源码中 `#[cfg(test)]` **之前**的部分：测试里为对比旧白名单写了
        // 大量扩展名字面量（如 `makefile`），误当注册项会产生假阳性。
        let registry_src = {
            let full = std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/core/document/filekind.rs"),
            )
            .expect("读取 filekind.rs");
            match full.find("#[cfg(test)]") {
                Some(i) => full[..i].to_string(),
                None => full,
            }
        };
        let mut registered: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        // `Matcher::Ext("x")` 直接登记
        {
            let mut rest = registry_src.as_str();
            while let Some(i) = rest.find("Matcher::Ext(\"") {
                rest = &rest[i + "Matcher::Ext(\"".len()..];
                if let Some(j) = rest.find('"') {
                    registered.insert(rest[..j].to_string());
                }
            }
        }
        // `for &ext in &[...]` 批量登记（数组可跨多行；块内注释用反引号，不含双引号）
        {
            let mut rest = registry_src.as_str();
            while let Some(i) = rest.find("for &ext in &[") {
                rest = &rest[i..];
                let Some(end) = rest.find(']') else { break };
                for part in rest[..end].split('"').skip(1).step_by(2) {
                    let p = part.trim();
                    if !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()) {
                        registered.insert(p.to_string());
                    }
                }
                rest = &rest[end..];
            }
        }
        // `DOC_FILE_NAMES`（无扩展名约定文件）：前端 `getExt("Makefile")` 拿到的就是
        // 整个文件名（不含点），因此这些名字必须同样出现在前端的可打开集合里。
        if let Some(i) = registry_src.find("DOC_FILE_NAMES: &[&str] = &[") {
            let rest = &registry_src[i..];
            if let Some(end) = rest.find(']') {
                for part in rest[..end].split('"').skip(1).step_by(2) {
                    let p = part.trim();
                    if !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()) {
                        registered.insert(p.to_string());
                    }
                }
            }
        }
        assert!(
            registered.len() > 40,
            "注册表解析异常（仅解析出 {} 项），本断言将失效",
            registered.len()
        );
        let mut openable: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for set_name in [
            "DOC_EXT_SET",
            "CODE_EXT_SET",
            "_VIDEO_EXT_SET",
            "_IMAGE_EXT_SET",
            "_PIC_IMAGE_EXT_SET",
            "_RAW_EXT_SET",
        ] {
            openable.extend(js_set(&html, set_name));
        }
        let unreachable: Vec<&String> = registered.difference(&openable).collect();
        assert!(
            unreachable.is_empty(),
            "以下扩展名后端可索引、但前端 checkFileExt 会拒开（用户在检索结果里点不开）：{:?}",
            unreachable
        );
    }

    /// WPS 仍不支持（决策 Q3）
    #[test]
    fn wps_is_still_unsupported() {
        assert!(registry().lookup_ext("wps").is_none(), "wps 需自行嗅探，本版不支持（Q3）");
    }

    /// PDF 在 Phase 1 走 pdf-inspector（逐页 Markdown + 页码 provenance）
    #[test]
    fn pdf_uses_pdf_inspector_in_phase1() {
        let k = registry().lookup_ext("pdf").expect("pdf 必须在册");
        assert_eq!(k.converter, Converter::PdfInspector);
        assert_eq!(
            k.form,
            DocumentForm::Markdown,
            "PDF 转换产物是 Markdown → 必须复用 AST 语义分块（C3）"
        );
        assert!(k.caps.binary, "pdf 必须标记 binary（禁止 UTF-8 直读）");
        assert!(k.caps.paginated, "pdf 必须标记 paginated");
        assert!(!k.caps.doc_like, "pdf 不属 Document 意图（保持既有检索语义）");
        assert_eq!(k.caps.source_kind, "pdf");
    }

    #[test]
    fn form_routing_is_consistent_with_capabilities() {
        for k in registry().kinds() {
            match (k.form, k.matcher) {
                (DocumentForm::Code, m) => {
                    assert!(k.caps.is_code, "form=Code 必须 is_code=true（{:?}）", m)
                }
                (DocumentForm::Tree, m) => assert!(k.caps.outline, "form=Tree 必须 outline=true（{:?}）", m),
                _ => {}
            }
            // 反向不变式成立：统计分类为 "Markdown" 的文件一定走 Markdown 分块。
            // （正向不成立且**刻意如此**：`pdf` 是 form=Markdown 但 category="其他" ——
            //  它由 Markdown 分块器处理，统计口径仍保持改造前的取值以免再次改动统计图。）
            if k.category == "Markdown" {
                assert_eq!(
                    k.form,
                    DocumentForm::Markdown,
                    "分类为 Markdown 的文件必须走 Markdown 分块（{:?}）",
                    k.matcher
                );
            }
        }
        // Markdown 家族（frontmatter 生效的那批）必须是 category=Markdown + form=Markdown
        for ext in ["md", "markdown", "mdown", "rst"] {
            let k = registry().lookup_ext(ext).expect("Markdown 家族应在册");
            assert_eq!(k.form, DocumentForm::Markdown, "{} 应走 Markdown 分块", ext);
            assert_eq!(k.category, "Markdown", "{} 统计分类应为 Markdown", ext);
            assert!(k.caps.doc_like, "{} 应属 Document 意图", ext);
        }
    }

    #[test]
    fn lookup_prefers_filename_rule() {
        let r = registry();
        let k = r.lookup("docs/Dockerfile").expect("Dockerfile 应命中");
        assert_eq!(k.matcher, Matcher::FileName("dockerfile"));
        assert_eq!(k.caps.source_kind, "text");
    }
}
