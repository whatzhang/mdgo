//! 查询理解层：把字符串查询结构化为可执行的 [`QueryPlan`]。
//!
//! # 演进路线（不废弃规则，逐步增强）
//! 1. 第一阶段（当前）：规则路由（`route_intent`）→ 结构化 QueryPlan
//! 2. 第二阶段：规则 + QueryPlan 补充信号（符号提取、过滤白名单）
//! 3. 第三阶段（预留）：LLM Planner 生成 `{"intent": "code_search"}` 等结构化指令
//!
//! 检索必须独立于 LLM 网关工作（Agent 循环中每次检索若额外调用 LLM 会叠加
//! 延迟与失败点），因此路由保持零 LLM 开销的规则实现。

/// 检索意图（轻量级规则路由）。
///
/// 不同意图限定不同的候选文件范围（元数据过滤），并决定是否注入代码符号命中，
/// 从而减少跨类型文件的噪声匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrievalIntent {
    /// 代码相关查询（函数/类/符号/接口等）→ 仅检索代码类文件
    Code,
    /// 文档/笔记查询（Markdown 等）→ 仅检索文档类文件
    Document,
    /// 大纲类查询（OPML/FreeMind 思维导图）→ 仅检索大纲类文件
    Outline,
    /// 通用查询 → 不限定文件类型
    General,
}

/// 代码文件扩展名统一清单。
///
/// 单一来源：`classify_ext`（索引类型统计）与 `intent_allowed_exts`（意图过滤）
/// 共用本清单，避免两套列表漂移导致"已索引的代码文件在 Code 意图下被漏检"。
pub const CODE_EXTENSIONS: &[&str] = &[
    "py", "js", "ts", "rs", "go", "java", "lua", "sh", "bat", "sql", "yaml", "yml", "toml",
    "conf", "c", "cpp", "cc", "h", "hpp", "rb", "php",
];

/// 意图 → 允许检索的文件扩展名白名单（候选过滤条件，检索前确定）。
fn intent_allowed_exts(intent: RetrievalIntent) -> Option<&'static [&'static str]> {
    match intent {
        RetrievalIntent::Code => Some(CODE_EXTENSIONS),
        RetrievalIntent::Document => Some(&["md", "markdown", "mdown", "rst", "txt"]),
        RetrievalIntent::Outline => Some(&["opml", "mm"]),
        RetrievalIntent::General => None,
    }
}

/// 查询计划：一次检索的完整决策输入（查询理解层的产物）。
///
/// 由 [`QueryPlanner`] 生成，供检索管线（候选过滤 / 多路召回 / 融合）消费。
/// 本阶段为"规则路由"实现，后续可平滑演进为 LLM Planner 生成同一结构。
#[derive(Debug, Clone)]
pub struct QueryPlan {
    /// 检索意图
    pub intent: RetrievalIntent,
    /// 文件扩展名白名单（`None` = 不限类型）。检索前确定，作为候选过滤前置条件。
    pub allowed_exts: Option<&'static [&'static str]>,
    /// 疑似代码符号的标识符 token（仅 Code 意图有值），用于符号路召回
    pub symbols: Vec<String>,
    /// 显式标签过滤（A3/C1：`tag:xxx` / `标签:xxx` 语法）——LanceDB only_if 下推
    pub tags: Vec<String>,
}

/// 查询计划器抽象（依赖倒置：检索管线只依赖 trait，不感知具体路由实现）。
pub trait QueryPlanner: Send + Sync {
    /// 将原始查询文本结构化为查询计划。
    fn plan(&self, query: &str) -> QueryPlan;
}

/// 规则查询计划器（第一阶段实现）。
///
/// 基于现有 `route_intent` 规则路由 + 符号提取，不引入任何模型开销。
pub struct RuleQueryPlanner;

