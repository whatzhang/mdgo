//! LLM 协议无关的消息/内容块/流式事件/错误类型。
//!
//! `core/loop` 的自有类型，替代 rig-core 的 `Message`/`AssistantContent`/`ToolCall`/
//! `StreamedAssistantContent`/`StreamingError`。协议适配器（OpenAI/Anthropic）与 Agent 循环
//! 只依赖本类型，不依赖任何具体 provider（依赖倒置）。设计对齐 DeepSeek Harness 的
//! `llm/llm` 包：`Message` = 角色 + `ContentBlock[]`，流式用 `StreamEvent` 增量词汇。

use serde::{Deserialize, Serialize};

/// 一次模型发起的工具调用（协议无关；`arguments` 为模型原始 JSON 字符串）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// 工具调用 ID（与 `ContentBlock::ToolResult.tool_call_id` 配对）
    pub id: String,
    /// 工具名
    pub name: String,
    /// 参数 JSON 字符串（模型原始产出，回放时原样解析）
    pub arguments: String,
}

/// 消息角色（OpenAI 协议 view）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LlmRole {
    System,
    User,
    Assistant,
    Tool,
}

impl LlmRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            LlmRole::System => "system",
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
            LlmRole::Tool => "tool",
        }
    }
}

/// 内容块（OpenAI/Anthropic 协议交集的最小模型）。
///
/// 对齐 DSH `ContentBlockMap`：text / reasoning / image / tool-call / tool-result。
/// 本最小实现只保留 text / tool-call / tool-result；reasoning 以流事件透传、不入消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// 文本内容
    Text(String),
    /// 模型发起的工具调用
    ToolCall(ToolCall),
    /// 工具结果消息（`tool_call_id` 与 assistant 消息中的 ToolCall 配对）
    ToolResult {
        tool_call_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

/// 一条协议无关的消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: LlmRole,
    pub content: Vec<ContentBlock>,
}

impl LlmMessage {
    /// 构造单文本块消息。
    pub fn text(role: LlmRole, content: impl Into<String>) -> Self {
        Self { role, content: vec![ContentBlock::Text(content.into())] }
    }

    /// 取纯文本（拼接所有 Text 块；忽略工具块）。
    pub fn plain_text(&self) -> String {
        let mut s = String::new();
        for b in &self.content {
            if let ContentBlock::Text(t) = b {
                s.push_str(t);
            }
        }
        s
    }

    /// 消息携带的工具调用列表。
    pub fn tool_calls(&self) -> Vec<&ToolCall> {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .collect()
    }
}

/// OpenAI 格式的 token 用量。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    /// 命中 provider 缓存的输入 token（缓存命中率 = cached / prompt）
    #[serde(default)]
    pub cached_input_tokens: u32,
    /// 写入 provider 缓存的输入 token
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
}

impl TokenUsage {
    pub fn is_empty(&self) -> bool {
        self.prompt_tokens == 0 && self.completion_tokens == 0 && self.total_tokens == 0
    }

    /// 跨 step 累加用量：一次 turn 可能有多个 step，最终上报给前端/落库的必须是**合计**，
    /// 而不是最后一步（旧实现每 step 覆盖，多步任务只统计到 1/N）。
    ///
    /// 口径说明（对齐 OpenAI / DeepSeek / Anthropic）：
    /// - `prompt_tokens` 已包含被缓存的输入 token；`cached_input_tokens` 是其中命中部分；
    /// - 两者都独立累加、互不抵扣，避免"缓存命中后总数变小"的错误统计。
    pub fn accumulate(&mut self, other: &TokenUsage) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self.completion_tokens.saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.cached_input_tokens =
            self.cached_input_tokens.saturating_add(other.cached_input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
    }

    /// 缓存命中率（0.0–1.0）；没有 prompt token 时返回 `None`（显示层显示占位而不是 0%）。
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        if self.prompt_tokens == 0 {
            return None;
        }
        Some((self.cached_input_tokens as f64 / self.prompt_tokens as f64).clamp(0.0, 1.0))
    }
}

/// 流式结束原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinishReason {
    /// 模型自然结束
    Stop,
    /// 模型请求调用工具（assistant 消息含 tool_calls）
    ToolCalls,
    /// 达到 max_tokens / 长度上限
    Length,
    /// 内容被过滤
    ContentFilter,
    /// 其他/未知
    Other(String),
}

/// 流式事件（adapter → loop）。adapter 负责把 SSE 增量装配成完整工具调用后按序抛出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// 可见文本增量
    TextDelta(String),
    /// 推理/思考增量（可选，若 provider 上报；不入模型可见文本）
    ReasoningDelta(String),
    /// 一个**完整**的工具调用（在 finish_reason=ToolCalls 前按模型序逐个抛出；
    /// `index` 为模型给定的调用序号，供调试与排序）
    ToolCall { index: usize, call: ToolCall },
    /// 用量（含 `stream_options.include_usage` 的收尾块）
    Usage(TokenUsage),
    /// 流结束原因
    Finish(FinishReason),
}

