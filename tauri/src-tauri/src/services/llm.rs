use std::collections::HashSet;

use rig_core::client::completion::CompletionClient;
use rig_core::completion::{AssistantContent, CompletionModel, CompletionRequest, Message, Usage};
use rig_core::providers::openai;
use rig_core::OneOrMany;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

// ─── 公共类型 ───

/// 对话消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
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

// ─── 工具函数 ───

/// 将配置中的 LLM 端点归一化为 Rig 的 base_url。
///
/// 配置可能携带完整路径（如 `http://host/v1/chat/completions`），而 Rig 的
/// OpenAI provider 会自动拼接 `/chat/completions`，因此需要剥离该后缀。
/// 注意只剥离 `/chat/completions`，保留版本前缀 `/v1`：否则 `.../v1/chat/completions`
/// 会退化为 `.../chat/completions`，被部分网关（如 LM Studio 前置代理）拒绝。
fn normalize_base_url(endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    let lower = trimmed.to_ascii_lowercase();
    if let Some(idx) = lower.rfind("/chat/completions") {
        return trimmed[..idx].to_string();
    }
    trimmed.to_string()
}

/// 将项目内的 `ChatMessage` 转换为 Rig 的消息类型
pub fn chat_message_to_rig(msg: &ChatMessage) -> Message {
    match msg.role.as_str() {
        "system" => Message::system(&msg.content),
        "assistant" => Message::assistant(&msg.content),
        _ => Message::user(&msg.content),
    }
}

/// 将 Rig 的用量信息转换为项目内的 `UsageInfo`
pub fn usage_to_info(usage: &Usage) -> UsageInfo {
    UsageInfo {
        prompt_tokens: usage.input_tokens as u32,
        completion_tokens: usage.output_tokens as u32,
        total_tokens: usage.total_tokens as u32,
    }
}

// ─── LLM 客户端 ───

/// 基于 Rig OpenAI provider（Chat Completions）的流式客户端。
///
/// 内部持有的 `CompletionModel` 通过 `reqwest::Client`（内部 Arc 共享连接池）
/// 发请求，`Clone` 廉价，可安全缓存复用。
///
/// 已知限制：Rig 对流式请求默认注入 `stream_options: {"include_usage": true}`，
/// 主流本地服务器（Ollama / llama.cpp / vLLM / LM Studio）均宽松忽略；若对接
/// 严格校验参数的兼容网关返回 400，需自定义 Provider 扩展关闭该字段。
#[derive(Clone)]
pub struct LLMClient {
    /// 归一化后的 base_url（不含 /chat/completions 后缀）
    endpoint: String,
    model: String,
    completion_model: openai::CompletionModel,
}

impl LLMClient {
    /// 构建客户端。配置非法（如 api_key 含非法 HTTP 头字符）时返回 Err。
    pub fn new(endpoint: String, model: String, api_key: String) -> Result<Self, String> {
        let base_url = normalize_base_url(&endpoint);

        let client = openai::CompletionsClient::builder()
            .api_key(&api_key)
            .base_url(&base_url)
            .build()
            .map_err(|e| format!("创建 LLM 客户端失败: {}", e))?;
        let completion_model = client.completion_model(&model);

        log::debug!("[llm] LLMClient init base_url={} model={}", base_url, model);

        Ok(Self {
            endpoint: base_url,
            model,
            completion_model,
        })
    }

    /// 快速判断配置是否有效
    pub fn is_configured(&self) -> bool {
        !self.endpoint.is_empty() && !self.model.is_empty()
    }

    /// 获取底层 Rig 补全模型（用于构建 Agent）
    pub fn completion_model(&self) -> &openai::CompletionModel {
        &self.completion_model
    }