impl QueryPlanner for RuleQueryPlanner {
    fn plan(&self, query: &str) -> QueryPlan {
        let intent = route_intent(query);
        let symbols = if intent == RetrievalIntent::Code {
            extract_symbol_tokens(query)
        } else {
            Vec::new()
        };
        QueryPlan {
            intent,
            allowed_exts: intent_allowed_exts(intent),
            symbols,
            tags: extract_tag_filters(query),
        }
    }
}

/// 提取显式标签过滤（`tag:rag` / `标签:redis`）：用于 metadata 过滤下推。
///
/// 支持中英文标签词（不含空白/标点）；`[^A-Za-z0-9_]` 为显式 ASCII 字符类
/// （`\w` 默认 Unicode 会把汉字算词字符，故显式列出），允许中文前缀
/// （"找一下标签:redis" 可命中）且排除 "retag:xx" 类误匹配；无匹配返回空。
fn extract_tag_filters(query: &str) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?:^|[^A-Za-z0-9_])(?:tag|标签)\s*:\s*([^\s,，。;；]+)").unwrap()
    });
    re.captures_iter(query)
        .filter_map(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
        .filter(|t| !t.is_empty())
        .collect()
}

/// 根据查询文本进行轻量级意图路由（规则启发式，无 LLM 开销）。
///
/// 优先判定代码（符号/关键字/代码语法特征/显式扩展名），其次大纲（opml/思维导图等），
/// 再文档（readme/markdown 等），默认通用。
pub fn route_intent(query: &str) -> RetrievalIntent {
    // P0-2：显式代码扩展名（`config.rs`、`.py 文件` 等）→ 直接 Code（精确文件检索）
    if has_explicit_extension(query) {
        return RetrievalIntent::Code;
    }
    if is_code_query(query) {
        return RetrievalIntent::Code;
    }
    let lower = query.to_lowercase();
    // 仅使用具象的大纲文件格式词，避免泛化词"大纲"把普通文档查询误路由为 Outline
    let outline_kws = [
        "opml", "freemind", "free mind", "outline", "知识库中的思维导图", "知识库中的大纲笔记",
        "opml文件", "mm文件",
    ];
    if outline_kws.iter().any(|kw| lower.contains(kw)) {
        return RetrievalIntent::Outline;
    }
    // 仅使用名词性文档词，避免动词性"说明/解释"等把中文代码提问误路由为文档
    let doc_kws = ["readme", "markdown", "文档", "笔记", "文章"];
    if doc_kws.iter().any(|kw| lower.contains(kw)) {
        return RetrievalIntent::Document;
    }
    RetrievalIntent::General
}

/// 查询是否包含显式代码扩展名（`.rs`、`.py` 等，词边界保护避免 "5.0" 误匹配）。
///
/// 扩展名清单与 [`CODE_EXTENSIONS`] 保持一致（避免"路由为 Code 但过滤白名单不含该扩展名"的漏检）。
/// 🟠 M18 修复：同时校验**前导边界**与**后随边界**——扩展名前必须是文件名主干字符
/// （字母/数字/下划线/连字符），扩展名后必须是结尾或非词延续字符，排除
/// `config.rs_backup`、`config.rs-old`、`.rsx`、`config.rst` 等误匹配。
fn has_explicit_extension(query: &str) -> bool {
    CODE_EXTENSIONS.iter().any(|ext| {
        let needle = format!(".{}", ext);
        let bytes = query.as_bytes();
        let mut start = 0usize;
        while let Some(rel) = query[start..].find(&needle) {
            let idx = start + rel;
            if idx > 0 {
                let before = bytes[idx - 1];
                if !(before.is_ascii_alphanumeric() || before == b'_' || before == b'-') {
                    start = idx + needle.len();
                    continue;
                }
            }
            let after = idx + needle.len();
            let ok_after = after >= bytes.len()
                || !(bytes[after].is_ascii_alphanumeric()
                    || bytes[after] == b'_'
                    || bytes[after] == b'-');
            if ok_after {
                return true;
            }
            start = after;
        }
        false
    })
}

