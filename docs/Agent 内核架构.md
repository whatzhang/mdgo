# Agent 内核架构（core/loop 自研 Loop 内核）

> 最后更新：2026-08-23

> 定位：本文是 mdgo 自研 Agent 内核 **现状架构参考**。仓库已自 commit `7278d50` 起移除 rig
> 依赖，现 HEAD=`edab77e`，`cargo test --lib` 321 通过。文中所列符号均以 `tauri/src-tauri/src/core/loop/`
> 与 `tauri/src-tauri/src/core/agent/` 实际代码为准。

---

## 1. 总览：内核是什么

`core/loop` 是替代 rig 的自研 Agent 内核（Loop），负责「模型请求 → 工具执行 → 事件溯源」的
完整循环。业务层（`core/search`、`core/skill`、`core/memory`、`core/approval`、`core/context`
与命令层 `commands/llm.rs`，规划器在 `core/agent/planner.rs`）只依赖本模块的公开窄接口
（`mod.rs` 顶部 `pub use`），协议适配、会话、工具、钩子全部内聚在本模块内。

模块分层（依赖方向自上而下单向，见 `core/loop/mod.rs` 头注释）：

```text
+--------------------------------------------------------------+
| 业务层 / 命令层（commands/llm.rs、core/subagent、core/agent） |
|  只依赖下方公开窄接口（LlmAdapter / LoopAgent / Tool / ...）  |
+--------------------------------------------------------------+
                              │
┌──────────────────────────────────────────────────────────────┐
│ core/loop（自研内核，依赖方向自上而下单向）                    │
│                                                              │
│  loop.rs    LoopAgent：turn/step 状态机（主循环）              │
│  hooks.rs   LoopHook：pre_request / on_tool_call /            │
│             on_invalid_tool_call / on_request_error           │
│  tool_calls.rs  并行调度器（exclusive barrier + 有界池）       │
│  tool.rs    ToolSpec / Tool / ToolRegistry / ToolEventSink    │
│  session.rs 事件溯源会话（SessionEvent + derive_history）      │
│  openai.rs  OpenAI 兼容 SSE 适配器（LlmAdapter 实现）          │
│  anthropic.rs Anthropic Messages 适配器（LlmAdapter 实现）     │
│  llm_seam.rs LlmAdapter 抽象 + CompletionRequest/Response      │
│  types.rs   协议无关类型（LlmMessage/StreamEvent/...）         │
│  error.rs   LoopError（MaxTurns/ContextOverflow/Cancelled）    │
└──────────────────────────────────────────────────────────────┘
```

关键设计原则（代码注释中反复强调）：

- **transport 与 policy 分离**：`LlmAdapter` 只做单次请求的传输与 SSE 解析；重试/超时/输出校验
  是策略，由调用方包装（`services/llm.rs::retry_loop`、LoopAgent 的 `try_recover`）。
- **"模型可见即已记录"**：所有进入模型请求的消息均来自 `Session::derive_history()` 投影；
  system prompt 与工具 schema 每轮现装配，不落会话日志。
- **接口隔离/依赖倒置/开闭**：工具不感知 LLM/循环/审批，只依赖 `ToolRunContext` 注入的运行信息
  与 `ToolEventSink`（轨迹/前端出口）；新增工具 = 实现 `Tool` + 注册一行。

---

## 2. LoopAgent 主循环（loop.rs）

### 2.1 配置（LoopConfig）

```rust
pub struct LoopConfig {
    pub max_turns: usize,                 // 模型调用轮次预算（1-based：第 max_turns 次是最后一次）
    pub system_prompt: String,            // 基础 system prompt（每轮请求前置）
    pub budget_warning_threshold: usize,  // 剩余轮次 ≤ 此值时注入预算提醒（默认 3）
    pub max_request_retries: usize,       // on_request_error 返回 Retry 的最大重试次数（默认 1）
    pub max_parallel_tools: usize,        // 并行工具上限（默认 4）
    pub max_tokens: Option<u32>,          // 最大输出 token（None = 服务器默认）
    pub retry_prepare: Option<Arc<dyn Fn(&mut Session) -> bool + Send + Sync>>,
    //                                    // 请求失败重试前的准备回调（如压缩会话）
}
```

- `LoopConfig::new(max_turns, system_prompt)` 给出默认值：`budget_warning_threshold=3`、
  `max_request_retries=1`、`max_parallel_tools=4`、`max_tokens=None`、`retry_prepare=None`。
- 命令层实际使用的轮次预算常量是 `core/agent/limits.rs::DEFAULT_MAX_TURNS = 20`
  （对齐 Claude Code / Codex 的多步工具任务预算；第 21 次请求触发 MaxTurns）。

### 2.2 turn / step 语义

```
turn = claim 输入 → turn/start → 0..n 个 step → turn/end
step = 一次模型请求 + 该请求产出的工具批次
```

`LoopAgent::turn(request_id, input, cancel, on_event)` 执行流程（loop.rs）：

