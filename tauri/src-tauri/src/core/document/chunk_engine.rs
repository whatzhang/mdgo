//! AST 语义分块引擎：从 `DocumentNode` 构建语义 chunk。
//!
//! 输入为结构化文档树（标题层级 + 内容块），输出为带完整上下文的语义 chunk，
//! 替代旧的"按行切片 + 长度切分"方案，从源头消除段落/句子被中途截断的问题。
//!
//! 与 `db::chunk_splitter` 的分层关系：本模块位于 `document`（基础层），
//! 不依赖 `db`；`Chunk` → `ChunkResult` 的转换由 `db` 层完成（A3 修复）。
//!
//! P0-2（token 感知）：分组预算按 **token** 计（`TokenCounter` 注入），
//! 消除"字符预算 × 中英文 token 密度差异"导致的窗口浪费/静默超限；
//! embedding 前缀截断至 `prefix_max_tokens`（最近 ≤3 级标题）；组间拼接前块尾部
//! 实现索引期 overlap。硬上限由 `db::token_budget::TokenBudgetValidator` 最终裁决。

use std::collections::HashMap;
use std::sync::Arc;

use super::node::{DocumentNode, NodeType};
use super::text_split::{char_len, split_text_token_aware, split_text_with_separators};
use super::token_budget::{char_budget_pair, TokenCounter};

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
    /// **源码行区间**（1-based 闭区间，来自成员块的 `NodeMetadata`）。
    ///
    /// Phase 1 provenance：`db::chunk_splitter` 用「行 → 页」映射把它翻译成
    /// `page_start/page_end`（方案 §4.4）。**区间只做溯源，不参与分块语义**——
    /// chunk 边界仍完全由语义分组决定，因此可以跨页。
    ///
    /// `(0, 0)` 表示无源码行来源（保留给未来的合成 chunk）。
    pub line_start: usize,
    pub line_end: usize,
    /// **表格表头列名**（JSON 数组字符串，Plan B v2 §4.7 / 决策 R3）。
    ///
    /// 仅当 `chunk_type == "table"` 且正文里能找到 GFM 表格时非空。**只进 metadata，
    /// 绝不进 `embedding_text`**——表头词（如 `Sales_Rep_Name`）进向量会稀释正文语义。
    /// 表头本来就在 chunk 正文里，所以 BM25 已能命中列名；本字段的价值是让**结构化
    /// 消费方**（前端展示、后续按列名过滤）不必再解析 Markdown。
    pub table_headers: Option<String>,
}

/// 分组用的块条目：块类型 + 正文 + **源码行区间**。
///
/// 改造前这里是 `(String, String)`，行区间在收集时被丢弃，导致 provenance 无法
/// 从 AST 传播到 chunk（Phase 1 的卡点）。本结构只**多携带**行区间，
/// 不改变任何分组/切分决策。
#[derive(Debug, Clone)]
struct BlockEntry {
    kind: String,
    text: String,
    /// 1-based 闭区间
    line_start: usize,
    line_end: usize,
}

impl BlockEntry {
    fn from_node(node: &DocumentNode) -> Self {
        Self {
            kind: node.node_type.as_str().to_string(),
            text: node.content.clone(),
            line_start: node.metadata.start_line,
            line_end: node.metadata.end_line,
        }
    }

    /// 成员块行区间并集（空集合返回 `(0, 0)`）
    fn union_lines(entries: &[BlockEntry]) -> (usize, usize) {
        let mut start = usize::MAX;
        let mut end = 0usize;
        for e in entries {
            if e.line_start == 0 && e.line_end == 0 {
                continue;
            }
            start = start.min(e.line_start);
            end = end.max(e.line_end);
        }
        if start == usize::MAX {
            (0, 0)
        } else {
            (start, end)
        }
    }
}

/// Chunk 引擎：从 DocumentNode 构建语义 chunk。
pub trait ChunkEngine: Send + Sync {
    fn build(&self, document: &DocumentNode) -> Vec<Chunk>;
}

