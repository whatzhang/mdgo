# mdgo Agent 后端重构实施蓝图（去 rig · 仿 DSH 核心 · 事件溯源第一天地基）

> 最后更新：2026-08-23（原版本：2026-08）
> 决策依据：`docs/去 rig 自研 Agent 内核可行性评估.md`（可行性）；DSH 架构调研报告（原 `docs/deepseek-harness-architecture-report.md` 已移出 docs/，本仓库以 `core/loop` 现状为准）
> 已锁定边界：① 后端全重构、前端事件协议（`rag:*/agent:*/trace:event`）兼容零改动；② 会话存储从第一天按**事件溯源**设计；③ 蓝图 + 分期开工，每期可编译可测可回滚。
> 硬约束：**知识库业务逻辑（检索/技能/记忆/规划/审批/压缩）正确性不可破坏**——业务层代码零改动，仅换其依赖的传输/循环/会话层。
>
> **实施状态总览（2026-08-23）：本蓝图描述的 `core/loop` 自研内核已按 Phase 0-6 全部落地**（commit `7278d50`「refactor: 自研 Agent 内核替代 rig」起，现 HEAD = `edab77e`）。Cargo.toml 已无 rig 依赖（`cargo tree` 无 rig-core/rig-agent）；代码注释保留「rig 路径已移除」字样。`cargo test --lib` 321 通过。§2-§5 保留为设计契约（实现与契约逐符号吻合）；§6 为滚动实施状态，含内核落地后的后续迭代（知识画布/书签/日程/RAG P0 批次/技能内存直读等）。

---

## 1. 目标架构总览

```
┌─ 命令层（现有 IPC/事件协议不变）───────────────────────────────┐
│  commands/llm.rs（agent_query / kb_llm_query）→ 调用 core/loop │
├─ Agent 内核 core/loop/（新，替代 rig，仿 DSH 四内核）───────────┤
│  types.rs      LLM 协议无关类型（消息/内容块/流事件/错误）        │
│  llm_seam.rs   LlmAdapter trait + CompletionRequest/Response     │
│  openai.rs     OpenAI 兼容 SSE 客户端（stream/complete）         │
│  anthropic.rs  Anthropic Messages 客户端（双协议，后续迭代补充） │
│  session.rs    事件溯源会话（SessionEvent + derive_history）     │
│  hooks.rs      pre_request / on_tool_call / on_invalid_tool_call │
│                / on_request_error 四组钩子                       │
│  tool_calls.rs 并行调度器（exclusive barrier + 有界池 + 模型序） │
│  loop.rs       LoopAgent：turn/step 状态机 + 取消 + max_turns    │
│  error.rs      LoopError（ContextOverflow/MaxTurns/...）         │
├─ 工具系统（替代 DynamicTool）────────────────────────────────────┤
│  core/agent/tool.rs   Tool trait（spec/output_schema/timeout +  │
│                       concurrency_safe + execute）               │
│  core/agent/loop_tools.rs  26+ 工具迁移（闭包→Tool，已全部落地） │
├─ 业务层（零改动）────────────────────────────────────────────────┤
│  core/search|skill|memory|planner|approval|context|subagent      │
│  services/chat.rs（会话读写 → session_events 事件落库）          │
└─────────────────────────────────────────────────────────────────┘
```

依赖方向：`types.rs`（无依赖）→ `llm_seam`/`session` → `openai`/`anthropic`/`hooks`/`tool_calls` → `loop.rs` → 命令层。业务层只依赖 `core/loop` 的公开类型与 `LlmAdapter`/`Session` 两个窄接口。

---

## 2. 模块契约

### 2.1 `core/loop/types.rs`（无依赖）
LLM 协议无关的最小模型（替代 rig 的 `Message`/`AssistantContent`/`ToolCall`/`StreamedAssistantContent`）：

