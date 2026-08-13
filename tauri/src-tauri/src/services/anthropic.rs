//! Anthropic Messages API 流式客户端（v2：双协议支持中的 anthropic 通道）。
//!
//! 仅实现 Chat 普通对话所需的最小协议面：
//! - `POST {base}/v1/messages` + `stream: true`（SSE）
//! - 解析 `content_block_delta`（`text_delta`）为增量文本；`thinking_delta` 忽略
//! - 取消传播：外部 `CancellationToken` 置位即断开连接，返回已累积文本
//!
//! 与 OpenAI 兼容通道（services/llm.rs）隔离：新增协议不改动现有 openai 路径（开闭原则）。

use std::time::Duration;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// LLM HTTP 请求级总超时（秒）：对齐 openai 通道的 LLM_REQUEST_TIMEOUT。
const ANTHROPIC_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// 默认最大输出 tokens（前端未配置时的兜底）。
const ANTHROPIC_DEFAULT_MAX_TOKENS: u32 = 4096;

/// Anthropic thinking 档位 → budget_tokens（逐档递增，对齐主流 Agent 的 token 预算映射；
/// Anthropic 官方建议范围 1024~32000）。
pub const THINK_BUDGET_LOW: u32 = 2048;
pub const THINK_BUDGET_STANDARD: u32 = 4096;
pub const THINK_BUDGET_HIGH: u32 = 8192;
pub const THINK_BUDGET_MAX: u32 = 16384;

/// 对话消息（与 services::llm::ChatMessage 同构，避免跨模块依赖）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: String,
}

/// 流式过程事件（命令层据此转发 llm:delta / llm:usage）。
#[derive(Debug, Clone)]
pub enum AnthropicEvent {
    /// 增量文本
    Delta(String),
    /// 用量（message_start / message_delta 的 usage）
    Usage {
        input_tokens: u32,
        output_tokens: u32,
    },
}

/// Anthropic Messages API 流式客户端。
#[derive(Debug, Clone)]
pub struct AnthropicStreamClient {
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
    /// 思考档位：None = 不启用 thinking；Some(budget_tokens) = 启用 extended thinking。
    thinking_budget: Option<u32>,
}

/// SSE 事件结构（Anthropic 官方流式事件，仅声明所需字段）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct SseEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<SseDelta>,
    #[serde(default)]
    message: Option<SseMessageMeta>,
    /// message_delta 事件的 usage 位于顶层（message_start 的在 message 内）
    #[serde(default)]
    usage: Option<SseUsage>,
    #[serde(default)]
    error: Option<SseError>,
}

