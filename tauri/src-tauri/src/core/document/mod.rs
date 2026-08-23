//! Document AST 层：将 Markdown 解析为结构化文档树。
//!
//! 设计目标：
//! - 保留 Markdown 结构（标题层级、段落、代码块、表格、列表、引用）
//! - 为 SemanticChunkEngine 提供 `DocumentNode` 输入
//! - 为后续 GraphRAG / 实体抽取 / 摘要提供统一结构基础
//!
//! 模块划分：
//! - `node.rs`：节点模型（DocumentNode / NodeType / NodeMetadata）
//! - `parser.rs`：解析器 trait
//! - `markdown.rs`：comrak 实现（CommonMark/GFM 完整规范）
//! - `text_split.rs`：纯文本切分工具（基础层，供 db::utils 复用）
//! - `chunk_engine.rs`：AST 语义分块引擎（Chunk / SemanticChunkEngine）

pub mod chunk_engine;
pub mod html_ast;
pub mod html_clean;
pub mod markdown;
pub mod node;
pub mod parser;
pub mod text_split;
pub mod token_budget;

pub use parser::MarkdownParser;
pub use markdown::ComrakMarkdownParser;
