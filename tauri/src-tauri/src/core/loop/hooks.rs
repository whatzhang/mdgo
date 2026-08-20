//! Loop Hook 抽象——把 rig 的 `AgentHook` 六类 1:1 迁移为 loop 语义（对齐 DSH 的
//! `agent/pre-step`、`agent/request`、`tools/pre-execute`、`agent/request-error` waterfall）。
//!
//! 现有 mdgo Hook 迁移映射：
//! - [`LoopHook::pre_request`] ← `SkillInstructionHook`（preamble/active_tools 注入）+ `LlmTraceHook` + `ReasoningEffortHook`
//! - [`LoopHook::on_tool_call`] ← `SkillGateHook` + `ApprovalGateHook` + 重复调用熔断（短路序：技能门禁 → 审批）
//! - [`LoopHook::on_invalid_tool_call`] ← `InvalidToolCallHook`
//! - [`LoopHook::on_request_error`] ← 溢出压缩重试 / MaxTurns 归类（现有 `commands/llm.rs` 逻辑）
//!
//! 所有方法带默认实现（`Run`/无补丁/`None`），新增 Hook 只需实现关心的方法（开闭原则）。

use async_trait::async_trait;
use serde_json::Value;

use super::error::LoopError;
use super::types::LlmMessage;

/// 每轮请求补丁（对齐 rig `RequestPatch`）。
#[derive(Debug, Clone, Default)]
pub struct RequestPatch {
    /// 覆盖/追加 system prompt（在基础规约之后拼接）
    pub preamble_override: Option<String>,
    /// 窄化本轮可见工具（`None` = 全部已注册工具）
    pub active_tools: Option<Vec<String>>,
    /// 附加顶层参数字段
    pub extra_params: Option<Value>,
}

/// 工具调用决策（对齐 rig `ToolCallAction`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDecision {
    /// 放行执行
    Run,
    /// 跳过并回填原因给模型自纠（对齐 rig Skip）
    Skip(String),
}

/// 错误恢复动作（对齐 DSH `agent/request-error` 的 retry 语义）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryAction {
    /// 重试（调用方需推进条件，如压缩后预算更紧；防无限循环由调用方控制次数）
    Retry,
    /// 中止（保留错误）
    Abort,
}

/// Hook 上下文（只读请求信息；全拥有字段，不携带 self 借用，Hook 与 loop 互不耦合）。
#[derive(Debug, Clone)]
pub struct HookCtx {
    pub turn: u32,
    pub step: u32,
    pub model: String,
    pub request_id: String,
    /// 剩余模型调用轮次（预算预警用；`max_turns - step`）
    pub remaining_turns: usize,
}

impl HookCtx {
    pub fn new(
        turn: u32,
        step: u32,
        model: impl Into<String>,
        request_id: impl Into<String>,
        remaining_turns: usize,
    ) -> Self {
        Self {
            turn,
            step,
            model: model.into(),
            request_id: request_id.into(),
            remaining_turns,
        }
    }
}

/// Loop Hook 抽象。
#[async_trait]
pub trait LoopHook: Send + Sync {
    /// 每轮模型请求前调用（组装请求体后、发送前），可改写 preamble/可见工具/附加参数。
    fn pre_request(&self, _ctx: &HookCtx, _messages: &[LlmMessage]) -> RequestPatch {
        RequestPatch::default()
    }

    /// 工具执行前调用（短路序由 loop 保证：任一返回 Skip 即停止后续判断，与 rig 一致）。
    /// async 支持审批门等异步策略。
    async fn on_tool_call(&self, _ctx: &HookCtx, _name: &str, _args: &Value) -> ToolDecision {
        ToolDecision::Run
    }

    /// 模型调用了不存在的工具（恢复自纠）。
    fn on_invalid_tool_call(
        &self,
        _ctx: &HookCtx,
        _name: &str,
        _available: &[String],
    ) -> Option<String> {
        None
    }

    /// 模型请求失败（如上下文溢出）时决定重试或中止。
    async fn on_request_error(&self, _ctx: &HookCtx, _err: &LoopError) -> Option<RetryAction> {
        None
    }
}
