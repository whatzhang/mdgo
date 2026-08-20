//! OpenAI 兼容补全客户端（实现 [`LlmAdapter`]）——替代 rig 的 openai provider。
//!
//! - 流式：SSE 解析（text / reasoning_content / tool_calls 增量 / usage / finish_reason），
//!   adapter 内部把增量装配成完整工具调用后按模型序抛 [`StreamEvent`]。
//! - 非流式：`/chat/completions` JSON 解析。
//! - 上下文溢出：HTTP 400 + `context_length_exceeded` 识别为 [`LlmError::ContextOverflow`]。
//!
//! SSE 解析抽成纯函数/纯状态机（`SseParser`），供单测；HTTP 仅负责字节流。参照
//! `services/anthropic.rs` 的 `find_frame_end`/`parse_data_line` 模式。

use std::collections::VecDeque;
use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use futures::Stream;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::llm_seam::{CompletionRequest, CompletionResponse, LlmAdapter};
use super::types::{FinishReason, LlmError, LlmMessage, LlmRole, StreamEvent, TokenUsage, ToolCall};

/// 默认请求级总超时（含 SSE 读取期），对齐现有 300s。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// OpenAI 兼容补全适配器。
#[derive(Clone)]
pub struct OpenAiAdapter {
    /// 归一化后的 base_url（不含 `/chat/completions` 后缀）
    base_url: String,
    model: String,
    api_key: String,
    reasoning_effort: Option<String>,
    timeout: Duration,
}