```rust
pub enum LlmRole { System, User, Assistant, Tool }
pub enum ContentBlock {
    Text(String),
    ToolCall { id: String, name: String, arguments: String }, // arguments=模型原始 JSON 字符串
    ToolResult { tool_call_id: String, content: String, is_error: bool },
}
pub struct LlmMessage { pub role: LlmRole, pub content: Vec<ContentBlock> }

pub struct TokenUsage {
    pub prompt_tokens: u32, pub completion_tokens: u32,
    pub cached_input_tokens: u32, pub cache_creation_input_tokens: u32,
}
pub enum FinishReason { Stop, ToolCalls, MaxTokens, Length, ContentFilter, Other }
pub enum StreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCall { index: usize, id: String, name: String, arguments: String }, // 完整调用按序
    Usage(TokenUsage),
    Finish(FinishReason),
}
pub enum LlmError {
    Http(String), StatusCode(u16, String), Sse(String), Json(String),
    Timeout, Cancelled, ContextOverflow, InvalidRequest(String), Provider(String), Other(String),
}
```

> 现状核对（2026-08-23）：`LlmError::is_retryable()` 已实现（429/408/5xx/连接/超时 → true；401/403/400/ContextOverflow/InvalidRequest → false），与 `services/llm.rs::retry_loop` 共用同一判定语义。

### 2.2 `core/loop/llm_seam.rs`（依赖 types）
```rust
pub struct CompletionRequest {
    pub messages: Vec<LlmMessage>,
    pub max_tokens: Option<u32>,
    pub reasoning_effort: Option<String>,
    pub output_schema: Option<serde_json::Value>,
    pub temperature: Option<f32>,
    pub stream: bool,
    pub extra_params: Option<serde_json::Value>, // 顶层附加字段
}
pub struct CompletionResponse { pub content: String, pub tool_calls: Vec<ToolCall>, pub usage: Option<TokenUsage>, pub finish_reason: Option<FinishReason> }

pub trait LlmAdapter: Send + Sync {
    fn model(&self) -> &str;
    async fn complete(&self, req: CompletionRequest, cancel: CancellationToken) -> Result<CompletionResponse, LlmError>;
    async fn stream(&self, req: CompletionRequest, cancel: CancellationToken)
        -> Result<futures::stream::BoxStream<'static, Result<StreamEvent, LlmError>>, LlmError>;
}
```
重试/超时/输出校验为**策略**，由调用方包装（不进入 adapter）——对齐 DSH "transport 与 policy 分离"。

> 现状核对：trait 已按此落成；`OpenAiAdapter`（core/loop/openai.rs）与 `AnthropicAdapter`（core/loop/anthropic.rs）双实现，`build_loop_adapter`（core/agent/loop_tools.rs）按 `LlmConfig.protocol` 选择。

### 2.3 `core/loop/openai.rs`（依赖 types + llm_seam）
`OpenAiAdapter` 实现 `LlmAdapter`：归一化 base_url、注入超时 reqwest、流式 SSE 解析（text/tool_calls 增量/usage/finish_reason）、非流式 JSON 解析。SSE 解析抽成纯函数供单测（参照 `anthropic.rs` 的 `find_frame_end`/`parse_data_line` 模式）。上下文溢出从 HTTP 400 + `context_length_exceeded` 识别为 `LlmError::ContextOverflow`。

> 现状核对：已落地；`core/loop/anthropic.rs` 提供 Anthropic Messages 协议实现（评估文档中 `services/anthropic.rs` 的 SSE 模式已迁入 loop 层）。

### 2.4 `core/loop/session.rs`（依赖 types，事件溯源——第一天地基）
```rust
pub enum TurnEndReason { Completed, Blocked, Aborted, Error, MaxTokens, Interrupted }
pub enum SessionEvent {
    TurnStart { turn: u32 },
    TurnEnd   { turn: u32, reason: TurnEndReason },
    StepStart { turn: u32, step: u32 },
    StepEnd   { turn: u32, step: u32 },
    UserMessage      { id: String, content: String, source: String },
    AssistantMessage { content: String, tool_calls: Vec<ToolCall>, usage: Option<TokenUsage>, interrupted: bool },
    ToolCall   { call_id: String, name: String, arguments: String },
    ToolResult { call_id: String, content: String, is_error: bool },
    CompactionSummary { summary: String, shadowed_seqs: Vec<u64> },
}
pub struct Session {
    seq: u64,
    events: Vec<PersistedEvent>, // (seq, SessionEvent)
    // 派生缓存
    derived: Vec<LlmMessage>,
}
impl Session {
    pub fn append(&mut self, ev: SessionEvent) -> u64;
    pub fn derive_history(&mut self) -> &[LlmMessage]; // 增量投影，工具单元成组（复用 chat_types::group_tool_units）
    pub fn events(&self) -> &[PersistedEvent];
}
```
持久化：`session_events` SQLite 表（`session_id, seq, event_type, payload, created_at`，主键 `(session_id, seq)` 幂等覆盖——实现时列名为 `payload`）；**已接入** `commands/llm.rs`（`upsert_session_events` / `load_session_events` / `clear_session_events`，services/chat.rs 建表）。`derive_history` 为纯逻辑可单测（配对规则：call_id 须同时出现在 `AssistantMessage.tool_calls` 与 `ToolResult`，孤儿剔除）。**"模型可见即已记录"不变式**：任何进入 `CompletionRequest.messages` 的消息必须来自 `derive_history`，由 runtime 断言。