/// AST 语义分块引擎（token 感知版，P0-2）。
///
/// 算法：
/// 1. DFS 遍历文档树，维护标题路径栈（heading_path）
/// 2. 每个标题节收集其下内容块（段落/代码/表格/列表/引用）
/// 3. 节内按块边界贪心分组：每个 chunk ≤ `max_tokens`（token 预算），**绝不在块中间截断**
/// 4. 单块超长时才降级二次切分：代码块按行、**表格按行分组并重复表头**、正文按句子 → 字符
///    （token 感知：一次 tokenize + 按 token 预算定位切分点）
/// 5. 组间拼接前块尾部（`overlap_tokens`），索引期保持跨块上下文完整
///
/// 输出约定：
/// - `text`（上下文文本）= Markdown 标题渲染 + 正文，供 LLM 阅读
/// - `embedding_text`（向量化文本）= 紧凑标题路径（最近 ≤3 级、≤ prefix_max_tokens）
///   + 正文，避免标题词污染向量
pub struct SemanticChunkEngine {
    /// 分组目标（token）
    max_tokens: usize,
    /// 相邻 chunk 间 overlap（token）
    overlap_tokens: usize,
    /// 拆分时正文最小预留 token 数，防止标题前缀占满空间
    min_body_reserve_tokens: usize,
    /// embedding 路径前缀最大 token 数
    prefix_max_tokens: usize,
    counter: Arc<dyn TokenCounter>,
}

/// embedding 前缀保留的最大标题级数（最近 N 级）
const EMBED_PREFIX_MAX_LEVELS: usize = 3;

impl SemanticChunkEngine {
    pub fn new(
        max_tokens: usize,
        overlap_tokens: usize,
        min_body_reserve_tokens: usize,
        prefix_max_tokens: usize,
        counter: Arc<dyn TokenCounter>,
    ) -> Self {
        Self {
            max_tokens,
            overlap_tokens,
            min_body_reserve_tokens,
            prefix_max_tokens,
            counter,
        }
    }

    /// token 计数（counter 不可用时退化 1 字符 ≈ 1 token，CJK 最坏情形）
    fn tokens(&self, s: &str) -> usize {
        self.counter.count(s).unwrap_or_else(|| s.chars().count())
    }

