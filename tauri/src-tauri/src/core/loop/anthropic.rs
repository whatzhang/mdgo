//! Anthropic Messages 适配器（实现 [`LlmAdapter`]）——完成 LLM seam 的协议统一。
//!
//! 包装 `services::anthropic::AnthropicStreamClient`（Chat 最小协议面：SSE text_delta +
//! usage + 取消传播）：`stream()` 经 mpsc 通道把回调事件转为 [`StreamEvent`] 流；
//! `complete()` 走流式收集。由此 **v3 对话与 Agent 路径均可支持 Anthropic**（消除
//! "Agent 模式暂不支持 Anthropic" 的 rig 路径限制）。
//!
//! 说明：现有客户端不含工具编排（tool_use/tool_result 块），v3 Agent 路径使用 Anthropic
//! 时模型不可见工具（等同纯对话）；工具协议映射后续扩展。

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use super::llm_seam::{CompletionRequest, CompletionResponse, LlmAdapter};
use super::types::{FinishReason, LlmError, LlmMessage, LlmRole, StreamEvent, TokenUsage};
use crate::services::anthropic::{AnthropicEvent, AnthropicMessage, AnthropicStreamClient};

/// Anthropic Messages 适配器。
#[derive(Clone)]
pub struct AnthropicAdapter {
    client: AnthropicStreamClient,
    model: String,
}

impl AnthropicAdapter {
    /// `max_tokens`：0 = 客户端默认（4096）；`thinking_budget`：None = 不启用 extended thinking。
    pub fn new(
        base_url: String,
        api_key: String,
        model: String,
        max_tokens: u32,
        thinking_budget: Option<u32>,
    ) -> Self {
        let client = AnthropicStreamClient::new(base_url, api_key, model.clone(), max_tokens, thinking_budget);
        Self { client, model }
    }
}

/// 把 [`LlmMessage`] 转为 Anthropic 消息 + system（Anthropic 的 system 是顶层字段，
/// 消息仅 user/assistant；忽略 tool 消息——本适配器暂不含工具协议面）。
fn split_system(req: &CompletionRequest) -> (Option<String>, Vec<AnthropicMessage>) {
    let system = req
        .messages
        .iter()
        .find(|m| m.role == LlmRole::System)
        .map(|m| m.plain_text())
        .filter(|s| !s.trim().is_empty());
    let messages: Vec<AnthropicMessage> = req
        .messages
        .iter()
        .filter(|m| m.role != LlmRole::System)
        .map(|m| AnthropicMessage {
            role: m.role.as_str().to_string(),
            content: m.plain_text(),
        })
        .collect();
    (system, messages)
}

#[async_trait]
impl LlmAdapter for AnthropicAdapter {
    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(
        &self,
        req: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionResponse, LlmError> {
        // 现有客户端仅流式；complete 走流式收集（可接受：规划/摘要等调用文本量小）
        let mut stream = self.stream(req, cancel).await?;
        let mut content = String::new();
        let mut usage: Option<TokenUsage> = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(StreamEvent::TextDelta(t)) => content.push_str(&t),
                Ok(StreamEvent::Usage(u)) => usage = Some(u),
                Ok(StreamEvent::Finish(_)) => break,
                Err(e) => return Err(e),
                _ => {}
            }
        }
        Ok(CompletionResponse {
            content,
            tool_calls: Vec::new(),
            usage,
            finish_reason: Some(FinishReason::Stop),
        })
    }

    async fn stream(
        &self,
        req: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, LlmError>> + Send>>, LlmError> {
        let (system, messages) = split_system(&req);
        if messages.is_empty() {
            return Err(LlmError::InvalidRequest("消息列表为空".into()));
        }
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, LlmError>>(64);
        let client = self.client.clone();
        let cancel2 = cancel.clone();
        // 回调 → 通道（Delta/Usage 实时转发）；结束后发送 Finish/错误并随任务结束关闭通道
        tokio::spawn(async move {
            let result = client
                .stream_chat(system.as_deref(), &messages, cancel2, |ev| match ev {
                    AnthropicEvent::Delta(t) => {
                        let _ = tx.try_send(Ok(StreamEvent::TextDelta(t)));
                    }
                    AnthropicEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_read_input_tokens,
                        cache_creation_input_tokens,
                    } => {
                        let _ = tx.try_send(Ok(StreamEvent::Usage(TokenUsage {
                            prompt_tokens: input_tokens,
                            completion_tokens: output_tokens,
                            total_tokens: input_tokens.saturating_add(output_tokens),
                            cached_input_tokens: cache_read_input_tokens,
                            cache_creation_input_tokens,
                        })));
                    }
                })
                .await;
            match result {
                Ok(_) => {
                    let _ = tx.send(Ok(StreamEvent::Finish(FinishReason::Stop))).await;
                }
                Err(e) => {
                    let _ = tx.send(Err(LlmError::Provider(e))).await;
                }
            }
        });
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        Ok(Box::pin(stream))
    }
}