impl OpenAiAdapter {
    pub fn new(
        endpoint: String,
        model: String,
        api_key: String,
        reasoning_effort: Option<String>,
    ) -> Self {
        Self {
            base_url: normalize_base_url(&endpoint),
            model,
            api_key,
            reasoning_effort,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// 自定义超时（默认 300s）。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn http_client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .expect("reqwest client build")
    }

    fn build_body(&self, req: &CompletionRequest, stream: bool) -> Value {
        let messages: Vec<Value> = req.messages.iter().map(to_openai_message).collect();
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": stream,
        });
        if let Some(mt) = req.max_tokens {
            body["max_tokens"] = json!(mt);
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        // 请求级 effort 优先，回退适配器默认
        if let Some(e) = req.reasoning_effort.as_ref().or(self.reasoning_effort.as_ref()) {
            if !e.trim().is_empty() {
                body["reasoning_effort"] = json!(e);
            }
        }
        if let Some(schema) = &req.output_schema {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": { "name": "output", "strict": true, "schema": schema }
            });
        }
        if !req.tools.is_empty() {
            // OpenAI tools 数组：{type:"function", function:{name, description, parameters}}
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }
        if stream {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if let Some(extra) = &req.extra_params {
            if let (Some(obj), Some(extra_obj)) = (body.as_object_mut(), extra.as_object()) {
                for (k, v) in extra_obj {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
        body
    }

    /// 根据状态码 + 响应体映射为错误（识别上下文溢出）。
    fn map_http_error(status: reqwest::StatusCode, body: &str) -> LlmError {
        let code = status.as_u16();
        let lower = body.to_ascii_lowercase();
        if code == 400
            && (lower.contains("context_length_exceeded")
                || lower.contains("maximum context length")
                || lower.contains("context window"))
        {
            return LlmError::ContextOverflow;
        }
        LlmError::StatusCode(code, truncate(body, 2000))
    }
}

#[async_trait]
impl LlmAdapter for OpenAiAdapter {
    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(
        &self,
        req: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionResponse, LlmError> {
        let client = self.http_client();
        let body = self.build_body(&req, false);
        let send = client
            .post(self.chat_url())
            .bearer_auth(&self.api_key)
            .json(&body)
            .send();

        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(LlmError::Cancelled),
            r = send => r.map_err(|e| LlmError::Http(e.to_string()))?,
        };
        let status = resp.status();
        let text = resp.text().await.map_err(|e| LlmError::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(Self::map_http_error(status, &text));
        }
        parse_completion_response(&text)
    }

    async fn stream(
        &self,
        req: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, LlmError>> + Send>>, LlmError> {
        let client = self.http_client();
        let body = self.build_body(&req, true);
        let send = client
            .post(self.chat_url())
            .bearer_auth(&self.api_key)
            .json(&body)
            .send();

        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(LlmError::Cancelled),
            r = send => r.map_err(|e| LlmError::Http(e.to_string()))?,
        };
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.map_err(|e| LlmError::Http(e.to_string()))?;
            return Err(Self::map_http_error(status, &text));
        }

        let bytes = resp.bytes_stream();
        let state = StreamState {
            bytes: bytes.boxed(),
            parser: SseParser::new(),
            cancel,
            errored: false,
        };
        // 具体 unfold 类型在源头固定：Pin<Box<Unfold>>（Unpin + Stream），coerce 到 trait 对象
        let unfold = futures::stream::unfold(state, |mut st| async move {
            loop {
                // 1) 优先弹出已解析事件
                if let Some(ev) = st.parser.pending.pop_front() {
                    return Some((Ok(ev), st));
                }
                // 2) 解析结束
                if st.parser.done {
                    return None;
                }
                // 3) 取消
                if st.cancel.is_cancelled() {
                    if st.errored {
                        return None;
                    }
                    st.errored = true;
                    return Some((Err(LlmError::Cancelled), st));
                }
                // 4) 拉取下一字节块
                match st.bytes.next().await {
                    Some(Ok(chunk)) => {
                        st.parser.push_bytes(&chunk);
                        // 继续循环弹出 pending
                    }
                    Some(Err(e)) => {
                        st.parser.finish();
                        if st.errored {
                            return None;
                        }
                        st.errored = true;
                        return Some((Err(LlmError::Http(e.to_string())), st));
                    }
                    None => {
                        st.parser.finish();
                        // finish() 可能压入收尾 Finish 事件，循环弹出后再结束
                        if st.parser.pending.is_empty() {
                            return None;
                        }
                    }
                }
            }
        });
        Ok(Box::pin(unfold))
    }
}

/// 流式 unfold 状态机。
struct StreamState {
    bytes: BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
    parser: SseParser,
    cancel: CancellationToken,
    errored: bool,
}

/// 归一化 base_url：剥离 `/chat/completions` 后缀，保留 `/v1` 前缀。
pub fn normalize_base_url(endpoint: &str) -> String {
    let mut s = endpoint.trim().trim_end_matches('/').to_string();
    if let Some(pos) = s.rfind("/chat/completions") {
        s.truncate(pos);
    }
    s
}

/// 工具调用增量装配中间态（按模型 index）。
#[derive(Debug, Clone, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

/// SSE 解析状态机（纯逻辑，可单测）。
struct SseParser {
    buf: Vec<u8>,
    pending: VecDeque<StreamEvent>,
    tool_calls: Vec<PartialToolCall>,
    finish_seen: bool,
    done: bool,
}

impl SseParser {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            pending: VecDeque::new(),
            tool_calls: Vec::new(),
            finish_seen: false,
            done: false,
        }
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        self.drain_frames();
    }

    fn drain_frames(&mut self) {
        while let Some(pos) = find_frame_end(&self.buf) {
            let frame: Vec<u8> = self.buf.drain(..pos).collect();
            // 消费分隔符（`\n\n` 或 `\r\n\r\n`）——必须消费，否则 find_frame_end
            // 在下一次迭代返回 0，drain(..0) 不推进 → 死循环
            if self.buf.starts_with(b"\r\n\r\n") {
                self.buf.drain(..4);
            } else if self.buf.starts_with(b"\n\n") {
                self.buf.drain(..2);
            } else if self.buf.first() == Some(&b'\n') {
                self.buf.drain(..1);
            }
            if let Some(data) = parse_data_line(&frame) {
                self.handle_data(&data);
            }
        }
    }

    fn handle_data(&mut self, data: &str) {
        if data == "[DONE]" {
            self.finish();
            return;
        }
        let chunk: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return, // 未知/非事件行忽略
        };
        self.handle_chunk(&chunk);
    }

    fn handle_chunk(&mut self, chunk: &Value) {
        // 用量块（含 stream_options.include_usage 的收尾块；无论是否已 finish 都解析）
        if chunk.get("usage").is_some() {
            let usage = parse_usage(chunk);
            if !usage.is_empty() {
                self.pending.push_back(StreamEvent::Usage(usage));
            }
        }

        let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) else {
            return;
        };
        for ch in choices {
            // finish_reason 处理（含 delta 之前，若同 chunk 携带）
            let finish = ch.get("finish_reason").and_then(|f| f.as_str());
            let finish = finish.filter(|f| !f.is_empty() && f != &"null").map(parse_finish_reason);
            if let Some(reason) = &finish {
                if !self.finish_seen {
                    self.finish_seen = true;
                    // 若为 ToolCalls，先按模型序抛装配完成的工具调用
                    if *reason == FinishReason::ToolCalls {
                        self.emit_tool_calls();
                    }
                    self.pending.push_back(StreamEvent::Finish(reason.clone()));
                    // 标记 done：OpenAI 正常流在 finish 后仍有 usage 块与 [DONE]，
                    // 但为稳妥，finish 后不再处理 delta；done 由 [DONE]/字节流结束置位。
                }
                continue;
            }
            // 未 finish 才处理 delta（防御）
            if self.finish_seen {
                continue;
            }
            let Some(delta) = ch.get("delta") else { continue };
            if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
                if !c.is_empty() {
                    self.pending.push_back(StreamEvent::TextDelta(c.to_string()));
                }
            }
            if let Some(r) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                if !r.is_empty() {
                    self.pending.push_back(StreamEvent::ReasoningDelta(r.to_string()));
                }
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    while self.tool_calls.len() <= idx {
                        self.tool_calls.push(PartialToolCall::default());
                    }
                    let slot = &mut self.tool_calls[idx];
                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        if !id.is_empty() {
                            slot.id = Some(id.to_string());
                        }
                    }
                    if let Some(f) = tc.get("function") {
                        if let Some(n) = f.get("name").and_then(|v| v.as_str()) {
                            if !n.is_empty() {
                                slot.name.push_str(n);
                            }
                        }
                        if let Some(a) = f.get("arguments").and_then(|v| v.as_str()) {
                            if !a.is_empty() {
                                slot.arguments.push_str(a);
                            }
                        }
                    }
                }
            }
        }
    }

    fn emit_tool_calls(&mut self) {
        let calls = std::mem::take(&mut self.tool_calls);
        for (i, p) in calls.iter().enumerate() {
            if let Some(id) = &p.id {
                self.pending.push_back(StreamEvent::ToolCall {
                    index: i,
                    call: ToolCall {
                        id: id.clone(),
                        name: p.name.clone(),
                        arguments: p.arguments.clone(),
                    },
                });
            }
        }
    }

    /// 字节流结束或 [DONE]：补发未显式 finish 的工具调用与收尾 Finish。
    fn finish(&mut self) {
        if !self.finish_seen {
            self.finish_seen = true;
            if !self.tool_calls.is_empty() {
                self.emit_tool_calls();
                self.pending.push_back(StreamEvent::Finish(FinishReason::ToolCalls));
            } else {
                self.pending.push_back(StreamEvent::Finish(FinishReason::Other(
                    "stream_ended".into(),
                )));
            }
        }
        self.done = true;
    }
}