1. `turn = session.current_turn() + 1`，append `SessionEvent::TurnStart`；非空用户文本 append
   `SessionEvent::UserMessage`。
2. 进入 step 循环（每轮循环顶部做两个检查）：
   - `cancel.is_cancelled()` → 记 `TurnEndReason::Aborted`，返回 `TurnOutcome::Cancelled`；
   - `steps > config.max_turns` → 记 `TurnEndReason::MaxTokens`，返回 `TurnOutcome::MaxTurns`
     （`turns_used = steps - 1`）。
3. append `SessionEvent::StepStart`；`history = session.derive_history()`；
   构造 `HookCtx`（turn / step / model / request_id / remaining_turns）。
4. **组装请求**（`assemble_request` + `assemble_tool_schemas`）：
   - system = `[当前时间]` 块（本地时间 + 星期 + 时区，逐请求刷新、不落会话缓存）
     + 基础规约 + 各 Hook `pre_request` 的 `preamble_override` 拼接 + 预算预警
     （`remaining <= budget_warning_threshold` 时强制引导"停止调用工具、直接生成最终答案"）；
   - `active_tools` 由 Hook patch 窄化（`None` = 注册表全部工具）；
   - `CompletionRequest`：`messages = [System] + history`，`stream=true`，`max_tokens`，
     `tools = assemble_tool_schemas(active_tools)`，`extra_params` 合并。
5. `adapter.stream(req, cancel.clone())`：初始失败走 `try_recover`（见 §2.4）。
6. **消费流式事件**（`tokio::select! { biased; cancel / stream.next() }`，取消优先）：
   - `StreamEvent::TextDelta` → 追加 step_content 与总 content，回调 `LoopEvent::Delta`；
   - `ReasoningDelta` → 回调 `LoopEvent::ReasoningDelta`（不占回答文本）；
   - `ToolCall` → 回调 `LoopEvent::ToolCall`，暂存到 `step_tool_calls`；
   - `Usage` → 更新 step_usage/usage，回调 `LoopEvent::Usage`；
   - `Finish(_)` → `finished = true`；流 `Err` → `step_failed`；`None` → 结束。
7. **收尾处理**：
   - 取消 → append `AssistantMessage{interrupted: true}` + `TurnEnd(Aborted)` → `Cancelled`；
   - 流错误：**已产出内容时不重试**（重放会重复工具副作用），仅当 `step_content` 与
     `step_tool_calls` 均为空才走 `try_recover`；否则 `TurnEnd(Error)` → `Failed`；
   - 否则 append `AssistantMessage{content, tool_calls, usage, interrupted: false}`。
8. **工具执行**：`step_tool_calls` 为空 → `break`（turn 完成）；非空 → 逐个 append
   `SessionEvent::ToolCall`，调 `execute_tool_calls(tools, calls, hooks, hook_ctx, request_id,
   cancel, sink, max_parallel_tools)`（§4.2），每个结果回调 `LoopEvent::ToolResult` 并 append
   `SessionEvent::ToolResult`；然后回到步骤 2 开始下一个 step。
9. 正常退出 → append `TurnEnd(Completed)`，返回 `TurnOutcome::Completed`。

### 2.3 终止条件与取消语义

| 条件 | 结果 | 说明 |
|---|---|---|
| 模型在某 step 不再请求工具 | `Completed` | 无工具调用即 turn 完成 |
| `steps > max_turns` | `MaxTurns` | 轮次预算耗尽；预算预警 Hook 提前引导收敛 |
| `cancel.is_cancelled()` | `Cancelled` | 保留已产出内容；`TurnEndReason::Aborted` |
| 请求/流错误不可恢复 | `Failed` | 携带 `LoopError`；已产出内容时保留部分内容 |

- 取消令牌统一为 `tokio_util::sync::CancellationToken`：turn 内 `biased select!` 优先响应取消；
  流式读取、`adapter.stream`/`complete` 发送、工具执行（`run_one` 内 `tokio::select!`）全部感知。
- `LoopEvent` 枚举（命令层据此转发前端协议：`rag:delta` / `agent:tool_call` /
  `agent:tool_result` / `llm:usage`）：`Delta` / `ReasoningDelta` / `ToolCall` / `ToolResult` / `Usage`。
- `TurnOutcome` 四种：`Completed` / `Cancelled` / `MaxTurns` / `Failed`，均携带
  `content`、`usage`、`turns_used`（`Failed` 另有 `err: LoopError`）。

### 2.4 请求失败恢复（try_recover）

```text
adapter.stream 失败 / 流式 Err（且无产出）→ 依次跑各 Hook 的 on_request_error(ctx, err)
  ├─ RetryAction::Retry → 检查 *retries >= config.max_request_retries（默认 1）→
  │    调 config.retry_prepare(&mut session)（如压缩会话）→ 返回 true 表示已推进、可安全重发；
  │    false（未推进）按失败处理，防死循环
  ├─ RetryAction::Abort → 按失败处理
  └─ None（无 Hook 表态）→ 按失败处理
```