#[derive(Debug, Deserialize)]
struct SseDelta {
    // content_block_delta 的 delta 含 type；message_delta 的 delta 无 type（stop_reason 等）→ 需 default
    #[serde(rename = "type", default)]
    delta_type: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SseMessageMeta {
    #[serde(default)]
    usage: Option<SseUsage>,
}

#[derive(Debug, Deserialize, Default)]
struct SseUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct SseError {
    #[serde(default)]
    message: Option<String>,
}

impl AnthropicStreamClient {
    pub fn new(
        base_url: String,
        api_key: String,
        model: String,
        max_tokens: u32,
        thinking_budget: Option<u32>,
    ) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
            max_tokens: if max_tokens > 0 { max_tokens } else { ANTHROPIC_DEFAULT_MAX_TOKENS },
            thinking_budget,
        }
    }

    fn messages_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.to_ascii_lowercase().ends_with("/v1") {
            format!("{}/messages", base)
        } else {
            format!("{}/v1/messages", base)
        }
    }

    /// 是否已具备发起请求的必要配置。
    pub fn is_configured(&self) -> bool {
        !self.base_url.is_empty() && !self.model.is_empty()
    }

    /// 流式对话：`system` 单独提取（Anthropic 顶层字段），`messages` 仅 user/assistant。
    ///
    /// 通过 `on_event` 回调推送增量/用量事件；返回完整文本。
    /// 取消时立即断开并返回已累积文本（不报错）。
    pub async fn stream_chat(
        &self,
        system: Option<&str>,
        messages: &[AnthropicMessage],
        cancel: CancellationToken,
        on_event: impl Fn(AnthropicEvent),
    ) -> Result<String, String> {
        if !self.is_configured() {
            return Err("Anthropic 未配置（缺少地址或模型）".into());
        }

        // 过滤非法 role：Anthropic 仅接受 user / assistant（system 已提取）
        let body_messages: Vec<serde_json::Value> = messages
            .iter()
            .filter(|m| m.role == "user" || m.role == "assistant")
            .map(|m| {
                serde_json::json!({ "role": m.role, "content": m.content })
            })
            .collect();
        if body_messages.is_empty() {
            return Err("消息列表为空".into());
        }

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": body_messages,
            "stream": true,
        });
        if let Some(s) = system.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            body["system"] = serde_json::json!(s);
        }
        if let Some(budget) = self.thinking_budget {
            // thinking 开启时 max_tokens 必须大于 budget_tokens
            let max_tokens = (self.max_tokens as u32).max(budget + 1024);
            body["max_tokens"] = serde_json::json!(max_tokens);
            body["thinking"] = serde_json::json!({ "type": "enabled", "budget_tokens": budget });
        }

        let client = reqwest::Client::builder()
            .timeout(ANTHROPIC_REQUEST_TIMEOUT)
            .build()
            .map_err(|e| format!("创建 HTTP 客户端失败: {}", e))?;

        let resp = client
            .post(self.messages_url())
            .header("Content-Type", "application/json")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Anthropic 请求失败: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let err_msg = parse_error_body(&text).unwrap_or_else(|| format!("HTTP {}", status));
            return Err(format!("Anthropic API 错误 ({}): {}", status, err_msg));
        }

        // SSE 流式解析：按 \n\n 分帧，逐行处理 data:
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut full_content = String::new();
        let mut input_tokens: u32 = 0;
        let mut output_tokens: u32 = 0;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    // 取消：返回已累积内容，由命令层按"取消保留部分内容"处理
                    break;
                }
                item = stream.next() => {
                    match item {
                        Some(Ok(chunk)) => {
                            buf.extend_from_slice(&chunk);
                            // 处理完整帧（以 \n\n 分隔）
                            while let Some(pos) = find_frame_end(&buf) {
                                let frame: Vec<u8> = buf.drain(..pos).collect();
                                if let Some(line) = parse_data_line(&frame) {
                                    handle_sse_line(&line, &mut full_content, &mut input_tokens, &mut output_tokens, &on_event);
                                }
                            }
                        }
                        Some(Err(e)) => {
                            if cancel.is_cancelled() { break; }
                            return Err(format!("Anthropic 流式读取失败: {}", e));
                        }
                        None => break,
                    }
                }
            }
        }
        Ok(full_content)
    }
}

/// 查找帧结束位置（SSE 帧以 `\n\n` 分隔；Anthropic 官方流式使用 `\n\n`，
/// 若需兼容 `\r\n\r\n` 帧需扩展此函数）。
fn find_frame_end(buf: &[u8]) -> Option<usize> {
    if buf.len() < 2 {
        return None;
    }
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i);
        }
    }
    None
}

/// 提取帧内的 data: 行内容（可能多行 data:，取拼接）。
fn parse_data_line(frame: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(frame);
    let mut data = String::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim_start());
            data.push('\n');
        }
    }
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    Some(data.to_string())
}

