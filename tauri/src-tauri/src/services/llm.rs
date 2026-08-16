use std::collections::HashSet;
use std::time::Duration;

use async_trait::async_trait;

use rig_core::client::completion::CompletionClient;
use rig_core::completion::{
    AssistantContent, CompletionError, CompletionModel, CompletionRequest, CompletionResponse,
    Message, Usage,
};
use rig_core::providers::openai;
use rig_core::OneOrMany;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

// ─── 公共类型 ───

/// 对话消息
///
/// `role` 支持 `system` / `user` / `assistant` / `tool`：
/// - `assistant` 可携带 `tool_calls`（模型发起的工具调用，回放时还原为 rig ToolCall 内容）
/// - `tool` 是工具结果消息，`tool_call_id` 与对应 assistant 消息中的调用配对
/// 旧数据/旧前端不携带这些字段（`#[serde(default)]`），行为保持向后兼容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// 消息 id（数据库主键，前端透传；用于压缩检查点定位，可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<crate::core::ToolCallDto>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
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
    /// 本次请求命中 provider 缓存的输入 token 数（缓存命中率 = cached / prompt）
    #[serde(default)]
    pub cached_input_tokens: u32,
    /// 本次请求写入 provider 缓存的输入 token 数（openai 通道 rig 未解析，恒为 0）
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
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

/// 将 Rig 的用量信息转换为项目内的 `UsageInfo`
pub fn usage_to_info(usage: &Usage) -> UsageInfo {
    UsageInfo {
        prompt_tokens: usage.input_tokens as u32,
        completion_tokens: usage.output_tokens as u32,
        total_tokens: usage.total_tokens as u32,
        cached_input_tokens: usage.cached_input_tokens as u32,
        cache_creation_input_tokens: usage.cache_creation_input_tokens as u32,
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
/// LLM HTTP 请求级总超时（秒）：作用于每一次 LLM 请求（含 SSE 流式读取期），
/// 防止服务端挂起导致请求永久悬挂。正常单轮生成远低于该值；子代理等多轮
/// 流程每轮独立计超时，不叠加；父链取消时由 drop 传播中止，此超时仅兜底
/// 极端挂起场景。
const LLM_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

// ─── LLM 调用重试（P0-4）：非流式调用指数退避，对齐 Pi provider retry ───
/// 最大重试次数（首次调用之外的额外尝试次数；总尝试 = 重试次数 + 1）
const LLM_RETRY_MAX: usize = 3;
/// 退避起始延迟（毫秒），此后每次翻倍
const LLM_RETRY_BASE_MS: u64 = 2000;
/// 退避延迟上限（毫秒）
const LLM_RETRY_MAX_MS: u64 = 60_000;

/// 判断 HTTP 状态码是否可重试（429 限流 / 408 超时 / 5xx 服务端错误）
pub(crate) fn is_retryable_status_code(code: u16) -> bool {
    code == 429 || code == 408 || (500..=599).contains(&code)
}

/// 判断 rig 补全错误是否为瞬时错误（可重试）。
///
/// - `HttpError`：连接/超时/流中断类视为瞬时；HTTP 状态码按 `is_retryable_status_code` 判定
/// - `ProviderResponse`：带状态码则按码判定，无状态码保守重试
/// - `ProviderError`：provider 文本错误（429/5xx 常在此处），保守重试
/// - 其余（JsonError/UrlError/RequestError/ResponseError）：确定性错误，不重试
pub(crate) fn is_retryable_completion_error(e: &CompletionError) -> bool {
    match e {
        CompletionError::HttpError(http_err) => match http_err {
            rig_core::http_client::Error::InvalidStatusCode(s)
            | rig_core::http_client::Error::InvalidStatusCodeWithMessage(s, _) => {
                is_retryable_status_code(s.as_u16())
            }
            // 连接错误/超时/流中断/协议错误：瞬时
            rig_core::http_client::Error::Instance(_)
            | rig_core::http_client::Error::StreamEnded
            | rig_core::http_client::Error::Protocol(_) => true,
            // InvalidHeaderValue / NoHeaders / InvalidContentType：确定性
            _ => false,
        },
        CompletionError::ProviderResponse(pe) => pe
            .status
            .map(|s| is_retryable_status_code(s.as_u16()))
            .unwrap_or(true),
        CompletionError::ProviderError(_) => true,
        _ => false,
    }
}

/// 泛型指数退避重试循环（依赖倒置：调用方注入"执行一次"的闭包与可重试判定）。
///
/// - 可重试错误：退避 `base_delay * 2^attempt`（上限 `max_delay`）后重试，至多 `max_retries` 次
/// - 不可重试错误 / 达到上限：立即返回
/// - 退避期间监听 `cancel`：取消后立即返回当前错误，不继续等待
pub(crate) async fn retry_loop<F, Fut, T, E>(
    mut call: F,
    is_retryable: fn(&E) -> bool,
    max_retries: usize,
    base_delay: Duration,
    max_delay: Duration,
    cancel: CancellationToken,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut attempt = 0usize;
    loop {
        if cancel.is_cancelled() {
            // 取消时不发起新尝试：透传当前错误（若尚未调用则返回一个不可重试语义）
        }
        match call().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                if attempt >= max_retries || !is_retryable(&e) {
                    return Err(e);
                }
                let delay = base_delay
                    .saturating_mul(2u32.saturating_pow(attempt as u32))
                    .min(max_delay);
                log::warn!(
                    "[llm] 调用失败，{}ms 后第 {} 次重试: {}",
                    delay.as_millis(),
                    attempt + 1,
                    e
                );
                tokio::select! {
                    _ = cancel.cancelled() => return Err(e),
                    _ = tokio::time::sleep(delay) => {}
                }
                attempt += 1;
            }
        }
    }
}