`LlmError::is_retryable()`（types.rs）只对瞬时错误返回 true：
`Http` / `Timeout` / `Provider`，以及状态码 `429` / `408` / `500..=599`；
`ContextOverflow`、`InvalidRequest`（确定性 4xx）不重试。

### 2.5 现网接入（commands/llm.rs v3 路径）

`core/loop/mod.rs` 头注释"尚未被命令层引用"为分期落地期（Phase 0-3）的旧说明，**目前已过时**。
现状：

- `commands/llm.rs::kb_llm_query_loop_v2`：纯对话路径（无工具），`LoopAgent::new` + `replace_session`
  （历史经压缩后由 `seed_session_from_messages` 播种）→ `turn()`。
- `commands/llm.rs::agent_generate_loop_v2`：Agent/RAG 生成路径——`build_loop_tool_registry` 注册表 +
  `register_skill_tools` + `register_mcp_tools` + 业务 Hook（`SkillGateHook` → `ApprovalHook` →
  `SkillInstructionHook`，挂载顺序即此）+ `BusToolEventSink` → `LoopAgent::turn()`。
- `core/subagent/mod.rs`：子代理执行同样走 `LoopAgent` + 白名单过滤注册表（`filter_registry`）。
- 两处路径在 turn 结束后均把 `agent.session().events()` 经
  `ChatStore::upsert_session_events` 落库（§6.2）。

---

## 3. LoopHook 钩子体系（hooks.rs + core/agent/loop_hooks.rs）

`LoopHook` trait 共 **四组钩子方法**，全部带默认实现（开闭原则：新增 Hook 只需实现关心的方法）：

```rust
#[async_trait]
pub trait LoopHook: Send + Sync {
    /// ① 每轮模型请求前（组装请求体后、发送前）：改写 preamble / 窄化可见工具 / 附加参数
    fn pre_request(&self, _ctx: &HookCtx, _messages: &[LlmMessage]) -> RequestPatch;

    /// ② 工具执行前（短路序由 loop 保证：任一返回 Skip 即停止后续判断）；async 支持审批门
    async fn on_tool_call(&self, _ctx: &HookCtx, _name: &str, _args: &Value) -> ToolDecision;

    /// ③ 模型调用了不存在的工具（恢复自纠）
    fn on_invalid_tool_call(&self, _ctx: &HookCtx, _name: &str, _available: &[String]) -> Option<String>;

    /// ④ 模型请求失败（如上下文溢出）时决定重试或中止
    async fn on_request_error(&self, _ctx: &HookCtx, _err: &LoopError) -> Option<RetryAction>;
}
```

配套类型：

- `HookCtx { turn, step, model, request_id, remaining_turns }`——只读请求信息，全拥有字段，
  Hook 与 loop 互不耦合。
- `RequestPatch { preamble_override, active_tools, extra_params }`——`pre_request` 的产物。
- `ToolDecision::Run | Skip(String)`——对齐 rig `ToolCallAction`；Skip 会以错误形式回填给模型自纠。
- `RetryAction::Retry | Abort`。

业务 Hook 实现（`core/agent/loop_hooks.rs`，替代 rig `AgentHook`）：

| Hook | 钩子 | 职责 |
|---|---|---|
| `SkillInstructionHook` | `pre_request` | 每轮注入已激活技能约束摘要（≤800 字符）；`active_tools` 窄化为 BASE_TOOLS ∪ 软门禁可见 ∪ 外部工具 ∪ MCP ∪ 已激活技能声明工具 |
| `SkillGateHook` | `on_tool_call` | 防重复调用熔断（同请求内连续相同 (工具,参数) ≥2 次后第 3 次起 Skip）；BASE_TOOLS / `allow_extra` / 技能声明 → Run，否则 Skip 引导 |
| `ApprovalHook` | `on_tool_call` | `ApprovalGate::check` 审批门；拒绝按 `DenialCategory` 生成分类反馈文案（见 §4.4） |

挂载顺序（与 rig 一致）：先技能门禁、后审批——避免对"本就不该调用的工具"弹窗打扰用户。

---

## 4. Tool 体系

### 4.1 契约（tool.rs）

```rust
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,          // JSON Schema（模型可见 parameters）
    pub output_schema: Option<Value>, // 输出契约（永不上模型）
    pub timeout_ms: Option<u64>,    // 单次执行超时（永不上模型）
    pub concurrency_safe: bool,     // 默认 false = exclusive 串行
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError>;
}

pub trait ToolRegistry: Send + Sync { fn get(&self, name: &str) -> Option<Arc<dyn Tool>>; fn names(&self) -> Vec<String>; }
pub struct HashMapToolRegistry { /* name → Arc<dyn Tool> */ }  // 实现 ToolRegistry + register()

pub struct ToolRunContext<'a> { pub request_id: &'a str, pub call_id: &'a str, pub cancel: &'a CancellationToken, pub sink: &'a dyn ToolEventSink }
pub trait ToolEventSink: Send + Sync { fn on_call(...); fn on_result(...); }   // NullSink = 默认空实现
```