/// LLM 调用错误（协议无关，供 loop 与策略层判定）。
#[derive(Debug, Clone)]
pub enum LlmError {
    /// HTTP 传输层错误（连接/超时前/流中断）
    Http(String),
    /// 非 2xx 状态码（`status` + 响应体截断）
    StatusCode(u16, String),
    /// SSE 帧/事件解析失败
    Sse(String),
    /// JSON 解析失败
    Json(String),
    /// 请求级超时
    Timeout,
    /// 已取消
    Cancelled,
    /// 上下文窗口溢出（HTTP 400 + context_length_exceeded）
    ContextOverflow,
    /// 确定性 4xx 业务错误（400 非溢出 / 401 / 403 / 404 等）
    InvalidRequest(String),
    /// provider 文本错误（无状态码，保守可重试）
    Provider(String),
    /// 其他
    Other(String),
}

impl LlmError {
    /// 是否可重试（瞬时错误：429/408/5xx/连接/超时/流中断/provider 文本）。
    /// 对齐 DSH `retryableCodes`（RATE_LIMIT/SERVER/TIMEOUT/TRANSPORT）。
    pub fn is_retryable(&self) -> bool {
        match self {
            LlmError::Http(_) | LlmError::Timeout | LlmError::Provider(_) => true,
            LlmError::StatusCode(code, _) => *code == 429 || *code == 408 || (500..=599).contains(code),
            _ => false,
        }
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Http(e) => write!(f, "http: {e}"),
            LlmError::StatusCode(c, b) => write!(f, "status {c}: {b}"),
            LlmError::Sse(e) => write!(f, "sse: {e}"),
            LlmError::Json(e) => write!(f, "json: {e}"),
            LlmError::Timeout => write!(f, "timeout"),
            LlmError::Cancelled => write!(f, "cancelled"),
            LlmError::ContextOverflow => write!(f, "context overflow"),
            LlmError::InvalidRequest(e) => write!(f, "invalid request: {e}"),
            LlmError::Provider(e) => write!(f, "provider: {e}"),
            LlmError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LlmError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_as_str() {
        assert_eq!(LlmRole::System.as_str(), "system");
        assert_eq!(LlmRole::Tool.as_str(), "tool");
    }

    #[test]
    fn message_plain_text_concats_text_blocks_only() {
        let msg = LlmMessage {
            role: LlmRole::Assistant,
            content: vec![
                ContentBlock::Text("hello".into()),
                ContentBlock::ToolCall(ToolCall { id: "c1".into(), name: "read".into(), arguments: "{}".into() }),
                ContentBlock::Text("world".into()),
            ],
        };
        assert_eq!(msg.plain_text(), "helloworld");
        assert_eq!(msg.tool_calls().len(), 1);
    }

    #[test]
    fn retryable_classification() {
        assert!(LlmError::Http("conn".into()).is_retryable());
        assert!(LlmError::Timeout.is_retryable());
        assert!(LlmError::StatusCode(429, String::new()).is_retryable());
        assert!(LlmError::StatusCode(503, String::new()).is_retryable());
        assert!(!LlmError::StatusCode(401, String::new()).is_retryable());
        assert!(!LlmError::ContextOverflow.is_retryable());
        assert!(!LlmError::InvalidRequest("x".into()).is_retryable());
    }

    #[test]
    fn usage_accumulates_across_steps_without_double_counting() {
        let mut acc = TokenUsage::default();
        acc.accumulate(&TokenUsage {
            prompt_tokens: 10_000,
            completion_tokens: 200,
            total_tokens: 10_200,
            cached_input_tokens: 8_000,
            cache_creation_input_tokens: 0,
        });
        acc.accumulate(&TokenUsage {
            prompt_tokens: 10_500,
            completion_tokens: 300,
            total_tokens: 10_800,
            cached_input_tokens: 8_400,
            cache_creation_input_tokens: 0,
        });
        assert_eq!(acc.prompt_tokens, 20_500);
        assert_eq!(acc.completion_tokens, 500);
        assert_eq!(acc.total_tokens, 21_000);
        // cached 是 prompt 的子集：累加后仍 ≤ prompt，命中率按"合计/合计"计算
        assert_eq!(acc.cached_input_tokens, 16_400);
        let ratio = acc.cache_hit_ratio().expect("有 prompt token 时必须有命中率");
        assert!((ratio - 0.8).abs() < 1e-9, "命中率应为 80%，实际 {ratio}");
        assert!(TokenUsage::default().cache_hit_ratio().is_none(), "无用量时不给 0%");
    }
}
