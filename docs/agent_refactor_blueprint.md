# mdgo Agent 后端重构实施蓝图（去 rig · 仿 DSH 核心 · 事件溯源第一天地基）

> 版本：2026-08 · 决策依据：`docs/agent_self_loop_feasibility.md`（可行性）；`docs/deepseek-harness-architecture-report.md`（DSH 行号级基准）
> 已锁定边界：① 后端全重构、前端事件协议（`rag:*/agent:*/trace:event`）兼容零改动；② 会话存储从第一天按**事件溯源**设计；③ 蓝图 + 分期开工，每期可编译可测可回滚。
> 硬约束：**知识库业务逻辑（检索/技能/记忆/规划/审批/压缩）正确性不可破坏**——业务层代码零改动，仅换其依赖的传输/循环/会话层。

---

## 1. 目标架构总览

```
┌─ 命令层（现有 IPC/事件协议不变）───────────────────────────────┐
│  commands/llm.rs（agent_query / kb_llm_query）→ 改为调用 core/loop│
├─ Agent 内核 core/loop/（新，替代 rig，仿 DSH 四内核）───────────┤
│  types.rs      LLM 协议无关类型（消息/内容块/流事件/错误）        │
│  llm_seam.rs   LlmAdapter trait + CompletionRequest/Response     │
│  openai.rs     OpenAI 兼容 SSE 客户端（stream/complete）         │
│  session.rs    事件溯源会话（SessionEvent + derive_history）     │
│  hooks.rs      pre_request / on_tool_call / on_invalid_tool_call │
│                / on_request_error 四组钩子                       │
│  tool_calls.rs 并行调度器（exclusive barrier + 有界池 + 模型序） │
│  loop.rs       LoopAgent：turn/step 状态机 + 取消 + max_turns    │
│  error.rs      LoopError（ContextOverflow/MaxTurns/...）         │
├─ 工具系统（替代 DynamicTool）────────────────────────────────────┤
│  core/agent/tool.rs   Tool trait（schema+output+timeout+         │
│                       concurrency_safe+present）                 │
│  core/agent/tools/*   33 工具迁移（闭包→Tool）                   │
├─ 业务层（零改动）────────────────────────────────────────────────┤
│  core/search|skill|memory|planner|approval|context|subagent      │
│  services/chat.rs（会话读写 → 改接 session.rs 事件）             │
└─────────────────────────────────────────────────────────────────┘
```

依赖方向：`types.rs`（无依赖）→ `llm_seam`/`session` → `openai`/`hooks`/`tool_calls` → `loop.rs` → 命令层。业务层只依赖 `core/loop` 的公开类型与 `LlmAdapter`/`Session` 两个窄接口。

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

### 2.3 `core/loop/openai.rs`（依赖 types + llm_seam）
`OpenAiAdapter` 实现 `LlmAdapter`：归一化 base_url、注入超时 reqwest、流式 SSE 解析（text/tool_calls 增量/usage/finish_reason）、非流式 JSON 解析。SSE 解析抽成纯函数供单测（参照 `anthropic.rs` 的 `find_frame_end`/`parse_data_line` 模式）。上下文溢出从 HTTP 400 + `context_length_exceeded` 识别为 `LlmError::ContextOverflow`。

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
持久化：`session_events` SQLite 表（`seq, session_id, event_type, payload_json, created_at`，Phase 3 接入）；`derive_history` 为纯逻辑可单测。**"模型可见即已记录"不变式**：任何进入 `CompletionRequest.messages` 的消息必须来自 `derive_history`，由 runtime 断言。

### 2.5 `core/loop/hooks.rs`（依赖 types）
```rust
pub enum ToolDecision { Run, Skip(String), Ask }
pub struct RequestPatch { pub preamble_override: Option<String>, pub active_tools: Option<Vec<String>>, pub extra_params: Option<serde_json::Value> }
pub trait LoopHook: Send + Sync {
    fn pre_request(&self, ctx: &HookCtx, messages: &[LlmMessage]) -> RequestPatch;         // LlmTrace/SkillInstruction/ReasoningEffort 迁移
    fn on_tool_call(&self, ctx: &HookCtx, name: &str, args: &Value) -> ToolDecision;       // SkillGate+LoopGuard+ApprovalGate 迁移（短路序不变）
    fn on_invalid_tool_call(&self, ctx: &HookCtx, name: &str, available: &[String]) -> Option<String>; // Skip reason
    async fn on_request_error(&self, ctx: &HookCtx, err: &LoopError) -> Option<RetryAction>; // 溢出压缩重试/MaxTurns 归类
}
```

