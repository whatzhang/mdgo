//! Loop 级错误（聚合 adapter 错误 + 循环语义错误）——替代 rig 的 `StreamingError`。
//!
//! 语义分类供 [`crate::core::loop::hooks::LoopHook::on_request_error`] 与命令层判定：
//! - [`LoopError::MaxTurns`]：模型调用轮次预算耗尽（用户可见截断原因）
//! - [`LoopError::Llm(LlmError::ContextOverflow)`]：上下文窗口溢出（收紧预算重试）
//! - [`LoopError::Cancelled`]：请求取消（保留部分内容收尾）

use super::types::LlmError;

/// Agent 循环错误。
#[derive(Debug, Clone)]
pub enum LoopError {
    /// LLM 传输/协议错误
    Llm(LlmError),
    /// 模型调用轮次预算耗尽
    MaxTurns { max_turns: usize },
    /// 工具执行失败（工具名 + 可读信息）
    Tool { name: String, message: String },
    /// 请求被取消
    Cancelled,
    /// 内部状态错误（不应发生）
    Internal(String),
}

impl LoopError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, LoopError::Cancelled)
    }

    pub fn is_context_overflow(&self) -> bool {
        matches!(self, LoopError::Llm(LlmError::ContextOverflow))
    }

    pub fn is_max_turns(&self) -> bool {
        matches!(self, LoopError::MaxTurns { .. })
    }
}

impl From<LlmError> for LoopError {
    fn from(e: LlmError) -> Self {
        LoopError::Llm(e)
    }
}

impl std::fmt::Display for LoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoopError::Llm(e) => write!(f, "llm: {e}"),
            LoopError::MaxTurns { max_turns } => {
                write!(f, "达到模型调用轮次上限（{max_turns} 轮），回答可能不完整")
            }
            LoopError::Tool { name, message } => write!(f, "工具 {name} 执行失败: {message}"),
            LoopError::Cancelled => write!(f, "cancelled"),
            LoopError::Internal(m) => write!(f, "internal: {m}"),
        }
    }
}

impl std::error::Error for LoopError {}
