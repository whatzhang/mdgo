//! 上下文预算（单一入口）：把"按窗口百分比"换成"按延迟预算"。
//!
//! 背景（见 `docs/Agent 上下文体积治理方案.md`）：
//! - 旧口径 `context_length × 0.4 / 0.8` 在 1M 窗口下等于不设防（40 万 / 80 万 token），
//!   唯一真正生效的是会话层的字符闸门（单条 24k / 累计 120k 字符）；
//! - 实际延迟与成本由**真正发出去的 token** 决定，窗口只是能力上限。
//!
//! 本模块把预算拆成"固定段（system / tools / context）+ 历史段"，并给历史段一个
//! **与窗口无关的硬上限**（默认 24k token），保证大窗口模型不会把延迟拖垮。
//!
//! 单位约定：
//! - 对外一律 **token**；
//! - 需要和字符打交道的旧接口（`derive_history` 的截断阈值等）用 [`CHARS_PER_TOKEN`] 折算，
//!   与 `core::context::ApproxTokenEstimator`（`chars / 2`）同口径，对中文偏保守。

/// 字符 → token 的保守折算系数（与 `core::context::ApproxTokenEstimator` 一致）。
pub const CHARS_PER_TOKEN: usize = 2;

/// 输出预留的上限：即使模型配置了更大的 `max_tokens`，也只从预算里预留这么多。
pub const MAX_RESERVE_OUTPUT: u32 = 8_192;

/// 历史预算下限（防呆：窗口很小时也不至于压到没有上下文）。
pub const MIN_HISTORY_TOKENS: u32 = 4_000;

/// 未配置上下文窗口（`context_length = 0`）时的历史预算兜底。
///
/// 数值与 `core::agent::limits::MAX_MESSAGE_TOKENS` 保持一致；此处不直接引用该常量，
/// 是因为 `core::loop` 不能反向依赖业务层（`core::agent`）。
pub const DEFAULT_HISTORY_TOKENS: u32 = 15_000;

/// 上下文预算。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextBudget {
    /// 模型上下文窗口（0 = 未配置，走兜底口径）
    pub window: u32,
    /// 为输出预留的 token（`min(max_tokens, MAX_RESERVE_OUTPUT)`）
    pub reserve_output: u32,
    /// 安全系数：只使用窗口的这一部分（默认 0.6）
    pub safety_ratio: f32,
    /// 历史段硬上限（与窗口无关；默认 32k token）
    pub hard_cap_tokens: u32,
    /// 固定段预算：system 规约
    pub system_cap: u32,
    /// 固定段预算：工具 schema（默认 20k token ≈ 40k 字符；
    /// 现有工具集约 26.5k 字符，留出 ~50% 余量，避免描述被兜底截断后影响工具调用精度）
    pub tools_cap: u32,
    /// 固定段预算：本轮注入的检索/记忆上下文
    pub context_cap: u32,
    /// 单条工具结果进入历史的 token 上限
    pub tool_result_item_cap: u32,
    /// 历史中所有工具结果的累计 token 上限
    pub tool_result_total_cap: u32,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            window: 0,
            reserve_output: MAX_RESERVE_OUTPUT,
            safety_ratio: 0.6,
            hard_cap_tokens: 32_000,
            system_cap: 8_000,
            tools_cap: 20_000,
            context_cap: 6_000,
            tool_result_item_cap: 10_000,
            tool_result_total_cap: 24_000,
        }
    }
}

impl ContextBudget {
    /// 按模型窗口与输出上限构造预算。
    pub fn from_window(window: u32, max_tokens: Option<u32>) -> Self {
        let mut budget = Self {
            window,
            ..Self::default()
        };
        if let Some(mt) = max_tokens {
            budget.reserve_output = mt.min(MAX_RESERVE_OUTPUT);
        }
        budget
    }

    /// 单次请求的 prompt 预算（token）：窗口 × 安全系数。
    ///
    /// `window = 0` 时退回旧口径（历史 15k + 固定段），保持与改造前一致的行为。
    pub fn prompt_tokens(&self) -> u32 {
        if self.window == 0 {
            return DEFAULT_HISTORY_TOKENS * 2;
        }
        ((self.window as f64 * self.safety_ratio as f64) as u32).max(8_000)
    }

    /// 固定段（system + tools + context）的合计预算。
    ///
    /// 小窗口下按 `prompt / 2` 收缩，避免固定段把预算吃光。
    pub fn fixed_tokens(&self) -> u32 {
        let fixed = self.system_cap + self.tools_cap + self.context_cap;
        fixed.min(self.prompt_tokens() / 2)
    }