/// 检测查询是否为"代码风格"的查询。
///
/// 代码查询的特征（满足任一即可）：
/// - 包含 `::`、`->`、`()` 等代码特定符号
/// - 包含 CamelCase 标识符（如 "LRUCache"、"parseJSON"）
/// - 包含 snake_case 标识符（如 "lru_cache"、"index_all"）
/// - 包含代码文件扩展名（如 ".py"、".rs"、".ts"）
/// - 包含常见代码关键字（function、class、fn、def、struct）
fn is_code_query(query: &str) -> bool {
    //关键词
    let lower = query.to_lowercase();
    let outline_kws = ["知识库中的代码"];
    if outline_kws.iter().any(|kw| lower.contains(kw)) {
        return true;
    }

    let mut has_camel = false;
    // CamelCase 检测：连续大写字母 + 小写字母（如 "LRUCache", "parseJSON"）
    if query.chars().any(|c| c.is_uppercase()) {
        // 至少一个完整的词包含大小写混合
        has_camel = query
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| {
                word.len() >= 3
                    && word.chars().any(|c| c.is_uppercase())
                    && word.chars().any(|c| c.is_lowercase())
                    && !word.chars().all(|c| c.is_uppercase())
            });
    }
    // snake_case 检测（'_' 视为词内字符，避免 "handle_timeout" 被拆成两段而漏判）
    let has_snake = query
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|word| word.contains('_') && word.len() >= 3);
    // 🟠 P0-1 修复：裸标识符 + 代码定位词 → Code（旧实现要求 `::`/`->` 语法特征，
    // 使 "RrfConfig 结构体定义"/"SCHEMA_VERSION 常量在哪"/"ValidationReport 有哪些字段"
    // 类查询路由到 General，丢失符号路召回——v3 基线 q026/q027/q028/q039 recall=0）。
    // 定位词从紧（"定义/在哪/常量/结构体/字段"等明确指向代码实体），避免误伤
    // 文档类查询（"README 文档"、"FTP 服务器部署"等无标识符或无数码定位词不受影响）。
    let code_locators = [
        "定义", "在哪", "常量", "结构体", "字段", "变量", "枚举", "函数", "方法",
        "实现", "声明", "源码", "代码", "写法", "签名",
    ];
    if (has_camel || has_snake)
        && code_locators.iter().any(|kw| lower.contains(kw))
    {
        return true;
    }
    // 代码语法特征
    if (has_camel || has_snake) && (query.contains("::") || query.contains("->")) {
        return true;
    }
    // 代码文件扩展名（🟠 M18：与 CODE_EXTENSIONS 单一来源一致，含 c/cpp/h/rb/php）
    let has_code_ext = CODE_EXTENSIONS
        .iter()
        .any(|ext| query.contains(&format!(".{}", ext)));
    if has_code_ext {
        let code_keywords = [
            "function", "class", "struct", "enum", "trait", "interface", "namespace", "lambda",
            "async", "await", "callback", "prototype", "constructor", "方法", "函数", "变量",
            "常量", "类", "算法", "数据结构",
        ];
        if code_keywords.iter().any(|kw| lower.contains(kw)) {
            return true;
        }
        if query.contains("()") {
            return true;
        }
    }
    false
}

/// 扩展查询的类型（P2 预检索优化器：查询变体携带检索语义，供下游路由/去重/证据融合消费）。
///
/// 与 [`RetrievalIntent`]（文件范围过滤）正交：kind 描述"这条查询在检索什么角度"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryKind {
    /// 关键词聚焦：剔除语气词，提取核心实体+动作的紧凑短语（BM25 友好）
    Keyword,
    /// 实体精准：围绕具体实体/符号的完整查询（符号/精确匹配友好）
    Entity,
    /// 同义场景扩展：同义词/领域术语替换表述（语义向量友好）
    Semantic,
}

impl Default for QueryKind {
    fn default() -> Self {
        QueryKind::Keyword
    }
}

