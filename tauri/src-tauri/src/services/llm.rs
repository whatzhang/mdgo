use std::collections::HashSet;
use std::time::Duration;

use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ─── 公共类型 ───

/// 对话消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// SSE 流式事件
#[derive(Debug)]
pub enum LLMEvent {
    /// 内容增量
    Delta(String),
    /// 用量信息（OpenAI 格式）
    Usage(UsageInfo),
}

/// OpenAI 格式的使用量
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageInfo {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default, rename = "total_tokens")]
    pub total_tokens: u32,
}

// ─── SSE 响应解析（兼容 OpenAI / 本地 LLM 格式）───

/// OpenAI 流式 chunk
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SSEChunk {
    Standard {
        #[serde(default)]
        choices: Vec<SSEChoice>,
        #[serde(default)]
        usage: Option<UsageInfo>,
    },
    /// 某些本地模型直接返回 text 字段而非 choices
    Text {
        #[serde(default)]
        text: Option<String>,
    },
    /// 未知格式，静默跳过
    Unknown {},
}

#[derive(Debug, Deserialize)]
struct SSEChoice {
    #[serde(default)]
    delta: SSEDelta,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SSEDelta {
    #[serde(default)]
    content: String,
}

/// 从 SSE 数据行中提取文本增量（兼容多种格式）
fn extract_sse_content(data: &str) -> (String, Option<UsageInfo>) {
    let chunk: SSEChunk = match serde_json::from_str(data) {
        Ok(c) => c,
        Err(_) => return (String::new(), None),
    };
    match chunk {
        SSEChunk::Standard { choices, usage } => {
            let mut content = String::new();
            for choice in choices {
                if let Some(text) = choice.text {
                    content.push_str(&text);
                } else {
                    content.push_str(&choice.delta.content);
                }
            }
            (content, usage)
        }
        SSEChunk::Text { text } => (text.unwrap_or_default(), None),
        SSEChunk::Unknown {} => (String::new(), None),
    }
}

// ─── LLM 客户端 ───

/// 兼容 OpenAI / 本地 LLM 的流式客户端
pub struct LLMClient {
    endpoint: String,
    model: String,
    api_key: String,
    http: Client,
}

impl LLMClient {
    pub fn new(endpoint: String, model: String, api_key: String) -> Self {
        Self {
            endpoint,
            model,
            api_key,
            http: Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .expect("创建 HTTP 客户端失败"),
        }
    }

    /// 快速判断配置是否有效
    pub fn is_configured(&self) -> bool {
        !self.endpoint.is_empty() && !self.model.is_empty()
    }

    /// 流式对话补全
    ///
    /// 以 `mpsc` 频道的形式返回增量内容。调用方通过 `cancel` token 中止。
    pub async fn stream_chat_completion(
        &self,
        messages: &[ChatMessage],
        temperature: Option<f32>,
        max_tokens: Option<u32>,
        cancel: CancellationToken,
    ) -> Result<mpsc::Receiver<LLMEvent>, String> {
        let (tx, rx) = mpsc::channel::<LLMEvent>(64);

        let body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "temperature": temperature,
            "max_tokens": max_tokens,
        });

        let client = self.http.clone();
        let url = self.endpoint.clone();
        let api_key = self.api_key.clone();

