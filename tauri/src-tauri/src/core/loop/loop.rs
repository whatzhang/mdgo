//! LoopAgent：turn/step 状态机——对齐 DeepSeek Harness `ReactLoopAgent`（`docs/.../architecture-report.md` §2.2-2.3）。
//!
//! turn 语义：claim 输入 → `turn/start` → 0..n 个 step（每个 step = 一次模型请求 + 其工具批次）
//! → `turn/end`。step 内部：组装请求（system + 派生历史 + Hook patch + 预算预警 + 工具 schema）
//! → `LlmAdapter::stream` → 消费 `StreamEvent`（文本增量/工具调用/用量/finish）→ 若 finish=ToolCalls
//! → `execute_tool_calls`（Hook 裁决 + 并行调度）→ 结果以 `tool/result` 事件回填 → 下一 step；
//! 否则 stop/max_turns 收尾。
//!
//! 不变式（"模型可见即已记录"）：所有进入请求的消息均来自 `Session::derive_history` 投影；
//! system prompt 与工具 schema 是每轮装配的（不落会话日志，对齐 DSH）。

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Datelike;
use futures::StreamExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::error::LoopError;
use super::hooks::{HookCtx, LoopHook, RetryAction};
use super::llm_seam::{CompletionRequest, LlmAdapter, ToolSchema};
use super::session::{Session, SessionEvent, TurnEndReason};
use super::tool::{NullSink, ToolEventSink, ToolRegistry, ToolResult};
use super::tool_calls::execute_tool_calls;
use super::types::{LlmMessage, LlmRole, StreamEvent, TokenUsage, ToolCall};

#[cfg(test)]
use std::pin::Pin;

#[cfg(test)]
use super::tool::ToolError;

#[cfg(test)]
use super::types::FinishReason;

/// 循环配置。
pub struct LoopConfig {
    /// 模型调用轮次预算（1-based：第 `max_turns` 次是最后一次）
    pub max_turns: usize,
    /// 基础 system prompt（角色/规约；每轮请求前置）
    pub system_prompt: String,
    /// 剩余轮次 ≤ 阈值时注入预算提醒（默认 3）
    pub budget_warning_threshold: usize,
    /// `on_request_error` 返回 Retry 的最大重试次数（默认 1；防无限循环）
    pub max_request_retries: usize,
    /// 并行工具上限（默认 4）
    pub max_parallel_tools: usize,
    /// 最大输出 token（`None` = 不设置，由服务器/模型默认）
    pub max_tokens: Option<u32>,
    /// 请求失败重试前的准备回调（如压缩会话）。返回 `true` 表示已推进、可安全重试；
    /// `false` 表示未推进（按失败处理，防死循环）。
    pub retry_prepare: Option<Arc<dyn Fn(&mut Session) -> bool + Send + Sync>>,
}

impl LoopConfig {
    pub fn new(max_turns: usize, system_prompt: impl Into<String>) -> Self {
        Self {
            max_turns,
            system_prompt: system_prompt.into(),
            budget_warning_threshold: 3,
            max_request_retries: 1,
            max_parallel_tools: 4,
            max_tokens: None,
            retry_prepare: None,
        }
    }
}

/// 循环实时事件（命令层据此转发前端协议：rag:delta / agent:tool_call / agent:tool_result / llm:usage）。
#[derive(Debug, Clone)]
pub enum LoopEvent {
    Delta(String),
    ReasoningDelta(String),
    ToolCall { call: ToolCall },
    ToolResult { call: ToolCall, result: ToolResult },
    Usage(TokenUsage),
}

/// turn 结果。
#[derive(Debug, Clone)]
pub enum TurnOutcome {
    Completed { content: String, usage: Option<TokenUsage>, turns_used: u32 },
    Cancelled { content: String, usage: Option<TokenUsage>, turns_used: u32 },
    MaxTurns { content: String, usage: Option<TokenUsage>, turns_used: u32 },
    Failed { content: String, usage: Option<TokenUsage>, turns_used: u32, err: LoopError },
}

