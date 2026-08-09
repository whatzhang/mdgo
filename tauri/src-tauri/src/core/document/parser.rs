//! Markdown 解析器 trait：将 Markdown 源码解析为 `DocumentNode` 树。

use super::node::DocumentNode;

/// Markdown 解析器统一接口。
///
/// 实现方负责将文本解析为结构化的 [`DocumentNode`] 树：
/// - 标题成为层级结构节点，其下挂载子块
/// - 段落/代码块/表格/列表/引用保留为叶子内容节点
/// - 节点 `content` 保持与源码渲染一致（原文切片）
pub trait MarkdownParser {
    /// 解析 Markdown 文本为文档树。
    ///
    /// 返回的 Root 节点始终存在；`ignore_setext` 控制是否识别 Setext 标题
    /// （`===` / `---` 二级标题）。
    fn parse(&self, text: &str, ignore_setext: bool) -> DocumentNode;
}