    /// DFS 遍历：标题维护路径栈，内容块累积到当前节
    fn walk(
        &self,
        nodes: &[DocumentNode],
        path: &mut Vec<String>,
        blocks: &mut Vec<BlockEntry>,
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
                // 🟠 M7：记录本标题子树开始前的**正文 chunk 数**（非导航 chunk）——
                // 用「子树是否产出过正文」判定，而非 out.len() 差值：纯嵌套标题的
                // 父节（子节全是导航 chunk）不再被漏掉，每一级标题都可检索。
                let body_before = out.iter().filter(|c| c.chunk_type != "heading").count();
                self.walk(&node.children, path, blocks, out);
                // 该标题下无嵌套标题的尾部平级块（path 仍含本标题）
                self.flush_section(path, blocks, out);
                // D2：heading-only 章节（子树无任何正文块）不丢失——标题本身作为
                // 可检索的导航 chunk（chunk_type="heading"，正文为空；检索命中后由
                // indexer 检索管线的 ±window 上下文扩展（fetch_chunks_between）补齐上下文）。
                let body_after = out.iter().filter(|c| c.chunk_type != "heading").count();
                if body_after == body_before {
                    let (ctx, emb) =
                        Self::build_prefixes(path, self.prefix_max_tokens, &*self.counter);
                    let text = if ctx.is_empty() {
                        path.join(" > ")
                    } else {
                        ctx
                    };
                    log::debug!(
                        "[chunk_engine]   heading-only 章节 → 导航 chunk: {:?}",
                        path
                    );
                    out.push(Chunk {
                        text,
                        embedding_text: emb,
                        path: path.to_vec(),
                        chunk_type: "heading".to_string(),
                        // heading 导航 chunk 的溯源 = 该标题节点自身的行区间
                        line_start: node.metadata.start_line,
                        line_end: node.metadata.end_line,
                        table_headers: None,
                    });
                }
                log::debug!(
                    "[chunk_engine] 离开标题(level={}, path={:?})",
                    path.len(),
                    path
                );
                path.pop();
            } else if node.node_type != NodeType::ThematicBreak {
                // 主题分割线不产生内容
                blocks.push(BlockEntry::from_node(node));
            }
        }
    }

    /// 冲刷当前节的块：构建语义 chunk
    fn flush_section(
        &self,
        path: &[String],
        blocks: &mut Vec<BlockEntry>,
        out: &mut Vec<Chunk>,
    ) {
        if blocks.is_empty() {
            return;
        }
        let blocks = std::mem::take(blocks);
        let (context_prefix, embed_prefix) =
            Self::build_prefixes(path, self.prefix_max_tokens, &*self.counter);
        let body = blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunk_type = dominant_type(&blocks);
        let body_tokens = self.tokens(&body);
        let embed_prefix_tokens = self.tokens(&embed_prefix);
        let block_desc = blocks
            .iter()
            .map(|b| format!("{}:{}字", b.kind, char_len(&b.text)))
            .collect::<Vec<_>>()
            .join(", ");
        log::debug!(
            "[chunk_engine] 冲刷节 path={:?} 块数={} 正文={}token 前缀={}token 上限={}token 主导类型={} 块明细=[{}]",
            path,
            blocks.len(),
            body_tokens,
            embed_prefix_tokens,
            self.max_tokens,
            chunk_type,
            block_desc
        );

        // 整节未超目标 → 整体单 chunk（embedding 侧 token 判定）
        // 🟠 M3：按「prefix + "\n" + body」整体精确计数（含分隔符 token），
        // 旧实现用 count(prefix)+count(body) 两次独立计数，各含 special tokens 且
        // 漏算换行，恰好贴预算的节会系统性超 1~2 token。
        let single_embed_tokens = if embed_prefix.is_empty() {
            body_tokens
        } else {
            self.tokens(&format!("{}\n{}", embed_prefix, body))
        };
        if single_embed_tokens <= self.max_tokens {
            log::debug!(
                "[chunk_engine]   整节未超长({}+{}<={}token) → 整体单 chunk",
                embed_prefix_tokens,
                body_tokens,
                self.max_tokens
            );
            out.push(self.make_chunk(
                path,
                &context_prefix,
                &embed_prefix,
                body,
                &chunk_type,
                BlockEntry::union_lines(&blocks),
            ));
            return;
        }

        // 超长节：贪心分组（token 预算；预留 overlap 头寸）
        // 🟠 M4 修复：正文可用预算上限 = max − prefix（不得被 min_body_reserve 下限
        // 顶破——旧实现 `.max(min_body_reserve)` 在长标题路径下产出 prefix+64 的
        // 恒超限 chunk，全节系统性触发重切）；前缀已占满预算时直接产出单 chunk
        // 交由 Validator 按类型重切/显式降级。
        const MIN_BODY_FLOOR: usize = 16;
        let body_ceiling = self.max_tokens.saturating_sub(embed_prefix_tokens);
        if body_ceiling < MIN_BODY_FLOOR {
            log::debug!(
                "[chunk_engine]   前缀已占满预算(prefix={}token ≥ max={}) → 单 chunk 交 Validator",
                embed_prefix_tokens,
                self.max_tokens
            );
            out.push(self.make_chunk(
                path,
                &context_prefix,
                &embed_prefix,
                body,
                &chunk_type,
                BlockEntry::union_lines(&blocks),
            ));
            return;
        }
        let overlap_reserve_tokens = self.overlap_tokens.min(
            body_ceiling.saturating_sub(self.min_body_reserve_tokens),
        );
        let available = (body_ceiling.saturating_sub(overlap_reserve_tokens))
            .max(self.min_body_reserve_tokens.min(body_ceiling));
        let overlap_chars = self.overlap_chars(&body, overlap_reserve_tokens);
        log::debug!(
            "[chunk_engine]   节超长({}+{}>{}token) → 贪心分组, 单块可用={}token, 组间 overlap={}字({}token)",
            embed_prefix_tokens,
            body_tokens,
            self.max_tokens,
            available,
            overlap_chars,
            overlap_reserve_tokens
        );
        let mut group: Vec<BlockEntry> = Vec::new();
        let mut group_tokens = 0usize;
        let mut prev_tail: String = String::new(); // 前一块正文尾部（跨块上下文）
        for block in blocks {
            let block_tokens = self.tokens(&block.text);
            if block_tokens > available {
                // 先冲刷当前组，再拆分超长块（代码按行 / 表格按行分组 / 正文按句子）
                self.flush_group(
                    path,
                    &context_prefix,
                    &embed_prefix,
                    &mut group,
                    &chunk_type,
                    out,
                    &mut prev_tail,
                    overlap_chars,
                );
                group_tokens = 0;
                log::debug!(
                    "[chunk_engine]   单块超长: type={} len={}token(可用{}token) → 二次切分",
                    block.kind,
                    block_tokens,
                    available
                );
                let pieces = self.split_oversize_block(&block, available);
                let last_piece_tail = pieces.last().cloned().unwrap_or_default();
                for piece in pieces {
                    // 每个分片继承源块的行区间（分片是块内二次切分，provenance 相同）
                    out.push(self.make_chunk(
                        path,
                        &context_prefix,
                        &embed_prefix,
                        piece,
                        &chunk_type,
                        (block.line_start, block.line_end),
                    ));
                }
                // 后续组沿用最后一片的尾部（保持连续性）
                prev_tail = tail_chars(&last_piece_tail, overlap_chars);
                continue;
            }
            if !group.is_empty() && group_tokens + block_tokens > available {
                self.flush_group(
                    path,
                    &context_prefix,
                    &embed_prefix,
                    &mut group,
                    &chunk_type,
                    out,
                    &mut prev_tail,
                    overlap_chars,
                );
                group_tokens = 0;
            }
            group_tokens += block_tokens;
            group.push(block);
        }
        self.flush_group(
            path,
            &context_prefix,
            &embed_prefix,
            &mut group,
            &chunk_type,
            out,
            &mut prev_tail,
            overlap_chars,
        );
    }

    /// overlap token → 字符（按本节正文密度折算）
    fn overlap_chars(&self, body: &str, overlap_tokens: usize) -> usize {
        if overlap_tokens == 0 {
            return 0;
        }
        let body_chars = char_len(body);
        let body_tokens = self.tokens(body).max(1);
        let density = body_chars as f64 / body_tokens as f64;
        ((overlap_tokens as f64) * density).ceil() as usize
    }

    /// 冲刷贪心组：正文前拼接前块尾部（overlap），输出 chunk 并更新尾部
    #[allow(clippy::too_many_arguments)]
    fn flush_group(
        &self,
        path: &[String],
        context_prefix: &str,
        embed_prefix: &str,
        group: &mut Vec<BlockEntry>,
        chunk_type: &str,
        out: &mut Vec<Chunk>,
        prev_tail: &mut String,
        overlap_chars: usize,
    ) {
        if group.is_empty() {
            return;
        }
        let body = group
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut body_for_chunk = if prev_tail.is_empty() {
            body.clone()
        } else {
            format!("{}\n{}", prev_tail, body)
        };
        // 🟠 M3：精确核对最终 embedding 文本（prefix + "\n" + prev_tail + body）——
        // overlap 尾部按「整节密度」折算，混排文档（正文含代码尾）密度波动时
        // 尾部 token 可能超出预留头寸；超限时丢弃尾部（正文完整，避免 Validator
        // 误判重切）。
        if !prev_tail.is_empty() {
            let full_embed = if embed_prefix.is_empty() {
                body_for_chunk.clone()
            } else {
                format!("{}\n{}", embed_prefix, body_for_chunk)
            };
            if self.tokens(&full_embed) > self.max_tokens {
                log::debug!(
                    "[chunk_engine]   组 chunk 含 overlap 尾部超预算 → 丢弃尾部(prev_tail={}字)",
                    char_len(prev_tail)
                );
                body_for_chunk = body.clone();
            }
        }
        let group_desc = group
            .iter()
            .map(|b| format!("{}:{}字", b.kind, char_len(&b.text)))
            .collect::<Vec<_>>()
            .join(", ");
        log::debug!(
            "[chunk_engine]   组内合并 {} 块(共{}字, 类型={}) 块明细=[{}] overlap_tail={}字",
            group.len(),
            char_len(&body),
            chunk_type,
            group_desc,
            char_len(prev_tail)
        );
        out.push(self.make_chunk(
            path,
            context_prefix,
            embed_prefix,
            body_for_chunk,
            chunk_type,
            BlockEntry::union_lines(group),
        ));
        *prev_tail = tail_chars(&body, overlap_chars);
        group.clear();
    }

    /// 单块超长：按块类型选择切分策略（token 感知）。
    ///
    /// 业界共识（MDKeyChunker / LangChain / LlamaIndex）：表格、代码块等结构化单元
    /// 不得被拆成零散片段；超长时仅在行边界切分，且表格分片重复表头，保证
    /// 每个分片仍是语义完整的最小单元。
    fn split_oversize_block(&self, block: &BlockEntry, available_tokens: usize) -> Vec<String> {
        let (typ, text) = (&block.kind, &block.text);
        let pieces = match typ.as_str() {
            // 代码块按行边界切分，避免打断语句
            "code" => {
                if let Some(p) = split_text_token_aware(
                    text,
                    available_tokens,
                    self.overlap_tokens,
                    &["\n", " ", ""],
                    &*self.counter,
                ) {
                    p
                } else {
                    split_text_with_separators(text, available_tokens, self.overlap_tokens, &["\n", " ", ""])
                }
            }
            // 表格按行分组并重复表头：绝不按单元格/管道符号拆散（表格原子性）
            "table" => {
                let (char_budget, _) = char_budget_pair(text, available_tokens, self.overlap_tokens, &*self.counter);
                split_oversize_table(text, char_budget)
            }
            // 正文按段落 → 句子 → 字符逐级降级（token 感知）
            _ => {
                if let Some(p) = split_text_token_aware(
                    text,
                    available_tokens,
                    self.overlap_tokens,
                    &["\n\n", "\n", ". ", "。", "！", "？", "；", " ", ""],
                    &*self.counter,
                ) {
                    p
                } else {
                    split_text_with_separators(
                        text,
                        available_tokens,
                        self.overlap_tokens,
                        &["\n\n", "\n", ". ", "。", "！", "？", "；", " ", ""],
                    )
                }
            }
        };
        log::debug!(
            "[chunk_engine]   切分策略: type={} len={}字 可用={}token → {} 片",
            typ,
            char_len(text),
            available_tokens,
            pieces.len()
        );
        pieces
    }

    /// 组装 chunk：上下文文本带 Markdown 标题渲染，向量化文本带紧凑路径
    fn make_chunk(
        &self,
        path: &[String],
        context_prefix: &str,
        embed_prefix: &str,
        body: String,
        chunk_type: &str,
        lines: (usize, usize),
    ) -> Chunk {
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
            line_start: lines.0,
            line_end: lines.1,
            // 只有表格块才提取；且只挂在 metadata 上（`embedding_text` 已在上面定稿，
            // 刻意不掺入表头词——见字段注释与 §8.1 `table_headers_metadata` 的断言要求）
            table_headers: if chunk_type == "table" {
                extract_table_headers(&body)
            } else {
                None
            },
        }
    }

    /// 构建标题前缀：
    /// - context：`# Kubernetes\n## Network`（LLM 可读的完整 Markdown 渲染）
    /// - embedding：紧凑路径（最近 ≤`EMBED_PREFIX_MAX_LEVELS` 级 + token 预算裁剪，
    ///   ≤ `prefix_max_tokens`），供向量化
    fn build_prefixes(
        path: &[String],
        prefix_max_tokens: usize,
        counter: &dyn TokenCounter,
    ) -> (String, String) {
        // context：完整渲染（给 LLM 阅读，不受 embedding 预算约束）
        let mut context = String::new();
        for (i, heading) in path.iter().enumerate() {
            context.push_str(&"#".repeat(i + 1));
            context.push(' ');
            context.push_str(heading);
            context.push('\n');
        }
        context.pop(); // 去掉末尾换行

        // embed：最近 ≤3 级，逐步丢弃最左层级直到 ≤ prefix_max_tokens；单级仍超则
        // 按 token 实测二分截断（🟠 M2 修复：旧实现按「4 字符 ≈ 1 token」近似，
        // 中文 1 字符 ≈ 1 token 下会把前缀放大到预算的 4 倍、标题词主导向量）。
        let n = path.len();
        let mut start = n.saturating_sub(EMBED_PREFIX_MAX_LEVELS);
        let embed = loop {
            let parts = &path[start..];
            let candidate = parts.join(" ");
            let tokens = counter.count(&candidate).unwrap_or_else(|| candidate.chars().count());
            if tokens <= prefix_max_tokens || start >= n.saturating_sub(1) {
                if tokens <= prefix_max_tokens {
                    break candidate;
                }
                // 单级仍超预算：二分查找预算内最长前缀（counter 精确；不可用时
                // 退化 1 字符 ≈ 1 token，CJK 保守）。预留 1 token 给结尾省略号。
                let budget = prefix_max_tokens.saturating_sub(1).max(1);
                let total_chars = candidate.chars().count();
                let (mut lo, mut hi, mut best) = (1usize, total_chars, 1usize);
                while lo <= hi {
                    let mid = (lo + hi) / 2;
                    let prefix: String = candidate.chars().take(mid).collect();
                    let t = counter.count(&prefix).unwrap_or(mid);
                    if t <= budget {
                        best = mid;
                        lo = mid + 1;
                    } else {
                        hi = mid - 1;
                    }
                }
                if best < total_chars {
                    let truncated: String = candidate.chars().take(best).collect();
                    break format!("{}…", truncated);
                }
                break candidate;
            }
            start += 1;
        };

        (context, embed)
    }
}

