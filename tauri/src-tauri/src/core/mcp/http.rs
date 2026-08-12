//! MCP streamable HTTP 传输（规范 2025-03-26 最小实现）。
//!
//! - `POST {url}` 发送 JSON-RPC 请求（`application/json`），响应为单个 JSON 或 SSE 流；
//! - `GET {url}` 建立 SSE 接收流（后台任务消费服务端消息/notifications），失败自动降级
//!   为纯请求-响应模式（不阻断连接）；
//! - `mcp-session-id` 从响应头捕获并随后续请求回传；
//! - 与 stdio 共用 `McpTransport` trait（注册表/Agent 无感知）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::Mutex;

use super::client::{McpServerConfig, McpTransport, MCP_CALL_TIMEOUT_SECS};

/// streamable HTTP 客户端。
pub struct HttpStreamableClient {
    url: String,
    headers: HashMap<String, String>,
    session_id: Arc<Mutex<Option<String>>>,
    next_id: AtomicU64,
    http: reqwest::Client,
    sse_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    closed: Arc<AtomicBool>,
}

impl HttpStreamableClient {
    pub fn new(cfg: &McpServerConfig) -> Self {
        Self {
            url: cfg.url.clone().unwrap_or_default(),
            headers: cfg.headers.clone(),
            session_id: Arc::new(Mutex::new(None)),
            next_id: AtomicU64::new(1),
            // 总超时放宽至 180s：覆盖 initialize 握手与首次慢响应；
            // 普通调用（tools/call 等）的实际等待由 call_with_timeout 外层控制（默认 60s）
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(180))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            sse_task: Mutex::new(None),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 建立连接：校验 url 并启动 SSE 接收流（失败不阻断，降级请求-响应模式）。
    pub async fn connect(&self) -> Result<(), String> {
        if !self.url.starts_with("http://") && !self.url.starts_with("https://") {
            return Err("HTTP 传输 url 需以 http:// 或 https:// 开头".into());
        }
        self.start_sse().await;
        Ok(())
    }

    /// 后台 GET 建立 SSE 接收流（消费服务端消息；失败静默降级）。
    async fn start_sse(&self) {
        let url = self.url.clone();
        let headers = self.headers.clone();
        let session_id = self.session_id.clone();
        let closed = self.closed.clone();
        let task = tokio::spawn(async move {
            let mut req = reqwest::Client::new()
                .get(&url)
                .header("accept", "text/event-stream");
            for (k, v) in &headers {
                req = req.header(k, v);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(_) => return, // GET 不可用：降级为纯请求-响应
            };
            if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
                *session_id.lock().await = Some(sid.to_string());
            }
            let mut stream = resp.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            while !closed.load(Ordering::Relaxed) {
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        buf.extend_from_slice(&chunk);
                        while let Some(pos) = find_frame_end(&buf) {
                            buf.drain(..pos);
                        }
                    }
                    _ => break,
                }
            }
        });
        *self.sse_task.lock().await = Some(task);
    }

    /// 发起 JSON-RPC 请求（默认超时 60s）。
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.call_with_timeout(method, params, MCP_CALL_TIMEOUT_SECS)
            .await
    }

    /// 发起 JSON-RPC 请求（自定义超时；内层 reqwest 总超时 180s 兜底）。
    async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        secs: u64,
    ) -> Result<Value, String> {
        match tokio::time::timeout(
            Duration::from_secs(secs),
            self.call_impl(method, params),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => Err(format!(
                "MCP 调用超时（{}s）: {}（服务器未响应，请检查地址与网络）",
                secs, method
            )),
        }
    }

    /// 请求-响应主体：POST → JSON 或 SSE 响应，按 id 匹配；捕获 mcp-session-id。
    async fn call_impl(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut req = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        {
            let sid = self.session_id.lock().await;
            if let Some(s) = sid.as_deref() {
                req = req.header("mcp-session-id", s);
            }
        }
        let resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("HTTP 请求失败: {}", e))?;
        if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session_id.lock().await = Some(sid.to_string());
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("MCP HTTP 错误 ({}): {}", status, truncate(&text)));
        }
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        if ct.contains("text/event-stream") {
            // SSE 响应：逐帧解析，找 id 匹配的 JSON-RPC 响应
            let mut stream = resp.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            let mut found: Option<Result<Value, String>> = None;
            while found.is_none() {
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        buf.extend_from_slice(&chunk);
                        while let Some(pos) = find_frame_end(&buf) {
                            let frame: Vec<u8> = buf.drain(..pos).collect();
                            if let Some(data) = parse_data_line(&frame) {
                                if let Ok(v) = serde_json::from_str::<Value>(&data) {
                                    if let Some(resp_v) = match_response(&v, id) {
                                        found = Some(Ok(resp_v));
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        found = Some(Err(format!("SSE 读取失败: {}", e)));
                    }
                    None => {
                        found = Some(Err("SSE 流结束（未收到响应）".into()));
                    }
                }
            }
            return found.unwrap();
        }
        let v: Value = resp.json().await.map_err(|e| format!("响应解析失败: {}", e))?;
        if let Some(err) = v.get("error") {
            return Err(err
                .get("message")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "MCP 错误".into()));
        }
        Ok(v.get("result").cloned().unwrap_or(v))
    }

    /// 发送 notification（fire-and-forget，带 mcp-session-id）。
    fn send_notification(&self, method: &str, params: Value) {
        let url = self.url.clone();
        let headers = self.headers.clone();
        let session_id = self.session_id.clone();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        tokio::spawn(async move {
            let mut req = reqwest::Client::new()
                .post(&url)
                .header("content-type", "application/json")
                .json(&body);
            for (k, v) in &headers {
                req = req.header(k, v);
            }
            let sid = session_id.lock().await;
            if let Some(s) = sid.as_deref() {
                req = req.header("mcp-session-id", s);
            }
            drop(sid);
            let _ = req.send().await;
        });
    }

    /// 断开：取消 SSE 接收流。
    fn disconnect(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let task = self.sse_task.blocking_lock().take();
        if let Some(t) = task {
            t.abort();
        }
    }
}