#[derive(Clone)]
pub struct LLMClient {
    /// 归一化后的 base_url（不含 /chat/completions 后缀）
    endpoint: String,
    model: String,
    /// 推理努力等级（P2-18：low/medium/high，透传 additional_params）
    reasoning_effort: Option<String>,
    completion_model: openai::CompletionModel,
}

impl LLMClient {
    /// 构建客户端。配置非法（如 api_key 含非法 HTTP 头字符）时返回 Err。
    pub fn new(
        endpoint: String,
        model: String,
        api_key: String,
        reasoning_effort: Option<String>,
    ) -> Result<Self, String> {
        let base_url = normalize_base_url(&endpoint);

        // 注入带超时的 http client：rig_core 对 reqwest::Client 直接实现了
        // HttpClientExt，CompletionsClient::builder().http_client(...) 可注入。
        // 注意必须用 rig_core 重新导出的 reqwest(0.13) 类型——rig 的 HttpClientExt
        // 只对该版本实现；mdgo 直接依赖的 reqwest 0.12 是不同 crate 实例，不满足约束。
        // timeout 为请求级总时长（含 SSE 流式），300s 内正常生成不受影响，仅兜底挂起。
        let http_client = rig_core::http_client::ReqwestClient::builder()
            .timeout(LLM_REQUEST_TIMEOUT)
            .build()
            .map_err(|e| format!("创建 HTTP 客户端失败: {}", e))?;
        let client = openai::CompletionsClient::builder()
            .api_key(&api_key)
            .base_url(&base_url)
            .http_client(http_client)
            .build()
            .map_err(|e| format!("创建 LLM 客户端失败: {}", e))?;
        let completion_model = client.completion_model(&model);

        log::info!("[llm] LLMClient init base_url={}，api_key={}， model={}", base_url, api_key, model);

        Ok(Self {
            endpoint: base_url,
            model,
            reasoning_effort,
            completion_model,
        })
    }

    /// 向补全请求注入通用参数（P2-18：reasoning_effort 透传 additional_params）。
    ///
    /// 非流式调用点（查询扩展/规划/摘要/评审）构造 `CompletionRequest` 后统一调用。
    fn apply_common_params(&self, mut request: CompletionRequest) -> CompletionRequest {
        if let Some(effort) = &self.reasoning_effort {
            let effort = effort.trim().to_lowercase();
            if !effort.is_empty() {
                let mut params = request.additional_params.take().unwrap_or_else(|| serde_json::json!({}));
                if let Some(obj) = params.as_object_mut() {
                    obj.insert("reasoning_effort".into(), serde_json::Value::String(effort));
                    request.additional_params = Some(params);
                }
            }
        }
        request
    }

    /// 快速判断配置是否有效
    pub fn is_configured(&self) -> bool {
        !self.endpoint.is_empty() && !self.model.is_empty()
    }

