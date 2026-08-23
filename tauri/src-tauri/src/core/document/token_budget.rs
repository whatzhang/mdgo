//! Token Budget 纯类型层（document 基础层，无 db 依赖）。
//!
//! 分层说明：`ChunkBudget` / `TokenCounter` 是纯抽象，供 `document` 层的
//! 分块引擎（`chunk_engine.rs` / `text_split.rs`）与 `db` 层的
//! [`crate::core::db::token_budget::TokenBudgetValidator`] 共同使用；
//! 放在 `document` 基础层维持"document 不依赖 db"的既有方向。
//!
//! 背景见 `docs/分块 Token 预算设计.md`（P0-1 / P0-2）。

use std::sync::{Arc, OnceLock};

// ─── ChunkBudget：token 预算的单一事实来源 ───

/// 分块 token 预算（唯一预算来源，所有 splitter / validator / 统计共用）。
///
/// 预算约束的是**最终送进 embedding 模型的 `embedding_text`**（含路径前缀 + 正文 +
/// overlap 尾部），而不是 `text` 的字符数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkBudget {
    /// 硬上限：最终 `embedding_text` 的最大 token 数。
    /// 必须满足 `hard_max_tokens ≤ embedding_max_tokens - special_tokens_reserve`。
    pub hard_max_tokens: usize,
    /// 目标值：分组时尽量接近的 token 数（贪心分组的触发阈值）。
    pub target_tokens: usize,
    /// 最小下限：碎块合并目标（"合并目标"而非整文档硬下限——单句文档仍产出 1 个 chunk）。
    pub min_tokens: usize,
    /// 相邻 chunk 间的 overlap（拼接前块尾部 token 数）。必须满足 `target + overlap ≤ hard_max`。
    pub overlap_tokens: usize,
    /// embedding 路径前缀（heading_path / 树形路径）最大 token 预算。
    pub prefix_max_tokens: usize,
    /// special tokens 预留（[CLS]/[SEP] 等，BGE 系取 8）。
    pub special_tokens_reserve: usize,
}

/// special tokens 预留默认值（[CLS]/[SEP] 等）
const DEFAULT_SPECIAL_RESERVE: usize = 8;
/// 路径前缀默认预算（token）
const DEFAULT_PREFIX_MAX: usize = 40;

impl ChunkBudget {
    /// 从 embedding 模型窗口构建默认预算（唯一生产构造点）。
    ///
    /// 默认值（max_position_embeddings=512 示例）：
    /// hard=504 / target=448 / overlap=56 / min=160 / prefix=40。
    pub fn from_model_window(max_position_embeddings: usize) -> Self {
        let hard_max = max_position_embeddings
            .saturating_sub(DEFAULT_SPECIAL_RESERVE)
            .max(64);
        // 🟠 修复（M1）：小窗口下 target 不得超过 hard_max——
        // 旧实现 `clamp(128, 1024)` 在窗口 ≤191 时 target > hard_max，
        // debug 构建触发 assemble 的 panic，release 静默破坏 target+overlap ≤ hard 不变式。
        let target = hard_max.saturating_sub(56).clamp(64, hard_max);
        Self::assemble(hard_max, target, 56)
    }

    /// 从现有配置（`chunk_size` / `chunk_overlap`，语义为 token）构建。
    ///
    /// `chunk_size` 作为 target；`hard_max` 受模型窗口与 overlap 共同约束，
    /// 保证即使 overlap 拼接后也不超过模型窗口。
    pub fn from_config(
        chunk_size: usize,
        chunk_overlap: usize,
        max_position_embeddings: usize,
    ) -> Self {
        let hard_max = max_position_embeddings
            .saturating_sub(DEFAULT_SPECIAL_RESERVE)
            .max(64);
        let target = chunk_size.clamp(64, hard_max);
        let overlap = chunk_overlap.min(hard_max.saturating_sub(target));
        Self::assemble(hard_max, target, overlap)
    }

    fn assemble(hard_max: usize, target: usize, overlap: usize) -> Self {
        debug_assert!(
            hard_max <= 1_000_000 && target >= 64 && target <= hard_max,
            "ChunkBudget 不变式 I-budget-1/3 被破坏: hard={} target={}",
            hard_max,
            target
        );
        // 🟠 修复（M1）：overlap 强制钳到 `hard - target` 之内，保证 I-budget-2
        // （target + overlap ≤ hard）在任何窗口下成立；from_config 已预先钳制，
        // 此处为 from_model_window 小窗口场景的兜底。
        let overlap = overlap.min(hard_max.saturating_sub(target));
        let budget = Self {
            hard_max_tokens: hard_max,
            target_tokens: target,
            min_tokens: ((target as f64) * 0.36) as usize,
            overlap_tokens: overlap,
            prefix_max_tokens: DEFAULT_PREFIX_MAX.min(target.saturating_sub(16)),
            special_tokens_reserve: DEFAULT_SPECIAL_RESERVE,
        };
        debug_assert!(
            budget.target_tokens + budget.overlap_tokens <= budget.hard_max_tokens,
            "ChunkBudget 不变式 I-budget-2 被破坏: target+overlap={} > hard={}",
            budget.target_tokens + budget.overlap_tokens,
            budget.hard_max_tokens
        );
        budget
    }
}

