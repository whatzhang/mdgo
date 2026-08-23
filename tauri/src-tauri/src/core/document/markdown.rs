//! comrak 实现的 Markdown 解析器。
//!
//! 将 CommonMark/GFM 完整解析为 [`DocumentNode`] 树：
//! - 顶层块级节点按标题层级组织成树（Heading 为结构节点，挂载其下子块）
//! - 内容节点文本取**原始源码行切片**（sourcepos），保证与渲染一致
//! - 剥离 YAML FrontMatter（Obsidian 笔记），避免污染结构

use comrak::nodes::{Node, NodeValue};
use comrak::{Arena, Options};

use super::node::{DocumentNode, NodeType};
use super::parser::MarkdownParser;

/// 使用 comrak 解析 Markdown 的解析器。
pub struct ComrakMarkdownParser;

/// FrontMatter 最大行数（超过视为普通文档）
const FRONTMATTER_MAX_LINES: usize = 50;

impl MarkdownParser for ComrakMarkdownParser {
    fn parse(&self, text: &str, ignore_setext: bool) -> DocumentNode {
        // 唯一 normalize 点：统一换行符（消除 Windows \r\n 对行号切片的影响）+ 剥离 YAML FrontMatter
        let uniform_text = text.replace("\r\n", "\n");
        let body_text = strip_frontmatter(&uniform_text);

        let mut options = Options::default();
        options.parse.ignore_setext = ignore_setext;
        // GFM 扩展：由运行时 Options 控制（与 cargo feature 无关）。
        // 表格必须开启，否则 Table 节点落入 Paragraph，chunk_type="table" 永不产生。
        options.extension.table = true;
        options.extension.strikethrough = true;
        options.extension.tasklist = true;

        let arena = Arena::new();
        let root = comrak::parse_document(&arena, &body_text, &options);

        // 预构建行起始字节偏移表，块切片 O(1)（避免逐块全量扫描文本 O(n²)）
        let line_offsets = build_line_offsets(&body_text);

        // 第一遍：递归收集块（列表/引用内的标题同样提升为结构节点）。
        // 必须遍历根节点的每个子块：collect_blocks 只处理单个节点，
        // 若直接传入 Document 根节点会落入默认分支被整体当作一个 Paragraph。
        let mut blocks: Vec<Block> = Vec::new();
        for child in root.children() {
            collect_blocks(child, &mut blocks);
        }

        // 第二遍：按标题层级递归建树
        let children = build_section(&blocks, &body_text, &line_offsets);
        let mut root_node = DocumentNode::new(NodeType::Root, "");
        root_node.children = children;
        root_node.metadata.start_line = 1;
        root_node.metadata.end_line = body_text.lines().count().max(1);
        root_node
    }
}

/// 递归收集扁平块列表（深度优先，保持文档顺序）。
///
/// 规则：
/// - Heading 在任意深度（含列表/引用内）均提升为结构节点
/// - 容器节点（List/Item/BlockQuote）：内部含标题时递归拆解（标题成为结构边界），
///   否则作为整体内容块（保留 list/quote 类型，避免破坏 chunk_type 分类）
/// - 其余叶子内容（Paragraph/CodeBlock/Table/ThematicBreak/HtmlBlock）作为内容块
fn collect_blocks<'a>(node: Node<'a>, blocks: &mut Vec<Block>) {
    let sp = node.data().sourcepos;
    let start = sp.start.line;
    let end = sp.end.line;
    match &node.data().value {
        NodeValue::Heading(h) => {
            let heading_text = node.collect_text().trim().to_string();
            if heading_text.is_empty() {
                return;
            }
            // 兼容旧启发式：--- 后跟超长段落会被 comrak 解析为 Setext H2。
            // 旧实现直接 return 导致该长段落整段丢失（B2）：改为降级为
            // Paragraph 内容块保留原文，避免"长段落 + ---"（分割线）内容被误删。
            if h.setext && h.level == 2 && heading_text.chars().count() > 100 {
                blocks.push(Block::Content {
                    node_type: NodeType::Paragraph,
                    start,
                    end,
                });
                return;
            }
            blocks.push(Block::Heading {
                level: h.level,
                text: heading_text,
                start,
                end,
            });
        }
        NodeValue::List(_) | NodeValue::BlockQuote => {
            // 容器内存在标题 → 递归拆解；否则作为整体内容块
            if node.children().any(contains_heading) {
                for child in node.children() {
                    collect_blocks(child, blocks);
                }
            } else {
                let node_type = match &node.data().value {
                    NodeValue::List(_) => NodeType::List,
                    _ => NodeType::Quote,
                };
                blocks.push(Block::Content {
                    node_type,
                    start,
                    end,
                });
            }
        }
        NodeValue::Item(_) => {
            // 仅当父容器含标题时被递归访问：继续下钻内容块
            for child in node.children() {
                collect_blocks(child, blocks);
            }
        }
        NodeValue::ThematicBreak => blocks.push(Block::Content {
            node_type: NodeType::ThematicBreak,
            start,
            end,
        }),
        NodeValue::CodeBlock(_) => blocks.push(Block::Content {
            node_type: NodeType::CodeBlock,
            start,
            end,
        }),
        NodeValue::Table(_) => blocks.push(Block::Content {
            node_type: NodeType::Table,
            start,
            end,
        }),
        NodeValue::HtmlBlock(_) => blocks.push(Block::Content {
            node_type: NodeType::HtmlBlock,
            start,
            end,
        }),
        _ => blocks.push(Block::Content {
            node_type: NodeType::Paragraph,
            start,
            end,
        }),
    }
}