    /// 获取底层 Rig 补全模型（用于构建 Agent）
    pub fn completion_model(&self) -> &openai::CompletionModel {
        &self.completion_model
    }

    /// 非流式补全 + 指数退避重试（P0-4）。
    ///
    /// 仅对瞬时错误（429/408/5xx/连接/超时/流中断）重试；
    /// 重试间隔受 `cancel` 控制（取消即中止，不再等待）。
    /// 流式主链路（agent 生成）不做请求级重试：rig agent 流重放会重复执行
    /// 工具副作用，由 300s 超时 + 用户重试兜底（设计取舍，见规划文档 P0-4）。
    async fn completion_with_retry(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionResponse<openai::CompletionResponse>, CompletionError> {
        let model = self.completion_model.clone();
        retry_loop(
            || model.completion(request.clone()),
            is_retryable_completion_error,
            LLM_RETRY_MAX,
            Duration::from_millis(LLM_RETRY_BASE_MS),
            Duration::from_millis(LLM_RETRY_MAX_MS),
            cancel,
        )
        .await
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
            max_tokens: Some(1024),
            tool_choice: None,
            additional_params: None,
            output_schema: None,
            record_telemetry_content: false,
        };
        let request = self.apply_common_params(request);

        log::info!("[llm] [输入语义扩展] input: query='{}' history_count={} ", text, history.len());

        // 直接非流式调用：expand_queries 只需要完整结果，无需流式体验。
        // 非流式请求（stream: false）返回 application/json，兼容性最好，
        // 也规避 thinking 类模型 SSE 中 reasoning 内容的解析差异。
        let mut full = String::new();

        // 非流式补全 + 指数退避重试（取消在重试循环内响应，失败降级为原查询）
        let result = self.completion_with_retry(request, cancel.clone()).await;
        
        match result {
            Ok(response) => {
                log::info!(
                    "[llm] [输入语义扩展] response choice={:?} usage={:?}",
                    response.choice, response.usage
                );
                for item in response.choice.iter() {
                    if let AssistantContent::Text(text) = item {
                        full.push_str(&text.text);
                    }
                }
            }
            Err(e) => {
                log::warn!("[llm] [输入语义扩展] 非流式调用失败 err={}", e);
                return Vec::new();
            }
        }

        if full.trim().is_empty() {
            log::info!("[llm] [输入语义扩展] empty response");
            return Vec::new();
        }

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
        log::info!("[llm] [输入语义扩展] output: {:?}", deduped);
        deduped
    }

    /// 轻量任务规划：对复杂任务产出结构化计划 JSON 文本。
    ///
    /// 失败/取消返回 `None`，由调用方降级为"不规划"继续原流程（fail-open）；
    /// 规划是一次独立非流式调用，不占用 Agent 的 `DEFAULT_MAX_TURNS` 执行预算。
    pub async fn generate_plan_json(
        &self,
        query: &str,
        history: &[ChatMessage],
        cancel: CancellationToken,
        correction: Option<&str>,
    ) -> Option<String> {
        // 指令部分放 preamble（system role，提高弱模型遵守度——review 修复 A1）
        let preamble = String::from(concat!(
            "你是任务规划助手。用户将提出一个需要多步骤执行的复杂任务。请输出一个 JSON 计划，严格遵循以下格式（除 JSON 外不要输出任何其他内容、注释或代码围栏）：\n",
            "{\"goal\": \"一句话任务目标\", \"steps\": [\"步骤1\", ...], \"acceptance\": [\"可验证的验收标准1\", ...], \"risks\": [...], \"touchpoints\": [...], \"non_goals\": [...], \"rollback\": [...]}\n",
            "要求：\n",
            "1. 键名必须严格为 goal / steps / acceptance / risks / touchpoints / non_goals / rollback，禁止添加 plan_id、name 等其他任何键\n",
            "2. goal 一句话概括目标，不含冗长描述\n",
            "3. steps 3-8 步，每步不超过 60 字，具体、可执行、按顺序\n",
            "4. acceptance 2-5 条，每条可客观验证\n",
            "5. risks / touchpoints / non_goals / rollback 无相关内容时给空数组\n",
            "6. 只输出一个合法 JSON 对象，不要输出任何其他内容、注释或代码围栏\n",
        ));

        // 用户消息：对话上下文（最近 4 条）+ 用户任务 + 修正指令
        let mut user_msg = String::new();
        let recent_count = history.len().min(4);
        if recent_count > 0 {
            user_msg.push_str("对话历史（最近几条）：\n");
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
                user_msg.push_str(&format!("{}: {}\n", role_label, content));
            }
            user_msg.push('\n');
        }

