//! Token Budget 体系（P0-1 / P0-2 核心层）。
//!
//! 背景（见 `docs/分块 Token 预算设计.md`）：
//! - 分块层（ChunkSplitter / SemanticChunkEngine）只负责**语义分组**，
//!   不再"猜测" embedding 模型能否吃下该 chunk；
//! - 由本模块的 [`TokenBudgetValidator`] 做**最终裁决**：任何进入 embedding 的
//!   `embedding_text` 必须先通过 token 预算校验，超限走"按类型重切 → 显式降级"，
//!   彻底取消"超限自动截断后继续 embedding"的静默行为（embedding 层截断保留为
//!   兜底但必须计数告警，见 `core/embedding.rs`）。
//!
//! 分层说明：纯类型（`ChunkBudget` / `TokenCounter` / `char_budget_pair` 等）
//! 位于 `document` 基础层（`core::document::token_budget`，供分块引擎使用），
//! 本模块位于 `db` 层（依赖 `ChunkResult`），承载重切策略与最终裁决；
//! 通过 `pub use` 转发纯类型，外部调用点（`core::db::token_budget::*`）保持不变。

use std::sync::Arc;

use super::chunk_splitter::ChunkResult;

// ─── 纯类型转发（定义见 core::document::token_budget） ───
// 仅转发外部调用点实际使用的符号，避免未使用告警；其余符号请直接引用
// `crate::core::document::token_budget::*`。

pub use crate::core::document::token_budget::{
    budget_from_config, global_token_counter, max_chunk_tokens, ChunkBudget, TokenCounter,
};

// ─── ChunkNormalizer：规范化（P0-5 内容哈希的前置步骤） ───