- `concurrency_safe` 语义（对齐 DSH `isConcurrencySafe`）：**只有显式声明 true 才可并行**，
  缺省/异常一律按 exclusive 串行（写工具副作用不可重叠）。
- `ToolError`：`NotFound` / `InvalidArgs` / `Failed` / `Timeout{timeout_ms}` / `Cancelled` / `Internal`。
- `ToolResult { call_id, content, is_error }`：回填模型历史；`is_error=true` 时 content 为错误信息。

### 4.2 并行调度器（tool_calls.rs）

`execute_tool_calls(...)` 两阶段：

```text
Phase 1  ordered pre（Hook 裁决，模型序短路）：
         对每个调用按模型序依次跑各 Hook 的 on_tool_call；任一返回 Skip 即短路记录
Phase 2a Skip 调用立即回填（错误结果："（被拦截）{reason}"，不执行）
Phase 2b concurrent execute：
         通过裁决的调用按 concurrency_safe 分组——exclusive 单独成组串行；
         concurrency_safe 调用走 chunks(max_parallel) 有界滚动池 + join_all 并行；
         结果严格按模型序提交（out[i] 按下标填充，顺序天然保持）
```

- 默认并行上限 `DEFAULT_MAX_PARALLEL = 4`；LoopAgent 经 `config.max_parallel_tools` 传入。
- 取消：取消后未启动的调用产出 `ToolError::Cancelled` 结果（保回放完整），已启动的 drain 到
  quiescence（工具尊重 `ctx.cancel`）。
- 工具未注册 → 结果 "工具不存在: {name}"（is_error=true），不致命。

### 4.3 业务工具注册表（core/agent/loop_tools.rs）

`build_loop_tool_registry(cfg)` 返回 `HashMapToolRegistry`，按类注册（写操作均为 exclusive）：

| 分组 | 工具（`concurrency_safe`） |
|---|---|
| 只读检索/文件 | `kb_search` `code_lookup` `read` `grep` `ls` `glob`（均 true） |
| 写/文件/Git | `write` `edit` `multi_edit` `delete` `git_commit` `git_checkout`（false）；`git_status` `git_diff`（true，只读） |
| 长期记忆 + 任务清单 | `remember` `forget`（false，写）；`search_memory` `todo_write`（true） |
| 子代理 + 网络 | `deep_research` `read_subagent_result` `parallel_research` `webfetch`（true）；`spawn_subagent`（false） |
| 反思质量门 + 用户澄清 | `self_review`（true）；`ask_user_question`（false，等待用户回答期间独占） |
| 日程 + 书签 | `schedule`（false，独占）；`search_bookmarks` `get_bookmark`（true） |
| 技能激活 | `activate_skill` `deactivate_skill`（`register_skill_tools` 单独注册；子代理白名单不含） |
| 前端桥接 | `pomodoro` `raw-parse` `open-ui`（`BridgeTool`，技能声明软门禁） |
| 外部 HTTP | 配置驱动 `ExternalHttpTool`（`register_external_tools`） |
| MCP | `McpTool`（`register_mcp_tools`，按需挂载） |

要点：

- **BridgeTool（Rust 内置工具桥）**：协议与 rig 版完全一致——软门禁（技能声明）→ 动作解析
  （缺省回退默认动作）→ 轨迹事件 → `core::bridge::request`（`frontend_bridge:request` 事件，
  5s 桥超时兜底）→ 结果回填。前端注册同名 handler 监听。
- **McpTool**：注册名规范化 `mcp_<server>_<tool>`（下划线，兼容 OpenAI function name 约束），
  闭包内按原始 server/tool 名调 `McpRegistry::call_tool`；`register_mcp_tools` 只挂载
  `STATUS_CONNECTED` 的服务器，返回注册名列表供 Hook 可见性/放行集补齐；参数先经
  `core::mcp::validate_args` 校验；输出经 `MCP_MAX_OUTPUT_CHARS = 60_000` 截断护栏。
- **工具可见性语义**（core/agent/mod.rs）：
  - `BASE_TOOLS`：25 个常驻基础工具（`activate_skill`/`deactivate_skill`/`read`/`ls`/`glob`/
    `grep`/`write`/`edit`/`multi_edit`/`delete`/`git_*`/`webfetch`/`deep_research`/
    `read_subagent_result`/`remember`/`forget`/`search_memory`/`todo_write`/`spawn_subagent`/
    `parallel_research`/`self_review`/`ask_user_question`），不随技能白名单窄化；
  - `SKILL_GATED_VISIBLE_TOOLS`：`kb_search`/`code_lookup`/`schedule`/`pomodoro`/`raw-parse`/
    `open-ui`/`search_bookmarks`/`get_bookmark`——软门禁可见可调，未激活技能时由
    `SkillGateHook` Skip + 工具闭包守卫返回引导，不产生 UnknownToolCall 致命错误；
  - Canvas 是知识文件格式（非工具），读写走通用 `read`/`write`，不列入本清单。