/// 扩展查询语义去重（P0-4，确定性、零 LLM 开销）。
///
/// 在扩展查询向量化后调用（向量反正都要计算，去重零额外推理）：
/// - 与原始查询向量 cosine ≥ `threshold` → 丢弃（无增量价值）
/// - 与已保留扩展向量 cosine ≥ `threshold` → 丢弃
/// - 新增实体的查询不因语义相近被误杀（是否含新实体由调用方结合 kind/符号判定）
///
/// 返回保留的扩展查询下标（保持原顺序），最多 `max_keep` 个。
pub fn dedup_expanded_queries(
    original_vec: Option<&[f32]>,
    vectors: &[Vec<f32>],
    threshold: f32,
    max_keep: usize,
) -> Vec<usize> {
    let mut kept: Vec<usize> = Vec::new();
    for (i, vec) in vectors.iter().enumerate() {
        if kept.len() >= max_keep {
            break;
        }
        if let Some(ov) = original_vec {
            if crate::core::db::utils::cosine_similarity(ov, vec) >= threshold as f64 {
                continue;
            }
        }
        let dup = kept
            .iter()
            .any(|&k| crate::core::db::utils::cosine_similarity(&vectors[k], vec) >= threshold as f64);
        if !dup {
            kept.push(i);
        }
    }
    kept
}

/// 预检索是否值得调用 LLM 查询扩展（规则门，零 LLM 开销）。
///
/// 直接跳过的场景：空/超短查询、明显的文件名/路径、纯代码符号查询
/// （这些场景 LLM 扩展无增益，且浪费一次调用与检索预算）。
pub fn should_expand(query: &str, plan: &QueryPlan) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return false;
    }
    if q.chars().count() < 8 {
        return false; // 超短查询
    }
    if plan.intent == RetrievalIntent::Code && !plan.symbols.is_empty() {
        return false; // 纯符号/精确代码检索（符号路由已覆盖）
    }
    if is_file_path_like(q) {
        return false; // main.rs / Cargo.toml / a/b.ts
    }
    true
}