### 2.5 `core/loop/hooks.rs`（依赖 types）
```rust
pub enum ToolDecision { Run, Skip(String), Ask }
pub struct RequestPatch { pub preamble_override: Option<String>, pub active_tools: Option<Vec<String>>, pub extra_params: Option<serde_json::Value> }
pub trait LoopHook: Send + Sync {
    fn pre_request(&self, ctx: &HookCtx, messages: &[LlmMessage]) -> RequestPatch;         // SkillInstruction/ReasoningEffort 迁移
    fn on_tool_call(&self, ctx: &HookCtx, name: &str, args: &Value) -> ToolDecision;       // SkillGate+ApprovalGate 迁移（短路序不变）
    fn on_invalid_tool_call(&self, ctx: &HookCtx, name: &str, available: &[String]) -> Option<String>; // Skip reason
    async fn on_request_error(&self, ctx: &HookCtx, err: &LoopError) -> Option<RetryAction>; // 溢出压缩重试/MaxTurns 归类
}
```

> 现状核对：`ToolDecision` 现为 `Run | Skip(String)`（`Ask` 由 on_tool_call 的异步返回值承载）；`HookCtx` 全拥有字段（turn/step/model/request_id/remaining_turns）；`RetryAction` 为 `Retry | Abort`。业务 Hook 迁移到 `core/agent/loop_hooks.rs`：`SkillInstructionHook`（pre_request）、`SkillGateHook`（on_tool_call，含重复调用熔断）、`ApprovalHook`（on_tool_call，`ApprovalGate::check` + DenialCategory 反馈）。

### 2.6 `core/loop/tool_calls.rs`（并行调度，仿 DSH tool-calls.ts）
```rust
pub struct PlannedCall { pub call: ToolCall, pub concurrency_safe: bool }
pub async fn execute_tool_calls<Tool>(
    tools: &ToolRegistry, planned: Vec<PlannedCall>, cancel: CancellationToken,
    on_call: impl Fn(&ToolCall), on_result: impl Fn(&ToolCall, &ToolResult),
) -> Vec<ToolResult>;
```
- exclusive 调用串行成 barrier；concurrency_safe 调用走有界并行池（`LoopConfig.max_parallel_tools`，默认 4）。
- **结果严格按模型序提交**；写工具（edit/write/delete/git_commit/multi_edit）一律 exclusive（副作用不可重叠）。
- 取消：未启动调用产出合成错误结果（保回放），已启动 drain 到 quiescence。

### 2.7 `core/loop/loop.rs`（turn/step 状态机）
```rust
pub struct LoopConfig { pub max_turns: usize, pub system_prompt: String, /* 预算预警阈值/最大请求重试/并行上限/max_tokens/retry_prepare */ }
pub struct LoopAgent {
    adapter: Arc<dyn LlmAdapter>, hooks: Vec<Arc<dyn LoopHook>>, tools: Arc<ToolRegistry>,
    session: Session, config: LoopConfig,
}
impl LoopAgent {
    pub async fn turn(&mut self, input: LlmMessage, cancel: CancellationToken) -> TurnOutcome;
}
```
- `turn()`：claim 输入 → `user/message` 事件 → 循环 step：组装请求（`pre_request` patch → `derive_history` + input）→ `adapter.stream()` → 消费 `StreamEvent`（TextDelta→回传、ToolCall→收集、Usage/Finish）→ 若 finish=ToolCalls → `execute_tool_calls` → `tool/result` 事件回填 → 下一 step；否则 stop/max_turns。
- 取消：`tokio::select!` 每处检查点（loop.rs:258）；`turn/end` 记 reason；`TurnOutcome` 四态（Completed/Cancelled/MaxTurns/Failed，均保留部分内容）。
- 事件协议发射（`rag:delta`/`rag:done`/`agent:tool_call`/`tool_result`/`trace:event`）由命令层从 `StreamEvent` + session 事件派生——**前端零改动**。