/// 判断子树内是否包含标题节点。
fn contains_heading<'a>(node: Node<'a>) -> bool {
    match &node.data().value {
        NodeValue::Heading(_) => true,
        _ => node.children().any(contains_heading),
    }
}

/// 顶层块（扁平表示，行号为源码 1-based 含端点）
enum Block {
    Heading {
        level: u8,
        text: String,
        start: usize,
        end: usize,
    },
    Content {
        node_type: NodeType,
        start: usize,
        end: usize,
    },
}

/// 将扁平块列表递归构建为标题层级树。
///
/// 规则：Heading 节点收集其下所有子块（含嵌套标题），
/// 直到遇到同级或更高级（数字更小）的标题为止。
fn build_section(blocks: &[Block], text: &str, line_offsets: &[usize]) -> Vec<DocumentNode> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < blocks.len() {
        match &blocks[i] {
            Block::Heading { level, text: heading_text, start, end } => {
                let mut node = DocumentNode::new(NodeType::Heading, heading_text.clone());
                node.metadata.level = Some(*level);
                node.metadata.start_line = *start;
                node.metadata.end_line = *end;
                i += 1;
                // 收集子块：直到同级/更高级标题
                let child_start = i;
                while i < blocks.len() {
                    if let Block::Heading { level: lv, .. } = &blocks[i] {
                        if *lv <= *level {
                            break;
                        }
                    }
                    i += 1;
                }
                node.children = build_section(&blocks[child_start..i], text, line_offsets);
                result.push(node);
            }
            Block::Content { node_type, start, end } => {
                let mut node = DocumentNode::new(*node_type, slice_lines(text, *start, *end, line_offsets));
                node.metadata.start_line = *start;
                node.metadata.end_line = *end;
                result.push(node);
                i += 1;
            }
        }
    }
    result
}

/// 按 1-based 行号切片源码（含端点），保持与渲染一致。
///
/// 借助预构建的行起始偏移表 O(1) 定位，替代逐块全量扫描文本的 O(n²) 实现。
fn slice_lines(text: &str, start: usize, end: usize, offsets: &[usize]) -> String {
    let byte_start = offsets.get(start.saturating_sub(1)).copied().unwrap_or(0);
    let byte_end = offsets.get(end).copied().unwrap_or(text.len());
    let mut sliced = text.get(byte_start..byte_end).unwrap_or("").to_string();
    // 与原实现（lines().join("\n")）保持一致：行间用 \n 连接，尾部无多余换行
    if sliced.ends_with('\n') {
        sliced.pop();
    }
    sliced
}

/// 构建每行起始字节偏移表（offsets[i] = 第 i+1 行起始字节；len = 行数 + 尾部哨兵）。
fn build_line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (idx, b) in text.bytes().enumerate() {
        if b == b'\n' {
            offsets.push(idx + 1);
        }
    }
    offsets
}

/// 识别并剥离文档开头的 YAML FrontMatter（Obsidian 等笔记格式）。
///
/// 规则：首行必须是 `---`，且在限定行数内找到闭合 `---`，且中间内容像 YAML
/// （至少含一行 `键: 值`），才认定为 frontmatter；否则视为普通文档
/// （首行 `---` 只是 ThematicBreak 分割线，B3：`---\n段落\n---` 不得被剥离）。
fn strip_frontmatter(text: &str) -> String {
    parse_frontmatter(text).1
}

// ─── FrontMatter 元数据（P0-1：tags/aliases/title 重新纳入检索） ───

/// FrontMatter 元数据（Obsidian tags/aliases/title 等检索信号）。
#[derive(Debug, Clone, Default)]
pub struct FrontmatterMeta {
    /// `title:` 字段（文档显式标题；缺失时检索侧回退文件名）
    pub title: Option<String>,
    /// `tags:` 字段（YAML 数组或逗号分隔字符串）
    pub tags: Vec<String>,
    /// `aliases:` 字段（标题别名，与 tags 同路参与检索）
    pub aliases: Vec<String>,
    /// `category:` 字段（分类）
    pub category: Option<String>,
}

