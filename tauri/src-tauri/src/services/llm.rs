use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::core::agent::limits::{MAX_EXPANDED_QUERIES, QUERY_EXPANSION_RETRY_MAX};
use crate::core::r#loop::{
    CompletionRequest as LoopRequest, CompletionResponse as LoopResponse, LlmAdapter, LlmError,
    LlmMessage, LlmRole, OpenAiAdapter,
};
use crate::core::search::query_plan::QueryKind;

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

/// 书签 LLM 摘要产物（summary + 分类 + 标签），由 `LLMClient::summarize_bookmark` 产出并落库。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BookmarkSummaryOut {
    /// 内容摘要（必填）
    pub summary: String,
    /// 分类（可选，模型未给出时留空）
    #[serde(default)]
    pub category: Option<String>,
    /// 标签列表（可选，模型未给出时为空）
    #[serde(default)]
    pub tags: Vec<String>,
}

/// 查询扩展的结构化结果（P0/P2 预检索优化器）。
///
/// `queries` 不含原始查询（原始查询由调用方必保）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExpansionResult {
    /// 扩展查询列表（0 ~ `MAX_EXPANDED_QUERIES` 条）
    #[serde(default)]
    pub queries: Vec<ExpandedQuery>,
}

impl ExpansionResult {
    /// 扩展查询文本列表（供调用方批量向量化/检索）
    pub fn texts(&self) -> Vec<&str> {
        self.queries.iter().map(|q| q.text.as_str()).collect()
    }

    /// 是否为空（无需扩展）
    pub fn is_empty(&self) -> bool {
        self.queries.is_empty()
    }
}

/// 单条扩展查询（text + 检索语义类型）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExpandedQuery {
    /// 查询文本
    pub text: String,
    /// 检索语义类型（keyword / entity / semantic）；模型未给出时默认 keyword
    #[serde(default)]
    pub kind: QueryKind,
}

/// 解析 LLM 扩展输出：优先 JSON（结构化 text+kind），失败回退按行解析（旧格式）。
///
/// 解析器只做"尽力解析"，任何失败都返回空列表由调用方 fail-open；
/// 绝不因解析失败把 LLM 原始噪声文本透传给检索。
fn parse_expansion_output(full: &str) -> Vec<ExpandedQuery> {
    let trimmed = full.trim();

    // 1) JSON 优先：容忍 ```json 围栏与前后杂质，截取首个 { 到最后一个 }
    let body = if let Some(stripped) = trimmed.strip_prefix("```") {
        stripped
            .find("```")
            .map(|idx| &stripped[..idx])
            .unwrap_or(stripped)
    } else {
        trimmed
    };
    if let (Some(start), Some(end)) = (body.find('{'), body.rfind('}')) {
        if end > start {
            if let Ok(parsed) = serde_json::from_str::<ExpansionResult>(&body[start..=end]) {
                // JSON 合法即采用（即使 queries 为空数组——模型明确表示无需扩展，
                // 此时不应回退按行解析，避免把解释性文字当成查询）
                return parsed
                    .queries
                    .into_iter()
                    .filter(|q| !q.text.trim().is_empty())
                    .collect();
            }
        }
    }

    // 2) 回退：按行解析（兼容旧版"每行一条查询"输出），全部标 Keyword
    full.split('\n')
        .map(|l| {
            l.trim()
                .trim_start_matches(|c: char| c.is_ascii_digit() || ".-、). ".contains(c))
                .trim()
                .to_string()
        })
        .filter(|l| l.len() > 5)
        .map(|text| ExpandedQuery {
            text,
            kind: QueryKind::Keyword,
        })
        .collect()
}

// ─── 工具函数 ───