### 2.8 `core/loop/tool.rs`（工具契约，仿 DSH ToolDefinition）
```rust
pub struct ToolSpec {
    pub name: String, pub description: String,
    pub parameters: serde_json::Value,        // JSON Schema（模型可见）
    pub output_schema: Option<serde_json::Value>, // 永不上模型
    pub timeout_ms: Option<u64>,              // 永不上模型
    pub concurrency_safe: bool,               // 默认 false = exclusive
}
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    async fn execute(&self, args: serde_json::Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError>;
}
pub struct ToolRunContext<'a> { pub request_id: &'a str, pub call_id: &'a str, pub cancel: &'a CancellationToken, pub sink: &'a dyn ToolEventSink }
pub trait ToolRegistry: Send + Sync { fn get(&self, name: &str) -> Option<Arc<dyn Tool>>; fn names(&self) -> Vec<String>; }
pub struct HashMapToolRegistry { /* ... */ }   // register/get/names
```
现有 `record_tool_call`/`record_tool_result`/技能门禁/审批全部保留：`BusToolEventSink` 把新 loop 工具事件写入现有 `ToolCallBus`，前端 `agent:tool_call/result` 协议零改动。

---

## 3. 业务层契约（零改动清单）

| 模块 | 处理 | 理由 |
|---|---|---|
| `core/search/*` | 保留 | 检索/精排/聚簇与 LLM 调用解耦，仅被工具与命令层调用 |
| `core/skill/*` | 保留 | 激活/注入/指标；`build_rag_agent` 中的 SkillInstruction/SkillGate 逻辑迁移到 hooks（loop_hooks.rs） |
| `core/memory/*` | 保留 | 记忆检索与注入（`MemoryStore`） |
| `core/agent/planner.rs` | 保留 | `should_plan`/`parse_plan`/full plan 字段（touchpoints/non_goals/risks/rollback）；LLM 调用经 `LlmAdapter` |
| `core/approval/*` | 保留 | `ApprovalGate` 挂到 hooks.on_tool_call（ApprovalHook） |
| `core/context/*` | 保留 | 压缩器/检查点（`SummarizeThenWindowCompressor` + `CompactionState` 落库）；转换层 `group_tool_units` 复用 |
| `core/trace.rs` | 保留 | `TraceBus` 五阶段事件流 |
| `core/subagent/*` | 改 | 子代理从嵌套 rig 改为 `LoopAgent` + 白名单（`SubagentRunner::run` 接收 `Arc<dyn LlmAdapter>`） |
| `core/mcp/*`、`external_tools` | 改适配层 | `DynamicTool` → `Tool` trait（`McpTool` + `register_mcp_tools`；`ExternalHttpTool`） |
| `services/chat.rs` | 改 | 会话读写从消息表改为 session 事件（`session_events` 双写/幂等覆盖） |
| `services/llm.rs` | 改 | 非流式调用点（plan/expand/summarize/review）从 rig 换 `LlmAdapter`（`LLMClient` 承载 `Arc<dyn LlmAdapter>`） |

---

## 4. 迁移顺序（每期可编译可测可回滚）