### 2.6 `core/loop/tool_calls.rs`（并行调度，仿 DSH tool-calls.ts）
```rust
pub struct PlannedCall { pub call: ToolCall, pub concurrency_safe: bool }
pub async fn execute_tool_calls<Tool>(
    tools: &ToolRegistry, planned: Vec<PlannedCall>, cancel: CancellationToken,
    on_call: impl Fn(&ToolCall), on_result: impl Fn(&ToolCall, &ToolResult),
) -> Vec<ToolResult>;
```
- exclusive 调用串行成 barrier；concurrency_safe 调用走 `buffer_unordered(n)` 有界池。
- **结果严格按模型序提交**；写工具（edit/write/delete/git_commit/multi_edit）一律 exclusive（副作用不可重叠）。
- 取消：未启动调用产出合成错误结果（保回放），已启动 drain 到 quiescence。

### 2.7 `core/loop/loop.rs`（turn/step 状态机）
```rust
pub struct LoopConfig { pub max_turns: usize, pub model: String, pub system_prompt: String }
pub struct LoopAgent {
    adapter: Arc<dyn LlmAdapter>, hooks: Vec<Arc<dyn LoopHook>>, tools: Arc<ToolRegistry>,
    session: Session, config: LoopConfig,
}
impl LoopAgent {
    pub async fn turn(&mut self, input: LlmMessage, cancel: CancellationToken) -> TurnOutcome;
}
```
- `turn()`：claim 输入 → `user/message` 事件 → 循环 step：组装请求（`pre_request` patch → `derive_history` + input）→ `adapter.stream()` → 消费 `StreamEvent`（TextDelta→回传、ToolCall→收集、Usage/Finish）→ 若 finish=ToolCalls → `execute_tool_calls` → `tool/result` 事件回填 → 下一 step；否则 stop/max_turns。
- 取消：`select!` 每处检查点；`turn/end` 记 reason。
- 事件协议发射（`rag:delta`/`rag:done`/`agent:tool_call`/`tool_result`/`trace:event`）由命令层从 `StreamEvent` + session 事件派生——**前端零改动**。

### 2.8 `core/agent/tool.rs`（工具契约，仿 DSH ToolDefinition）
```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;              // JSON Schema
    fn output_schema(&self) -> Option<serde_json::Value>;
    fn timeout_ms(&self) -> Option<u64>;
    fn concurrency_safe(&self) -> bool { false }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolRunContext) -> Result<Value, ToolError>;
}
pub struct ToolRunContext { /* KbSearchConfig 等价物：skill_state/approval/trace/bus/cancel */ }
```
现有 `record_tool_call`/`record_tool_result`/`ToolCallBus`/技能门禁/审批全部保留在 `ToolRunContext` 内部——工具闭包只需改为实现 `execute`，逻辑原样搬入。

---

## 3. 业务层契约（零改动清单）

| 模块 | 处理 | 理由 |
|---|---|---|
| `core/search/*` | 保留 | 检索/精排/聚簇与 LLM 调用解耦，仅被工具与命令层调用 |
| `core/skill/*` | 保留 | 激活/注入/指标；`build_rag_agent` 中的 SkillInstruction/SkillGate 逻辑迁移到 hooks |
| `core/memory/*` | 保留 | 记忆检索与注入 |
| `core/agent/planner.rs` | 保留 | `should_plan`/`parse_plan`；仅把 `generate_plan_json` 的 LLM 调用从 rig 换 adapter |
| `core/approval/*` | 保留 | `ApprovalGate` 挂到 hooks.on_tool_call |
| `core/context/*` | 保留 | 压缩器/检查点；转换层 `group_tool_units` 复用 |
| `core/trace.rs` | 保留 | 五阶段事件流 |
| `core/subagent/*` | 改 | 子代理从嵌套 rig 改为 `LoopAgent` + 白名单（逻辑保留） |
| `core/mcp/*`、`external_tools` | 改适配层 | `DynamicTool` → `Tool` trait |
| `services/chat.rs` | 改 | 会话读写从消息表改为 session 事件（Phase 3 双写迁移） |
| `services/llm.rs` | 改 | 非流式调用点（plan/expand/summarize/review）从 rig 换 `LlmAdapter` |

---

## 4. 迁移顺序（每期可编译可测可回滚）

