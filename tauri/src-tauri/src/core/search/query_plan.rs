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
    "conf",
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
        }
    }
}

/// 根据查询文本进行轻量级意图路由（规则启发式，无 LLM 开销）。
///
/// 优先判定代码（符号/关键字/代码语法特征），其次大纲（opml/思维导图等），
/// 再文档（readme/markdown 等），默认通用。
pub fn route_intent(query: &str) -> RetrievalIntent {
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
    // 代码语法特征
    if (has_camel || has_snake) && (query.contains("::") || query.contains("->")) {
        return true;
    }
    // 代码文件扩展名
    if query.contains(".py")
        || query.contains(".rs")
        || query.contains(".ts")
        || query.contains(".js")
        || query.contains(".go")
        || query.contains(".java")
        || query.contains(".cpp")
        || query.contains(".c")
        || query.contains(".h")
        || query.contains(".rb")
        || query.contains(".php")
    {
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

/// 从查询中提取疑似代码符号的标识符 token（CamelCase / snake_case）。
///
/// 中文与数字 token 会被过滤（汉字的 is_uppercase/is_lowercase 均为 false），
/// 仅保留形如 `handleTimeout`、`lru_cache`、`parseJSON` 的标识符，用于
/// 代码符号精确检索（见符号路召回）。
fn extract_symbol_tokens(query: &str) -> Vec<String> {
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