/// 查找 SSE 帧结束位置：`\n\n` 或 `\r\n\r\n` 取先出现者，返回**分隔符起始下标**
/// （即帧内容结束位置；分隔符需由调用方消费）。
fn find_frame_end(buf: &[u8]) -> Option<usize> {
    let mut lf: Option<usize> = None;
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            lf = Some(i);
            break;
        }
    }
    let mut crlf: Option<usize> = None;
    for i in 0..buf.len().saturating_sub(3) {
        if buf[i] == b'\r' && buf[i + 1] == b'\n' && buf[i + 2] == b'\r' && buf[i + 3] == b'\n' {
            crlf = Some(i);
            break;
        }
    }
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// 提取帧内 `data:` 行（多行 data: 拼接）；`[DONE]` 结束标记**原样返回**，
/// 由 `SseParser::handle_data` 识别并触发收尾（依赖连接关闭是脆弱的——keep-alive
/// 连接可能不立即关闭响应体）。
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
    if data.is_empty() {
        return None;
    }
    Some(data.to_string())
}

fn parse_finish_reason(s: &str) -> FinishReason {
    match s {
        "stop" => FinishReason::Stop,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "length" | "max_tokens" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_string()),
    }
}

fn parse_usage(chunk: &Value) -> TokenUsage {
    let u = chunk.get("usage").unwrap_or(&Value::Null);
    let n = |path: &[&str]| -> u32 {
        let mut cur = u;
        for key in path {
            match cur.get(*key) {
                Some(v) => cur = v,
                None => return 0,
            }
        }
        cur.as_u64().unwrap_or(0) as u32
    };
    TokenUsage {
        prompt_tokens: n(&["prompt_tokens"]),
        completion_tokens: n(&["completion_tokens"]),
        total_tokens: n(&["total_tokens"]),
        cached_input_tokens: n(&["prompt_tokens_details", "cached_tokens"]),
        cache_creation_input_tokens: 0,
    }
}

