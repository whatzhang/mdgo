//! `LlmAdapter` 抽象（LLM seam）——对齐 DeepSeek Harness 的 `ctx.llm` 适配器 seam。
//!
//! 职责：transport（把 `CompletionRequest` 发送到 provider，产出 `CompletionResponse` 或
//! `StreamEvent` 流）。**重试/超时/输出校验是策略**，由调用方包装（对齐 DSH "transport 与
//! policy 分离"）——adapter 只做单次请求。当前提供 OpenAI 兼容实现（[`crate::core::loop::openai`]）；
//! Anthropic 可后续包装 `services::anthropic`（消除当前"Agent 模式不支持 Anthropic"的硬限制）。

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use tokio_util::sync::CancellationToken;

use super::types::{FinishReason, LlmError, LlmMessage, StreamEvent, TokenUsage, ToolCall};

/// 模型可见的工具 schema（对齐 OpenAI `tools` 数组与 DSH `ToolSchema`）。
#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema（`parameters`）
    pub parameters: serde_json::Value,
}

/// 一次补全请求（协议无关；`output_schema` 为 JSON Schema，OpenAI 兼容端点映射到
/// `response_format.json_schema`；`tools` 为模型可见工具定义）。
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub messages: Vec<LlmMessage>,
    /// 模型可见工具（空 = 无工具）
    pub tools: Vec<ToolSchema>,
    pub max_tokens: Option<u32>,
    /// OpenAI 兼容顶层 `reasoning_effort`（low/medium/high）
    pub reasoning_effort: Option<String>,
    /// JSON Schema（结构化输出；`None` = 不约束）
    pub output_schema: Option<serde_json::Value>,
    pub temperature: Option<f32>,
    pub stream: bool,
    /// 附加顶层参数字段（合并进请求体；供 provider 特定参数透传）
    pub extra_params: Option<serde_json::Value>,
}

impl CompletionRequest {
    pub fn new(messages: Vec<LlmMessage>) -> Self {
        Self {
            messages,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: None,
            output_schema: None,
            temperature: None,
            stream: false,
            extra_params: None,
        }
    }
}

/// 非流式补全响应。
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    /// 模型返回的纯文本（拼接所有文本内容块）
    pub content: String,
    /// 模型发起的工具调用（若 finish_reason=ToolCalls）
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<TokenUsage>,
    pub finish_reason: Option<FinishReason>,
}

impl CompletionResponse {
    pub fn is_empty(&self) -> bool {
        self.content.is_empty() && self.tool_calls.is_empty()
    }
}

/// LLM 适配器抽象（transport seam）。
///
/// `stream` 返回一个异步流，逐项产出 [`StreamEvent`]；adapter 内部负责把 SSE 增量装配为
/// 完整工具调用并按模型序抛出。`complete` 为非流式单次调用（规划/扩展/摘要/评审用）。
#[async_trait]
pub trait LlmAdapter: Send + Sync {
    /// 模型标识
    fn model(&self) -> &str;
    /// 非流式补全（单次尝试）
    async fn complete(
        &self,
        req: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionResponse, LlmError>;
    /// 流式补全（单次尝试；取消后流以 `LlmError::Cancelled` 结束）。
    ///
    /// 返回已固定的 `Pin<Box<dyn Stream + Send>>`（Unpin，可直接 `next()`）——适配器在源头
    /// 固定具体流类型。适配器内部把 SSE 增量装配为完整工具调用并按模型序抛出。
    async fn stream(
        &self,
        req: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, LlmError>> + Send>>, LlmError>;
}

/// 方便的 `Arc<dyn LlmAdapter>` 别名。
pub type LlmAdapterRef = Arc<dyn LlmAdapter>;