- **子代理白名单**：`filter_registry(full, whitelist)` 按白名单过滤注册表——白名单外工具不注册，
  模型不可见不可调（只读/写型子代理各自的白名单集合）。

### 4.4 工具审批（core/approval + ApprovalHook）

- **ApprovalGate**（approval/mod.rs）：组合多个 `ApprovalPolicy` + 一个 `ApprovalTransport`
  + 会话内已决缓存（key = `(run_id, tool, canonical_args)`，上限 256 条超限清空）。
  检查顺序：策略级 `allow` 短路放行 → 策略级 `deny` 直接拒绝（不弹窗）→ 首个 `evaluate`
  命中者决定是否弹窗 → 缓存命中复用 → 走通道请求用户决定（超时/通道异常默认拒绝，fail-closed）。
- **策略**（approval/policy.rs）：
  - `DestructiveWritePolicy`：`edit`/`delete`/`write`/`multi_edit`/`git_commit`/`git_checkout`、
    `mcp_*` 通配（外部服务器可执行任意逻辑，默认需确认）、`open-ui` 的 `open_file` 动作需审批；
  - `ConfigApprovalPolicy`（P2-19 配置驱动）：`approval.yaml`（`%APPDATA%/com.mdgo/approval.yaml`，
    `load_approval_rules` 读取；不存在 → 空集）。规则形如 `- tool: edit / action: allow|ask|deny`，
    `tool: "*"` 通配；按表顺序首个匹配者生效；`allow` 覆盖默认策略、`deny` 直接禁止、`ask` 走确认。
- **通道**（approval/transport.rs）：`IpcApprovalTransport`——`app.emit("approval:request")` 弹确认框，
  前端 `invoke("approval_respond", {requestId, approved, reason})` 回传，经共享挂起表
  `PendingApprovals`（oneshot）等待；前端未监听 → 超时兜底（`DenialCategory::Timeout`）。
- **拒绝类别**（`DenialCategory`）：`UserRejected` / `ChannelUnavailable` / `Timeout` /
  `PolicyDenied`；`ApprovalHook` 的 `skip_message` 按类别生成差异化模型反馈，明确"审批由系统
  弹窗处理，不在对话中进行"，消除模型文本式确认。
- **ask_user_question 工具**（loop_tools.rs）：信息不足时向用户澄清——oneshot 挂起表
  `AppState.user_question_pending` + `question:request` 事件 → 前端弹窗 → `question_respond` IPC
  回传；超时（`limits::ASK_USER_TIMEOUT_SECS = 120`s）与父链取消均视为"未回答"，返回引导让模型
  改用已有信息作答或如实说明缺口；等待期间独占执行（`concurrency_safe=false`）。

---

## 5. LlmAdapter 双协议（llm_seam / openai / anthropic）

### 5.1 seam 抽象（llm_seam.rs）

```rust
pub trait LlmAdapter: Send + Sync {
    fn model(&self) -> &str;
    async fn complete(&self, req: CompletionRequest, cancel: CancellationToken)
        -> Result<CompletionResponse, LlmError>;               // 非流式（规划/扩展/摘要/评审用）
    async fn stream(&self, req: CompletionRequest, cancel: CancellationToken)
        -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, LlmError>> + Send>>, LlmError>;
}
```

- `CompletionRequest { messages, tools: Vec<ToolSchema>, max_tokens, reasoning_effort,
  output_schema, temperature, stream, extra_params }`。
- `CompletionResponse { content, tool_calls, usage, finish_reason }`。
- 协议无关类型（types.rs）：`LlmMessage { role: LlmRole, content: Vec<ContentBlock> }`
  （`LlmRole::System/User/Assistant/Tool`；`ContentBlock::Text/ToolCall/ToolResult`）、
  `StreamEvent { TextDelta / ReasoningDelta / ToolCall{index,call} / Usage / Finish }`、
  `TokenUsage { prompt/completion/total_tokens, cached_input_tokens, cache_creation_input_tokens }`、
  `ToolCall { id, name, arguments }`、`FinishReason { Stop/ToolCalls/Length/ContentFilter/Other }`。
- `ToolSchema { name, description, parameters }` = 模型可见工具定义（对齐 OpenAI `tools` 数组）。

### 5.2 OpenAI 兼容适配器（openai.rs）

- 构造：`OpenAiAdapter::new(endpoint, model, api_key, reasoning_effort)`；
  `normalize_base_url` 剥离 `/chat/completions` 后缀、保留 `/v1` 前缀；`with_timeout` 可自定义
  非流式超时（默认 `DEFAULT_TIMEOUT = 600s`）。
- **超时三档**（常量在 openai.rs）：
  - `CONNECT_TIMEOUT = 15s`：连接阶段快速失败；
  - `STREAM_IDLE_TIMEOUT = 600s`：流式**空闲看门狗**——无数据块持续 600s 判定死流并中止；
    每收到一个数据块重新计时（慢速持续吐字的流不会被误杀）；
  - `STREAM_TOTAL_TIMEOUT = 1800s`：流式请求总超时兜底（reqwest `.timeout()`，覆盖连接+发送+读体）；
    30 分钟兜底死流。
