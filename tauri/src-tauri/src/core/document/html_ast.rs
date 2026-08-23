//! HTML → 语义 AST（Semantic AST）解析器。
//!
//! 用 `scraper` 解析 HTML 为 [`DocumentNode`] 树（标题层级 + 块级节点），
//! 与 `ComrakMarkdownParser` 的输出结构约定一致，可直接喂给
//! [`crate::core::document::chunk_engine::SemanticChunkEngine`] 做语义分块。
//!
//! 这是 P2 HTML chunk 解析的结构中间层：HTML 文档经此归一化为
//! Markdown 同构的 AST，后续可扩展正文提取（readability）、
//! HTML→Markdown 转换（htmd）与消毒（ammonia）能力。

use scraper::{ElementRef, Html, Selector};

use super::node::{DocumentNode, NodeMetadata, NodeType};

/// HTML 文档解析器（scraper 实现）。
pub struct HtmlDocumentParser;

impl HtmlDocumentParser {
    /// 解析 HTML 文档为文档树（Root 始终存在）。
    ///
    /// 结构约定（与 Markdown 解析器一致）：
    /// - Root children 为顶层块
    /// - Heading children 为其下所有子块，直到同级/更高级标题出现
    /// - 容器元素（div/section/article 等）递归展开，标题层级栈生效；
    ///   叶子块（p/pre/table/list/quote/hr）直接挂载，content 为元素文本
    pub fn parse(&self, html: &str) -> Result<DocumentNode, String> {
        if html.trim().is_empty() {
            return Err("HTML 内容为空".to_string());
        }
        let doc = Html::parse_document(html);
        let mut root = DocumentNode::new(NodeType::Root, "");
        // 标题层级栈：栈顶为当前最近的父标题（新标题挂其 children）；空 = 挂 Root
        let mut heading_stack: Vec<(u8, DocumentNode)> = Vec::new();
        // 只遍历 body（parse_document 保证有 body；无则回退根元素）
        let body = Selector::parse("body")
            .ok()
            .and_then(|sel| doc.select(&sel).next())
            .unwrap_or_else(|| doc.root_element());
        for child in body.child_elements() {
            Self::walk_element(&child, &mut root, &mut heading_stack);
        }
        // 收尾：栈内标题互为父子（后入栈的挂在先入栈者的 children 中，是 clone 副本），
        // 把子标题的完整子树合并进父标题中的对应副本，最后将栈底标题挂 Root。
        for i in (1..heading_stack.len()).rev() {
            let child_level = heading_stack[i].0;
            let child_content = heading_stack[i].1.content.clone();
            let child_children = heading_stack[i].1.children.clone();
            let parent_children = &mut heading_stack[i - 1].1.children;
            if let Some(child_h) = parent_children.iter_mut().find(|c| {
                c.node_type == NodeType::Heading
                    && c.metadata.level == Some(child_level)
                    && c.content == child_content
            }) {
                child_h.children = child_children;
            }
        }
        if let Some((_, bottom)) = heading_stack.first() {
            root.children.push(bottom.clone());
        }
        Ok(root)
    }