/// 是否为"文件名/路径"类查询：单 token 带扩展名，或含路径分隔符。
fn is_file_path_like(q: &str) -> bool {
    if q.contains('/') || q.contains('\\') {
        return true;
    }
    let tokens: Vec<&str> = q.split(|c: char| c.is_whitespace()).collect();
    if tokens.len() == 1 {
        let t = tokens[0];
        if let Some(dot) = t.rfind('.') {
            let ext = &t[dot + 1..];
            if !ext.is_empty()
                && ext.len() <= 5
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_expand_gates_trivial_queries() {
        let general = QueryPlan {
            intent: RetrievalIntent::General,
            allowed_exts: None,
            symbols: Vec::new(),
            tags: Vec::new(),
        };
        assert!(!should_expand("", &general));
        assert!(!should_expand("Redis", &general)); // 超短
        assert!(!should_expand("main.rs", &general)); // 文件名
        assert!(!should_expand("src/lib/core.rs", &general)); // 路径
        assert!(should_expand("为什么项目里的 Redis 锁这样实现？", &general)); // 正常问题
    }

    #[test]
    fn should_expand_skips_pure_symbol_queries() {
        let code = QueryPlan {
            intent: RetrievalIntent::Code,
            allowed_exts: Some(&["rs"]),
            symbols: vec!["handleTimeout".into()],
            tags: Vec::new(),
        };
        assert!(!should_expand("handleTimeout 在哪里定义的", &code));
        let code_no_symbol = QueryPlan {
            intent: RetrievalIntent::Code,
            allowed_exts: Some(&["rs"]),
            symbols: Vec::new(),
            tags: Vec::new(),
        };
        assert!(should_expand("Rust 的异步错误处理", &code_no_symbol));
    }

    #[test]
    fn tag_filter_extraction() {
        let p = RuleQueryPlanner.plan("tag:rag 的笔记");
        assert_eq!(p.tags, vec!["rag"]);
        let p2 = RuleQueryPlanner.plan("找一下标签:redis 相关的文档");
        assert_eq!(p2.tags, vec!["redis"]);
        let p3 = RuleQueryPlanner.plan("普通查询没有标签语法");
        assert!(p3.tags.is_empty());
    }

    #[test]
    fn route_intent_explicit_extension_is_code() {
        // P0-2：显式扩展名 → Code（无论是否含代码关键字）
        assert_eq!(route_intent("config.rs 在哪里"), RetrievalIntent::Code);
        assert_eq!(route_intent("src/tools/parse_json.py 的实现"), RetrievalIntent::Code);
        assert_eq!(route_intent("看下 indexer.rs"), RetrievalIntent::Code);
        // 🟠 M18：清单新增的语言扩展名同样路由 Code
        assert_eq!(route_intent("main.c 的实现"), RetrievalIntent::Code);
        assert_eq!(route_intent("utils.cpp 在哪"), RetrievalIntent::Code);
        assert_eq!(route_intent("types.h 文件"), RetrievalIntent::Code);
        // 词边界保护：不得误匹配 "5.0"、"config.rst"
        assert_eq!(route_intent("版本 5.0 的说明"), RetrievalIntent::General);
        assert_eq!(route_intent("config.rst 文档"), RetrievalIntent::Document);
        // 🟠 M18：前后边界——".rs" 后接 "_"/"-" 或前无文件名主干时不得误判 Code
        assert_eq!(route_intent("config.rs_backup 的说明"), RetrievalIntent::General);
        assert_eq!(route_intent("config.rs-old 的说明"), RetrievalIntent::General);
        assert_eq!(route_intent("看下 .rs 配置"), RetrievalIntent::General);
        // 文档扩展名不进代码清单
        assert_eq!(route_intent("分块 Token 预算设计.md 讲了什么"), RetrievalIntent::General);
    }

    /// 🟠 P0-1 回归：裸标识符 + 代码定位词 → Code（旧实现要求 `::`/`->`，
    /// 使 "RrfConfig 结构体定义" 类查询路由到 General 丢失符号路——v3 基线 recall=0）
    #[test]
    fn route_intent_bare_identifier_with_locator_is_code() {
        // 应路由 Code（恢复符号路召回）
        assert_eq!(route_intent("RrfConfig 结构体定义"), RetrievalIntent::Code);
        assert_eq!(route_intent("SCHEMA_VERSION 常量在哪"), RetrievalIntent::Code);
        assert_eq!(route_intent("ValidationReport 有哪些字段"), RetrievalIntent::Code);
        assert_eq!(route_intent("LocalBgeReranker 在哪个文件"), RetrievalIntent::Code);
        assert_eq!(route_intent("index_all 函数的实现"), RetrievalIntent::Code);
        assert_eq!(route_intent("tokenize_with_offsets 的签名"), RetrievalIntent::Code);
        // 反例：无标识符或无数码定位词 → 不被误路由
        assert_eq!(route_intent("README 文档讲了什么"), RetrievalIntent::Document);
        assert_eq!(route_intent("FTP 服务器的部署说明"), RetrievalIntent::General);
        assert_eq!(route_intent("混合检索的设计文档"), RetrievalIntent::Document);
        assert_eq!(route_intent("版本 5.0 的说明"), RetrievalIntent::General);
    }
}

/// 从查询中提取疑似代码符号的标识符 token（CamelCase / snake_case）。
///
/// 中文与数字 token 会被过滤（汉字的 is_uppercase/is_lowercase 均为 false），
/// 仅保留形如 `handleTimeout`、`lru_cache`、`parseJSON` 的标识符，用于
/// 代码符号精确检索（见符号路召回）与符号实体发现（P2 预检索优化器）。
pub fn extract_symbol_tokens(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for t in query.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let t = t.trim();
        if t.len() < 2 || t.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let has_upper = t.chars().any(|c| c.is_uppercase());
        let has_lower = t.chars().any(|c| c.is_lowercase());
        if (has_upper && has_lower) || t.contains('_') {
            tokens.push(t.to_string());
        }
    }
    tokens
}