impl TurnOutcome {
    pub fn content(&self) -> &str {
        match self {
            TurnOutcome::Completed { content, .. }
            | TurnOutcome::Cancelled { content, .. }
            | TurnOutcome::MaxTurns { content, .. }
            | TurnOutcome::Failed { content, .. } => content,
        }
    }

    pub fn usage(&self) -> Option<&TokenUsage> {
        match self {
            TurnOutcome::Completed { usage, .. }
            | TurnOutcome::Cancelled { usage, .. }
            | TurnOutcome::MaxTurns { usage, .. }
            | TurnOutcome::Failed { usage, .. } => usage.as_ref(),
        }
    }

    pub fn turns_used(&self) -> u32 {
        match self {
            TurnOutcome::Completed { turns_used, .. }
            | TurnOutcome::Cancelled { turns_used, .. }
            | TurnOutcome::MaxTurns { turns_used, .. }
            | TurnOutcome::Failed { turns_used, .. } => *turns_used,
        }
    }
}

/// Agent 循环主体。
pub struct LoopAgent {
    adapter: Arc<dyn LlmAdapter>,
    hooks: Vec<Arc<dyn LoopHook>>,
    tools: Arc<dyn ToolRegistry>,
    sink: Arc<dyn ToolEventSink>,
    session: Session,
    config: LoopConfig,
}