/// 解析并剥离文档开头的 YAML FrontMatter。
///
/// 判定规则与 [`strip_frontmatter`] 一致；成功时返回 `(元数据, 剥离后的正文)`，
/// 未命中（非 frontmatter）时返回 `(None, 原文本)`。元数据解析失败仅丢弃元数据，
/// **绝不丢弃正文**（B3 保证）。
pub fn parse_frontmatter(text: &str) -> (Option<FrontmatterMeta>, String) {
    // 🟠 L9：BOM 剥离——Windows 记事本保存的 UTF-8 文件常带 \u{feff} 前缀，
    // `trim()` 不剥离 BOM，旧实现首行 `---` 判定失效导致 frontmatter 既不剥离
    // 也不解析（原始 YAML 进 chunk 正文污染索引）。
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.first().map(|l| l.trim()) != Some("---") {
        return (None, text.to_string());
    }
    let mut close_idx = None;
    for (idx, line) in lines.iter().enumerate().skip(1) {
        if idx > FRONTMATTER_MAX_LINES {
            break;
        }
        if line.trim() == "---" {
            close_idx = Some(idx);
            break;
        }
    }
    let Some(close_idx) = close_idx else {
        return (None, text.to_string());
    };
    let body = &lines[1..close_idx];
    if !looks_like_yaml(body) {
        return (None, text.to_string());
    }
    let yaml_text = body.join("\n");
    let meta = parse_frontmatter_yaml(&yaml_text);
    (Some(meta), lines[close_idx + 1..].join("\n"))
}

/// 从 YAML 文本解析元数据（serde_yaml，容错：失败仅丢元数据）。
fn parse_frontmatter_yaml(yaml_text: &str) -> FrontmatterMeta {
    let mut meta = FrontmatterMeta::default();
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(yaml_text) else {
        return meta;
    };
    let serde_yaml::Value::Mapping(map) = value else {
        return meta;
    };
    let get = |key: &str| map.get(&serde_yaml::Value::String(key.to_string()));

    if let Some(v) = get("title").and_then(|v| v.as_str()) {
        meta.title = Some(v.trim().to_string());
    }
    if let Some(v) = get("category").and_then(|v| v.as_str()) {
        meta.category = Some(v.trim().to_string());
    }
    meta.tags = extract_string_list(get("tags"));
    meta.aliases = extract_string_list(get("aliases"));
    meta
}

/// 提取字符串列表：支持 YAML 数组 / 逗号分隔字符串（含中文逗号）。
fn extract_string_list(v: Option<&serde_yaml::Value>) -> Vec<String> {
    let Some(v) = v else {
        return Vec::new();
    };
    match v {
        serde_yaml::Value::Sequence(seq) => seq
            .iter()
            .filter_map(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        serde_yaml::Value::String(s) => s
            .split([',', '，'])
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// 判断 frontmatter 候选内容是否像 YAML：至少包含一行 `键: 值`。
///
/// 键允许中文等 Unicode 字符，但不应含空格/冒号（排除普通散文段落与 URL 行）。
fn looks_like_yaml(body: &[&str]) -> bool {
    body.iter().any(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            return false;
        }
        match line.find(':') {
            None => false,
            Some(idx) => {
                let key = &line[..idx];
                !key.is_empty()
                    && !key
                        .chars()
                        .any(|c| c == ' ' || c == '\t' || c == ':')
            }
        }
    })
}

// ─── P0-1 测试：FrontMatter 解析 ───

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_frontmatter_extracts_metadata() {
        let text = "---\ntitle: Redis 连接池\ntags:\n  - redis\n  - 运维\naliases:\n  - Redis Pool\n  - 连接池\ncategory: 技术笔记\n---\n# 正文\n内容";
        let (meta, body) = parse_frontmatter(text);
        let meta = meta.expect("应识别 frontmatter");
        assert_eq!(meta.title.as_deref(), Some("Redis 连接池"));
        assert_eq!(meta.tags, vec!["redis", "运维"]);
        assert_eq!(meta.aliases, vec!["Redis Pool", "连接池"]);
        assert_eq!(meta.category.as_deref(), Some("技术笔记"));
        assert_eq!(body, "# 正文\n内容");
    }

    #[test]
    fn parse_frontmatter_comma_string_tags() {
        let text = "---\ntags: rag, search, 混合检索\n---\n正文";
        let (meta, body) = parse_frontmatter(text);
        let meta = meta.unwrap();
        assert_eq!(meta.tags, vec!["rag", "search", "混合检索"]);
        assert_eq!(body, "正文");
    }

    #[test]
    fn parse_frontmatter_not_yaml_keeps_text() {
        // 首行 --- 但中间不是 YAML（B3 回归：不得误剥普通文档）
        let text = "---\n普通段落文字\n---\n正文内容";
        let (meta, body) = parse_frontmatter(text);
        assert!(meta.is_none());
        assert_eq!(body, text);
        // 无闭合
        let text2 = "---\ntitle: x\n没有闭合";
        let (m2, b2) = parse_frontmatter(text2);
        assert!(m2.is_none());
        assert_eq!(b2, text2);
    }

    #[test]
    fn strip_frontmatter_delegates() {
        let text = "---\ntags: [a, b]\n---\n正文";
        assert_eq!(strip_frontmatter(text), "正文");
        assert_eq!(strip_frontmatter("无 frontmatter 文档"), "无 frontmatter 文档");
    }
}