- 请求体（`build_body`）：`model`/`messages`/`stream`/`max_tokens`/`temperature`/
  `reasoning_effort`（请求级优先，回退适配器默认）；`output_schema` → `response_format:
  {"type":"json_schema","json_schema":{...strict:true}}`；`tools` → OpenAI `{type:"function",
  function:{name,description,parameters}}`；流式注入 `stream_options:{"include_usage":true}`；
  `extra_params` 合并进顶层。
- 消息映射 `to_openai_message`：`role=tool` 消息 → `{role, content, tool_call_id}`；
  assistant 带 `tool_calls` 时 content 可为 null/""，`tool_calls` 独立数组。
- **SSE 解析**：`SseParser` 为纯状态机（可单测）——`find_frame_end`（`\n\n` 或 `\r\n\r\n`）、
  `parse_data_line`（多行 data: 拼接，`[DONE]` 原样返回）；`delta.content` → TextDelta、
  `delta.reasoning_content` → ReasoningDelta、`delta.tool_calls` 按 index 装配
  （`PartialToolCall`：id/name/arguments 分片累积）；`finish_reason="tool_calls"` 时先按模型序
  抛装配完成的 ToolCall 再抛 `Finish(ToolCalls)`；usage 块（含收尾块）→ `StreamEvent::Usage`；
  `[DONE]`/字节流结束补发收尾 `Finish`（`finish()`）。
- **上下文溢出识别**（`map_http_error`）：HTTP 400 且响应体含
  `context_length_exceeded` / `maximum context length` / `context window` → `LlmError::ContextOverflow`；
  其余非 2xx → `StatusCode(code, body截断2000)`。
- 流经 `futures::stream::unfold` 实现，源头固定为 `Pin<Box<Unfold>>`（Unpin，可直接 `next()`）。

### 5.3 Anthropic 适配器（anthropic.rs + services/anthropic.rs）

- `AnthropicAdapter::new(base_url, api_key, model, max_tokens, thinking_budget)`，包装
  `services::anthropic::AnthropicStreamClient`（Chat 最小协议面：SSE text_delta + usage + 取消传播）。
- `stream()`：`split_system`（Anthropic 的 system 是**顶层字段**，消息仅 user/assistant）→
  `tokio::spawn` 中经 mpsc 通道（容量 64）把回调事件（`AnthropicEvent::Delta/Usage`）转为
  `StreamEvent` 流，结束后发 `Finish(Stop)`；失败发 `Err(LlmError::Provider)`。
- `complete()`：走流式收集（现有客户端仅流式；规划/摘要等调用文本量小，可接受）。
- **限制（代码明示）**：现有客户端不含工具编排（tool_use/tool_result 块）——Agent 路径使用
  Anthropic 时模型不可见工具（等同纯对话）；工具协议映射后续扩展。
- 选择入口：`core/agent/loop_tools::build_loop_adapter(llm_cfg)`——`protocol == "anthropic"` 用
  AnthropicAdapter（`thinking_budget` 暂不映射），否则 OpenAI 兼容。

### 5.4 非流式调用重试（services/llm.rs，供 LLMClient 用）

```rust
const LLM_RETRY_MAX: usize = 5;      // 最大重试次数（总尝试 = 重试次数 + 1）
const LLM_RETRY_BASE_MS: u64 = 2000; // 退避起始延迟（毫秒），此后每次翻倍
const LLM_RETRY_MAX_MS: u64 = 120_000; // 退避延迟上限（毫秒）
```

- `retry_loop`：可重试错误退避 `base_delay * 2^attempt`（封顶 `max_delay`）后重试；不可重试 /
  达到上限 / 已取消 → 立即返回；退避期间监听 `cancel`（取消优先于重试）。
- 只对瞬时错误重试（`is_retryable_llm_error` = `LlmError::is_retryable`）。
- 与 LoopAgent 内部重试（§2.4，on_request_error + `max_request_retries` 默认 1 + retry_prepare）
  是**两条独立的重试路径**：LLMClient 的非流式规划/扩展/摘要/评审走指数退避；主循环的请求失败
  走 Hook 裁决。

---

## 6. 事件溯源 Session（session.rs + services/chat.rs）

### 6.1 内存态（core/loop/session.rs）

- 会话是**仅追加**的事件日志（`Vec<PersistedEvent>`，seq 单调递增）；LLM 历史由
  `derive_history()` **派生**（增量缓存，`dirty` 时失效），从不单独存储。
- `SessionEvent` 最小词汇表：`TurnStart` / `TurnEnd{reason}` / `StepStart` / `StepEnd` /
  `UserMessage{id,content,source}` / `AssistantMessage{content,tool_calls,usage,interrupted}` /
  `ToolCall{call_id,name,arguments}` / `ToolResult{call_id,content,is_error}` /
  `CompactionSummary{summary,shadowed_seqs}`；`TurnEndReason`：`Completed` / `Blocked` /
  `Aborted` / `Error` / `MaxTokens` / `Interrupted`。