        user_msg.push_str("用户任务：");
        user_msg.push_str(query);

        // P0-3 结构化输出：上一次输出校验失败时，追加修正指令引导模型重发
        if let Some(c) = correction {
            let c = c.trim();
            if !c.is_empty() {
                user_msg.push_str("\n\n你上一次的输出不符合要求，请修正后重新输出：");
                user_msg.push_str(c);
                user_msg.push_str("\n只输出合法 JSON，不要输出任何其他内容、注释或代码围栏。");
            }
        }

        // 构造 Rig 请求（非流式，与 expand_queries 同构；不用 output_schema 保证网关兼容）
        let request = CompletionRequest {
            model: None,
            preamble: Some(preamble),
            chat_history: OneOrMany::one(Message::user(user_msg)),
            documents: Vec::new(),
            tools: Vec::new(),
            temperature: Some(0.3),
            // review 修复 A2：1024 易截断中文计划，提到 2048
            max_tokens: Some(2048),
            tool_choice: None,
            additional_params: None,
            output_schema: None,
            record_telemetry_content: false,
        };
        let request = self.apply_common_params(request);

        log::info!("[llm] [任务规划] input: query_len={} history_count={}", query.len(), history.len());

        let result = self.completion_with_retry(request, cancel.clone()).await;

        match result {
            Ok(response) => {
                let mut full = String::new();
                for item in response.choice.iter() {
                    if let AssistantContent::Text(text) = item {
                        full.push_str(&text.text);
                    }
                }
                let trimmed = full.trim();
                if trimmed.is_empty() {
                    log::warn!("[llm] [任务规划] 空响应");
                    return None;
                }
                log::info!("[llm] [任务规划] response_len={}", trimmed.len());
                Some(trimmed.to_string())
            }
            Err(e) => {
                log::warn!("[llm] [任务规划] 规划调用失败 err={}", e);
                None
            }
        }
    }
}


// ─── 历史摘要(上下文压缩用):将一段对话历史压缩为要点摘要 ───

/// 一条评审发现的问题与修正建议（P1-8 反思质量门）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewIssue {
    pub issue: String,
    pub fix: String,
}

/// 反思评审结果（P1-8）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewResult {
    /// 评审结论：`通过` 或 `需修正`
    pub verdict: String,
    /// 待修正问题列表（无问题时为空）
    pub issues: Vec<ReviewIssue>,
}

impl ReviewResult {
    /// 是否有待修正问题
    pub fn needs_fix(&self) -> bool {
        !self.issues.is_empty()
    }
}

/// 评审输出 JSON Schema（P0-3 结构化校验）
fn review_json_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["verdict", "issues"],
        "properties": {
            "verdict": { "type": "string" },
            "issues": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["issue", "fix"],
                    "properties": {
                        "issue": { "type": "string", "minLength": 1 },
                        "fix": { "type": "string" }
                    }
                }
            }
        },
        "additionalProperties": true
    })
}