/// 将配置中的 LLM 端点归一化（剥离 `/chat/completions` 后缀，保留 `/v1` 前缀）。
fn normalize_base_url(endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    let lower = trimmed.to_ascii_lowercase();
    if let Some(idx) = lower.rfind("/chat/completions") {
        return trimmed[..idx].to_string();
    }
    trimmed.to_string()
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
// ─── LLM 调用重试（P0-4）：非流式调用指数退避，对齐主流 Agent（DeepSeek Harness）设置 ───
// DSH 默认：maxRetries=5、初始退避 500ms、最大退避 10s、jitter 0.1。
// mdgo 面向慢速本地/自建端点，在 DSH 基础上把耐心拉长：
/// 最大重试次数（首次调用之外的额外尝试次数；总尝试 = 重试次数 + 1）
const LLM_RETRY_MAX: usize = 5;
/// 退避起始延迟（毫秒），此后每次翻倍
const LLM_RETRY_BASE_MS: u64 = 2000;
/// 退避延迟上限（毫秒）
/// 🟠 M26 修复：32s 使上限可达（退避序列 2/4/8/16/32 在第 5 次重试触及）；
/// 旧值 120s 在默认序列下永远达不到，是死常量，且让最坏等待（6×单请求超时 +
/// 退避）膨胀到小时级——交互式规划/摘要路径由命令层外层 deadline 兜底（见
/// `commands/llm.rs` 的规划总时限），本常量只控制单次调用内的重试耐心。
const LLM_RETRY_MAX_MS: u64 = 32_000;

/// 判断 HTTP 状态码是否可重试（429 限流 / 408 超时 / 5xx 服务端错误）。
/// 当前仅测试路径使用（`completion_with_retry` 经 `LlmError` 判定），保留供回归。
#[allow(dead_code)]
pub(crate) fn is_retryable_status_code(code: u16) -> bool {
    code == 429 || code == 408 || (500..=599).contains(&code)
}

/// P0-1（安全）：对敏感凭据（api_key 等）输出不可逆掩码，禁止明文落日志。
///
/// 使用确定性 FNV-1a 64 位哈希 + 长度，任何能读日志的人也无法还原凭据；
/// 仍可凭哈希比对配置是否变化（运维排查用）。空串显示 `<empty>`。
pub(crate) fn mask_secret(secret: &str) -> String {
    if secret.is_empty() {
        return "<empty>".to_string();
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a 偏移基
    for b in secret.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("<len={} fnv={:016x}>", secret.len(), h)
}

/// 判断 LlmError 是否可重试（瞬时错误：429/408/5xx/连接/超时/provider 文本）。
/// 对齐 `core::loop::LlmError::is_retryable`（P0-4 语义：确定性 4xx/溢出不重试）。
pub(crate) fn is_retryable_llm_error(e: &LlmError) -> bool {
    e.is_retryable()
}

/// 泛型指数退避重试循环（依赖倒置：调用方注入"执行一次"的闭包与可重试判定）。
///
/// - 可重试错误：退避 `base_delay * 2^attempt`（上限 `max_delay`）后重试，至多 `max_retries` 次
/// - 不可重试错误 / 达到上限 / 已取消：立即返回
/// - 退避期间监听 `cancel`：取消后立即返回当前错误，不继续等待
/// - 注：泛型错误类型 E 无法凭空构造，故「进入本循环前已取消」时仍会执行首次调用；
///   调用方（commands/llm.rs 各调用点）在进入前已检查 `cancel.is_cancelled()`，
///   此处负责失败后与退避间隙的取消兜底（P0-3 修复：删除原先的空块死代码）。
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
        match call().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                // 达到上限 / 不可重试 / 已取消 → 立即返回（取消优先于重试）
                if attempt >= max_retries || !is_retryable(&e) || cancel.is_cancelled() {
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
    /// 非流式 LLM 适配器（LlmAdapter seam；OpenAI 兼容）
    adapter: Arc<dyn LlmAdapter>,
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

        // 非流式适配器（LlmAdapter seam；OpenAI 兼容，transport 与业务解耦）
        let adapter: Arc<dyn LlmAdapter> = Arc::new(OpenAiAdapter::new(
            base_url.clone(),
            model.clone(),
            api_key.clone(),
            reasoning_effort.clone(),
        ));

        // P0-1（安全）：api_key 绝不明文落日志（见 mask_secret）。仅输出不可逆掩码。
        log::info!("[llm] LLMClient init base_url={}，api_key={}， model={}", base_url, mask_secret(&api_key), model);

        Ok(Self {
            endpoint: base_url,
            model,
            reasoning_effort,
            adapter,
        })
    }

    /// 向补全请求注入通用参数（P2-18：reasoning_effort 透传）。
    fn apply_common_params(&self, mut request: LoopRequest) -> LoopRequest {
        if let Some(effort) = &self.reasoning_effort {
            let effort = effort.trim().to_lowercase();
            if !effort.is_empty() {
                request.reasoning_effort = Some(effort);
            }
        }
        request
    }

    /// 快速判断配置是否有效
    pub fn is_configured(&self) -> bool {
        !self.endpoint.is_empty() && !self.model.is_empty()
    }

    /// 通用非流式补全（含指数退避重试与取消）——供对话外业务（总结/分析/提炼）复用。
    ///
    /// - `system`：系统提示词（可空，传空串则仅发 user 消息）
    /// - `user`：用户输入（必填）
    /// - `max_tokens`：输出上限（None 使用模型默认）
    /// - `temperature`：采样温度
    ///
    /// 返回模型纯文本；失败返回 `Err`（调用方自行降级）。遵循 SOLID：
    /// 复用同一重试策略（`completion_with_retry`），避免各业务重复实现退避逻辑。
    pub async fn complete_text(
        &self,
        system: &str,
        user: &str,
        max_tokens: Option<u32>,
        temperature: Option<f32>,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let mut request = if system.trim().is_empty() {
            LoopRequest::new(vec![LlmMessage::text(LlmRole::User, user)])
        } else {
            LoopRequest::new(vec![
                LlmMessage::text(LlmRole::System, system),
                LlmMessage::text(LlmRole::User, user),
            ])
        };
        request.max_tokens = max_tokens;
        request.temperature = temperature;
        let request = self.apply_common_params(request);

        let response = self
            .completion_with_retry(request, cancel)
            .await
            .map_err(|e| format!("LLM 调用失败: {}", e))?;
        Ok(response.content)
    }

    /// 非流式补全 + 指数退避重试（P0-4，经 LlmAdapter）。
    ///
    /// 仅对瞬时错误（429/408/5xx/连接/超时）重试；
    /// 重试间隔受 `cancel` 控制（取消即中止，不再等待）。
    async fn completion_with_retry(
        &self,
        request: LoopRequest,
        cancel: CancellationToken,
    ) -> Result<LoopResponse, LlmError> {
        self.completion_with_retry_n(request, cancel, LLM_RETRY_MAX)
            .await
    }

    /// 非流式补全 + 指数退避重试（指定重试次数，经 LlmAdapter）。
    async fn completion_with_retry_n(
        &self,
        request: LoopRequest,
        cancel: CancellationToken,
        max_retries: usize,
    ) -> Result<LoopResponse, LlmError> {
        let adapter = self.adapter.clone();
        let cancel_for_call = cancel.clone();
        retry_loop(
            || adapter.complete(request.clone(), cancel_for_call.clone()),
            is_retryable_llm_error,
            max_retries,
            Duration::from_millis(LLM_RETRY_BASE_MS),
            Duration::from_millis(LLM_RETRY_MAX_MS),
            cancel,
        )
        .await
    }

    /// 查询扩展：使用 LLM 将用户问题改写为 0~2 条差异化搜索查询（P0 预检索优化器）。
    ///
    /// 返回结构化扩展结果（`queries`，不含原始问题）。请求失败、取消、超时或
    /// 解析失败时返回空结果，由调用方降级为仅使用原始查询（fail-open），
    /// 避免错误文本污染检索。
    ///
    /// 设计边界（预检索层一次性 LLM 调用）：
    /// - 数量自适应：LLM 输出 0~2 条，不足时不得凑数；系统侧上限 `MAX_EXPANDED_QUERIES`
    /// - 结构化输出：`{"queries": [{"text": ..., "kind": "keyword|entity|semantic"}]}`，
    ///   解析失败回退按行解析（旧格式，全部标 Keyword）
    /// - 语义去重不在本函数内做：扩展查询向量在检索阶段必然计算，由调用方
    ///   用 embedding cosine 去重（P0-4，零额外推理），此处仅做精确去重
    pub async fn expand_queries(
        &self,
        text: &str,
        history: &[ChatMessage],
        cancel: CancellationToken,
    ) -> ExpansionResult {
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
            "你是专业检索查询扩展助手，用于对话输入的语义理解与本地知识库混合检索。请将用户当前问题扩展为最多2条差异化检索查询，输出 JSON（除 JSON 外不要输出任何其他内容、注释或代码围栏）：\n",
            "{\"queries\": [{\"text\": \"查询1\", \"kind\": \"keyword\"}, {\"text\": \"查询2\", \"kind\": \"entity\"}]}\n",
            "严格遵循以下规则：\n",
            "1. 最多 2 条；查询角度不足 2 条时不要凑数，允许只给 1 条甚至空数组\n",
            "2. 严格保留用户原始问题的核心意图，不得新增、删减或篡改任何需求\n",
            "3. kind 取值：keyword（关键词聚焦：剔除语气词与冗余，提取核心实体+核心动作的紧凑短语，适配关键词检索）/ entity（实体精准：围绕核心对象/实体的完整查询，适配精确匹配与符号检索）/ semantic（同义场景扩展：同义词、领域常用术语/典型组件替换表述，覆盖不同行文与场景）。三条尽量语义不重叠\n",
            "4. 当用户问题本身就是清晰的实体/符号/文件名查询时，输出空数组（不需要扩展）\n",
            "\n",
            "示例：\n",
            "问题：如何在 Rust 中处理异步错误？\n",
            "输出：{\"queries\": [{\"text\": \"Rust async/await 错误类型处理\", \"kind\": \"entity\"}, {\"text\": \"Rust tokio 异步错误处理最佳实践\", \"kind\": \"semantic\"}]}\n",
            "问题：Redis 分布式锁代码在哪里？\n",
            "输出：{\"queries\": [{\"text\": \"Redis 分布式锁实现\", \"kind\": \"keyword\"}, {\"text\": \"Redisson RLock 实现\", \"kind\": \"entity\"}]}\n",
            "问题：",
        ));
        system_msg.push_str(text);

        // 构造 LlmAdapter 请求（单条 user 消息，模型参数固定为低温度 + 短输出）
        let mut request = LoopRequest::new(vec![LlmMessage::text(LlmRole::User, system_msg)]);
        request.temperature = Some(0.2);
        request.max_tokens = Some(1024);
        let request = self.apply_common_params(request);

        log::info!("[llm] [输入语义扩展] input: query='{}' history_count={} ", text, history.len());

        // 直接非流式调用：expand_queries 只需要完整结果，无需流式体验。
        // 预检索预算从紧：重试 QUERY_EXPANSION_RETRY_MAX 次（总时限由调用方 timeout 包裹）。
        let result = self
            .completion_with_retry_n(request, cancel.clone(), QUERY_EXPANSION_RETRY_MAX)
            .await;

        let full = match result {
            Ok(response) => {
                log::info!(
                    "[llm] [输入语义扩展] response chars={} usage={:?}",
                    response.content.chars().count(),
                    response.usage
                );
                response.content
            }
            Err(e) => {
                log::warn!("[llm] [输入语义扩展] 非流式调用失败 err={}", e);
                return ExpansionResult::default();
            }
        };

        if full.trim().is_empty() {
            log::info!("[llm] [输入语义扩展] empty response");
            return ExpansionResult::default();
        }

        // 解析：优先 JSON（结构化 text+kind），失败回退按行解析（旧格式）
        let parsed = parse_expansion_output(&full);

        // 轻量清洗：字节长度过滤 + 精确去重 + 数量上限
        // （语义去重由调用方在向量化后做 embedding cosine 判定，见函数文档）
        let mut seen: HashSet<String> = HashSet::new();
        let mut queries: Vec<ExpandedQuery> = Vec::new();
        for q in parsed {
            let t = q.text.trim().to_string();
            if t.len() <= 5 {
                continue;
            }
            if !seen.insert(t.clone()) {
                continue;
            }
            queries.push(ExpandedQuery { text: t, kind: q.kind });
            if queries.len() >= MAX_EXPANDED_QUERIES {
                break;
            }
        }

        log::info!("[llm] [输入语义扩展] output: {:?}", queries);
        ExpansionResult { queries }
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
        // 指令部分放 preamble（system role，提高弱模型遵守度——review 修复 A1）。
        // 字段最小化：risks/touchpoints/non_goals/rollback 仅在确有内容时输出
        // （Plan 解析对缺省字段宽容），避免模型为凑空数组多吐数百字符拖慢响应。
        let preamble = String::from(concat!(
            "你是任务规划助手。用户将提出一个需要多步骤执行的复杂任务。请输出一个 JSON 计划，严格遵循以下格式（除 JSON 外不要输出任何其他内容、注释或代码围栏）：\n",
            "{\"goal\": \"一句话任务目标\", \"steps\": [\"步骤1\", ...], \"acceptance\": [\"可验证的验收标准1\", ...]}\n",
            "要求：\n",
            "1. 键名必须严格为 goal / steps / acceptance（必填）；risks / touchpoints / non_goals / rollback 可选，禁止添加其他任何键\n",
            "2. goal 一句话概括目标，不含冗长描述\n",
            "3. steps 3-8 步，每步不超过 60 字，具体、可执行、按顺序\n",
            "4. acceptance 2-5 条，每条可客观验证\n",
            "5. risks / touchpoints / non_goals / rollback 仅在确有内容时输出，没有则省略该字段，不要输出空数组\n",
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

        // 构造 LlmAdapter 请求（非流式，与 expand_queries 同构；不用 output_schema 保证网关兼容）
        let mut request = LoopRequest::new(vec![
            LlmMessage::text(LlmRole::System, preamble),
            LlmMessage::text(LlmRole::User, user_msg),
        ]);
        request.temperature = Some(0.3);
        // 计划 JSON 输出有界（steps 3-8 步 + goal + acceptance），1024 token 足够；
        // 收紧上限可截断推理模型的思考/冗余输出，直接压缩慢端点的生成时长。
        // 🟠 L30：权衡——推理模型把思考 token 计入同一 completion 预算时可能截断
        // JSON（→ 触发修正重试，属一次额外调用的降级路径，可接受）；若后续对
        // reasoning_effort 非空的模型放宽到 1536 可减少重试。
        request.max_tokens = Some(1024);
        let request = self.apply_common_params(request);

        log::info!("[llm] [任务规划] input: query_len={} history_count={}", query.len(), history.len());

        let result = self.completion_with_retry(request, cancel.clone()).await;

        match result {
            Ok(response) => {
                let full = response.content;
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

        // 构造 LlmAdapter 请求（非流式，与 expand_queries 一致）
        let mut request = LoopRequest::new(vec![LlmMessage::text(LlmRole::User, prompt)]);
        request.temperature = Some(0.3);
        request.max_tokens = Some((max_chars / 2).clamp(128, 2048) as u32);
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
                let full = response.content;
                let trimmed = full.trim().to_string();
                if trimmed.is_empty() {
                    // 空响应：区分「模型输出空」与「响应解析为空」——记录 model/预算/输入规模
                    log::warn!(
                        "[llm] [历史摘要] 空响应: model={} turns={} chars={} max_tokens={}",
                        self.model, turns.len(), turns_chars, max_tokens_out
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
        let mut request = LoopRequest::new(vec![LlmMessage::text(LlmRole::User, system_msg)]);
        request.temperature = Some(0.2);
        request.max_tokens = Some(1024);
        let request = self.apply_common_params(request);
        let result = self.completion_with_retry(request, cancel).await.ok()?;
        let full = result.content;
        // P0-3 结构化校验：非法输出视为评审失败（降级不评审）
        let validator =
            crate::core::validation::JsonSchemaValidator::new(review_json_schema()).ok()?;
        let value = validator.validate_json_text(full.trim()).ok()?;
        serde_json::from_value(value).ok()
    }

    /// 书签内容摘要：一次调用 LLM 产出摘要 + 分类 + 标签（JSON），供书签 Enrichment 落库。
    ///
    /// 返回：
    /// - `Ok(Some(out))`：成功结构化解析出摘要产物
    /// - `Ok(None)`：模型空响应或输出无法解析为有效 JSON（视为不可用）
    /// - `Err(e)`：网络/超时/取消等调用失败（携带可写入 last_error 的原因）
    ///
    /// 非流式 OpenAI 兼容通道；**使用网关结构化输出**（`output_schema` →
    /// `response_format.json_schema`，rig 自动加 strict + additionalProperties:false +
    /// 全字段 required），模型输出由网关保证符合 Schema；本地
    /// `parse_bookmark_summary_json` 校验仍作兜底（网关不支持 response_format 时
    /// 退回 prompt 约束）。`reasoning_effort` 透传。
    pub async fn summarize_bookmark(
        &self,
        title: &str,
        url: &str,
        content: &str,
        context_length: u32,
        cancel: CancellationToken,
    ) -> Result<Option<BookmarkSummaryOut>, String> {
        const FALLBACK_MAX_INPUT_CHARS: usize = 600;
        // 系统提示词+分类+标题/URL+输出 的字符开销（约 1200 中文字符 ≈ 1500-2400 tokens）。
        // 未配置 context_length 时，fallback 600 + 开销 1200 + 输出 600 ≈ 2400 chars，
        // 按 2 tokens/char 计约 4800 tokens，对小模型（4096）安全。
        const FIXED_OVERHEAD_CHARS: usize = 1200;
        const SUMMARY_OUTPUT_TOKENS: u64 = 1024;
        let output_budget_chars = SUMMARY_OUTPUT_TOKENS as usize;
        let max_input_chars = if context_length > 0 {
            // context_length 是 token 数，按 1 token ≈ 0.5 中文字符保守估算字符预算，
            // 再打 80% 安全余量（避免 tokenizer 差异 + 结构输出 schema 注入撑爆窗口）。
            let char_budget = (context_length as usize) / 2 * 80 / 100;
            char_budget
                .saturating_sub(FIXED_OVERHEAD_CHARS + output_budget_chars)
                .max(200) // 下限：至少保留 200 字符正文（标题+URL 兜底）
        } else {
            FALLBACK_MAX_INPUT_CHARS
        };
        let content: String = if content.chars().count() > max_input_chars {
            let cut: String = content.chars().take(max_input_chars).collect();
            format!("{}…（已截断）", cut)
        } else {
            content.to_string()
        };
        let content_chars = content.chars().count();
        let prompt = format!(
"你是一个信息结构化引擎书签内容整理助手。请阅读下面的书签标题/URL/内容，提取出分类、标签和摘要。只输出 JSON，不输出解释。\n\
\n\
输出格式：\n\
{{\"summary\":\"80~120字摘要\",\"category\":\"分类\",\"tags\":[\"标签1\",\"标签2\"]}}\n\
\n\
分类枚举（严格匹配）：基础科学/工程技术/计算机与AI/实验数据/学术文献/教程讲义/工具软件/行业资讯/项目案例/标准规范/资源素材/行业政策/学术创作/其他\n\
- 基础科学：自然科学理论原理\n\
- 工程技术：工科应用技术工艺\n\
- 计算机与AI：编程开发AI算法\n\
- 实验数据：测试报告数据集\n\
- 学术文献：论文综述研究报告\n\
- 教程讲义：课程学习指南\n\
- 工具软件：开发工具数据分析平台\n\
- 行业资讯：技术动态产业进展\n\
- 项目案例：工程落地实践\n\
- 标准规范：国标行标设计准则\n\
- 资源素材：模型图纸数据集\n\
- 行业政策：产业科研政策\n\
- 学术创作：技术博客科普文稿\n\
- 其他：无法归类时用\n\
\n\
冲突优先：教程+工具→工具软件 / 资讯动态→行业资讯 / 论文+数据→学术文献\n\
\n\
标签：5~8个关键词，含领域词+技术主题词，禁用空泛词（文章/方法/总结）\n\
\n\
标题：{title}\n\
链接：{url}\n\
内容：{content}");

        // 不走 output_schema（本地引擎如 LM Studio 不支持 strict json_schema → 输出过短/
        // 解析失败）。改为纯 prompt 要求 JSON，用本地 parse_bookmark_summary_json 兜底校验。
        // 仍用 LlmAdapter 的非流式 completion 单次调用；enable_thinking 关闭透传 extra_params。
        let mut request = LoopRequest::new(vec![LlmMessage::text(LlmRole::User, prompt.clone())]);
        request.temperature = Some(0.3);
        request.max_tokens = Some(SUMMARY_OUTPUT_TOKENS as u32);
        request.extra_params = Some(serde_json::json!({ "enable_thinking": false }));
        let request = self.apply_common_params(request);

        // 记录本次请求体（便于与 LM Studio / Postman 对照定位）
        log::info!(
            "[llm] [书签摘要] 请求: model={} title={} url={} chars_in={} prompt_len={}",
            self.model, title, url, content_chars, prompt.chars().count()
        );

        let response = self
            .completion_with_retry(request, cancel)
            .await
            .map_err(|e| format!("LLM 调用失败: {}", e))?;

        let full = response.content;
        // 记录响应全文（便于定位解析失败与真实返回）
        log::info!(
            "[llm] [书签摘要] 响应: model={} chars_out={} body={}",
            self.model, full.chars().count(), truncate_log(full.trim(), 2000)
        );

        let trimmed = full.trim();
        let parsed = if trimmed.is_empty() {
            log::warn!(
                "[llm] [书签摘要] 模型空输出: model={} chars_in={}",
                self.model, content_chars
            );
            None
        } else {
            let p = parse_bookmark_summary_json(trimmed);
            if p.is_none() {
                log::warn!(
                    "[llm] [书签摘要] 输出不可解析（非合法 JSON）: model={} chars_in={} raw_chars={} raw={}",
                    self.model, content_chars, trimmed.chars().count(), truncate_log(trimmed, 500)
                );
            }
            p
        };
        Ok(parsed)
    }
}

/// 书签摘要输出 JSON Schema（对齐 `BookmarkSummaryOut`；同时用于网关结构化输出与本地校验）。
/// 注意：category 枚举与 prompt 分类判定一致，网关侧可强约束；不用 minLength
/// （OpenAI strict 模式不支持该关键字；非空 summary 由本地解析单独校验）。
fn bookmark_summary_json_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["summary", "tags"],
        "properties": {
            "summary": { "type": "string" },
            "category": {
                "type": "string",
                "enum": [
                    "基础科学",
                    "工程技术",
                    "计算机与AI",
                    "实验数据",
                    "学术文献",
                    "教程讲义",
                    "工具软件",
                    "行业资讯",
                    "项目案例",
                    "标准规范",
                    "资源素材",
                    "行业政策",
                    "学术创作",
                    "其他"
                ]
            },
            "tags": { "type": "array", "items": { "type": "string" } }
        },
        "additionalProperties": false
    })
}

/// 从 LLM 回复解析 `BookmarkSummaryOut`：剥离 markdown 代码块后，用 JSON Schema 严格校验再反序列化。
/// 结构/字段/类型/必填不符即视为不可用（对齐 `review_text` 的服务端校验模式）。
fn parse_bookmark_summary_json(raw: &str) -> Option<BookmarkSummaryOut> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    // 剥离 ```json ... ``` 围栏（部分模型会包裹）
    let t = strip_code_fence(t);
    let validator = crate::core::validation::JsonSchemaValidator::new(bookmark_summary_json_schema()).ok()?;
    let value = validator.validate_json_text(t).ok()?;
    let out = serde_json::from_value::<BookmarkSummaryOut>(value).ok()?;
    if out.summary.trim().is_empty() {
        return None;
    }
    Some(out)
}

/// 剥离 ```json``` 代码围栏
fn strip_code_fence(s: &str) -> &str {
    let s = s.trim();
    if s.starts_with("```") {
        if let Some(rest) = s.strip_prefix("```") {
            let after_lang = match rest.find('\n') {
                Some(i) => &rest[i + 1..],
                None => rest,
            };
            return after_lang.strip_suffix("```").unwrap_or(after_lang).trim();
        }
    }
    s
}

/// 截断日志文本（防超长输出刷屏）
fn truncate_log(s: &str, max: usize) -> &str {
    if s.chars().count() > max {
        &s[..s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len())]
    } else {
        s
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
    fn retryable_llm_error_classification() {
        use crate::core::r#loop::LlmError;
        // 瞬时：连接/超时/provider 文本
        assert!(is_retryable_llm_error(&LlmError::Http("conn reset".into())));
        assert!(is_retryable_llm_error(&LlmError::Timeout));
        assert!(is_retryable_llm_error(&LlmError::Provider("rate limited".into())));
        // 状态码：429/5xx 可重试；401/400 不重试（P0-4）
        assert!(is_retryable_llm_error(&LlmError::StatusCode(429, String::new())));
        assert!(is_retryable_llm_error(&LlmError::StatusCode(503, String::new())));
        assert!(!is_retryable_llm_error(&LlmError::StatusCode(401, String::new())));
        assert!(!is_retryable_llm_error(&LlmError::StatusCode(400, String::new())));
        // 确定性：上下文溢出/业务 4xx 不重试
        assert!(!is_retryable_llm_error(&LlmError::ContextOverflow));
        assert!(!is_retryable_llm_error(&LlmError::InvalidRequest("bad".into())));
    }

    #[test]
    fn mask_secret_never_leaks_plaintext() {
        let secret = "sk-abcdef1234567890";
        let masked = mask_secret(secret);
        assert!(!masked.contains("sk-"), "掩码不得包含明文前缀");
        assert!(!masked.contains("abcdef"), "掩码不得包含明文片段");
        assert_eq!(mask_secret(secret), mask_secret(secret), "确定性（可比对配置变更）");
        assert_ne!(mask_secret("sk-other-key"), masked, "不同凭据不同掩码");
        assert_eq!(mask_secret(""), "<empty>");
    }

    #[test]
    fn bookmark_summary_schema_accepts_valid_and_rejects_invalid() {
        // 合法：summary + tags（category 可选，须在枚举内）
        let ok = parse_bookmark_summary_json(
            r#"{"summary":"摘要内容","category":"工程技术","tags":["a","b"]}"#,
        );
        assert!(ok.is_some(), "合法结构化输出应通过");
        assert_eq!(ok.unwrap().category.as_deref(), Some("工程技术"));
        // category 缺省也可（tags 允许空数组）
        assert!(parse_bookmark_summary_json(r#"{"summary":"摘要","tags":[]}"#).is_some());
        // 枚举外 category → 拒绝
        assert!(parse_bookmark_summary_json(r#"{"summary":"x","category":"AI","tags":[]}"#).is_none());
        // 缺必填 tags → 拒绝
        assert!(parse_bookmark_summary_json(r#"{"summary":"x"}"#).is_none());
        // 类型错误（tags 为字符串而非数组）→ 拒绝
        assert!(parse_bookmark_summary_json(r#"{"summary":"x","tags":"nope"}"#).is_none());
        // summary 缺失 → 拒绝
        assert!(parse_bookmark_summary_json(r#"{"tags":["a"]}"#).is_none());
        // 空输入 → 拒绝
        assert!(parse_bookmark_summary_json("").is_none());
    }
}
