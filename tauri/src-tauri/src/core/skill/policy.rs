//! 技能检索策略解析（P4：收拢 llm.rs 的参数兜底与 clamp 逻辑）。
//!
//! 单一职责：把「技能声明（保守合并结果）→ 请求级/全局兜底 → Security 硬边界」的
//! 解析收敛到本模块，`llm.rs` 只消费解析结果，不重复实现合并规则。
//!
//! 决策语义（与 V3 方案一致）：
//! - **技能显式声明参数优先**（Q1），未声明回退请求级/全局兜底——技能 frontmatter
//!   的 top_k/min_score/max_docs/max_chunks_per_doc 是生效依据；
//! - **Security 硬边界**仅作 clamp（top_k 1..=50、min_score 0..=1、容量 >=1），
//!   不覆盖技能声明（技能参数在边界内自由）；
//! - 多技能同时激活时的保守合并（top_k 取 min、min_score 取 max）由
//!   `SkillExecutionContext::from_skills` 完成，本模块消费其合并结果。

use crate::core::config::IndexerConfig;

use super::context::SkillExecutionContext;

/// 解析后的检索运行时策略（单一事实源，供预检索与 kb_search 工具共用）。
#[derive(Debug, Clone)]
pub struct RuntimePolicy {
    pub top_k: u32,
    pub min_score: f32,
    /// 精排 sigmoid 阈值（仅全局，技能无此声明）
    pub rerank_min_score: f32,
    pub max_docs: usize,
    pub max_chunks_per_doc: usize,
}

/// 解析检索策略：技能声明优先 → 请求级/全局兜底 → Security clamp。
///
/// - `skill_ctx`：预激活技能的保守合并参数（None 表示无预激活技能）
/// - `request_top_k`：请求级 top_k（前端可传；技能未声明时作为中间兜底）
/// - `global`：全局索引器配置（最终兜底与精排阈值）
pub fn resolve_retrieval_policy(
    skill_ctx: Option<&SkillExecutionContext>,
    request_top_k: u32,
    global: &IndexerConfig,
) -> RuntimePolicy {
    RuntimePolicy {
        top_k: skill_ctx
            .and_then(|c| c.top_k)
            .unwrap_or(request_top_k)
            .clamp(1, 50),
        min_score: skill_ctx
            .and_then(|c| c.min_score)
            .unwrap_or(global.min_score)
            .clamp(0.0, 1.0),
        rerank_min_score: global.rerank_min_score.clamp(0.0, 1.0),
        max_docs: skill_ctx
            .and_then(|c| c.max_docs)
            .unwrap_or(global.max_context_docs)
            .max(1),
        max_chunks_per_doc: skill_ctx
            .and_then(|c| c.max_chunks_per_doc)
            .unwrap_or(global.max_chunks_per_doc)
            .max(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::IndexerConfig;

    fn ctx_with(top_k: Option<u32>, min_score: Option<f32>, max_docs: Option<usize>, max_chunks: Option<usize>) -> SkillExecutionContext {
        let mut ctx = SkillExecutionContext::default();
        ctx.top_k = top_k;
        ctx.min_score = min_score;
        ctx.max_docs = max_docs;
        ctx.max_chunks_per_doc = max_chunks;
        ctx
    }

    #[test]
    fn skill_params_take_precedence_over_global() {
        let mut global = IndexerConfig::default();
        global.top_k = 10;
        global.min_score = 0.3;
        let p = resolve_retrieval_policy(
            Some(&ctx_with(Some(8), Some(0.5), Some(5), Some(3))),
            7,
            &global,
        );
        assert_eq!(p.top_k, 8);
        assert_eq!(p.min_score, 0.5);
        assert_eq!(p.max_docs, 5);
        assert_eq!(p.max_chunks_per_doc, 3);
    }

    #[test]
    fn falls_back_to_request_then_global() {
        let mut global = IndexerConfig::default();
        global.top_k = 10;
        global.min_score = 0.3;
        global.max_context_docs = 4;
        global.max_chunks_per_doc = 3;
        // 技能声明 top_k，其余回退
        let p = resolve_retrieval_policy(Some(&ctx_with(None, None, None, None)), 7, &global);
        assert_eq!(p.top_k, 7, "请求级 top_k 兜底");
        assert_eq!(p.min_score, 0.3, "全局 min_score 兜底");
        assert_eq!(p.max_docs, 4);
        assert_eq!(p.max_chunks_per_doc, 3);
        // 无技能上下文：请求 top_k 优先于全局
        let p2 = resolve_retrieval_policy(None, 12, &global);
        assert_eq!(p2.top_k, 12);
    }

    #[test]
    fn security_clamp_enforced() {
        let mut global = IndexerConfig::default();
        global.top_k = 10;
        let p = resolve_retrieval_policy(Some(&ctx_with(Some(200), None, None, None)), 3, &global);
        assert_eq!(p.top_k, 50, "超上限 clamp 到 50");
        let p2 = resolve_retrieval_policy(Some(&ctx_with(Some(1), Some(2.0), None, None)), 3, &global);
        assert_eq!(p2.min_score, 1.0, "超上限 clamp 到 1.0");
        assert!(p2.rerank_min_score >= 0.0 && p2.rerank_min_score <= 1.0);
    }
}