fn parse_completion_response(text: &str) -> Result<CompletionResponse, LlmError> {
    let v: Value = serde_json::from_str(text).map_err(|e| LlmError::Json(e.to_string()))?;
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    let mut finish_reason = None;
    if let Some(choices) = v.get("choices").and_then(|c| c.as_array()) {
        if let Some(ch) = choices.first() {
            if let Some(msg) = ch.get("message") {
                content = content_to_string(msg.get("content").unwrap_or(&Value::Null));
                if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        let id = tc.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let arguments = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        tool_calls.push(ToolCall { id, name, arguments });
                    }
                }
            }
            finish_reason = ch
                .get("finish_reason")
                .and_then(|x| x.as_str())
                .map(parse_finish_reason);
        }
    }
    let usage = v.get("usage").map(|_| parse_usage(&v));
    Ok(CompletionResponse {
        content,
        tool_calls,
        usage,
        finish_reason,
    })
}

/// 消息 content 字段：字符串 或 数组（OpenAI 多模态格式）。
fn content_to_string(c: &Value) -> String {
    if let Some(s) = c.as_str() {
        return s.to_string();
    }
    if let Some(arr) = c.as_array() {
        let mut s = String::new();
        for p in arr {
            if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                s.push_str(t);
            }
        }
        return s;
    }
    String::new()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

/// 把 [`LlmMessage`] 转换为 OpenAI 协议消息 JSON。
fn to_openai_message(msg: &LlmMessage) -> Value {
    match msg.role {
        LlmRole::Tool => {
            let mut tool_call_id = String::new();
            let mut content = String::new();
            for b in &msg.content {
                match b {
                    super::types::ContentBlock::Text(t) => content.push_str(t),
                    super::types::ContentBlock::ToolResult {
                        tool_call_id: id,
                        content: c,
                        ..
                    } => {
                        tool_call_id = id.clone();
                        content.push_str(c);
                    }
                    _ => {}
                }
            }
            json!({ "role": "tool", "content": content, "tool_call_id": tool_call_id })
        }
        _ => {
            let mut text = String::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            for b in &msg.content {
                match b {
                    super::types::ContentBlock::Text(t) => text.push_str(t),
                    super::types::ContentBlock::ToolCall(tc) => tool_calls.push(json!({
                        "id": tc.id,
                        "type": "function",
                        "function": { "name": tc.name, "arguments": tc.arguments }
                    })),
                    _ => {}
                }
            }
            let mut m = json!({ "role": msg.role.as_str() });
            if msg.role == LlmRole::Assistant && !tool_calls.is_empty() {
                // OpenAI 要求 assistant 带 tool_calls 时 content 可为 null/""，tool_calls 独立
                m["content"] = json!(if text.is_empty() {
                    Value::Null
                } else {
                    Value::String(text)
                });
                m["tool_calls"] = json!(tool_calls);
            } else {
                m["content"] = json!(text);
            }
            m
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_url_strips_chat_suffix() {
        assert_eq!(normalize_base_url("http://x/v1/chat/completions"), "http://x/v1");
        assert_eq!(normalize_base_url("http://x/v1"), "http://x/v1");
        assert_eq!(normalize_base_url("http://x/"), "http://x");
    }

    #[test]
    fn frame_end_and_data_line() {
        assert_eq!(find_frame_end(b"data: a\n\n"), Some(7));
        assert_eq!(find_frame_end(b"data: a\r\n\r\n"), Some(7)); // \r\n\r\n 分隔符起始下标
        assert_eq!(find_frame_end(b"data: a"), None);
        let frame = b"data: {\"a\":1}\ndata: {\"b\":2}\n\n";
        assert_eq!(
            parse_data_line(frame).unwrap(),
            "{\"a\":1}\n{\"b\":2}"
        );
        assert!(parse_data_line(b"\n\n").is_none());
    }

    #[test]
    fn sse_text_stream() {
        let mut p = SseParser::new();
        p.push_bytes(b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"index\":0,\"finish_reason\":null}]}\n\n");
        p.push_bytes(b"data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"index\":0,\"finish_reason\":null}]}\n\n");
        p.push_bytes(b"data: {\"choices\":[{\"delta\":{},\"index\":0,\"finish_reason\":\"stop\"}]}\n\n");
        p.push_bytes(b"data: [DONE]\n\n");
        let evs: Vec<StreamEvent> = p.pending.into_iter().collect();
        assert_eq!(evs[0], StreamEvent::TextDelta("Hel".into()));
        assert_eq!(evs[1], StreamEvent::TextDelta("lo".into()));
        assert_eq!(evs[2], StreamEvent::Finish(FinishReason::Stop));
    }

    #[test]
    fn sse_tool_call_assembly() {
        let mut p = SseParser::new();
        // index 0: 先 name，再分片 arguments
        p.push_bytes(br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":""}}]},"index":0,"finish_reason":null}]}

"#);
        p.push_bytes(br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"a"}}]},"index":0,"finish_reason":null}]}