- `derive_history` 配对规则（对齐 OpenAI 协议与 `core::chat_types::group_tool_units`）：
  一个 `call_id` 必须**同时**出现在 `AssistantMessage.tool_calls` 与 `ToolResult` 中才重放——
  未配对的工具调用（assistant 侧）与孤儿 tool 结果（tool 侧）均剔除，避免 OpenAI 协议因
  tool_call 无配对结果拒绝请求；空 assistant 消息（无文本且无有效工具调用）不进历史；
  turn/step/compaction 等非模型可见事件不进历史。
- `Session::append` 维护 `current_turn/current_step`（供 loop 用）；`derived_chars()` 为历史长度
  （token 计量占位，Phase 6 接真实 tokenizer）。

### 6.2 SQLite 持久化（services/chat.rs `session_events` 表）

```sql
-- v3 事件溯源会话日志（append-only；seq 单调，按 (session_id, seq) 幂等覆盖）
CREATE TABLE IF NOT EXISTS session_events (
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (session_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_session_events_session ON session_events(session_id);
```

- `upsert_session_events(session_id, events)`：**全量删除后插入**（单事务原子）——前端按上下文
  预算裁剪历史后重发时，事件日志 = 本次请求实际发送/产生的窗口（裁剪历史 + 新轮），无残留旧事件。
  `event_type` = `SessionEvent::type_name()`（判别名索引），`payload` 为无损 JSON。
- `load_session_events(session_id)`：按 seq 升序反序列化（回放/未来迁移）。
- `clear_session_events(session_id)`：清空（`chat_session_clear_messages` 配套）。
- 写入时机：`commands/llm.rs` 在 `turn()` 结束后把 `agent.session().events()` 全量 upsert
  （chat 与 RAG 两条 v3 路径均如此）。
- **会话恢复**：跨请求时命令层用历史消息 + 压缩检查点重建会话——`seed_session_from_messages`
  （`ChatMessage` → `SessionEvent` 播种）+ `LoopAgent::replace_session`；
  `load_compaction_checkpoint` / `apply_compaction_checkpoint` 先应用摘要 + cutoff 增量，
  再按上下文预算压缩（`prepare_history`，token 预算门 → 摘要+滑窗 / 纯滑窗）后播种。
- 现状注记（B4 决策）：前端 UI 数据源仍是 legacy `chat_messages` 表；`session_events` 事件日志
  作为「本请求窗口审计 + 未来迁移数据」。

---

## 7. 与旧 rig 内核的差异要点

rig 依赖已自 commit `7278d50` 移除（现 HEAD=`edab77e`），代码中仅注释保留对照说明。对照表：

| 维度 | 旧 rig 内核（已移除） | 自研 core/loop 内核 |
|---|---|---|
| 循环 | rig `Agent` + `stream_chat` | `LoopAgent::turn`（turn/step 状态机） |
| 消息类型 | rig `Message` / `AssistantContent` / `ToolCall` | `LlmMessage` + `ContentBlock` + `ToolCall`（types.rs） |
| 流式 | rig `StreamedAssistantContent` | `StreamEvent` 增量词汇（TextDelta/ReasoningDelta/ToolCall/Usage/Finish） |
| 工具 | rig `DynamicTool` | `Tool` trait + `ToolSpec` + `ToolRegistry`（接口隔离） |
| 工具过滤 | rig `active_tools` 硬过滤（UnknownToolCall 致命） | 软门禁：工具始终可见可调，`SkillGateHook` Skip + 闭包守卫引导 |
| Hook | rig `AgentHook`（六类） | `LoopHook` 四组（pre_request / on_tool_call / on_invalid_tool_call / on_request_error） |
| 审批 | 文本式确认 | 系统弹窗（`approval:request` + `approval_respond`），分类反馈文案 |
| 错误 | rig `StreamingError` | `LoopError`（MaxTurns/ContextOverflow/Cancelled/Tool/Internal/Llm） |
| provider | rig openai provider | `LlmAdapter` seam（OpenAI 兼容 + Anthropic 双实现） |
| 会话 | 内存消息列表 | 事件溯源 `Session`（append + derive_history），SQLite `session_events` 落库 |
| 前端工具轨迹 | 经 ToolCallBus | `BusToolEventSink` → ToolCallBus，前端事件协议零改动 |

---

## 8. 核心模块依赖图（文本形式）