| Phase | 内容 | 产出 | 验收 |
|---|---|---|---|
| **0** | `types.rs` + `llm_seam.rs` + `openai.rs`（SSE 客户端） | `core/loop/{types,llm_seam,openai}.rs` | cargo check + SSE 解析单测 + 真实模型联调 |
| **1** | `session.rs`（事件溯源 + derive_history） | `core/loop/session.rs` | 会话投影单测（工具配对/压缩 shadowed） |
| **2** | `hooks.rs` + `error.rs` + `loop.rs`（turn/step 状态机，顺序工具） | `core/loop/{hooks,error,loop}.rs` | 与 rig 版 feature flag 并行跑 diff；预算/取消/溢出集成测试 |
| **3** | `tool.rs` + 33 工具迁移 + `tool_calls.rs` 并行调度 | `core/agent/tool.rs` + 工具迁移 | 读并行/写串行；工具卡片/审批/技能门禁回归 |
| **4** | `commands/llm.rs` 改造 + session_events 落库（双写） | 命令层接 `core/loop` | 前端事件协议全兼容；fork/回放；`chat_messages` 兼容读 |
| **5** | 子代理迁移 + 移除 rig 依赖 | `core/subagent` 改 + Cargo.toml 删 rig | `cargo tree` 无 rig；全功能回归全绿 |
| **6** | 平台化（按需）：Anthropic Agent 模式、headless、eval 入 CI、精确 token | — | — |

**里程碑**：M1（Phase 0-2 后）自研 loop 与 rig 版并行 diff 对比；M2（Phase 3 后）摘 rig 依赖；M3（Phase 4 后）事件溯源上线。

---

## 5. 验收标准（每期通用）

1. `cargo check` / `cargo test --lib` 全绿；新增模块带单测。
2. 业务层模块（§3 保留清单）文件不改动（git diff 验证）。
3. 前端事件协议：`rag:status/delta/done/error`、`agent:tool_call/tool_result`、`trace:event`、`approval:*`、`plan:*`、`question:*` 字段不变（手工验收清单沿用 `docs/agent_capability_testing.md` 风格）。
4. Phase 2 起：同一请求 rig 版 vs 自研版输出 diff（golden 快照），收敛到零差异后切流。

---

## 6. 实施状态（滚动更新）

