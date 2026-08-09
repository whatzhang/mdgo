//! AST 语义分块引擎：从 `DocumentNode` 构建语义 chunk。
//!
//! 输入为结构化文档树（标题层级 + 内容块），输出为带完整上下文的语义 chunk，
//! 替代旧的"按行切片 + 长度切分"方案，从源头消除段落/句子被中途截断的问题。
//!
//! 与 `db::chunk_splitter` 的分层关系：本模块位于 `document`（基础层），
//! 不依赖 `db`；`Chunk` → `ChunkResult` 的转换由 `db` 层完成（A3 修复）。

use std::collections::HashMap;

use super::node::{DocumentNode, NodeType};
use super::text_split::{char_len, split_text_with_separators};

/// AST 分块结果：区分上下文文本（给 LLM）与向量化文本（给 embedding）。
#[derive(Debug, Clone)]
pub struct Chunk {
    /// 上下文文本：标题渲染 + 正文（给 LLM / 前端展示）
    pub text: String,
    /// 向量化文本：紧凑标题路径 + 正文（给 embedding，避免标题词稀释正文语义）
    pub embedding_text: String,
    /// 标题层级路径（heading_path），如 ["Kubernetes", "Network", "Calico"]
    pub path: Vec<String>,
    /// 分块类型（paragraph/code/table/list/quote/section 等）
    pub chunk_type: String,
}

/// Chunk 引擎：从 DocumentNode 构建语义 chunk。
pub trait ChunkEngine: Send + Sync {
    fn build(&self, document: &DocumentNode) -> Vec<Chunk>;
}

/// AST 语义分块引擎。
///
/// 算法：
/// 1. DFS 遍历文档树，维护标题路径栈（heading_path）
/// 2. 每个标题节收集其下内容块（段落/代码/表格/列表/引用）
/// 3. 节内按块边界贪心分组：每个 chunk ≤ max_size，**绝不在块中间截断**
/// 4. 单块超长时才降级二次切分：代码块按行、**表格按行分组并重复表头**、正文按句子 → 字符
///
/// 输出约定：
/// - `text`（上下文文本）= Markdown 标题渲染 + 正文，供 LLM 阅读
/// - `embedding_text`（向量化文本）= 紧凑标题路径 + 正文，避免标题词污染向量
pub struct SemanticChunkEngine {
    max_size: usize,
    overlap: usize,
    /// 单节宽松上限系数：总长未超 max_size×系数时直接作为一个 chunk
    oversize_factor: f32,
    /// 拆分时正文最小预留字符数，防止标题前缀占满空间
    min_body_reserve_chars: usize,
}

impl SemanticChunkEngine {
    pub fn new(
        max_size: usize,
        overlap: usize,
        oversize_factor: f32,
        min_body_reserve_chars: usize,
    ) -> Self {
        Self {
            max_size,
            overlap,
            oversize_factor,
            min_body_reserve_chars,
        }
    }

    /// DFS 遍历：标题维护路径栈，内容块累积到当前节
    fn walk(
        &self,
        nodes: &[DocumentNode],
        path: &mut Vec<String>,
        blocks: &mut Vec<(String, String)>,
        out: &mut Vec<Chunk>,
    ) {
        for node in nodes {
            if node.is_heading() {
                // 进入新标题前，先冲刷上一个节（属于父级标题或根）
                self.flush_section(path, blocks, out);
                path.push(node.content.clone());
                log::debug!(
                    "[chunk_engine] 进入标题(level={}, path={:?})",
                    path.len(),
                    path
                );
                self.walk(&node.children, path, blocks, out);
                // 该标题下无嵌套标题的尾部平级块（path 仍含本标题）
                self.flush_section(path, blocks, out);
                log::debug!(
                    "[chunk_engine] 离开标题(level={}, path={:?})",
                    path.len(),
                    path
                );
                path.pop();
            } else if node.node_type != NodeType::ThematicBreak {
                // 主题分割线不产生内容
                blocks.push((node.node_type.as_str().to_string(), node.content.clone()));
            }
        }
    }