| Phase | 内容 | 产出 | 验收 |
|---|---|---|---|
| **0** | `types.rs` + `llm_seam.rs` + `openai.rs`（SSE 客户端） | `core/loop/{types,llm_seam,openai}.rs` | cargo check + SSE 解析单测 + 真实模型联调 |
| **1** | `session.rs`（事件溯源 + derive_history） | `core/loop/session.rs` | 会话投影单测（工具配对/压缩 shadowed） |
| **2** | `hooks.rs` + `error.rs` + `loop.rs`（turn/step 状态机，顺序工具） | `core/loop/{hooks,error,loop}.rs` | 与 rig 版 feature flag 并行跑 diff；预算/取消/溢出集成测试 |
| **3** | `tool.rs` + 工具迁移 + `tool_calls.rs` 并行调度 | `core/agent/loop_tools.rs` + `core/loop/{tool,tool_calls}.rs` | 读并行/写串行；工具卡片/审批/技能门禁回归 |
| **4** | `commands/llm.rs` 改造 + session_events 落库（双写） | 命令层接 `core/loop` | 前端事件协议全兼容；fork/回放；`chat_messages` 兼容读 |
| **5** | 子代理迁移 + 移除 rig 依赖 | `core/subagent` 改 + Cargo.toml 删 rig | `cargo tree` 无 rig；全功能回归全绿 |
| **6** | 平台化（按需）：Anthropic Agent 模式、headless、eval 入 CI、精确 token | — | — |

**里程碑**：M1（Phase 0-2 后）自研 loop 与 rig 版并行 diff 对比；M2（Phase 3 后）摘 rig 依赖；M3（Phase 4 后）事件溯源上线。

> 现状核对：Phase 0-5 与 Phase 6 主体已全部落地（见 §6）；Phase 6 中 Anthropic Agent 模式已支持（经 LlmAdapter seam，Agent 模式下暂为纯对话语义，工具映射后续扩展）；headless CLI 与 eval 入 CI 未做（`core/eval` 断言/报告框架已建，真实执行器待 CLI 接入）。

---

## 5. 验收标准（每期通用）

1. `cargo check` / `cargo test --lib` 全绿；新增模块带单测。
2. 业务层模块（§3 保留清单）文件不改动（git diff 验证）。
3. 前端事件协议：`rag:status/delta/done/error`、`agent:tool_call/tool_result`、`trace:event`、`approval:*`、`plan:*`、`question:*` 字段不变（手工验收清单沿用 `docs/Agent 能力验收清单.md` 风格）。
4. Phase 2 起：同一请求 rig 版 vs 自研版输出 diff（golden 快照），收敛到零差异后切流。

---

## 6. 实施状态（滚动更新）