#[async_trait]
impl crate::core::context::HistorySummarizer for LLMClient {
    async fn summarize(
        &self,
        turns: &[crate::core::context::ChatTurn],
        max_chars: usize,
        cancel: CancellationToken,
    ) -> Option<String> {
        if turns.is_empty() {
            return Some(String::new());
        }
        let mut prompt = format!(
            "你是对话历史压缩助手。请将以下对话压缩为不超过 {max_chars} 字的要点摘要，只输出摘要正文，不要任何前后缀。必须保留：关键事实、已做的决定、未完成事项、用户的偏好与约束；不得编造内容。\n\n对话：\n"
        );
        for t in turns {
            let label = match t.role.as_str() {
                "user" => "用户",
                "assistant" => "助手",
                "system" => "系统",
                _ => "其他",
            };
            prompt.push_str(&format!("{label}: {}\n", t.content));
        }

        // 构造 Rig 请求(非流式调用模式与 expand_queries 一致:
        // stream=false 返回 application/json,兼容性最好)
        let request = CompletionRequest {
            model: None,
            preamble: None,
            chat_history: OneOrMany::one(Message::user(prompt)),
            documents: Vec::new(),
            tools: Vec::new(),
            temperature: Some(0.3),
            max_tokens: Some((max_chars / 2).clamp(128, 2048) as u64),
            tool_choice: None,
            additional_params: None,
            output_schema: None,
            record_telemetry_content: false,
        };
        let request = self.apply_common_params(request);

        // 可观测性：记录摘要请求上下文（模型/预算/输入规模），用于定位"空响应/失败"根因
        let turns_chars: usize = turns.iter().map(|t| t.content.chars().count()).sum();
        let max_tokens_out = (max_chars / 2).clamp(128, 2048);
        log::info!(
            "[llm] [历史摘要] 发起: model={} turns={} chars={} max_chars={} max_tokens={}",
            self.model, turns.len(), turns_chars, max_chars, max_tokens_out
        );

        let result = self.completion_with_retry(request, cancel.clone()).await;

        match result {
            Ok(response) => {
                let mut full = String::new();
                for item in response.choice.iter() {
                    if let AssistantContent::Text(text) = item {
                        full.push_str(&text.text);
                    }
                }
                let trimmed = full.trim().to_string();
                if trimmed.is_empty() {
                    // 空响应：区分「模型输出空」与「响应解析为空」——记录 model/预算/输入规模，
                    // 便于确认是否 summary_model 回退主模型后模型不可用或输出被截断
                    log::warn!(
                        "[llm] [历史摘要] 空响应: model={} turns={} chars={} max_tokens={} choices={}",
                        self.model, turns.len(), turns_chars, max_tokens_out, response.choice.len()
                    );
                    None
                } else {
                    log::info!(
                        "[llm] [历史摘要] 成功: model={} chars_in={} chars_out={}",
                        self.model, turns_chars, trimmed.chars().count()
                    );
                    Some(trimmed)
                }
            }
            Err(e) => {
                log::warn!(
                    "[llm] [历史摘要] 非流式调用失败 err={} model={} turns={} chars={}",
                    e, self.model, turns.len(), turns_chars
                );
                None
            }
        }
    }
}

impl LLMClient {
    /// 反思评审（P1-8 质量门）：对初稿做质量自检，返回结构化问题列表。
    ///
    /// 校验失败/取消/LLM 不可用返回 `None`（调用方降级为"不评审"，不影响主流程）；
    /// 空初稿直接视为通过。
    pub async fn review_text(
        &self,
        goal: &str,
        draft: &str,
        cancel: CancellationToken,
    ) -> Option<ReviewResult> {
        if draft.trim().is_empty() {
            return Some(ReviewResult {
                verdict: "通过".into(),
                issues: Vec::new(),
            });
        }
        let system_msg = format!(
            "你是答案质量审查助手。检查初稿是否：1) 完整覆盖用户目标；2) 事实/引用一致；3) 无遗漏步骤或明显错误；4) 结构清晰。\n\
             只输出 JSON：{{\"verdict\": \"通过\" 或 \"需修正\", \"issues\": [{{\"issue\": \"问题描述\", \"fix\": \"具体修正建议\"}}]}}，无问题时 issues 为空数组。\n\n\
             用户目标：{goal}\n\n初稿：\n{draft}"
        );
        let request = CompletionRequest {
            model: None,
            preamble: None,
            chat_history: OneOrMany::one(Message::user(system_msg)),
            documents: Vec::new(),
            tools: Vec::new(),
            temperature: Some(0.2),
            max_tokens: Some(1024),
            tool_choice: None,
            additional_params: None,
            output_schema: None,
            record_telemetry_content: false,
        };
        let request = self.apply_common_params(request);
        let result = self.completion_with_retry(request, cancel).await.ok()?;
        let mut full = String::new();
        for item in result.choice.iter() {
            if let AssistantContent::Text(text) = item {
                full.push_str(&text.text);
            }
        }
        // P0-3 结构化校验：非法输出视为评审失败（降级不评审）
        let validator =
            crate::core::validation::JsonSchemaValidator::new(review_json_schema()).ok()?;
        let value = validator.validate_json_text(full.trim()).ok()?;
        serde_json::from_value(value).ok()
    }
}