```text
                    commands/llm.rs（v3：kb_llm_query_loop_v2 / agent_generate_loop_v2）
                    core/subagent/mod.rs          core/agent（BASE_TOOLS / KbSearchConfig / planner / limits）
                          │                                    │
                          ▼                                    ▼
                    ┌──────────────────────────────────────────────────────┐
                    │  LoopAgent（loop.rs）                                 │
                    │  · turn/step 状态机                                   │
                    │  · assemble_request（system/active_tools/预算预警）    │
                    │  · try_recover（on_request_error + retry_prepare）    │
                    └──────┬───────────────┬───────────────┬───────────────┘
                           │               │               │
              ┌────────────▼───┐   ┌───────▼────────┐   ┌──▼───────────────┐
              │ LlmAdapter     │   │ LoopHook       │   │ ToolRegistry      │
              │（llm_seam）     │   │（hooks.rs）     │   │（tool.rs）        │
              └───┬────────┬───┘   └───────┬────────┘   └───┬──────────┬───┘
                  │        │               │               │          │
        ┌─────────▼──┐  ┌──▼─────────┐  ┌──▼────────────┐ ┌─▼─────────┐┌▼──────────┐
        │ openai.rs  │  │ anthropic  │  │ 业务 Hook      │ │ tool_calls││ 业务工具    │
        │ SSE 解析 + │  │ .rs 包装    │  │（loop_hooks）  │ │ 并行调度器 ││（loop_tools│
        │ 空闲看门狗 │  │ services/   │  │ SkillGate/    │ │（exclusive││ .rs ~30+） │
        │ 600s/1800s │  │ anthropic   │  │ Approval/     │ │ barrier + ││ Bridge/    │
        │            │  │             │  │ SkillInstruct │ │ 有界池）  ││ MCP/外部   │
        └────────────┘  └─────────────┘  └───────────────┘ └───────────┘└───────────┘
                           │                    │                 │           │
                           ▼                    ▼                 ▼           ▼
                    ┌──────────────────────────────────────────────────────────┐
                    │  Session（session.rs）事件溯源：append / derive_history     │
                    │    ↓ 持久化：services/chat.rs session_events 表（SQLite）   │
                    │  types.rs（LlmMessage/StreamEvent/...） · error.rs（LoopError）│
                    └──────────────────────────────────────────────────────────┘
```

---

## 9. 关键文件清单

| 文件 | 职责 |
|---|---|
| `core/loop/mod.rs` | 模块注册 + 公开窄接口聚合导出 |
| `core/loop/types.rs` | 协议无关消息/内容块/流事件/错误类型 |
| `core/loop/llm_seam.rs` | `LlmAdapter` 抽象 + `CompletionRequest/Response` + `ToolSchema` |
| `core/loop/openai.rs` | OpenAI 兼容 SSE 适配器（SseParser 纯状态机、空闲看门狗、溢出识别） |
| `core/loop/anthropic.rs` | Anthropic Messages 适配器（包装 services/anthropic，无工具协议面） |
| `core/loop/session.rs` | 事件溯源会话（SessionEvent + derive_history 配对投影） |
| `core/loop/tool.rs` | ToolSpec / Tool / ToolRegistry / ToolEventSink / ToolError |
| `core/loop/tool_calls.rs` | 并行调度器（ordered pre + exclusive barrier + 有界池 + 模型序提交） |
| `core/loop/hooks.rs` | LoopHook 四组钩子 + HookCtx / RequestPatch / ToolDecision / RetryAction |
| `core/loop/error.rs` | LoopError（Llm / MaxTurns / Tool / Cancelled / Internal） |
| `core/loop/loop.rs` | LoopAgent turn/step 状态机（主循环） |
| `core/agent/mod.rs` | BASE_TOOLS / SKILL_GATED_VISIBLE_TOOLS / kb_search / code_search / KbSearchConfig |
| `core/agent/limits.rs` | 指标参数集中配置（DEFAULT_MAX_TURNS=20 / MAX_CONTEXT_CHARS=12000 等） |
| `core/agent/planner.rs` | 轻量任务规划器（should_plan 规则路由 + Plan 结构化解析，单模型版） |
| `core/agent/loop_tools.rs` | 业务工具新内核实现（含 build_loop_tool_registry / BridgeTool / McpTool / build_loop_adapter / filter_registry / BusToolEventSink） |
| `core/agent/loop_hooks.rs` | 业务 Hook 新内核实现（SkillInstructionHook / SkillGateHook / ApprovalHook） |
| `core/agent/tools/mod.rs` | 业务助手（read/write/edit/grep/ls/glob/git/记忆/子代理等）+ 工具调用轨迹总线 |
| `core/agent/tools/canvas.rs` | Canvas 知识画布格式校验管线（详见《知识画布.md》） |
| `core/agent/external_tools.rs` | 外部 HTTP 工具定义加载 + mtime 缓存 |
| `core/approval/{mod,policy,transport}.rs` | 审批门（策略/通道/缓存；approval.yaml 配置） |
| `commands/llm.rs` | v3 命令层：纯对话与 Agent/RAG 两条 LoopAgent 路径 + 事件落库 |
| `services/llm.rs` | LLMClient（非流式 + retry_loop 指数退避）与 LoopRequest/LoopResponse |
| `services/chat.rs` | ChatStore：session_events 表 upsert/load/clear |
| `core/subagent/mod.rs` | 子代理执行（LoopAgent + filter_registry 白名单） |