    /// 历史段可用预算（token）：`prompt − 输出预留 − 固定段`，再与硬上限取小。
    pub fn history_tokens(&self) -> u32 {
        if self.window == 0 {
            return DEFAULT_HISTORY_TOKENS;
        }
        self.prompt_tokens()
            .saturating_sub(self.reserve_output)
            .saturating_sub(self.fixed_tokens())
            .min(self.hard_cap_tokens)
            .max(MIN_HISTORY_TOKENS)
    }

    /// 历史段预算（字符）：供仍以字符计量的会话层使用。
    pub fn history_chars(&self) -> usize {
        self.history_tokens() as usize * CHARS_PER_TOKEN
    }

    /// 历史"压力水位"（字符）：超过它就先做无模型剪枝/压缩。
    pub fn pressure_chars(&self) -> usize {
        self.history_chars() / 100 * 85
    }

    /// 工具 schema 的字符预算。
    pub fn tools_chars(&self) -> usize {
        self.tools_cap as usize * CHARS_PER_TOKEN
    }

    /// 单条工具结果的字符上限（进入历史时用）。
    ///
    /// 上限同时受"历史预算的 40%"约束：历史很小的时候，单条结果不该独占大半预算。
    pub fn tool_result_item_chars(&self) -> usize {
        let by_token = self.tool_result_item_cap as usize * CHARS_PER_TOKEN;
        by_token.min(self.history_chars() * 2 / 5).max(2_000)
    }

    /// 历史中工具结果累计字符上限。
    pub fn tool_result_total_chars(&self) -> usize {
        let by_token = self.tool_result_total_cap as usize * CHARS_PER_TOKEN;
        by_token.min(self.history_chars() * 4 / 5).max(4_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_window_is_capped_by_latency_budget_not_window() {
        // 1M 窗口：旧口径会给 800k token 的历史预算（等于不设防）
        let b = ContextBudget::from_window(1_000_000, Some(12_800));
        assert_eq!(b.history_tokens(), 32_000, "历史预算必须被硬上限压住");
        assert_eq!(b.reserve_output, MAX_RESERVE_OUTPUT);
        assert!(b.history_tokens() < b.prompt_tokens() / 10);
    }

    #[test]
    fn medium_window_still_capped() {
        let b = ContextBudget::from_window(128_000, Some(4_096));
        assert_eq!(b.history_tokens(), 32_000);
    }

    #[test]
    fn small_window_shrinks_fixed_segment_and_floors_history() {
        let b = ContextBudget::from_window(32_000, Some(4_096));
        // 32k × 0.6 = 19.2k；固定段被压到 prompt/2 = 9.6k；历史 = 19.2k − 4.1k − 9.6k ≈ 5.5k
        let history = b.history_tokens();
        assert!(
            (MIN_HISTORY_TOKENS..8_000).contains(&history),
            "小窗口下历史预算应被压缩但不低于下限，实际 {history}"
        );
        assert!(b.fixed_tokens() <= b.prompt_tokens() / 2);
    }

    #[test]
    fn unconfigured_window_keeps_legacy_default() {
        let b = ContextBudget::default();
        assert_eq!(b.history_tokens(), DEFAULT_HISTORY_TOKENS);
        assert_eq!(b.history_chars(), DEFAULT_HISTORY_TOKENS as usize * CHARS_PER_TOKEN);
    }

    #[test]
    fn tool_result_limits_are_tight_and_self_consistent() {
        let b = ContextBudget::from_window(1_000_000, Some(12_800));
        let item = b.tool_result_item_chars();
        let total = b.tool_result_total_chars();
        assert!(item <= total, "单条上限不应超过累计上限");
        // 旧实现是 24k / 120k 字符；新口径收紧但不至于把单条结果切得过碎
        assert!(item <= 20_000, "item={item}");
        assert!(item >= 10_000, "item 过小会让单条结果被剪得过碎：{item}");
        assert!(total <= 48_000, "total={total}");
        assert!(total >= 32_000, "total 过小会让多步任务过早清理旧结果：{total}");
        // 压力水位在 85%
        assert_eq!(b.pressure_chars(), b.history_chars() / 100 * 85);
    }

    /// 当前工具集（日志实测 ~26.5k 字符）必须能通过 schema 预算，不再触发兜底截断。
    #[test]
    fn tools_budget_leaves_room_for_current_toolset() {
        let b = ContextBudget::from_window(1_000_000, Some(12_800));
        assert!(
            b.tools_chars() >= 40_000,
            "tools 预算过小会把工具描述截断（实测工具集 ~26.5k 字符）：{}",
            b.tools_chars()
        );
    }
}