// ─── 重试逻辑单元测试（P0-4） ───

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    async fn retry_calls_times_until_success(max_failures: usize) -> usize {
        // 返回「实际调用次数」，失败前 max_failures 次返回 Err，之后返回 Ok
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_inner = calls.clone();
        let result = retry_loop(
            move || {
                let calls = calls_inner.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::Relaxed) + 1;
                    if n <= max_failures {
                        Err::<u32, String>(format!("transient {n}"))
                    } else {
                        Ok::<u32, String>(42)
                    }
                }
            },
            |_| true,
            3,
            Duration::from_millis(1),
            Duration::from_millis(10),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(result, Ok(42));
        calls.load(Ordering::Relaxed)
    }

    #[tokio::test]
    async fn retry_loop_retries_transient_errors() {
        // 429 类瞬时错误：重试 2 次后成功（共 3 次调用）
        assert_eq!(retry_calls_times_until_success(2).await, 3);
    }

    #[tokio::test]
    async fn retry_loop_succeeds_first_try_without_retry() {
        assert_eq!(retry_calls_times_until_success(0).await, 1);
    }

    #[tokio::test]
    async fn retry_loop_gives_up_after_max_retries() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_inner = calls.clone();
        let result = retry_loop(
            move || {
                let calls = calls_inner.clone();
                async move {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Err::<u32, String>("always fails".into())
                }
            },
            |_| true,
            2,
            Duration::from_millis(1),
            Duration::from_millis(10),
            CancellationToken::new(),
        )
        .await;
        assert!(result.is_err());
        // 总尝试 = max_retries + 1 = 3
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn retry_loop_no_retry_on_fatal_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_inner = calls.clone();
        let result = retry_loop(
            move || {
                let calls = calls_inner.clone();
                async move {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Err::<u32, String>("bad request".into())
                }
            },
            |_| false,
            3,
            Duration::from_millis(1),
            Duration::from_millis(10),
            CancellationToken::new(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1, "确定性错误不重试");
    }

    #[tokio::test]
    async fn retry_loop_aborts_when_cancelled_during_backoff() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cancel = CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_inner = calls.clone();
        let cancel_inner = cancel.clone();
        let started = std::time::Instant::now();
        let result = retry_loop(
            move || {
                let calls = calls_inner.clone();
                let cancel = cancel_inner.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::Relaxed) + 1;
                    // 首次调用失败后触发取消（退避等待期间应立即返回，不等待 60s 上限）
                    if n == 1 {
                        cancel.cancel();
                    }
                    Err::<u32, String>("transient".into())
                }
            },
            |_| true,
            5,
            Duration::from_secs(60),
            Duration::from_secs(60),
            cancel.clone(),
        )
        .await;
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(5), "取消后不应继续退避等待");
        assert_eq!(calls.load(Ordering::Relaxed), 1, "取消后不再重试");
    }

    #[test]
    fn retryable_status_code_covers_transient() {
        assert!(is_retryable_status_code(429));
        assert!(is_retryable_status_code(408));
        assert!(is_retryable_status_code(500));
        assert!(is_retryable_status_code(503));
        assert!(!is_retryable_status_code(400));
        assert!(!is_retryable_status_code(401));
        assert!(!is_retryable_status_code(404));
    }

    #[test]
    fn retryable_completion_error_classification() {
        // ProviderError：保守重试
        assert!(is_retryable_completion_error(&CompletionError::ProviderError(
            "rate limited".into()
        )));
        // ResponseError（解析失败）：不重试
        assert!(!is_retryable_completion_error(&CompletionError::ResponseError(
            "bad json".into()
        )));
        // HttpError::Instance（连接/超时类）：重试
        assert!(is_retryable_completion_error(&CompletionError::HttpError(
            rig_core::http_client::Error::Instance(Box::new(std::io::Error::other("conn reset")))
        )));
        // HttpError 带 429 状态码：重试
        assert!(is_retryable_completion_error(&CompletionError::HttpError(
            rig_core::http_client::Error::InvalidStatusCodeWithMessage(
                http::StatusCode::TOO_MANY_REQUESTS,
                "rate limit".into(),
            )
        )));
        // HttpError 带 400 状态码：不重试
        assert!(!is_retryable_completion_error(&CompletionError::HttpError(
            rig_core::http_client::Error::InvalidStatusCodeWithMessage(
                http::StatusCode::BAD_REQUEST,
                "bad".into(),
            )
        )));
    }
}