/// 规范化 chunk 文本。规则写死防漂移：
/// - BOM 剥离；
/// - 换行归一（`\r\n` → `\n`，与 markdown 解析器唯一 normalize 点对齐）；
/// - 每行行尾空白剔除；
/// - 不做大小写折叠 / 标点归一（避免过度归一化导致语义坍缩）。
fn normalize_text(s: &str) -> String {
    let s = s.strip_prefix('\u{feff}').unwrap_or(s);
    let mut out = String::with_capacity(s.len());
    for line in s.split('\n') {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.pop(); // 去掉最后一个 \n（空输入时 pop 无副作用）
    // 🟠 L10：旧 Mac `\r` 单独换行（非 \r\n）不在 split('\n') 的行尾，
    // trim_end 不处理——极边缘场景，保持现状（\r\n 已由 trim_end 归一）。
    out
}

/// 规范化整批 chunk（`text` 与 `embedding_text` 同步处理）。
pub fn normalize_chunks(chunks: Vec<ChunkResult>) -> Vec<ChunkResult> {
    chunks
        .into_iter()
        .map(|mut c| {
            c.text = normalize_text(&c.text);
            if let Some(emb) = c.embedding_text.take() {
                c.embedding_text = Some(normalize_text(&emb));
            }
            c
        })
        .collect()
}

// ─── ReSplitStrategy：按 chunk 类型的重切策略注册表 ───

/// 重切策略：在 token 预算内把超限 chunk 切成语义完整的小块。
///
/// 返回 `None` 表示该 chunk 是**原子块**（如单行超长代码、超宽表行），无法在
/// 不破坏语义的前提下重切——由 Validator 走显式降级分支。
pub trait ReSplitStrategy: Send + Sync {
    fn can_handle(&self, chunk: &ChunkResult) -> bool;
    fn resplit(
        &self,
        chunk: &ChunkResult,
        budget: &ChunkBudget,
        counter: &dyn TokenCounter,
    ) -> Option<Vec<ChunkResult>>;
}

/// 按实际 token 密度折算的字符预算：超限时用当前密度比例换算字符目标，
/// 并留 8 token 安全余量，保证重切后的分片能通过再校验。
fn proportional_char_budget(
    chunk: &ChunkResult,
    budget: &ChunkBudget,
    counter: &dyn TokenCounter,
) -> Option<usize> {
    let emb = chunk.embedding_text.as_deref().unwrap_or(&chunk.text);
    let tokens = counter.count(emb)?;
    let chars = emb.chars().count();
    if chars == 0 || tokens == 0 {
        return None;
    }
    let target = (chars as f64)
        * (budget.hard_max_tokens as f64 - 8.0)
        / (tokens as f64);
    Some(target.floor().max(1.0) as usize)
}

/// 把切好的文本片段重建为 ChunkResult 列表（保留路径/符号元数据）。
///
/// - 原 chunk 有 `embedding_text`（AST 路径）：`text` 与 `embedding_text` 同置为片段
///   （降级为紧凑路径文本，不再保留 Markdown 标题渲染——重切是低频兜底路径，可接受）；
/// - 原 chunk 无 `embedding_text`（代码/纯文本/树形）：保持 `None`（向量化退用 `text`）。
fn apply_pieces(chunk: &ChunkResult, pieces: Vec<String>) -> Vec<ChunkResult> {
    let has_embedding = chunk.embedding_text.is_some();
    pieces
        .into_iter()
        .map(|p| {
            let mut c = chunk.clone();
            c.text = p.clone();
            c.embedding_text = has_embedding.then_some(p);
            c
        })
        .collect()
}

/// 把 token 语义的 overlap 按文本实际 token 密度折算为**字符** overlap（🟠 M5 修复）。
///
/// `split_text_with_separators` 的 overlap 参数是字符维度——直接传 `budget.overlap_tokens`
/// （token 语义）在英文等低密度文本（≈4 字符/token）下会把实际 overlap 缩水到 1/4，
/// 跨块上下文衔接显著变弱。counter 不可用时退化 1 字符 ≈ 1 token（CJK 最坏情形）。
fn char_overlap_for(
    text: &str,
    overlap_tokens: usize,
    counter: &dyn TokenCounter,
) -> usize {
    if overlap_tokens == 0 {
        return 0; // 0 表示无重叠，不得被 max(1) 强改
    }
    match counter.count(text) {
        Some(t) if t > 0 => {
            let chars = text.chars().count();
            let density = chars as f64 / t as f64; // 每 token 字符数
            ((overlap_tokens as f64 * density) as usize).max(1)
        }
        _ => overlap_tokens,
    }
}

/// 正文策略：段落/引用/列表/章节等按 段落 → 句子 → 字符 逐级降级切分。
struct ProseReSplitStrategy;

impl ReSplitStrategy for ProseReSplitStrategy {
    fn can_handle(&self, chunk: &ChunkResult) -> bool {
        matches!(
            chunk.chunk_type.as_deref(),
            Some("paragraph" | "quote" | "list" | "section" | "html" | "root" | "heading")
                | None
        )
    }

    fn resplit(
        &self,
        chunk: &ChunkResult,
        budget: &ChunkBudget,
        counter: &dyn TokenCounter,
    ) -> Option<Vec<ChunkResult>> {
        let text = chunk.embedding_text.as_deref().unwrap_or(&chunk.text);
        let char_budget = proportional_char_budget(chunk, budget, counter)?;
        // 🟠 M5：overlap 按同一密度折算为字符（token 值不能直接当字符数传）
        let overlap_chars = char_overlap_for(text, budget.overlap_tokens, counter);
        let pieces = crate::core::document::text_split::split_text_with_separators(
            text,
            char_budget,
            overlap_chars,
            &["\n\n", "\n", ". ", "。", "！", "？", "；", " ", ""],
        );
        if pieces.len() <= 1 {
            return None; // 切不动 → 原子块
        }
        Some(apply_pieces(chunk, pieces))
    }
}

/// 代码策略：按行边界切分；符号名/类型仅保留在首片（后续片是延续代码）。
struct CodeReSplitStrategy;

impl ReSplitStrategy for CodeReSplitStrategy {
    fn can_handle(&self, chunk: &ChunkResult) -> bool {
        chunk.chunk_type.as_deref() == Some("code") || chunk.symbol_name.is_some()
    }

    fn resplit(
        &self,
        chunk: &ChunkResult,
        budget: &ChunkBudget,
        counter: &dyn TokenCounter,
    ) -> Option<Vec<ChunkResult>> {
        let text = chunk.embedding_text.as_deref().unwrap_or(&chunk.text);
        let char_budget = proportional_char_budget(chunk, budget, counter)?;
        // 🟠 M5：overlap 按同一密度折算为字符
        let overlap_chars = char_overlap_for(text, budget.overlap_tokens, counter);
        let pieces = crate::core::document::text_split::split_text_with_separators(
            text,
            char_budget,
            overlap_chars,
            &["\n", " ", ""],
        );
        if pieces.len() <= 1 {
            return None;
        }
        let has_embedding = chunk.embedding_text.is_some();
        let mut out = Vec::with_capacity(pieces.len());
        for (i, p) in pieces.into_iter().enumerate() {
            let mut c = chunk.clone();
            c.text = p.clone();
            c.embedding_text = has_embedding.then_some(p);
            if i > 0 {
                c.symbol_name = None;
                c.symbol_kind = None;
            }
            out.push(c);
        }
        Some(out)
    }
}

/// 表格策略：按行分组 + 重复表头（保持表格语义单元完整，与 SemanticChunkEngine 一致）。
struct TableReSplitStrategy;

impl ReSplitStrategy for TableReSplitStrategy {
    fn can_handle(&self, chunk: &ChunkResult) -> bool {
        chunk.chunk_type.as_deref() == Some("table")
    }

    fn resplit(
        &self,
        chunk: &ChunkResult,
        budget: &ChunkBudget,
        counter: &dyn TokenCounter,
    ) -> Option<Vec<ChunkResult>> {
        let text = chunk.embedding_text.as_deref().unwrap_or(&chunk.text);
        let char_budget = proportional_char_budget(chunk, budget, counter)?;
        let lines: Vec<&str> = text.lines().collect();
        // 表头 + 分隔行 + 至少 1 行数据；少于 4 行的表格视为原子块
        if lines.len() <= 3 {
            return None;
        }
        let header = format!("{}\n{}", lines[0], lines[1]);
        let header_chars = header.chars().count();
        if header_chars > char_budget {
            return None; // 表头本身超限：原子块
        }
        // 最小分片 = 表头 + 单行数据；若最小分片都超预算 → 表格过宽，整体视为
        // 原子块（显式降级一次），避免产出"注定再超限"的分片逐片截断刷屏。
        let min_row_chars = lines[2..]
            .iter()
            .map(|r| r.chars().count() + 1)
            .min()
            .unwrap_or(0);
        if header_chars + min_row_chars > char_budget {
            return None;
        }
        let mut pieces: Vec<String> = Vec::new();
        let mut current: Vec<&str> = Vec::new();
        let mut current_chars = 0usize;
        for row in &lines[2..] {
            let row_chars = row.chars().count() + 1; // +1 换行
            if !current.is_empty() && header_chars + current_chars + row_chars > char_budget {
                pieces.push(format!("{}\n{}", header, current.join("\n")));
                current.clear();
                current_chars = 0;
            }
            current.push(row);
            current_chars += row_chars;
        }
        if !current.is_empty() {
            pieces.push(format!("{}\n{}", header, current.join("\n")));
        }
        if pieces.len() <= 1 {
            return None;
        }
        Some(apply_pieces(chunk, pieces))
    }
}

// ─── TokenBudgetValidator：最终裁决层 ───

/// 校验结果统计（进 `KbIndexResult` / 日志；健康态要求 `truncated_count = 0`）。
#[derive(Debug, Default, Clone)]
pub struct ValidationReport {
    pub chunks_in: usize,
    pub chunks_out: usize,
    /// 重切次数（超限 chunk 被细分）
    pub resplit_count: usize,
    /// 显式降级数（原子块超限，允许截断但已计数告警）
    pub truncated_count: usize,
    /// tokenizer 不可用，降级为字符预算
    pub degraded_token_count: bool,
}

/// 最终裁决层：任何进入 embedding 的 `embedding_text` 必须通过预算校验。
pub struct TokenBudgetValidator {
    pub budget: ChunkBudget,
    counter: Arc<dyn TokenCounter>,
    strategies: Vec<Box<dyn ReSplitStrategy>>,
    max_resplit_rounds: usize,
}

impl TokenBudgetValidator {
    pub fn new(budget: ChunkBudget, counter: Arc<dyn TokenCounter>) -> Self {
        Self {
            budget,
            counter,
            strategies: vec![
                Box::new(TableReSplitStrategy),
                Box::new(CodeReSplitStrategy),
                Box::new(ProseReSplitStrategy),
            ],
            // 最多 2 轮重切（防振荡：重切后仍超限的分片走降级）
            max_resplit_rounds: 2,
        }
    }

    /// 主入口：校验 + 按需重切。返回（最终 chunks, 统计报告）。
    pub fn validate(&self, chunks: Vec<ChunkResult>) -> (Vec<ChunkResult>, ValidationReport) {
        let mut report = ValidationReport {
            chunks_in: chunks.len(),
            ..ValidationReport::default()
        };
        let mut out = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            out.extend(self.validate_one(chunk, 0, &mut report));
        }
        report.chunks_out = out.len();
        (out, report)
    }

    /// 只统计不重切（对话消息等原子单元场景）：chunks 原样返回，超限仅计数告警。
    pub fn count_only(&self, chunks: Vec<ChunkResult>) -> (Vec<ChunkResult>, ValidationReport) {
        let mut report = ValidationReport {
            chunks_in: chunks.len(),
            ..ValidationReport::default()
        };
        let mut out = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let emb = chunk.embedding_text.as_deref().unwrap_or(&chunk.text);
            match self.counter.count(emb) {
                Some(n) if n > self.budget.hard_max_tokens => {
                    report.truncated_count += 1;
                    let preview: String = emb.chars().take(60).collect();
                    log::warn!(
                        "[token_budget] 原子 chunk 超限（{} token > {}）将截断，前 {} 字符: {:?}",
                        n,
                        self.budget.hard_max_tokens,
                        preview.chars().count(),
                        preview
                    );
                }
                None => report.degraded_token_count = true,
                _ => {}
            }
            out.push(chunk);
        }
        report.chunks_out = out.len();
        (out, report)
    }

    fn validate_one(
        &self,
        chunk: ChunkResult,
        round: usize,
        report: &mut ValidationReport,
    ) -> Vec<ChunkResult> {
        let emb = chunk.embedding_text.as_deref().unwrap_or(&chunk.text);
        match self.counter.count(emb) {
            Some(n) if n <= self.budget.hard_max_tokens => vec![chunk],
            Some(n) => {
                if round >= self.max_resplit_rounds {
                    report.truncated_count += 1;
                    log::warn!(
                        "[token_budget] 重切 {} 轮后仍超限（{} token > {}），显式降级截断: chunk_type={:?}",
                        round,
                        n,
                        self.budget.hard_max_tokens,
                        chunk.chunk_type
                    );
                    vec![chunk]
                } else {
                    let strategy = self.strategies.iter().find(|s| s.can_handle(&chunk));
                    match strategy.and_then(|s| s.resplit(&chunk, &self.budget, &*self.counter)) {
                        Some(pieces) if !pieces.is_empty() => {
                            report.resplit_count += 1;
                            let mut out = Vec::new();
                            for p in pieces {
                                out.extend(self.validate_one(p, round + 1, report));
                            }
                            out
                        }
                        _ => {
                            // 原子块无法重切 → 显式降级（允许截断但计数告警，绝不静默）
                            report.truncated_count += 1;
                            log::warn!(
                                "[token_budget] 原子块无法重切（{} token > {}），显式降级截断: chunk_type={:?}",
                                n,
                                self.budget.hard_max_tokens,
                                chunk.chunk_type
                            );
                            vec![chunk]
                        }
                    }
                }
            }
            None => {
                // tokenizer 不可用 → 降级字符预算（保守：1 字符 ≈ 1 token，CJK 最坏情形）
                // 🟠 L11：降级分支的字符分片**不递归复检**，英文等低密度文本分片仍可能
                // 远超 token 预算（仅 degraded_token_count 置位告知）——降级路径不保证
                // 硬上限，由 embedding 层截断兜底（计数告警）。
                report.degraded_token_count = true;
                let char_limit = self.budget.hard_max_tokens;
                if emb.chars().count() > char_limit {
                    let pieces = crate::core::document::text_split::split_text_with_separators(
                        emb,
                        char_limit,
                        self.budget.overlap_tokens,
                        &["\n\n", "\n", ". ", "。", "！", "？", "；", " ", ""],
                    );
                    if pieces.len() <= 1 {
                        report.truncated_count += 1;
                        vec![chunk]
                    } else {
                        report.resplit_count += 1;
                        apply_pieces(&chunk, pieces)
                    }
                } else {
                    vec![chunk]
                }
            }
        }
    }
}