    /// 冲刷当前节的块：构建语义 chunk
    fn flush_section(
        &self,
        path: &[String],
        blocks: &mut Vec<(String, String)>,
        out: &mut Vec<Chunk>,
    ) {
        if blocks.is_empty() {
            return;
        }
        let blocks = std::mem::take(blocks);
        let (context_prefix, _) = Self::build_prefixes(path);
        let body = blocks
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let max_single = (self.max_size as f32 * self.oversize_factor) as usize;
        let chunk_type = dominant_type(&blocks);
        let block_desc = blocks
            .iter()
            .map(|(t, c)| format!("{}:{}字", t, char_len(c)))
            .collect::<Vec<_>>()
            .join(", ");
        let body_len = char_len(&body);
        let context_len = char_len(&context_prefix);
        log::debug!(
            "[chunk_engine] 冲刷节 path={:?} 块数={} 正文={}字 前缀={}字 上限={}字 主导类型={} 块明细=[{}]",
            path,
            blocks.len(),
            body_len,
            context_len,
            max_single,
            chunk_type,
            block_desc
        );

        if context_len + body_len <= max_single {
            log::debug!(
                "[chunk_engine]   整节未超长({}+{}<={}字) → 整体单 chunk",
                context_len,
                body_len,
                max_single
            );
            out.push(self.make_chunk(path, body, &chunk_type));
            return;
        }

        // 超长节：按块边界贪心分组，单块超长时单独拆分
        let available = self
            .max_size
            .saturating_sub(context_len)
            .max(self.min_body_reserve_chars);
        log::debug!(
            "[chunk_engine]   节超长({}+{}>{}字) → 贪心分组, 单块可用={}字",
            context_len,
            body_len,
            max_single,
            available
        );
        let mut group: Vec<(String, String)> = Vec::new();
        let mut group_len = 0usize;
        for block in blocks {
            let block_len = char_len(&block.1);
            if block_len > available {
                // 先冲刷当前组，再拆分超长块（代码按行 / 表格按行分组 / 正文按句子）
                self.flush_group(path, &mut group, out);
                group_len = 0;
                log::debug!(
                    "[chunk_engine]   单块超长: type={} len={}字(可用{}字) → 二次切分",
                    block.0,
                    block_len,
                    available
                );
                for piece in self.split_oversize_block(&block, available) {
                    out.push(self.make_chunk(path, piece, &chunk_type));
                }
                continue;
            }
            if !group.is_empty() && group_len + block_len > available {
                self.flush_group(path, &mut group, out);
                group_len = 0;
            }
            group_len += block_len;
            group.push(block);
        }
        self.flush_group(path, &mut group, out);
    }

    fn flush_group(
        &self,
        path: &[String],
        group: &mut Vec<(String, String)>,
        out: &mut Vec<Chunk>,
    ) {
        if group.is_empty() {
            return;
        }
        let body = group
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunk_type = dominant_type(group);
        let group_desc = group
            .iter()
            .map(|(t, c)| format!("{}:{}字", t, char_len(c)))
            .collect::<Vec<_>>()
            .join(", ");
        log::debug!(
            "[chunk_engine]   组内合并 {} 块(共{}字, 类型={}) 块明细=[{}]",
            group.len(),
            char_len(&body),
            chunk_type,
            group_desc
        );
        out.push(self.make_chunk(path, body, &chunk_type));
        group.clear();
    }

    /// 单块超长：按块类型选择切分策略。
    ///
    /// 业界共识（MDKeyChunker / LangChain / LlamaIndex）：表格、代码块等结构化单元
    /// 不得被拆成零散片段；超长时仅在行边界切分，且表格分片重复表头，保证
    /// 每个分片仍是语义完整的最小单元。
    fn split_oversize_block(&self, block: &(String, String), available: usize) -> Vec<String> {
        let (typ, text) = block;
        let pieces = match typ.as_str() {
            // 代码块按行边界切分，避免打断语句
            "code" => split_text_with_separators(text, available, self.overlap, &["\n", " ", ""]),
            // 表格按行分组并重复表头：绝不按单元格/管道符号拆散（表格原子性）
            "table" => split_oversize_table(text, available),
            // 正文按段落 → 句子 → 字符逐级降级
            _ => split_text_with_separators(
                text,
                available,
                self.overlap,
                &["\n\n", "\n", ". ", "。", "！", "？", "；", " ", ""],
            ),
        };
        log::debug!(
            "[chunk_engine]   切分策略: type={} len={}字 可用={}字 → {} 片",
            typ,
            char_len(text),
            available,
            pieces.len()
        );
        pieces
    }