/// 处理单条 SSE data（JSON 事件），更新累积文本与用量，并回调事件。
fn handle_sse_line(
    line: &str,
    full_content: &mut String,
    input_tokens: &mut u32,
    output_tokens: &mut u32,
    on_event: &impl Fn(AnthropicEvent),
) {
    let event: SseEvent = match serde_json::from_str(line) {
        Ok(e) => e,
        Err(_) => return, // 未知/非事件行忽略
    };
    match event.kind.as_str() {
        "content_block_delta" => {
            if let Some(d) = event.delta {
                match d.delta_type.as_str() {
                    "text_delta" => {
                        if let Some(t) = d.text {
                            if !t.is_empty() {
                                full_content.push_str(&t);
                                on_event(AnthropicEvent::Delta(t));
                            }
                        }
                    }
                    // thinking_delta / signature_delta：思考过程，不进入正文
                    _ => {}
                }
            }
        }
        "message_start" | "message_delta" => {
            let usage = event.message.and_then(|m| m.usage).or(event.usage);
            if let Some(usage) = usage {
                if usage.input_tokens > 0 {
                    *input_tokens = usage.input_tokens;
                }
                if usage.output_tokens > 0 {
                    *output_tokens = usage.output_tokens;
                }
                on_event(AnthropicEvent::Usage {
                    input_tokens: *input_tokens,
                    output_tokens: *output_tokens,
                });
            }
        }
        "error" => {
            if let Some(err) = event.error {
                let msg = err.message.unwrap_or_else(|| "未知错误".into());
                log::warn!("[anthropic] 流式错误: {}", msg);
                // 错误事件不终止流（模型可能在 error 后仍发送 message_stop）
            }
        }
        _ => { /* message_start/content_block_start/content_block_stop/message_stop/ping 等忽略 */ }
    }
}

/// 从非 2xx 响应体提取可读错误信息。
fn parse_error_body(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    v.get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            v.get("error")
                .and_then(|e| e.get("type"))
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_url_variants() {
        let c = AnthropicStreamClient::new(
            "https://api.anthropic.com".into(),
            "k".into(),
            "claude-sonnet-4-5".into(),
            4096,
            None,
        );
        assert_eq!(c.messages_url(), "https://api.anthropic.com/v1/messages");

        let c = AnthropicStreamClient::new(
            "https://api.anthropic.com/v1".into(),
            "k".into(),
            "claude-sonnet-4-5".into(),
            4096,
            None,
        );
        assert_eq!(c.messages_url(), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn parse_frame_with_data_lines() {
        let frame = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";
        let line = parse_data_line(frame).unwrap();
        assert!(line.contains("\"text\":\"hi\""));
    }

    #[test]
    fn sse_event_text_delta() {
        use std::cell::RefCell;
        let line = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let mut full = String::new();
        let mut it = 0u32;
        let mut ot = 0u32;
        let deltas = RefCell::new(Vec::new());
        handle_sse_line(line, &mut full, &mut it, &mut ot, &|e| {
            if let AnthropicEvent::Delta(t) = e {
                deltas.borrow_mut().push(t);
            }
        });
        assert_eq!(full, "Hello");
        assert_eq!(deltas.into_inner(), vec!["Hello".to_string()]);
    }

    #[test]
    fn sse_event_thinking_ignored() {
        let line = r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"secret"}}"#;
        let mut full = String::new();
        let mut it = 0u32;
        let mut ot = 0u32;
        handle_sse_line(line, &mut full, &mut it, &mut ot, &|_| {});
        assert_eq!(full, "");
    }

    #[test]
    fn sse_event_usage() {
        use std::cell::RefCell;
        let line = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#;
        let mut full = String::new();
        let mut it = 0u32;
        let mut ot = 0u32;
        let usage = RefCell::new(Vec::new());
        handle_sse_line(line, &mut full, &mut it, &mut ot, &|e| {
            if let AnthropicEvent::Usage { input_tokens, output_tokens } = e {
                usage.borrow_mut().push((input_tokens, output_tokens));
            }
        });
        assert_eq!(usage.into_inner(), vec![(0, 42)]);
        assert_eq!(full, "");
    }
}