    /// 深度优先遍历元素：标题维护层级栈；容器递归、叶子块挂载。
    fn walk_element(
        el: &ElementRef<'_>,
        root: &mut DocumentNode,
        heading_stack: &mut Vec<(u8, DocumentNode)>,
    ) {
        let name: String = el.value().name.local.as_ref().to_string();
        // 标题：弹出同级及更深的旧标题后，挂到栈顶父标题（无则挂 Root）再入栈
        if let Some(level) = parse_heading_level(&name) {
            while let Some((lvl, _)) = heading_stack.last() {
                if *lvl >= level {
                    heading_stack.pop();
                } else {
                    break;
                }
            }
            let node = DocumentNode {
                node_type: NodeType::Heading,
                content: el.text().collect::<String>().trim().to_string(),
                children: Vec::new(),
                metadata: NodeMetadata {
                    level: Some(level),
                    ..NodeMetadata::default()
                },
            };
            match heading_stack.last_mut() {
                // 挂到栈顶父标题（clone 副本；栈底不挂 Root，收尾统一处理）
                Some((_, parent)) => parent.children.push(node.clone()),
                None => {}
            }
            heading_stack.push((level, node));
            return;
        }
        match name.as_str() {
            // 非内容元素跳过（D3：footer 版权 / aside 侧边栏为常见噪音源，不参与索引）
            "script" | "style" | "nav" | "iframe" | "noscript" | "svg" | "form" | "button"
            | "input" | "select" | "textarea" | "label" | "footer" | "aside" => return,
            // 容器元素：含块级子元素时递归（标题层级栈生效）；否则作为段落叶子挂载
            // （修复 `<div>hello <a>link</a></div>` 裸文本丢失）
            "div" | "section" | "article" | "main" | "header" | "figure"
            | "details" | "summary" | "span" | "a" | "strong" | "em" | "b" | "i" | "u"
            | "small" | "mark" | "sub" | "sup" => {
                let has_block_child = el.children().filter_map(ElementRef::wrap).any(|ce| {
                    let n: String = ce.value().name.local.as_ref().to_string();
                    matches!(
                        n.as_str(),
                        "div" | "section" | "article" | "p" | "table" | "ul" | "ol" | "pre"
                            | "blockquote" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "main"
                            | "header" | "footer" | "figure" | "details"
                    )
                });
                if has_block_child {
                    for child in el.children() {
                        if let Some(ce) = ElementRef::wrap(child) {
                            Self::walk_element(&ce, root, heading_stack);
                        }
                    }
                } else {
                    let content = el.text().collect::<String>().trim().to_string();
                    if !content.is_empty() {
                        let node = DocumentNode::new(NodeType::Paragraph, content);
                        match heading_stack.last_mut() {
                            Some((_, heading)) => heading.children.push(node),
                            None => root.children.push(node),
                        }
                    }
                }
            }
            // 叶子块：映射类型并挂载（文本 trim；空内容跳过，hr 除外）
            _ => {
                let node_type = match name.as_str() {
                    "p" => NodeType::Paragraph,
                    "pre" => NodeType::CodeBlock,
                    "table" => NodeType::Table,
                    "ul" | "ol" | "li" => NodeType::List,
                    "blockquote" => NodeType::Quote,
                    "hr" => NodeType::ThematicBreak,
                    _ => NodeType::HtmlBlock,
                };
                let content = el.text().collect::<String>().trim().to_string();
                if content.is_empty() && node_type != NodeType::HtmlBlock && node_type != NodeType::ThematicBreak {
                    return;
                }
                let node = DocumentNode::new(node_type, content);
                match heading_stack.last_mut() {
                    Some((_, heading)) => heading.children.push(node),
                    None => root.children.push(node),
                }
            }
        }
    }
}

/// 解析标题层级：`h1`~`h6` → 1~6，其余 → None。
fn parse_heading_level(tag: &str) -> Option<u8> {
    let mut chars = tag.chars();
    if chars.next() != Some('h') {
        return None;
    }
    let rest: String = chars.collect();
    match rest.as_str() {
        "1" => Some(1),
        "2" => Some(2),
        "3" => Some(3),
        "4" => Some(4),
        "5" => Some(5),
        "6" => Some(6),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::document::node::NodeType;

    /// 标题层级嵌套应与 Markdown 解析器约定一致：h2 是 h1 的 child，段落挂在所属标题下。
    #[test]
    fn heading_nesting_matches_markdown_convention() {
        let parser = HtmlDocumentParser;
        let doc = parser
            .parse("<body><h1>Title</h1><p>intro</p><h2>Sub</h2><p>body</p></body>")
            .unwrap();
        assert_eq!(doc.children.len(), 1, "顶层应只有 h1");
        let h1 = &doc.children[0];
        assert_eq!(h1.node_type, NodeType::Heading);
        assert_eq!(h1.metadata.level, Some(1));
        assert!(
            h1.children.iter().any(|c| c.node_type == NodeType::Paragraph && c.content.contains("intro")),
            "h1 children = {:?}",
            h1.children.iter().map(|c| (c.node_type.as_str().to_string(), c.content.clone())).collect::<Vec<_>>()
        );
        let h2 = h1
            .children
            .iter()
            .find(|c| c.node_type == NodeType::Heading && c.metadata.level == Some(2))
            .expect("h2 应为 h1 的子节点");
        assert!(h2.children.iter().any(|c| c.node_type == NodeType::Paragraph && c.content.contains("body")));
    }

    /// 无块级子元素的容器（如 `<div>hello <a>link</a></div>`）应保留裸文本为段落。
    #[test]
    fn inline_container_preserves_bare_text() {
        let parser = HtmlDocumentParser;
        let doc = parser.parse("<body><div>hello <a>link</a></div></body>").unwrap();
        assert!(doc
            .children
            .iter()
            .any(|c| c.node_type == NodeType::Paragraph
                && c.content.contains("hello")
                && c.content.contains("link")));
    }
}