        tokio::spawn(async move {
            let mut builder = client.post(&url).json(&body);
            if !api_key.is_empty() {
                builder = builder.bearer_auth(&api_key);
            }

            let response = match builder.send().await {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("\n\n[网络错误] LLM 请求失败: {}", e);
                    let _ = tx.send(LLMEvent::Delta(msg)).await;
                    return;
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                let msg = format!("\n\n[HTTP {}] LLM 返回错误: {}", status, text);
                let _ = tx.send(LLMEvent::Delta(msg)).await;
                return;
            }

            let mut buffer = String::new();
            let mut stream = response.bytes_stream();

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        break;
                    }
                    chunk = stream.next() => {
                        match chunk {
                            Some(Ok(bytes)) => {
                                let s = String::from_utf8_lossy(&bytes);
                                buffer.push_str(&s);

                                // 逐行处理 SSE buffer
                                loop {
                                    if let Some(line_end) = buffer.find('\n') {
                                        let line = buffer[..line_end].trim().to_string();
                                        buffer = buffer[line_end + 1..].to_string();

                                        if line.starts_with("data: ") {
                                            let data = line[6..].trim().to_string();
                                            if data.is_empty() || data == "[DONE]" {
                                                continue;
                                            }
                                            let (content, usage) = extract_sse_content(&data);
                                            if !content.is_empty() {
                                                let _ = tx.send(LLMEvent::Delta(content)).await;
                                            }
                                            if let Some(u) = usage {
                                                let _ = tx.send(LLMEvent::Usage(u)).await;
                                            }
                                        }
                                    } else {
                                        break;
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                log::warn!("[llm] SSE 流读取错误: {}", e);
                                break;
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        Ok(rx)
    }

    /// 查询扩展：使用 LLM 将用户问题改写为多个搜索查询
    ///
    /// 返回扩展后的查询列表（不包含原始问题）。失败时返回空 Vec。
    pub async fn expand_queries(
        &self,
        text: &str,
        history: &[ChatMessage],
        cancel: CancellationToken,
    ) -> Vec<String> {
        // 构建携带上下文的扩展 prompt
        let mut system_msg = String::new();

        // 对话上下文（最近 4 条）
        let recent_count = history.len().min(4);
        if recent_count > 0 {
            system_msg.push_str("对话历史（最近几条）：\n");
            for msg in history.iter().rev().take(recent_count).rev() {
                let role_label = match msg.role.as_str() {
                    "user" => "用户",
                    _ => "助手",
                };
                let content = if msg.content.len() > 200 {
                    format!("{}...", &msg.content[..200])
                } else {
                    msg.content.clone()
                };
                system_msg.push_str(&format!("{}: {}\n", role_label, content));
            }
            system_msg.push('\n');
        }

        system_msg.push_str(
            "你是一个查询扩展助手。请将以下用户问题改写为3个不同的搜索查询，每行一条，不要序号或前缀。\n\
             改写时应从不同角度：关键词聚焦、实体提问、同义改写。\n\
             示例：\n\
             问题：如何在 Rust 中处理异步错误？\n\
             输出：\n\
             Rust 异步错误处理最佳实践\n\
             Rust async/await 错误类型处理\n\
             Rust 中 tokio 错误处理方式\n\
             问题：",
        );
        system_msg.push_str(text);

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: system_msg,
        }];

        let mut rx = match self
            .stream_chat_completion(&messages, Some(0.2), Some(300), cancel)
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                log::warn!("[llm] 查询扩展请求失败: {}", e);
                return Vec::new();
            }
        };

        let mut full = String::new();
        while let Some(event) = rx.recv().await {
            if let LLMEvent::Delta(text) = event {
                full.push_str(&text);
            }
        }

        // 解析结果为查询列表
        let lines: Vec<String> = full
            .split('\n')
            .map(|l| l.trim().to_string())
            .filter(|l| l.len() > 5)
            .collect();

        // 简单字符集 Jaccard 去重
        let similarity = |a: &str, b: &str| -> f64 {
            let set_a: HashSet<char> = a
                .to_lowercase()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            let set_b: HashSet<char> = b
                .to_lowercase()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            if set_a.is_empty() && set_b.is_empty() {
                return 1.0;
            }
            let intersect = set_a.intersection(&set_b).count();
            let union = set_a.len() + set_b.len() - intersect;
            if union == 0 {
                0.0
            } else {
                intersect as f64 / union as f64
            }
        };

        lines
            .into_iter()
            .filter(|l| similarity(l, text) < 0.7)
            .take(3)
            .collect()
    }
}
