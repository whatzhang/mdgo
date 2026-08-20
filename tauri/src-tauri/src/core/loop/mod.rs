//! `core/loop` —— mdgo 自研 Agent 内核（替代 rig）。
//!
//! 分层（依赖方向自上而下单向）：
//! - [`types`]：LLM 协议无关的消息/内容块/流事件/错误（无依赖）
//! - [`llm_seam`]：`LlmAdapter` 抽象 + 请求/响应类型（依赖 types）
//! - [`openai`]：OpenAI 兼容 SSE 客户端（实现 LlmAdapter，依赖 types+llm_seam）
//! - [`session`]：事件溯源会话（SessionEvent + derive_history，第一天地基）
//! - [`tool`]：工具系统契约（ToolSpec/Tool/ToolRegistry/ToolEventSink，替代 DynamicTool）
//! - [`tool_calls`]：并行调度器（exclusive barrier + 有界池 + 模型序提交）
//! - [`hooks`]：LoopHook 四组钩子（pre_request/on_tool_call/on_invalid_tool_call/on_request_error）
//! - [`error`]：LoopError（MaxTurns/ContextOverflow/Cancelled）
//! - [`loop`]：LoopAgent turn/step 状态机（消费 LlmAdapter 流 + 驱动工具调度 + 事件溯源会话）
//!
//! 设计对齐 DeepSeek Harness 核心（`docs/deepseek-harness-architecture-report.md` §2-§4）：
//! LLM adapter seam、事件溯源会话、turn/step 状态机、工具流水线与并行调度。业务层
//! （core/search|skill|memory|planner|approval|context）只依赖本模块的公开窄接口，
//! 重构期间零改动；后续按"以 DSH 核心为基石"原则将业务逐步迁移到本内核之上。
//!
//! 注：本模块处于分期落地期（Phase 0-3），尚未被命令层引用——`dead_code`/`unused_imports`
//! 告警为预期，Phase 4 接入 `commands/llm.rs` 后自然消除。届时移除下方 allow。

#![allow(dead_code)]
#![allow(unused_imports)]

pub mod anthropic;
pub mod error;
pub mod hooks;
pub mod llm_seam;
pub mod openai;
pub mod session;
pub mod tool;
pub mod tool_calls;
pub mod types;

// loop 是 Rust 关键字，模块以 r#loop 注册；本层聚合导出
pub mod r#loop;

// 公开 API（业务层/命令层依赖的窄接口）
pub use anthropic::AnthropicAdapter;
pub use error::LoopError;
pub use hooks::{HookCtx, LoopHook, RequestPatch, RetryAction, ToolDecision};
pub use llm_seam::{CompletionRequest, CompletionResponse, LlmAdapter, ToolSchema};
pub use openai::OpenAiAdapter;
pub use r#loop::{LoopAgent, LoopConfig, LoopEvent, TurnOutcome};
pub use session::{Session, SessionEvent, TurnEndReason};
pub use tool::{HashMapToolRegistry, NullSink, Tool, ToolError, ToolEventSink, ToolRegistry, ToolResult, ToolRunContext, ToolSpec};
pub use tool_calls::{execute_tool_calls, ToolExecution};
pub use types::{ContentBlock, FinishReason, LlmError, LlmMessage, LlmRole, StreamEvent, TokenUsage, ToolCall};