// ─── TokenCounter：token 计数抽象（测试可注入） ───

/// token 计数抽象：生产用真实 BGE tokenizer，测试注入确定性 fake。
pub trait TokenCounter: Send + Sync {
    /// 精确 token 数。**口径必须与 embedding 路径一致**（含 special tokens，
    /// 对齐 `core/embedding.rs` 的 `encode(text, true)`）。
    fn count(&self, text: &str) -> Option<usize>;

    /// 各 token 起始的字符偏移边界（含末尾哨兵）；`None` 表示不可用（降级字符切分）。
    /// 供 token 边界切分使用。
    fn token_char_boundaries(&self, text: &str) -> Option<Vec<usize>>;
}

/// 生产实现：BGE 本地 tokenizer（与 embedding 推理同口径）。
pub struct BgeTokenizerCounter;

impl TokenCounter for BgeTokenizerCounter {
    fn count(&self, text: &str) -> Option<usize> {
        crate::core::embedding::tokenize_with_offsets(text).map(|(n, _)| n)
    }

    fn token_char_boundaries(&self, text: &str) -> Option<Vec<usize>> {
        crate::core::embedding::tokenize_with_offsets(text).map(|(_, b)| b)
    }
}

/// 全局 TokenCounter（生产实现，懒初始化）
pub fn global_token_counter() -> Arc<dyn TokenCounter> {
    static COUNTER: OnceLock<Arc<dyn TokenCounter>> = OnceLock::new();
    COUNTER
        .get_or_init(|| Arc::new(BgeTokenizerCounter) as Arc<dyn TokenCounter>)
        .clone()
}

/// 从配置构建预算（模型窗口由 embedding 层提供；模型未初始化时回退 512 安全默认）
pub fn budget_from_config(chunk_size: usize, chunk_overlap: usize) -> ChunkBudget {
    ChunkBudget::from_config(
        chunk_size,
        chunk_overlap,
        crate::core::embedding::get_max_seq_len(),
    )
}

/// 给定模型窗口下的 `chunk_size` 合法上限（硬上限 = 窗口 - special tokens 预留）。
/// 供配置校验（`kb_update_indexer_config`）与前端提示使用。
pub fn max_chunk_tokens(max_position_embeddings: usize) -> usize {
    max_position_embeddings.saturating_sub(DEFAULT_SPECIAL_RESERVE)
}

/// 密度比例字符预算：把 token 预算按文本实际 token 密度折算为字符预算，
/// 供 char-based 分块器（代码/树形/纯文本降级路径）使用——这样它们接收的
/// `max_size`（token 语义）不会被误当字符数。
///
/// 返回 `(char_max, char_overlap)`；counter 不可用时原样返回 token 值
/// （退化 1 字符 ≈ 1 token，CJK 最坏情形）。
pub fn char_budget_pair(
    text: &str,
    max_tokens: usize,
    overlap_tokens: usize,
    counter: &dyn TokenCounter,
) -> (usize, usize) {
    match counter.count(text) {
        Some(t) if t > 0 => {
            let chars = text.chars().count();
            let density = chars as f64 / t as f64; // 每 token 字符数
            // 10% 余量，防文件内密度波动导致分片超限（Validator 兜底最终裁决）
            let char_max = ((max_tokens as f64 * density) * 0.9) as usize;
            // 🟠 L4：overlap_tokens == 0 时保持 0（无重叠语义），
            // 旧实现 `.max(1)` 把 0 强改成 1 字符 overlap
            let char_overlap = if overlap_tokens == 0 {
                0
            } else {
                ((overlap_tokens as f64 * density) as usize).max(1)
            };
            (char_max.max(16), char_overlap)
        }
        _ => (max_tokens, overlap_tokens),
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::TokenCounter;

    /// 固定 token 密度计数器：N 字符 = 1 token；token 边界按固定步长（测试注入用）
    pub struct FixedRateCounter {
        pub chars_per_token: usize,
    }

    impl TokenCounter for FixedRateCounter {
        fn count(&self, text: &str) -> Option<usize> {
            Some(text.chars().count().div_ceil(self.chars_per_token))
        }

        fn token_char_boundaries(&self, text: &str) -> Option<Vec<usize>> {
            let n = text.chars().count();
            let mut b: Vec<usize> = (0..=n).step_by(self.chars_per_token).collect();
            if *b.last().unwrap() != n {
                b.push(n);
            }
            Some(b)
        }
    }
}