| Phase | 状态 | 说明 |
|---|---|---|
| 0 | ✅ 已落地 | `core/loop/{types,llm_seam,openai}.rs`：LLM 协议无关类型、`LlmAdapter` trait（stream 返回已固定的 `Pin<Box<dyn Stream>>`）、OpenAI SSE 客户端（text/tool_calls 增量/usage/finish_reason + `[DONE]` 显式收尾 + 上下文溢出识别）；`CompletionRequest` 新增 `tools: Vec<ToolSchema>`（OpenAI tools 数组） |
| 1 | ✅ 已落地 | `core/loop/session.rs`：事件溯源会话（9 类事件 + `derive_history` 增量缓存 + 工具配对/孤儿剔除） |
| 2 | ✅ 已落地 | `core/loop/{error,hooks,loop}.rs`：`LoopError`、`LoopHook` 四组钩子（`HookCtx` 全拥有字段、`on_request_error` async）、**`LoopAgent::turn()` turn/step 状态机**——system 组装（Hook patch + 预算预警 ≤3 轮）、`adapter.stream` 消费、工具执行回填、`try_recover`（on_request_error → retry_prepare 真实会话压缩 → 重发，带重试预算）、取消/max_turns/部分内容保留 |
| 3 | ✅ 已落地 | `core/loop/{tool,tool_calls}.rs`：`Tool` trait（spec/output_schema/timeout/concurrency_safe）+ `HashMapToolRegistry` + `ToolEventSink`；**并行调度器**（ordered pre Hook 裁决短路 → exclusive barrier + 有界并行（默认 4）→ 模型序提交；取消产合成结果保回放）。**26 个业务工具已全部迁移**（`core/agent/loop_tools.rs`，直接构建于新基石、零 rig）：kb_search / code_lookup / read（多路径并行）/ grep / ls / glob / write / edit / multi_edit / delete / git_* / remember / forget / search_memory / todo_write / deep_research / read_subagent_result / spawn_subagent / parallel_research / webfetch / self_review / ask_user_question / schedule / search_bookmarks / get_bookmark 等；参数解析与软门禁抽为纯函数；`BusToolEventSink` 把新 loop 工具事件写入现有 ToolCallBus → **前端 `agent:tool_call/result` 协议零改动** |
| 4 | ✅ 已落地 | `core/agent/loop_hooks.rs`：业务 Hook 迁移到 `LoopHook`（替代 rig AgentHook）：`SkillInstructionHook`（pre_request 技能约束摘要 + active_tools 窄化）、`SkillGateHook`（BASE_TOOLS + allow_extra + 重复调用熔断）、`ApprovalHook`（`ApprovalGate::check` + DenialCategory 反馈）——技能门禁/审批/无效工具自纠语义与 rig 版对齐 |
| 5 | ✅ 已落地 | **services/llm.rs 换 LlmAdapter（最大 rig 消费方）**：`LLMClient` 改为 adapter 承载（`Arc<dyn LlmAdapter>` + OpenAiAdapter）：全部非流式调用（expand_queries / generate_plan_json / summarize / review_text / summarize_bookmark）经 `adapter.complete()` + `LlmError::is_retryable` 指数退避；删除 rig CompletionError 判定链 + 3 个 rig 测试替换为 LlmError 版 |
| 5-6 | ✅ 已落地 | **摘 rig 完成**：`agent_query`/`kb_llm_query` rig 分支删除（无条件走 v3）；`commands/llm.rs` 过渡访问器/`build_mcp_agent_tools` 删除；`tools/mod.rs` 35 个 rig 工具构建器（含 activate_skill/bridge 工具等孤儿段）清理；删除 `tool_registry.rs`、`approval/hook.rs`；`external_tools.rs` 删 `build_external_tool`（保留 `load_external_tools_or_default`）；`mcp/mod.rs` 删 `build_mcp_tool`（MCP 客户端保留）；**Cargo.toml 移除 rig-core/rig-agent**（另修复 tokenizers onig feature 回归）；`cargo tree` 确认无 rig；`USE_LOOP_V2`/`kb_set_loop_v2` 开关删除（v3 唯一实现，回滚=git revert） |
| 6 | ✅ 已落地（业务工具补全） | **工具流水线全量回归**：`loop_tools.rs` 新增 9 个工具 —— `BridgeTool`（通用前端桥：pomodoro / raw-parse / open-ui，技能声明门控 + 5s 桥超时）、`ExternalHttpTool`（P2-15 配置驱动 HTTP，响应截断护栏）、`McpTool` + `register_mcp_tools`（连接中 MCP 服务器工具，`mcp_<server>_<tool>` 命名 + required 校验 + 放行集并入）、`ScheduleTool`（直接调 Rust core::schedule，13 动作 + reminder_*，从 rig 版逐字移植）、`SearchBookmarksTool`/`GetBookmarkTool`（FTS5∪向量检索 + 详情）。`commands/llm.rs`：MCP 工具注册 + `allow_extra` = 外部工具 ∪ MCP 工具；`SkillInstructionHook.mcp_tool_names` 并入可见性。**技能声明的工具名与注册表全对齐**（schedule/pomodoro/raw-parse/open-ui/search_bookmarks/get_bookmark/kb_search/code_lookup/webfetch/deep_research 等 15 个技能全部覆盖）；子代理白名单（read_only/write 集）正确排除新工具 |
| 6+ | ✅ 已落地（双协议） | **Anthropic 协议经 LlmAdapter seam 统一支持**：`core/loop/anthropic.rs`（AnthropicAdapter）+ `build_loop_adapter` 按 `LlmConfig.protocol` 选择（OpenAI 兼容 / Anthropic Messages）；纯对话通道另保留 `kb_llm_query_anthropic`（services/anthropic.rs）。Anthropic Agent 模式暂为纯对话语义（工具协议面后续扩展） |

**内核落地后的后续迭代（v3 之上，滚动记录）**：