impl LoopAgent {
    pub fn new(
        adapter: Arc<dyn LlmAdapter>,
        config: LoopConfig,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            adapter,
            hooks: Vec::new(),
            tools: Arc::new(super::tool::HashMapToolRegistry::new()),
            sink: Arc::new(NullSink),
            session: Session::new(session_id),
            config,
        }
    }

    pub fn add_hook(&mut self, hook: Arc<dyn LoopHook>) {
        self.hooks.push(hook);
    }

    pub fn set_tools(&mut self, tools: Arc<dyn ToolRegistry>) {
        self.tools = tools;
    }

    pub fn set_sink(&mut self, sink: Arc<dyn ToolEventSink>) {
        self.sink = sink;
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// 替换会话（命令层跨请求恢复会话用）。
    pub fn replace_session(&mut self, session: Session) {
        self.session = session;
    }

    /// 执行一个 turn。
    pub async fn turn<F>(
        &mut self,
        request_id: &str,
        input: LlmMessage,
        cancel: CancellationToken,
        on_event: &mut F,
    ) -> TurnOutcome
    where
        F: FnMut(LoopEvent),
    {
        let turn = self.session.current_turn() + 1;
        self.session.append(SessionEvent::TurnStart { turn });

        let input_text = input.plain_text();
        if !input_text.is_empty() {
            self.session.append(SessionEvent::UserMessage {
                id: format!("u_{turn}"),
                content: input_text,
                source: "user".into(),
            });
        }

        let mut content = String::new();
        let mut usage: Option<TokenUsage> = None;
        let mut steps: u32 = 0;
        let mut retries: usize = 0;

        // 一轮 turn 内的请求失败重试（对齐 DSH request-error：仅当 retry_prepare 推进时重发）
        loop {
            if cancel.is_cancelled() {
                self.finish_turn(turn, TurnEndReason::Aborted);
                return TurnOutcome::Cancelled { content, usage, turns_used: steps };
            }
            steps += 1;
            if steps as usize > self.config.max_turns {
                self.finish_turn(turn, TurnEndReason::MaxTokens);
                return TurnOutcome::MaxTurns { content, usage, turns_used: steps - 1 };
            }
            self.session.append(SessionEvent::StepStart { turn, step: steps });

            let history = self.session.derive_history();
            let remaining = self.config.max_turns.saturating_sub(steps as usize);
            let hook_ctx = HookCtx::new(
                turn,
                steps,
                self.adapter.model(),
                request_id,
                remaining,
            );

            // ── 组装请求：system = 基础规约 + Hook patch + 预算预警；工具 = 注册表（active_tools 窄化）──
            let (system, active_tools, extra_params) = self.assemble_request(&hook_ctx, &history);
            let mut req = CompletionRequest::new(Vec::new());
            req.messages.push(LlmMessage::text(LlmRole::System, system));
            req.messages.extend(history);
            req.stream = true;
            req.max_tokens = self.config.max_tokens;
            req.tools = self.assemble_tool_schemas(active_tools.as_ref());
            if let Some(ep) = extra_params {
                req.extra_params = Some(ep);
            }

            // ── 发起模型请求（初始失败走 request-error 钩子）──
            let stream = match self.adapter.stream(req, cancel.clone()).await {
                Ok(s) => s,
                Err(e) => {
                    let err = LoopError::Llm(e);
                    match self.try_recover(&hook_ctx, &err, &mut retries).await {
                        Some(true) => continue, // retry_prepare 已推进，重发
                        Some(false) | None => {
                            self.finish_turn(turn, TurnEndReason::Error);
                            return TurnOutcome::Failed {
                                content,
                                usage,
                                turns_used: steps,
                                err,
                            };
                        }
                    }
                }
            };
            // 适配器已返回固定的 Pin<Box<dyn Stream + Send>>（Unpin），可直接 next()
            let mut stream = stream;

            // ── 消费流式事件 ──
            let mut step_content = String::new();
            let mut step_tool_calls: Vec<ToolCall> = Vec::new();
            let mut step_usage: Option<TokenUsage> = None;
            let mut step_failed: Option<LoopError> = None;
            let mut cancelled = false;
            let mut finished = false;
            while !finished {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => { cancelled = true; break; }
                    item = stream.next() => {
                        match item {
                            Some(Ok(ev)) => match ev {
                                StreamEvent::TextDelta(t) => {
                                    step_content.push_str(&t);
                                    content.push_str(&t);
                                    on_event(LoopEvent::Delta(t));
                                }
                                StreamEvent::ReasoningDelta(r) => {
                                    on_event(LoopEvent::ReasoningDelta(r));
                                }
                                StreamEvent::ToolCall { call, .. } => {
                                    on_event(LoopEvent::ToolCall { call: call.clone() });
                                    step_tool_calls.push(call);
                                }
                                StreamEvent::Usage(u) => {
                                    step_usage = Some(u.clone());
                                    usage = Some(u.clone());
                                    on_event(LoopEvent::Usage(u));
                                }
                                StreamEvent::Finish(_reason) => { finished = true; }
                            },
                            Some(Err(e)) => { step_failed = Some(LoopError::Llm(e)); break; }
                            None => { finished = true; }
                        }
                    }
                }
            }

            // ── 收尾处理 ──
            if cancelled {
                self.session.append(SessionEvent::AssistantMessage {
                    content: step_content,
                    tool_calls: step_tool_calls,
                    usage: step_usage,
                    interrupted: true,
                });
                self.finish_turn(turn, TurnEndReason::Aborted);
                return TurnOutcome::Cancelled { content, usage, turns_used: steps };
            }
            if let Some(err) = step_failed {
                // 已产出内容时不重试（重放会重复工具副作用）；无产出且可恢复才走 request-error
                let retryable = step_content.is_empty() && step_tool_calls.is_empty();
                if retryable {
                    match self.try_recover(&hook_ctx, &err, &mut retries).await {
                        Some(true) => continue,
                        _ => {}
                    }
                }
                self.finish_turn(turn, TurnEndReason::Error);
                return TurnOutcome::Failed { content, usage, turns_used: steps, err };
            }

            self.session.append(SessionEvent::AssistantMessage {
                content: step_content,
                tool_calls: step_tool_calls.clone(),
                usage: step_usage,
                interrupted: false,
            });

            // ── 工具执行（若有）──
            if step_tool_calls.is_empty() {
                break; // 无工具调用 → turn 完成
            }
            for tc in &step_tool_calls {
                self.session.append(SessionEvent::ToolCall {
                    call_id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                });
            }
            let executions = execute_tool_calls(
                self.tools.as_ref(),
                step_tool_calls,
                &self.hooks,
                &hook_ctx,
                request_id,
                cancel.clone(),
                self.sink.as_ref(),
                self.config.max_parallel_tools,
            )
            .await;
            for ex in executions {
                on_event(LoopEvent::ToolResult { call: ex.call.clone(), result: ex.result.clone() });
                self.session.append(SessionEvent::ToolResult {
                    call_id: ex.result.call_id,
                    content: ex.result.content,
                    is_error: ex.result.is_error,
                });
            }
            // 工具结果回填后，模型欠下一次请求 → 继续下一 step（'steps 循环顶部做 max_turns/取消检查）
        }

        self.finish_turn(turn, TurnEndReason::Completed);
        TurnOutcome::Completed { content, usage, turns_used: steps }
    }

    /// 组装每轮 system prompt（基础规约 + Hook pre_request patch + 预算预警）。
    fn assemble_request(
        &self,
        hook_ctx: &HookCtx,
        history: &[LlmMessage],
    ) -> (String, Option<Vec<String>>, Option<Value>) {
        let mut system = self.config.system_prompt.clone();
        let mut active_tools: Option<Vec<String>> = None;
        let mut extra: Option<Value> = None;
        for h in &self.hooks {
            let patch = h.pre_request(hook_ctx, history);
            if let Some(p) = patch.preamble_override {
                if !p.trim().is_empty() {
                    system.push_str("\n\n");
                    system.push_str(&p);
                }
            }
            if patch.active_tools.is_some() {
                active_tools = patch.active_tools;
            }
            if let Some(ep) = patch.extra_params {
                match (&mut extra, ep) {
                    (Some(e), v) => {
                        if let (Some(eo), Some(vo)) = (e.as_object_mut(), v.as_object()) {
                            for (k, val) in vo {
                                eo.insert(k.clone(), val.clone());
                            }
                        }
                    }
                    (slot, v) => *slot = Some(v),
                }
            }
        }
        // 当前本地时间上下文（主流 Agent 做法：每次请求在 system prompt 最前注入
        // 日期+时间+星期+时区，作为「今天/现在/明天」等相对时间解析的唯一权威依据；
        // 会话可能很长，时间必须逐请求刷新、绝不落会话日志缓存）
        let now_local = chrono::Local::now();
        let weekday_cn = match now_local.weekday() {
            chrono::Weekday::Mon => "一",
            chrono::Weekday::Tue => "二",
            chrono::Weekday::Wed => "三",
            chrono::Weekday::Thu => "四",
            chrono::Weekday::Fri => "五",
            chrono::Weekday::Sat => "六",
            chrono::Weekday::Sun => "日",
        };
        let time_block = format!(
            "[当前时间] 本地时间 {}（星期{}，{}）。用户说“今天/现在/明天/后天/本周/本月”等相对时间时一律以此为准，禁止自行推算或猜测日期。",
            now_local.format("%Y-%m-%d %H:%M"),
            weekday_cn,
            now_local.format("%:z")
        );
        system = format!("{}\n\n{}", time_block, system);
        // 预算预警（剩余轮次不足时强制引导收敛，避免 MaxTurnsError 丢失整段回答）
        let remaining = hook_ctx.remaining_turns;
        if remaining <= self.config.budget_warning_threshold {
            system.push_str(&format!(
                "\n\n[预算提醒] 本次请求的模型调用预算为 {} 轮，当前已到最后 {} 轮。请停止调用任何工具，直接基于已有信息生成最终答案；如果信息不足，请如实说明缺口。",
                self.config.max_turns,
                remaining.max(1)
            ));
        }
        (system, active_tools, extra)
    }

    /// 组装模型可见工具 schema（active_tools 窄化；未设置 = 全部已注册）。
    fn assemble_tool_schemas(&self, active_tools: Option<&Vec<String>>) -> Vec<ToolSchema> {
        let names: Vec<String> = match active_tools {
            Some(list) => list.clone(),
            None => self.tools.names(),
        };
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            if let Some(t) = self.tools.get(&name) {
                let s = t.spec();
                out.push(ToolSchema {
                    name: s.name.clone(),
                    description: s.description.clone(),
                    parameters: s.parameters.clone(),
                });
            }
        }
        out
    }

    /// 请求失败恢复：跑 on_request_error 钩子；Retry 时调用 retry_prepare（作用于真实会话），
    /// 返回是否可重发。
    async fn try_recover(
        &mut self,
        hook_ctx: &HookCtx,
        err: &LoopError,
        retries: &mut usize,
    ) -> Option<bool> {
        if err.is_cancelled() {
            return None;
        }
        for h in &self.hooks {
            if let Some(action) = h.on_request_error(hook_ctx, err).await {
                match action {
                    RetryAction::Retry => {
                        if *retries >= self.config.max_request_retries {
                            return Some(false); // 重试预算耗尽
                        }
                        let prepared = match &self.config.retry_prepare {
                            Some(f) => f(&mut self.session),
                            None => false,
                        };
                        if prepared {
                            *retries += 1;
                            return Some(true);
                        }
                        return Some(false);
                    }
                    RetryAction::Abort => return Some(false),
                }
            }
        }
        None
    }

    fn finish_turn(&mut self, turn: u32, reason: TurnEndReason) {
        self.session.append(SessionEvent::TurnEnd { turn, reason });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::r#loop::llm_seam::CompletionResponse;
    use crate::core::r#loop::tool::{HashMapToolRegistry, Tool, ToolRunContext, ToolSpec};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// 脚本化 mock adapter：每次 stream() 弹出一条脚本（事件序列），并记录收到的请求。
    struct MockAdapter {
        scripts: Mutex<VecDeque<Vec<Result<StreamEvent, crate::core::r#loop::types::LlmError>>>>,
        requests: Mutex<Vec<CompletionRequest>>,
        model: String,
    }

    impl MockAdapter {
        fn new(scripts: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                scripts: Mutex::new(
                    scripts
                        .into_iter()
                        .map(|s| s.into_iter().map(Ok).collect())
                        .collect(),
                ),
                requests: Mutex::new(Vec::new()),
                model: "mock".into(),
            }
        }

        fn requests(&self) -> Vec<CompletionRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl LlmAdapter for MockAdapter {
        fn model(&self) -> &str {
            &self.model
        }

        async fn complete(
            &self,
            _req: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<CompletionResponse, crate::core::r#loop::types::LlmError> {
            unimplemented!()
        }

        async fn stream(
            &self,
            req: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<StreamEvent, crate::core::r#loop::types::LlmError>> + Send>>, crate::core::r#loop::types::LlmError>
        {
            self.requests.lock().unwrap().push(req);
            let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
            Ok(Box::pin(futures::stream::iter(script)))
        }
    }

    struct ReadTool;

    #[async_trait]
    impl Tool for ReadTool {
        fn spec(&self) -> &ToolSpec {
            static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
            SPEC.get_or_init(|| {
                let mut s = ToolSpec::new(
                    "read",
                    "读取文件",
                    serde_json::json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
                );
                s.concurrency_safe = true;
                s
            })
        }

        async fn execute(&self, args: Value, _ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
            let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("?");
            Ok(serde_json::json!({"content": format!("内容({path})")}))
        }
    }

    fn read_call() -> ToolCall {
        ToolCall { id: "call_1".into(), name: "read".into(), arguments: "{\"path\":\"a.md\"}".into() }
    }

    fn reg_with_read() -> Arc<dyn ToolRegistry> {
        let mut reg = HashMapToolRegistry::new();
        reg.register(Arc::new(ReadTool));
        Arc::new(reg)
    }

    #[tokio::test]
    async fn multi_step_tool_round_trip() {
        let adapter = Arc::new(MockAdapter::new(vec![
            vec![
                StreamEvent::TextDelta("先查一下".into()),
                StreamEvent::ToolCall { index: 0, call: read_call() },
                StreamEvent::Finish(FinishReason::ToolCalls),
            ],
            vec![
                StreamEvent::TextDelta("结果：内容(a.md)".into()),
                StreamEvent::Finish(FinishReason::Stop),
            ],
        ]));
        let config = LoopConfig::new(10, "你是助手");
        let mut agent = LoopAgent::new(adapter.clone(), config, "s1");
        agent.set_tools(reg_with_read());

        let mut events: Vec<LoopEvent> = Vec::new();
        let outcome = agent
            .turn("r1", LlmMessage::text(LlmRole::User, "读取 a.md"), CancellationToken::new(), &mut |e| events.push(e))
            .await;

        match &outcome {
            TurnOutcome::Completed { content, turns_used, .. } => {
                assert_eq!(content, "先查一下结果：内容(a.md)");
                assert_eq!(*turns_used, 2);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        // 两次请求：第一次带 user 消息 + 工具 schema；第二次历史含 assistant tool_call + tool 结果
        let reqs = adapter.requests();
        assert_eq!(reqs.len(), 2);
        assert!(!reqs[0].tools.is_empty());
        assert_eq!(reqs[0].tools[0].name, "read");
        // 第二次请求消息：system + user + assistant(tool_call) + tool(result)
        let roles: Vec<&str> = reqs[1].messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["system", "user", "assistant", "tool"]);
        // 事件：delta / tool_call / tool_result / delta
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                LoopEvent::Delta(_) => "delta",
                LoopEvent::ToolCall { .. } => "tool_call",
                LoopEvent::ToolResult { .. } => "tool_result",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["delta", "tool_call", "tool_result", "delta"]);
        // 会话事件可回放（turn/end completed）
        assert!(matches!(
            agent.session().events().iter().find(|e| matches!(&e.event, SessionEvent::TurnEnd { reason: TurnEndReason::Completed, .. })),
            Some(_)
        ));
    }

    #[tokio::test]
    async fn max_turns_stops_loop() {
        // 每次都要求工具 → 预算耗尽
        let adapter = Arc::new(MockAdapter::new(vec![
            vec![StreamEvent::ToolCall { index: 0, call: read_call() }, StreamEvent::Finish(FinishReason::ToolCalls)],
            vec![StreamEvent::ToolCall { index: 0, call: read_call() }, StreamEvent::Finish(FinishReason::ToolCalls)],
        ]));
        let config = LoopConfig::new(2, "你是助手");
        let mut agent = LoopAgent::new(adapter, config, "s1");
        agent.set_tools(reg_with_read());
        let outcome = agent
            .turn("r1", LlmMessage::text(LlmRole::User, "q"), CancellationToken::new(), &mut |_| {})
            .await;
        assert!(matches!(outcome, TurnOutcome::MaxTurns { .. }));
        assert_eq!(outcome.turns_used(), 2);
    }

    #[tokio::test]
    async fn cancellation_returns_cancelled() {
        let adapter = Arc::new(MockAdapter::new(vec![
            vec![StreamEvent::TextDelta("部分内容".into()), StreamEvent::Finish(FinishReason::Stop)],
        ]));
        let config = LoopConfig::new(10, "你是助手");
        let mut agent = LoopAgent::new(adapter, config, "s1");
        let cancel = CancellationToken::new();
        cancel.cancel(); // 请求前已取消
        let outcome = agent
            .turn("r1", LlmMessage::text(LlmRole::User, "q"), cancel, &mut |_| {})
            .await;
        assert!(matches!(outcome, TurnOutcome::Cancelled { .. }));
    }

    #[tokio::test]
    async fn budget_warning_injected() {
        let adapter = Arc::new(MockAdapter::new(vec![
            vec![StreamEvent::TextDelta("x".into()), StreamEvent::Finish(FinishReason::Stop)],
        ]));
        let config = LoopConfig::new(3, "你是助手");
        let mut agent = LoopAgent::new(adapter.clone(), config, "s1");
        agent.set_tools(reg_with_read());
        agent
            .turn("r1", LlmMessage::text(LlmRole::User, "q"), CancellationToken::new(), &mut |_| {})
            .await;
        let req = &adapter.requests()[0];
        let sys = req.messages[0].plain_text();
        assert!(sys.contains("预算提醒"), "system prompt 应含预算提醒: {sys}");
    }

    /// 首次请求返回上下文溢出，配合 retry_prepare（压缩成功）后重发成功的恢复路径。
    struct OverflowOnceAdapter {
        inner: Arc<MockAdapter>,
        failed_once: std::sync::atomic::AtomicBool,
    }

    impl OverflowOnceAdapter {
        fn new(inner: Arc<MockAdapter>) -> Self {
            Self { inner, failed_once: std::sync::atomic::AtomicBool::new(false) }
        }
    }

    #[async_trait::async_trait]
    impl LlmAdapter for OverflowOnceAdapter {
        fn model(&self) -> &str {
            self.inner.model()
        }

        async fn complete(
            &self,
            _req: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<CompletionResponse, crate::core::r#loop::types::LlmError> {
            unimplemented!()
        }

        async fn stream(
            &self,
            req: CompletionRequest,
            cancel: CancellationToken,
        ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<StreamEvent, crate::core::r#loop::types::LlmError>> + Send>>, crate::core::r#loop::types::LlmError>
        {
            if !self.failed_once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::core::r#loop::types::LlmError::ContextOverflow);
            }
            self.inner.stream(req, cancel).await
        }
    }

    struct RetryHook;

    #[async_trait::async_trait]
    impl LoopHook for RetryHook {
        async fn on_request_error(
            &self,
            _ctx: &HookCtx,
            _err: &LoopError,
        ) -> Option<RetryAction> {
            Some(RetryAction::Retry)
        }
    }

    #[tokio::test]
    async fn request_error_retry_with_prepare() {
        let inner = Arc::new(MockAdapter::new(vec![
            vec![StreamEvent::TextDelta("ok".into()), StreamEvent::Finish(FinishReason::Stop)],
        ]));
        let adapter = Arc::new(OverflowOnceAdapter::new(inner));
        let mut config = LoopConfig::new(10, "你是助手");
        config.retry_prepare = Some(Arc::new(|_s: &mut Session| true)); // 模拟压缩成功
        let mut agent = LoopAgent::new(adapter, config, "s1");
        agent.add_hook(Arc::new(RetryHook));
        let outcome = agent
            .turn("r1", LlmMessage::text(LlmRole::User, "q"), CancellationToken::new(), &mut |_| {})
            .await;
        assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    }

    #[tokio::test]
    async fn retry_exhausted_fails() {
        // 永远溢出 + retry_prepare 返回 false（未推进）→ 失败
        let inner = Arc::new(MockAdapter::new(vec![]));
        let adapter = Arc::new(OverflowOnceAdapter::new(inner));
        let mut config = LoopConfig::new(10, "你是助手");
        config.retry_prepare = Some(Arc::new(|_s: &mut Session| false));
        let mut agent = LoopAgent::new(adapter, config, "s1");
        agent.add_hook(Arc::new(RetryHook));
        let outcome = agent
            .turn("r1", LlmMessage::text(LlmRole::User, "q"), CancellationToken::new(), &mut |_| {})
            .await;
        assert!(matches!(outcome, TurnOutcome::Failed { .. }));
    }
}