"#);
        p.push_bytes(br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"}"}}]},"index":0,"finish_reason":null}]}

"#);
        p.push_bytes(br#"data: {"choices":[{"delta":{},"index":0,"finish_reason":"tool_calls"}]}

"#);
        let evs: Vec<StreamEvent> = p.pending.into_iter().collect();
        // 顺序：ToolCall(index0) → Finish(ToolCalls)
        match &evs[0] {
            StreamEvent::ToolCall { index, call } => {
                assert_eq!(*index, 0);
                assert_eq!(call.id, "call_1");
                assert_eq!(call.name, "read");
                assert_eq!(call.arguments, "{\"path\":\"a\"}");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        assert_eq!(evs[1], StreamEvent::Finish(FinishReason::ToolCalls));
    }

    #[test]
    fn sse_usage_block() {
        let mut p = SseParser::new();
        p.push_bytes(b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15,\"prompt_tokens_details\":{\"cached_tokens\":3}}}\n\n");
        let evs: Vec<StreamEvent> = p.pending.into_iter().collect();
        match &evs[0] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.prompt_tokens, 10);
                assert_eq!(u.completion_tokens, 5);
                assert_eq!(u.cached_input_tokens, 3);
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn to_openai_assistant_with_tool_calls() {
        let msg = LlmMessage {
            role: LlmRole::Assistant,
            content: vec![
                super::super::types::ContentBlock::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                }),
            ],
        };
        let v = to_openai_message(&msg);
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(v["tool_calls"][0]["id"], "c1");
    }

    #[test]
    fn to_openai_tool_result() {
        let msg = LlmMessage {
            role: LlmRole::Tool,
            content: vec![super::super::types::ContentBlock::ToolResult {
                tool_call_id: "c1".into(),
                content: "result".into(),
                is_error: false,
            }],
        };
        let v = to_openai_message(&msg);
        assert_eq!(v["role"], "tool");
        assert_eq!(v["content"], "result");
        assert_eq!(v["tool_call_id"], "c1");
    }

    #[test]
    fn parse_non_stream_response() {
        let json = r#"{"choices":[{"message":{"role":"assistant","content":"hi","tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":4,"completion_tokens":3,"total_tokens":7}}"#;
        let r = parse_completion_response(json).unwrap();
        assert_eq!(r.content, "hi");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "read");
        assert_eq!(r.finish_reason, Some(FinishReason::ToolCalls));
        assert_eq!(r.usage.unwrap().prompt_tokens, 4);
    }
}