| 批次 | 状态 | 说明 |
|---|---|---|
| 知识画布 | ✅ 已落地 | canvas 技能（`resources/skills/canvas/SKILL.md`）+ 前端 `css_js/modules/canvas.js` + 后端 `core/agent/tools/canvas.rs`（布局校验 + 10 个 benchmark 用例，`docs/canvas-benchmark-cases/`）；commit `2d84342`/`cc5cac5`/`b1ef4c7`/`c13b866` |
| 书签知识资产 | ✅ 已落地 | `core/knowledge/bookmark/{repository,search,tree,vector,importer,enrichment}.rs` + `commands/bookmark.rs` + bookmark 技能（导入/检索/管理 + 分析扫描）；commit `1da8370`/`7f0dc82`；工具 `SearchBookmarksTool`/`GetBookmarkTool`（FTS5∪向量，见 Phase 6 行） |
| 日程管理 | ✅ 已落地 | `core/schedule/{scheduler,planner,analyze,rules,lunar,sqlite,store}.rs` + `ScheduleTool`（13 动作 + reminder_*）+ schedule 技能 + `css_js/modules/schedule.js`；commit `bec5e3f`/`6fc1d4c`/`e96fea2`/`edab77e`（含技能系统与时间上下文重构） |
| RAG P0 批次 | ✅ 已落地 | token 预算分块（`core/document/text_split.rs` + `TokenBudgetValidator`）、embedding 持久缓存（`core/db/embedding_cache.rs`，内容哈希增量索引）+ 查询 embedding 缓存（`core/db/query_embedding_cache.rs`，进程内 LRU）、标签检索（`core/db/bm25.rs` 轻量实现）、证据校验（`core/evidence` + `config.evidence_check_enabled`，C2 默认关）、检索 benchmark（`src/bin/benchmark.rs`：`cargo run --bin benchmark -- --kb <dir> --queries ...`）；预检索优化器 commit `4cb5372`；配套文档 `docs/分块 Token 预算设计.md`、`docs/检索 V2 架构评估与实施记录.md`、`docs/代码审查报告-RAG全链路P0批次.md`（审查基准 edab77e，321 测试通过） |
| 技能正文内存直读 | ✅ 已落地 | `core/skill/activation.rs` 把已激活技能 SKILL.md 完整正文加载进内存注册表，`read`/技能读取走内存直读不落盘；commit `124dfb4` |
| 前端模块化 | ✅ 已落地 | 原 index.html 内联 JS 迁移到 `css_js/modules/*.js`（agent.js / chat-history.js / agent_global.js / frontend-bridge.js / canvas.js / schedule.js / skill.js / mcp.js）；Tauri 主入口 `main.html`，`index.html`/`index_cdn.html` 为浏览器版；工具历史配对单一化（`core/chat_types.rs` + `css_js/modules/chat-history.js`） |
| 其他能力 | ✅ 已落地 | open-ui 工具（`BridgeTool` open-ui 变体，commit `a4f5086`）、大纲思维导图技能（outline-mindmap）、深度调研/反思质量门/用户澄清（deep_research / self_review / ask_user_question）、后台任务状态中心与全局任务条（f8a0d99） |

模块注册：`core/mod.rs` → `pub mod r#loop;`；`core/agent/mod.rs` → `pub mod loop_tools;` + `pub mod loop_hooks;`；`commands/llm.rs` → `agent_query`/`kb_llm_query` 无条件走 `agent_generate_loop_v2`/`kb_llm_query_loop_v2`。`cargo check --lib` exit 0；全量 lib 单测通过（`cargo test --lib` **321 passed / 0 failed**，2026-08-23 实测）；验证方式：devtools 直接发起纯对话与 Agent/RAG 查询（v3 即唯一实现，无开关）。

---

## 7. 风险与对策

| 风险 | 对策 |
|---|---|
| OpenAI SSE tool_calls 增量/推理模型差异 | mock SSE 集成测试 + 真实模型回归清单；只支持 OpenAI 兼容协议（与现状一致）→ 落地后扩展 Anthropic 双协议 |
| 并行调度副作用重叠 | `concurrency_safe` 契约默认 false（exclusive）；写工具强制 exclusive；模型序提交 |
| Hook 语义漂移（技能门禁/审批/无效工具自纠） | rig 版行为写成 golden 快照；M1 diff 兜底（落地后已由 loop_hooks.rs 单测 + 手动验收覆盖） |
| 事件溯源迁移一致性 | 双写 + `chat_messages` 兼容读 + 一次性 backfill；Phase 4 独立后置（已落地：session_events 幂等覆盖 + 兼容读） |
| 子代理取消级联/事件隔离 | 复用 ToolBusGuard + 独立 request_id；子代理集成测试（已落地：`SubagentRunner` 偏置 select! 优先响应取消） |
