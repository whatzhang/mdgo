//! 文档 AST 节点模型。

/// 文档节点类型。
///
/// 与 comrak 顶层块级节点一一对应，语义与 Markdown 渲染一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeType {
    /// 文档根节点
    Root,
    /// 标题（# ~ ######），`metadata.level` 记录层级
    Heading,
    /// 段落（含行内格式化，文本取原始源码行）
    Paragraph,
    /// 代码块（围栏/缩进，文本含围栏原样保留）
    CodeBlock,
    /// 表格（GFM）
    Table,
    /// 列表（有序/无序，文本为整个列表的原始源码）
    List,
    /// 引用块
    Quote,
    /// 主题分割线（--- / ***）
    ThematicBreak,
    /// 内联 HTML 块
    HtmlBlock,
}

impl NodeType {
    /// 人类可读的类型名（用于 chunk_type 元数据）
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeType::Root => "root",
            NodeType::Heading => "heading",
            NodeType::Paragraph => "paragraph",
            NodeType::CodeBlock => "code",
            NodeType::Table => "table",
            NodeType::List => "list",
            NodeType::Quote => "quote",
            NodeType::ThematicBreak => "hr",
            NodeType::HtmlBlock => "html",
        }
    }
}

/// 节点元数据。
#[derive(Debug, Clone, Default)]
pub struct NodeMetadata {
    /// 标题层级（仅 Heading 节点有值，1~6）
    pub level: Option<u8>,
    /// 节点在源码中的起始行号（1-based，含），用于原文切片
    pub start_line: usize,
    /// 节点在源码中的结束行号（1-based，含）
    pub end_line: usize,
}

/// 文档 AST 节点。
///
/// 结构约定：
/// - `Root` 的 children 为顶层块
/// - `Heading` 的 children 为其下所有子块（含嵌套标题），直到同级/更高级标题出现
/// - 叶子内容节点（Paragraph/CodeBlock/...）的 `content` 为原始源码文本
#[derive(Debug, Clone)]
pub struct DocumentNode {
    pub node_type: NodeType,
    pub content: String,
    pub children: Vec<DocumentNode>,
    pub metadata: NodeMetadata,
}

impl DocumentNode {
    pub fn new(node_type: NodeType, content: impl Into<String>) -> Self {
        Self {
            node_type,
            content: content.into(),
            children: Vec::new(),
            metadata: NodeMetadata::default(),
        }
    }

    pub fn is_heading(&self) -> bool {
        self.node_type == NodeType::Heading
    }
}