#[async_trait]
impl McpTransport for HttpStreamableClient {
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        HttpStreamableClient::call(self, method, params).await
    }
    async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        secs: u64,
    ) -> Result<Value, String> {
        HttpStreamableClient::call_with_timeout(self, method, params, secs).await
    }
    fn send_notification(&self, method: &str, params: Value) {
        HttpStreamableClient::send_notification(self, method, params);
    }
    fn disconnect(&self) {
        HttpStreamableClient::disconnect(self);
    }
}

/// 查找 SSE 帧结束位置：第一个空行（帧分隔符）。
///
/// 空行 = 行内无内容（可含一个 `\r`），兼容 LF / CRLF / 混合换行的全部合法变体：
/// `\n\n`、`\r\n\r\n`、`\n\r\n`、`\r\n\n`。仅匹配 `\n\n` 会漏掉 CRLF 系服务器
/// （如 tavily 返回 `event: message\r\ndata: {...}\r\n\r\n` 或 `...\n\r\n`），
/// 导致帧永远解析不完、调用挂起直至超时。
///
/// 返回「帧 + 分隔符」全部结束后的位置：调用方一次 `drain(..pos)` 消费干净，
/// 避免残留分隔符导致下一轮返回 0、产生空帧无限循环。
fn find_frame_end(buf: &[u8]) -> Option<usize> {
    let mut line_start = 0usize;
    let mut i = 0usize;
    while i < buf.len() {
        if buf[i] == b'\n' {
            // 当前行 = buf[line_start..i]；空行（行内无内容或仅 \r）即帧分隔符
            let line_is_empty = if line_start == i {
                true
            } else {
                buf[line_start..i].iter().all(|&b| b == b'\r')
            };
            if line_is_empty {
                // 跳过分隔符本身（\n 或 \r\n），返回下一帧数据起始位置
                return Some(i + 1);
            }
            line_start = i + 1;
        }
        i += 1;
    }
    None
}

/// 提取帧内 `data:` 行拼接的 JSON 文本（忽略其它行与 [DONE]）。
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

/// 匹配 JSON-RPC 响应：id 一致返回 result；error 包装为 __mcp_error__；其余（notification）忽略。
fn match_response(frame: &Value, id: u64) -> Option<Value> {
    let frame_id = frame.get("id").and_then(Value::as_u64)?;
    if frame_id != id {
        return None;
    }
    if let Some(err) = frame.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("MCP 错误")
            .to_string();
        return Some(serde_json::json!({ "__mcp_error__": msg }));
    }
    frame.get("result").cloned()
}