impl ChunkEngine for SemanticChunkEngine {
    fn build(&self, document: &DocumentNode) -> Vec<Chunk> {
        log::debug!(
            "[chunk_engine] 开始分块: max_tokens={} overlap={} min_body_reserve={} prefix_max={} 根子块数={}",
            self.max_tokens,
            self.overlap_tokens,
            self.min_body_reserve_tokens,
            self.prefix_max_tokens,
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

/// 取文本尾部最近 N 字符（跨块上下文 overlap 用）
fn tail_chars(text: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    text.chars().rev().take(n).collect::<Vec<_>>().into_iter().rev().collect()
}

/// 统计块集合的主导类型（单块 → 其类型；混合 → 频次最高者，平手归为 section）
fn dominant_type(blocks: &[BlockEntry]) -> String {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for b in blocks {
        *counts.entry(b.kind.as_str()).or_insert(0) += 1;
    }
    let mut best: (&str, usize) = ("section", 0);
    for (t, count) in counts {
        if count > best.1 {
            best = (t, count);
        }
    }
    best.0.to_string()
}

/// GFM 表格分隔行判定：形如 `| --- | :--: |`（只含 `-`、`:` 与管道符）。
fn is_gfm_separator_row(line: &str) -> bool {
    let t = line.trim();
    if !t.starts_with('|') {
        return false;
    }
    let inner = t.trim_matches('|');
    !inner.is_empty()
        && inner.split('|').all(|c| {
            let c = c.trim();
            !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':')
        })
}

/// 从 chunk 正文中提取**第一个 GFM 表格的表头列名**（JSON 数组字符串）。
///
/// Plan B v2 §4.7 / 决策 R3：只把**列名**补进 metadata，不新建 TableChunker。
///
/// 判定依据是「表头行 + 紧随其后的分隔行」这一 GFM 硬性结构，而不是"以 `|` 开头"——
/// 后者会把数据行误判成表头（`split_oversize_table` 的分片正文首行也可能看似表头）。
/// 正文里可能先出现段落（分组 chunk），故**扫描**而非只取首行。
///
/// 返回 `None` 表示正文中不存在结构完整的 GFM 表格。
fn extract_table_headers(body: &str) -> Option<String> {
    let lines: Vec<&str> = body.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if !line.trim().starts_with('|') {
            continue;
        }
        let Some(next) = lines.get(i + 1) else { break };
        if !is_gfm_separator_row(next) {
            continue;
        }
        let cols: Vec<String> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect();
        if cols.is_empty() {
            return None;
        }
        // 列名进 metadata：序列化为 JSON 数组，与 path_json/tags 的既有约定一致
        return serde_json::to_string(&cols).ok();
    }
    None
}

/// 超长表格按行分组切分，每个分片重复表头行（表头 + 分隔行），保持表格语义单元完整。
///
/// 规则：
/// - 少于 4 行（表头 + 分隔行 + 少于 2 行数据）的最小表格整体保留
///   （🟠 M6 对齐 `db/token_budget.rs::TableReSplitStrategy` 的原子阈值 `<= 3`，
///   避免「引擎可切 / Validator 判原子」的两套口径分歧）
/// - 正文行贪心分组：`表头 + 当前组` 超长时开新组
/// - 单行本身超长时不做行内切分（行是原子的，含超长单元格的宽表不被拆散）
fn split_oversize_table(text: &str, max_chars: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= 3 {
        log::debug!(
            "[chunk_engine]   表格行数={} (表头+分隔行+≤1 行数据) → 整体保留不拆分",
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
    log::info!(
        "[chunk_engine]   表格切分: 总行数={} 数据行={} 表头={}字 上限={}字 → {} 片(每片重复表头)",
        lines.len(),
        lines.len().saturating_sub(2),
        header_chars,
        max_chars,
        chunks.len()
    );
    chunks
}

// ─── P0-3 测试：token 感知引擎不变量（fake counter 注入，离线可跑） ───

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::document::parser::MarkdownParser;
    use crate::core::document::token_budget::test_util::FixedRateCounter;
    use crate::core::document::ComrakMarkdownParser;

    fn engine(max_tokens: usize, overlap: usize, prefix_max: usize) -> SemanticChunkEngine {
        SemanticChunkEngine::new(
            max_tokens,
            overlap,
            16,
            prefix_max,
            Arc::new(FixedRateCounter { chars_per_token: 1 }),
        )
    }

    fn doc_from(md: &str) -> DocumentNode {
        ComrakMarkdownParser.parse(md, true)
    }

    /// I-2（引擎）：每个 chunk 的 embedding_text 不超过 max_tokens + overlap（尾部）
    #[test]
    fn chunks_respect_token_budget() {
        let e = engine(30, 4, 10);
        let md = format!("## 主题\n\n{}", "这是一段正文内容，用于测试分块。".repeat(60));
        let doc = doc_from(&md);
        let chunks = e.build(&doc);
        assert!(!chunks.is_empty(), "应产出 chunk");
        for c in &chunks {
            let t = e.tokens(&c.embedding_text);
            assert!(t <= 34, "embedding 超预算: {} token", t);
        }
        assert!(chunks.len() > 1, "超长正文应被分块");
    }

    /// 整节未超预算 → 单 chunk（不碎片化）
    #[test]
    fn small_section_single_chunk() {
        let e = engine(100, 8, 20);
        let doc = doc_from("# 短节\n\n只有一小段。");
        let chunks = e.build(&doc);
        assert_eq!(chunks.len(), 1, "短节应保持单 chunk");
    }

    /// P0-2：embed 前缀截断——最多最近 3 级且 ≤ prefix_max_tokens
    #[test]
    fn embed_prefix_truncated_to_budget_and_levels() {
        let e = engine(200, 0, 12);
        let md = "## 层级一\n\n### 层级二\n\n#### 层级三\n\n##### 层级四\n\n正文内容段落。\n\n";
        let doc = doc_from(md);
        let chunks = e.build(&doc);
        assert!(!chunks.is_empty());
        let first = &chunks[0];
        assert_eq!(first.path.len(), 4, "context 保留完整路径");
        let prefix = first.embedding_text.split('\n').next().unwrap_or("");
        assert!(
            prefix.chars().count() <= 12,
            "embed 前缀超预算: {:?}",
            prefix
        );
        assert!(
            prefix.matches(' ').count() <= 2,
            "embed 前缀级数超过 3: {:?}",
            prefix
        );
        assert!(!prefix.contains("层级一"), "最左层级应被裁剪: {:?}", prefix);
    }

    /// 组间 overlap：后一块包含前一块正文尾部（跨块上下文完整）
    #[test]
    fn group_overlap_prepends_tail() {
        let e = engine(40, 6, 10);
        let md = format!(
            "# 章节\n\n{}",
            "段落甲内容段落甲内容。\n\n段落乙内容段落乙内容。\n\n段落丙内容段落丙内容。".repeat(6)
        );
        let doc = doc_from(&md);
        let chunks = e.build(&doc);
        assert!(chunks.len() >= 2, "应产出多个 chunk: {}", chunks.len());
        let t0: String = chunks[0]
            .embedding_text
            .chars()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert!(
            !t0.trim().is_empty() && chunks[1].embedding_text.contains(&t0),
            "后块应包含前块尾部（overlap）: {:?}",
            t0
        );
    }

    /// 超长表格：分片重复表头（token 预算经密度折算）
    #[test]
    fn oversize_table_repeats_header() {
        let e = engine(120, 0, 20);
        let mut md = String::from("# 表\n\n| 列A | 列B |\n|---|---|\n");
        for i in 0..30 {
            md.push_str(&format!("| 值{} | 数据内容{} |\n", i, i));
        }
        let doc = doc_from(&md);
        let chunks = e.build(&doc);
        assert!(chunks.len() >= 2, "超长表格应分片: {}", chunks.len());
        for c in &chunks {
            if c.chunk_type == "table" && c.text.contains("| 值") {
                assert!(
                    c.text.contains("| 列A | 列B |\n|---|---|"),
                    "表格分片必须重复表头: {:?}",
                    c.text.chars().take(60).collect::<String>() // 按字符截断，避免中文字节切片 panic
                );
            }
        }
    }

    /// §8.1 `table_headers_metadata`（Plan B v2 §4.7 / 决策 R3）：
    /// GFM 表格块的 `table_headers` 必须正确，且**绝不进 `embedding_text`**。
    #[test]
    fn table_headers_metadata_extracted_and_not_in_embedding() {
        let e = engine(400, 0, 40);
        let md = "# 销售\n\n| Postcode | Sales_Rep_Name | Value |\n|---|---|---|\n| 2121 | Jane | $84,219 |\n| 2092 | Ashish | $28,322 |\n";
        let doc = doc_from(md);
        let chunks = e.build(&doc);

        let table = chunks
            .iter()
            .find(|c| c.chunk_type == "table")
            .unwrap_or_else(|| panic!("应产出 table 类型 chunk，实际: {:?}",
                chunks.iter().map(|c| c.chunk_type.as_str()).collect::<Vec<_>>()));

        let raw = table.table_headers.as_deref().expect("表格块必须有表头列名");
        let cols: Vec<String> = serde_json::from_str(raw).expect("表头必须是 JSON 数组");
        assert_eq!(
            cols,
            vec!["Postcode", "Sales_Rep_Name", "Value"],
            "表头列名必须与源表格一致（按出现顺序）"
        );

        // 核心断言（§8.1「**不进 embedding_text**」的准确含义）：
        // 表头行本身就是 GFM 表格正文的一部分，**不可能**（也不应该）从正文里剔除；
        // 这条要求禁的是把 `table_headers` 这个 **metadata 字段额外注入**向量文本
        // （例如再拼一行 `表头: A/B/C`）。故判据是**出现次数不增加**：
        // 每个列名在 embedding_text 中只出现 1 次（来自正文表头行），
        // 若哪天真被额外注入，次数会变成 2 —— 该断言即失效报警。
        for col in ["Postcode", "Sales_Rep_Name", "Value"] {
            assert_eq!(
                table.embedding_text.matches(col).count(),
                1,
                "列名 {} 在 embedding_text 中出现次数应为 1（仅正文表头行），\
                 出现 >1 说明 table_headers 被额外注入了向量文本: {:?}",
                col,
                table.embedding_text.chars().take(80).collect::<String>()
            );
        }

        // 非表格块不得有表头（防止把普通段落误判为表格）
        let para_md = "# 标题\n\n这只是一段普通正文，没有表格。\n";
        for c in engine(400, 0, 40).build(&doc_from(para_md)) {
            assert!(
                c.table_headers.is_none(),
                "非表格块的 table_headers 必须为 None，实际: {:?} for type={}",
                c.table_headers,
                c.chunk_type
            );
        }
    }

    /// 表头判定必须依赖「表头行 + 分隔行」结构，不能只看"行首是 `|`"——
    /// 否则数据行会被误判成表头（`split_oversize_table` 的分片首行也有此风险）。
    #[test]
    fn table_header_detection_requires_separator_row() {
        // 只有数据行、没有分隔行 → 不是 GFM 表格
        assert_eq!(extract_table_headers("| a | b |\n| c | d |\n"), None);
        // 正常 GFM 表
        assert_eq!(
            extract_table_headers("| a | b |\n| --- | --- |\n| c | d |").as_deref(),
            Some(r#"["a","b"]"#)
        );
        // 对齐分隔行（:---:）同样成立
        assert_eq!(
            extract_table_headers("| x | y |\n|:--|--:|\n| 1 | 2 |").as_deref(),
            Some(r#"["x","y"]"#)
        );
        // 表头前有段落时仍能定位（分组 chunk 的正文可能先出现段落）
        assert_eq!(
            extract_table_headers("先来一段说明。\n\n| p | q |\n|---|---|\n| 1 | 2 |").as_deref(),
            Some(r#"["p","q"]"#)
        );
        // 无表格
        assert_eq!(extract_table_headers("纯文本，无表格。"), None);
    }

    /// 性能守卫：~1MB 文档分块在宽松时间上限内完成（防 O(n²) 回归）
    #[test]
    fn perf_guard_large_doc_bounded() {
        let e = engine(448, 56, 40);
        let md = format!("# 大文档\n\n{}", "段落内容，用于分块性能测试。\n\n".repeat(30_000));
        let start = std::time::Instant::now();
        let doc = doc_from(&md);
        let chunks = e.build(&doc);
        let elapsed = start.elapsed();
        assert!(!chunks.is_empty());
        assert!(
            elapsed.as_secs() < 20,
            "大文档分块过慢（防 O(n²) 回归）: {:?}",
            elapsed
        );
        // 内容覆盖抽查
        let merged: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert!(merged.contains("用于分块性能测试"), "内容不应丢失");
    }
}