    /// 组装 chunk：上下文文本带 Markdown 标题渲染，向量化文本带紧凑路径
    fn make_chunk(&self, path: &[String], body: String, chunk_type: &str) -> Chunk {
        let (context_prefix, embed_prefix) = Self::build_prefixes(path);
        let context_text = if context_prefix.is_empty() {
            body.clone()
        } else {
            format!("{}\n{}", context_prefix, body)
        };
        let embedding_text = if embed_prefix.is_empty() {
            body.clone()
        } else {
            format!("{}\n{}", embed_prefix, body)
        };
        log::debug!(
            "[chunk_engine]   产出 chunk: type={} path={:?} text={}字 embedding={}字",
            chunk_type,
            path,
            char_len(&context_text),
            char_len(&embedding_text)
        );
        Chunk {
            text: context_text,
            embedding_text,
            path: path.to_vec(),
            chunk_type: chunk_type.to_string(),
        }
    }

    /// 构建标题前缀：
    /// - context：`# Kubernetes\n## Network`（LLM 可读的 Markdown 渲染）
    /// - embedding：`Kubernetes Network`（紧凑路径，供向量化）
    fn build_prefixes(path: &[String]) -> (String, String) {
        let mut context = String::new();
        for (i, heading) in path.iter().enumerate() {
            context.push_str(&"#".repeat(i + 1));
            context.push(' ');
            context.push_str(heading);
            context.push('\n');
        }
        context.pop(); // 去掉末尾换行
        (context, path.join(" "))
    }
}

impl ChunkEngine for SemanticChunkEngine {
    fn build(&self, document: &DocumentNode) -> Vec<Chunk> {
        log::debug!(
            "[chunk_engine] 开始分块: max_size={} overlap={} oversize_factor={} min_body_reserve={} 根子块数={}",
            self.max_size,
            self.overlap,
            self.oversize_factor,
            self.min_body_reserve_chars,
            document.children.len()
        );
        let mut out = Vec::new();
        let mut path = Vec::new();
        let mut blocks = Vec::new();
        self.walk(&document.children, &mut path, &mut blocks, &mut out);
        // 冲刷根级剩余块
        self.flush_section(&path, &mut blocks, &mut out);
        log::debug!("[chunk_engine] 分块完成: 共 {} chunk", out.len());
        out
    }
}

/// 统计块集合的主导类型（单块 → 其类型；混合 → 频次最高者，平手归为 section）
fn dominant_type(blocks: &[(String, String)]) -> String {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for (t, _) in blocks {
        *counts.entry(t.as_str()).or_insert(0) += 1;
    }
    let mut best: (&str, usize) = ("section", 0);
    for (t, count) in counts {
        if count > best.1 {
            best = (t, count);
        }
    }
    best.0.to_string()
}

/// 超长表格按行分组切分，每个分片重复表头行（表头 + 分隔行），保持表格语义单元完整。
///
/// 规则：
/// - 少于 3 行（表头 + 分隔行 + 至少 1 行数据）的最小表格整体保留
/// - 正文行贪心分组：`表头 + 当前组` 超长时开新组
/// - 单行本身超长时不做行内切分（行是原子的，含超长单元格的宽表不被拆散）
fn split_oversize_table(text: &str, max_chars: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= 2 {
        log::debug!(
            "[chunk_engine]   表格行数={} (仅表头+分隔行) → 整体保留不拆分",
            lines.len()
        );
        return vec![text.to_string()];
    }
    // 表头 = 表头行 + 分隔行（GFM 表格前两行）
    let header = format!("{}\n{}", lines[0], lines[1]);
    let header_chars = header.chars().count();
    let body: Vec<&str> = lines[2..].to_vec();

    let mut chunks: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut current_chars = 0usize;
    for row in body {
        let row_chars = row.chars().count() + 1; // +1 换行
        if !current.is_empty() && header_chars + current_chars + row_chars > max_chars {
            chunks.push(format!("{}\n{}", header, current.join("\n")));
            current.clear();
            current_chars = 0;
        }
        current.push(row);
        current_chars += row_chars;
    }
    if !current.is_empty() {
        chunks.push(format!("{}\n{}", header, current.join("\n")));
    }
    if chunks.is_empty() {
        chunks.push(text.to_string());
    }
    log::debug!(
        "[chunk_engine]   表格切分: 总行数={} 数据行={} 表头={}字 上限={}字 → {} 片(每片重复表头)",
        lines.len(),
        lines.len().saturating_sub(2),
        header_chars,
        max_chars,
        chunks.len()
    );
    chunks
}