    /// 查询扩展：使用 LLM 将用户问题改写为多个搜索查询
    ///
    /// 返回扩展后的查询列表（不包含原始问题）。请求失败或取消时返回空 Vec，
    /// 由调用方降级为仅使用原始查询，避免错误文本污染检索。
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
                    let truncated: String = msg.content.chars().take(200).collect();
                    format!("{}...", truncated)
                } else {
                    msg.content.clone()
                };
                system_msg.push_str(&format!("{}: {}\n", role_label, content));
            }
            system_msg.push('\n');
        }

        system_msg.push_str(concat!(
            "你是专业检索查询扩展助手，用于对话输入的语义理解与本地知识库混合检索。请将用户当前问题扩展为3个差异化检索查询，严格遵循以下规则：\n",
            "1. 输出共3行，每行一条查询，无序号、无引号、无任何前缀后缀与解释内容\n",
            "2. 严格保留用户原始问题的核心意图，不得新增、删减或篡改任何需求\n",
            "3. 三条查询分别对应以下三个独立维度，语义不重叠：\n",
            "   - 关键词聚焦：剔除语气词与冗余表述，提取核心实体+核心动作，生成紧凑短语，适配关键词检索\n",
            "   - 实体精准提问：围绕问题核心对象，生成聚焦具体实体的完整查询，适配精准语义匹配\n",
            "   - 同义场景扩展：使用同义词、领域常用术语/典型组件替换表述，覆盖文档中的不同行文与场景\n",
            "\n",
            "示例：\n",
            "问题：如何在 Rust 中处理异步错误？\n",
            "输出：\n",
            "Rust 异步错误处理最佳实践\n",
            "Rust async/await 错误类型处理\n",
            "Rust 中 tokio 错误处理方式\n",
            "问题：",
        ));
        system_msg.push_str(text);

        // 构造 Rig 请求（单条 user 消息，模型参数固定为低温度 + 短输出）
        let request = CompletionRequest {
            model: None,
            preamble: None,
            chat_history: OneOrMany::one(Message::user(system_msg)),
            documents: Vec::new(),
            tools: Vec::new(),
            temperature: Some(0.2),
            max_tokens: Some(2048),
            tool_choice: None,
            additional_params: None,
            output_schema: None,
            record_telemetry_content: false,
        };

        log::debug!(
            "[llm] expand_queries request, input: query='{}' history_count={} base_url={} model={} body={}",
            text,
            history.len(),
            self.endpoint,
            self.model,
            serde_json::to_string(&request)
                .unwrap_or_else(|e| format!("<serialize failed: {}>", e))
        );

        // 直接非流式调用：expand_queries 只需要完整结果，无需流式体验。
        // 非流式请求（stream: false）返回 application/json，兼容性最好，
        // 也规避 thinking 类模型 SSE 中 reasoning 内容的解析差异。
        let model = self.completion_model.clone();
        let mut full = String::new();

        let result = tokio::select! {
            _ = cancel.cancelled() => {
                log::debug!("[llm] expand_queries cancelled");
                return Vec::new();
            }
            res = model.completion(request) => res,
        };

        match result {
            Ok(response) => {
                log::debug!(
                    "[llm] expand_queries response choice={:?} usage={:?}",
                    response.choice, response.usage
                );
                for item in response.choice.iter() {
                    if let AssistantContent::Text(text) = item {
                        full.push_str(&text.text);
                    }
                }
            }
            Err(e) => {
                log::warn!("[llm] expand_queries 非流式调用失败 err={}", e);
                return Vec::new();
            }
        }

        if full.trim().is_empty() {
            log::debug!("[llm] expand_queries empty response");
            return Vec::new();
        }

        log::debug!("[llm] expand_queries raw_response: {}", full);

        // 解析结果为查询列表：按行分割，去除编号/前缀
        let lines: Vec<String> = full
            .split('\n')
            .map(|l| l.trim().trim_start_matches(|c: char| c.is_ascii_digit() || ".-、). ".contains(c)).trim().to_string())
            .filter(|l| l.len() > 5)
            .collect();

        // 字符集 Jaccard 相似度（用于去重）
        let char_jaccard = |a: &str, b: &str| -> f64 {
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

        // 1) 初筛：过滤掉与原始查询过于相似的变体
        let mut candidates: Vec<String> = lines
            .into_iter()
            .filter(|l| char_jaccard(l, text) < 0.6)
            .collect();

        // 2) 交叉去重：如果两个变体彼此过于相似，保留更短的（更聚焦）
        //    先将候选按与原始查询的相似度升序排列（优先保留差异最大的）
        candidates.sort_by(|a, b| {
            let sa = char_jaccard(a, text);
            let sb = char_jaccard(b, text);
            sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut deduped: Vec<String> = Vec::new();
        for cand in candidates {
            let is_dup = deduped.iter().any(|existing| char_jaccard(&cand, existing) >= 0.8);
            if !is_dup {
                deduped.push(cand);
            }
        }

        // 3) 上限 3 个
        deduped.truncate(3);
        deduped
    }
}