| Phase | 状态 | 说明 |
|---|---|---|
| 0 | ✅ 已落地 | `core/loop/{types,llm_seam,openai}.rs`：LLM 协议无关类型、`LlmAdapter` trait（stream 返回已固定的 `Pin<Box<dyn Stream>>`）、OpenAI SSE 客户端（text/tool_calls 增量/usage/finish_reason + `[DONE]` 显式收尾 + 上下文溢出识别）；`CompletionRequest` 新增 `tools: Vec<ToolSchema>`（OpenAI tools 数组） |
| 1 | ✅ 已落地 | `core/loop/session.rs`：事件溯源会话（7 类事件 + `derive_history` 增量缓存 + 工具配对/孤儿剔除） |
| 2 | ✅ 已落地 | `core/loop/{error,hooks,loop}.rs`：`LoopError`、`LoopHook` 四组钩子（`HookCtx` 全拥有字段、`on_request_error` async）、**`LoopAgent::turn()` turn/step 状态机**——system 组装（Hook patch + 预算预警 ≤3 轮）、`adapter.stream` 消费、工具执行回填、`try_recover`（on_request_error → retry_prepare 真实会话压缩 → 重发，带重试预算）、取消/max_turns/部分内容保留 |
| 3 | ✅ 已落地 | `core/loop/{tool,tool_calls}.rs`：`Tool` trait（spec/output_schema/timeout/concurrency_safe）+ `HashMapToolRegistry` + `ToolEventSink`；**并行调度器**（ordered pre Hook 裁决短路 → exclusive barrier + `chunks(max_parallel)` 有界并行 → 模型序提交；取消产合成结果保回放）。**26 个业务工具已全部迁移**（`core/agent/loop_tools.rs`，直接构建于新基石、零 rig）：kb_search / code_lookup / read（多路径并行）/ grep / ls / glob / write / edit / multi_edit / delete / git_* / remember / forget / search_memory / todo_write / deep_research / read_subagent_result / spawn_subagent / parallel_research / webfetch / activate_skill / deactivate_skill / self_review 等；参数解析与软门禁抽为纯函数；`BusToolEventSink` 把新 loop 工具事件写入现有 ToolCallBus → **前端 `agent:tool_call/result` 协议零改动** |
| 4 | ✅ 已落地 | `core/agent/loop_hooks.rs`：业务 Hook 迁移到 `LoopHook`（替代 rig AgentHook）：`SkillInstructionHook`（pre_request 技能约束摘要 + active_tools 窄化）、`SkillGateHook`（BASE_TOOLS + allow_extra + 重复调用熔断）、`ApprovalHook`（`ApprovalGate::check` + DenialCategory 反馈）——技能门禁/审批/无效工具自纠语义与 rig 版对齐 |
| 5 | ✅ 已落地 | **services/llm.rs 换 LlmAdapter（最大 rig 消费方）**：`LLMClient` 改为 adapter 承载（`Arc<dyn LlmAdapter>` + OpenAiAdapter）：全部非流式调用（expand_queries / generate_plan_json / summarize / review_text / summarize_bookmark）经 `adapter.complete()` + `LlmError::is_retryable` 指数退避；删除 rig CompletionError 判定链 + 3 个 rig 测试替换为 LlmError 版 |
| 5-6 | ✅ 已落地 | **摘 rig 完成**：`agent_query`/`kb_llm_query` rig 分支删除（无条件走 v3）；`commands/llm.rs` 过渡访问器/`build_mcp_agent_tools` 删除；`tools/mod.rs` 35 个 rig 工具构建器（含 activate_skill/bridge 工具等孤儿段）清理；删除 `tool_registry.rs`、`approval/hook.rs`；`external_tools.rs` 删 `build_external_tool`（保留 `load_external_tools_or_default`）；`mcp/mod.rs` 删 `build_mcp_tool`（MCP 客户端保留）；**Cargo.toml 移除 rig-core/rig-agent**（另修复 tokenizers onig feature 回归）；`cargo tree` 确认无 rig；`USE_LOOP_V2`/`kb_set_loop_v2` 开关删除（v3 唯一实现，回滚=git revert） |
| 6 | ✅ 已落地（业务工具补全） | **工具流水线全量回归**：`loop_tools.rs` 新增 9 个工具 —— `BridgeTool`（通用前端桥：pomodoro / raw-parse / open-ui，技能声明门控 + 5s 桥超时）、`ExternalHttpTool`（P2-15 配置驱动 HTTP，响应截断护栏）、`McpTool` + `register_mcp_tools`（连接中 MCP 服务器工具，`mcp_<server>_<tool>` 命名 + required 校验 + 放行集并入）、`ScheduleTool`（直接调 Rust core::schedule，13 动作 + reminder_*，从 rig 版逐字移植）、`SearchBookmarksTool`/`GetBookmarkTool`（FTS5∪向量检索 + 详情）。`commands/llm.rs`：MCP 工具注册 + `allow_extra` = 外部工具 ∪ MCP 工具；`SkillInstructionHook.mcp_tool_names` 并入可见性。**技能声明的工具名与注册表全对齐**（schedule/pomodoro/raw-parse/open-ui/search_bookmarks/get_bookmark/kb_search/code_lookup/webfetch/deep_research 等 15 个技能全部覆盖）；子代理白名单（read_only/write 集）正确排除新工具 |

模块注册：`core/mod.rs` → `pub mod r#loop;`；`core/agent/mod.rs` → `pub mod loop_tools;` + `pub mod loop_hooks;`；`commands/llm.rs` → `agent_query`/`kb_llm_query` 无条件走 `agent_generate_loop_v2`/`kb_llm_query_loop_v2`。`cargo check --lib` exit 0；全量 lib 单测通过（见下节测试结果）；验证方式：devtools 直接发起纯对话与 Agent/RAG 查询（v3 即唯一实现，无开关）。

---

## 7. 风险与对策

| 风险 | 对策 |
|---|---|
| OpenAI SSE tool_calls 增量/推理模型差异 | mock SSE 集成测试 + 真实模型回归清单；只支持 OpenAI 兼容协议（与现状一致） |
| 并行调度副作用重叠 | `concurrency_safe` 契约默认 false（exclusive）；写工具强制 exclusive；模型序提交 |
| Hook 语义漂移（技能门禁/审批/无效工具自纠） | rig 版行为写成 golden 快照；M1 diff 兜底 |
| 事件溯源迁移一致性 | 双写 + `chat_messages` 兼容读 + 一次性 backfill；Phase 4 独立后置 |
| 子代理取消级联/事件隔离 | 复用 ToolBusGuard + 独立 request_id；子代理集成测试 |