// ─── 测试（P0-3 首批不变量；fake counter 注入，不依赖模型下载） ───

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定 token 密度计数器：`chars_per_token` 字符 = 1 token
    struct FixedRateCounter {
        chars_per_token: usize,
    }

    impl TokenCounter for FixedRateCounter {
        fn count(&self, text: &str) -> Option<usize> {
            Some(text.chars().count().div_ceil(self.chars_per_token))
        }

        fn token_char_boundaries(&self, text: &str) -> Option<Vec<usize>> {
            let n = text.chars().count();
            let mut boundaries: Vec<usize> = (0..=n).step_by(self.chars_per_token).collect();
            if *boundaries.last().unwrap() != n {
                boundaries.push(n);
            }
            Some(boundaries)
        }
    }

    fn budget(hard_max: usize) -> ChunkBudget {
        ChunkBudget {
            hard_max_tokens: hard_max,
            target_tokens: hard_max.saturating_sub(8),
            min_tokens: hard_max / 3,
            overlap_tokens: 4,
            prefix_max_tokens: 8,
            special_tokens_reserve: 8,
        }
    }

    fn validator(hard_max: usize) -> TokenBudgetValidator {
        TokenBudgetValidator::new(budget(hard_max), Arc::new(FixedRateCounter { chars_per_token: 1 }))
    }

    fn plain(text: &str) -> ChunkResult {
        ChunkResult::plain(text.to_string())
    }

    fn joined(chunks: &[ChunkResult]) -> String {
        chunks.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    /// I-2：任何输出 chunk 的 embedding_text token 数 ≤ hard_max（用 1 字符 = 1 token 断言）
    #[test]
    fn i2_no_chunk_exceeds_budget() {
        let v = validator(20);
        let input: Vec<ChunkResult> = (0..3)
            .map(|i| plain(&format!("第 {} 段。{}", i, "这是一个很长的段落。".repeat(30))))
            .collect();
        let (out, report) = v.validate(input);
        assert_eq!(report.truncated_count, 0, "常规超限应全部重切成功: {:?}", report);
        for c in &out {
            let text = c.embedding_text.as_deref().unwrap_or(&c.text);
            assert!(text.chars().count() <= 20, "chunk 超预算: {} 字符", text.chars().count());
        }
        assert!(out.len() > 3, "超限 chunk 应被细分");
    }

    /// I-1：全文覆盖——无 overlap 时拼接输出应包含全部输入内容
    #[test]
    fn i1_full_coverage_no_loss() {
        let mut b = budget(20);
        b.overlap_tokens = 0;
        let v = TokenBudgetValidator::new(b, Arc::new(FixedRateCounter { chars_per_token: 1 }));
        let body = "内容甲。内容乙。内容丙。内容丁。内容戊。内容己。内容庚。内容辛。";
        let (out, _) = v.validate(vec![plain(body)]);
        let merged = joined(&out);
        for kw in ["内容甲", "内容辛", "内容戊"] {
            assert!(merged.contains(kw), "内容丢失: {} 不在输出中", kw);
        }
        // 无 overlap 时拼接应恰好覆盖（允许分隔符差异，不允许缺内容）
        let stripped: String = out.iter().map(|c| c.text.replace('\n', "")).collect();
        assert_eq!(stripped, body, "无 overlap 时拼接应等于原文");
    }

    /// I-3：空输入 → 空输出；校验器不产生空 chunk
    #[test]
    fn i3_empty_handling() {
        let v = validator(20);
        let (out, report) = v.validate(vec![]);
        assert!(out.is_empty());
        assert_eq!(report.chunks_in, 0);
        assert_eq!(report.chunks_out, 0);
        // 超短 chunk（低于 min_tokens）仍应保留（min 是合并目标不是硬下限）
        let (out2, _) = v.validate(vec![plain("短")]);
        assert_eq!(out2.len(), 1);
    }

    /// I-4 / I-10：确定性 + 幂等——同输入两次输出一致；二次校验零重切零截断
    #[test]
    fn i4_deterministic_and_i10_idempotent() {
        let v = validator(20);
        let input: Vec<ChunkResult> = vec![
            plain(&"句子。".repeat(50)),
            plain(&format!("标题\n\n{}", "段落内容，".repeat(40))),
        ];
        let (out1, report1) = v.validate(input.clone());
        let (out2, report2) = v.validate(input);
        assert_eq!(joined(&out1), joined(&out2), "同输入应产出完全一致的输出");
        assert_eq!(report1.resplit_count, report2.resplit_count);

        let (out3, report3) = v.validate(out1);
        assert_eq!(report3.resplit_count, 0, "幂等：二次校验不应再重切");
        assert_eq!(report3.truncated_count, 0, "幂等：二次校验不应再降级");
        assert_eq!(joined(&out3), joined(&out2));
    }

    /// I-2（表格）：超长表格按行分片并重复表头，每片 ≤ 预算
    #[test]
    fn table_resplit_repeats_header() {
        let v = validator(50);
        let mut table = String::from("| 列A | 列B |\n|---|---|\n");
        for i in 0..12 {
            table.push_str(&format!("| 值{} | 数据内容{} |\n", i, i));
        }
        let chunk = {
            let mut c = plain(&table);
            c.chunk_type = Some("table".to_string());
            c
        };
        let (out, report) = v.validate(vec![chunk]);
        assert_eq!(report.truncated_count, 0, "可切表格不应截断: {:?}", report);
        assert!(out.len() >= 2, "12 行表格应被分片: {}", out.len());
        for c in &out {
            assert!(c.text.starts_with("| 列A | 列B |"), "每片必须重复表头");
            let t = c.embedding_text.as_deref().unwrap_or(&c.text);
            assert!(t.chars().count() <= 50, "分片超预算: {}", t.chars().count());
        }
    }

    /// I-2（表格-过宽原子块）：表头+单行都超预算时整体视为原子块，仅 1 次显式降级
    #[test]
    fn table_wide_atomic_single_truncation() {
        let v = validator(30); // 预算过小，表头+最小行 (34) > 30
        let mut table = String::from("| 列A | 列B |\n|---|---|\n");
        for i in 0..12 {
            table.push_str(&format!("| 值{} | 数据内容{} |\n", i, i));
        }
        let chunk = {
            let mut c = plain(&table);
            c.chunk_type = Some("table".to_string());
            c
        };
        let (out, report) = v.validate(vec![chunk]);
        assert_eq!(out.len(), 1, "过宽表格不应被拆散");
        assert_eq!(report.truncated_count, 1, "整体原子降级应为 1 次");
    }

    /// I-2（代码）：符号名只保留在首片
    #[test]
    fn code_resplit_keeps_symbol_on_first_piece() {
        let v = validator(24);
        let mut code = String::new();
        for i in 0..20 {
            code.push_str(&format!("line_{} = {};\n", i, i));
        }
        let mut chunk = ChunkResult::code(code, Some("main".to_string()), Some("function".to_string()));
        chunk.chunk_type = Some("code".to_string());
        let (out, report) = v.validate(vec![chunk]);
        assert_eq!(report.truncated_count, 0);
        assert!(out.len() >= 2);
        assert_eq!(out[0].symbol_name.as_deref(), Some("main"), "符号保留在首片");
        assert!(out[1..].iter().all(|c| c.symbol_name.is_none()), "延续片不携带符号");
    }

    /// count_only：原子单元（对话消息）只计数不重切
    #[test]
    fn count_only_does_not_resplit() {
        let v = validator(20);
        let long = plain(&"很长很长的消息内容。".repeat(20));
        let (out, report) = v.count_only(vec![long.clone()]);
        assert_eq!(out.len(), 1, "count_only 不重切");
        assert_eq!(out[0].text, long.text);
        assert_eq!(report.truncated_count, 1, "超限应被计数");
        assert_eq!(report.resplit_count, 0);
    }

    /// I-8 退化：normalize 不改变语义内容（仅清行尾空白/BOM）
    #[test]
    fn normalize_is_semantics_preserving() {
        let chunks = normalize_chunks(vec![plain("内容一 \n内容二  \n\n内容三")]);
        assert_eq!(chunks[0].text, "内容一\n内容二\n\n内容三");
        let bom = normalize_chunks(vec![plain("\u{feff}开头内容")]);
        assert_eq!(bom[0].text, "开头内容");
    }
}