/// 截断错误文本（防撑爆日志）。
fn truncate(s: &str) -> String {
    const MAX: usize = 500;
    if s.chars().count() > MAX {
        let cut: String = s.chars().take(MAX).collect();
        format!("{}…（{} 字符）", cut, s.chars().count())
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_response_filters_notifications() {
        let notification = serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert!(match_response(&notification, 1).is_none());
        let resp = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}});
        assert_eq!(match_response(&resp, 1), Some(serde_json::json!({"ok":true})));
        let err = serde_json::json!({"jsonrpc":"2.0","id":2,"error":{"message":"boom"}});
        assert_eq!(
            match_response(&err, 2),
            Some(serde_json::json!({"__mcp_error__": "boom"}))
        );
    }

    #[test]
    fn sse_data_line_parsing() {
        let frame = b"event: message\ndata: {\"id\":1}\n\n";
        assert_eq!(parse_data_line(frame).unwrap(), r#"{"id":1}"#);
        assert!(parse_data_line(b"data: [DONE]\n\n").is_none());
    }

    #[test]
    fn sse_frame_end_supports_crlf_and_mixed() {
        // 辅助：从帧缓冲解析出 data 内容
        fn frame_data(buf: &[u8]) -> Option<String> {
            let pos = find_frame_end(buf)?;
            let frame: Vec<u8> = buf[..pos].to_vec();
            parse_data_line(&frame)
        }
        // LF 帧（\n\n）
        assert_eq!(frame_data(b"data: {\"id\":1}\n\n").as_deref(), Some(r#"{"id":1}"#));
        // CRLF 帧（\r\n\r\n）
        assert_eq!(frame_data(b"event: message\r\ndata: {\"id\":1}\r\n\r\n").as_deref(), Some(r#"{"id":1}"#));
        // 混合换行（\n\r\n，tavily tools/list 实际格式）
        assert_eq!(frame_data(b"event: message\r\ndata: {\"id\":1}\n\r\n").as_deref(), Some(r#"{"id":1}"#));
        // 混合换行（\r\n\n）
        assert_eq!(frame_data(b"event: message\r\ndata: {\"id\":1}\r\n\n").as_deref(), Some(r#"{"id":1}"#));
        // 半帧：尚无空行分隔符 → None（等待更多数据）
        assert_eq!(find_frame_end(b"data: {\"id\":1}"), None);
        assert_eq!(find_frame_end(b"event: message\r\ndata: {\"id\":1}"), None);
        // 非空行 + \n 不是帧结束（还需一个空行）
        assert_eq!(find_frame_end(b"event: message\r\n"), None);
        // 空 buf
        assert_eq!(find_frame_end(b""), None);
        // 单个 \n 是合法空帧（心跳/空 data），返回帧结束位置；data 为空 → 无内容
        assert_eq!(find_frame_end(b"\n"), Some(1));
        // 单独 \r（半 CRLF）不构成空行
        assert_eq!(find_frame_end(b"\r"), None);
    }

    #[test]
    fn sse_frame_end_multiple_frames() {
        // 两帧连续到达：只匹配第一帧结束（CRLF 版）
        let buf = b"event: message\r\ndata: {\"id\":1}\r\n\r\nevent: message\r\ndata: {\"id\":2}\r\n\r\n";
        let pos = find_frame_end(buf).unwrap();
        let frame: Vec<u8> = buf[..pos].to_vec();
        assert_eq!(parse_data_line(&frame).unwrap(), r#"{"id":1}"#);
        // 剩余部分应能从下一帧继续解析
        let rest = &buf[pos..];
        let pos2 = find_frame_end(rest).unwrap();
        let frame2: Vec<u8> = rest[..pos2].to_vec();
        assert_eq!(parse_data_line(&frame2).unwrap(), r#"{"id":2}"#);
        // 多帧 LF 版
        let buf = b"data: {\"id\":1}\n\ndata: {\"id\":2}\n\n";
        let pos = find_frame_end(buf).unwrap();
        assert_eq!(parse_data_line(&buf[..pos].to_vec()).unwrap(), r#"{"id":1}"#);
        let rest = &buf[pos..];
        let pos2 = find_frame_end(rest).unwrap();
        assert_eq!(parse_data_line(&rest[..pos2].to_vec()).unwrap(), r#"{"id":2}"#);
    }

    #[test]
    fn http_client_requires_http_url() {
        let cfg = McpServerConfig {
            url: Some("ftp://x".into()),
            ..Default::default()
        };
        let c = HttpStreamableClient::new(&cfg);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(c.connect()).unwrap_err();
        assert!(err.contains("http:// 或 https://"));
    }
}

