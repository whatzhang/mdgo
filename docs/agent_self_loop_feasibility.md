# mdgo 去 rig、自研 Agent Loop 可行性评估（仿 DeepSeek Harness 核心 + 自研知识库业务）

> 版本：2026-08 · 基线：git HEAD `874bd5f`（P1/P2 已修复：统一工具配对 `chat-history.js`/`core/chat_types.rs`、澄清提问 `question.rs`、软门禁/记忆/重试/日志安全修复）
> 目标：**移除 rig-core / rig-agent 依赖**，按 DeepSeek Harness（DSH）核心范式自研 Agent 循环，知识库业务逻辑（检索/技能/记忆/规划/审批/压缩）全部保留。
> 参考：`docs/deepseek-harness-architecture-report.md`（DSH 行号级报告）§2-§4 为自研设计的事实基准。

---

## 0. 结论摘要

**可行，且当前正是最佳时机。** 理由：

1. **rig 只提供了 4 样东西**：① OpenAI 兼容 HTTP 客户端（流式 SSE 解析）；② 消息/历史类型；③ 多轮 Agent 循环（`drive_tool_calls`：调 LLM → 执行工具 → 回填结果 → 再调 LLM）；④ 工具运行时（`DynamicTool`）。**mdgo 的全部业务逻辑（检索、技能、记忆、规划、审批、压缩、子代理白名单）都在这 4 样之外**，由 mdgo 自己的代码编排。
2. **团队已证明能脱离 rig 手写流式客户端**：`services/anthropic.rs` 是纯 reqwest + SSE 帧解析 + select! 取消的独立流式实现（anthropic.rs:149-255），与 rig 零依赖。OpenAI 协议只是同一套技能的另一个变体。
3. **类型层已解耦 90%**：`core/chat_types.rs` 已定义 `ToolCallDto` 与统一配对语义（`group_tool_units`/`paired_tool_call_ids`），注释明确"与 rig 的 ToolCall 解耦（依赖倒置）"——正是为去 rig 铺的路。
4. **真正的重活只有一件**：多轮 Agent 循环（turn/step 状态机 + 工具调度 + Hook 等价物 + 并行执行 + 取消）。其余（客户端、类型、工具迁移）是机械工作量。

**总工作量估算：单人全职约 4-6 个月，分期 6 个 Phase，每期可独立交付、可回滚。** 风险集中在两处：OpenAI 流式 tool_calls 增量解析（推理模型差异）、并行工具调度的"模型序提交 + 副作用串行化"正确性；均有成熟对策（见 §5）。

**建议**：分期做，先"最小自研 loop"（Phase 0-3，摘掉 rig 依赖），验证一个迭代后再做事件溯源会话（Phase 4）。**仿 DSH 的四个内核（LLM seam / 事件溯源 / turn-step 循环 / 工具流水线+并行调度），不仿 Cordis 插件框架与 TS 栈**——插件化对单体桌面应用是过度设计。

---

## 1. rig 依赖面盘点（当前基线，已量化）

### 1.1 按文件（`rg "use rig"` 全量 40 处）

| 文件 | rig 使用 | 用途 |
|---|---|---|
| `services/llm.rs` | rig_core openai CompletionClient/CompletionModel、CompletionRequest、CompletionError、http_client（reqwest 0.13）、OneOrMany | **LLM 客户端**：非流式补全（规划/扩展/摘要/评审）+ 重试判定 |
| `commands/llm.rs` | MultiTurnStreamItem、StreamingChat、Message、AssistantContent、ToolCall、ToolFunction、StreamedAssistantContent、StreamingError（上下文溢出判定、MaxTurnsError 匹配） | **Agent 主循环**：`agent.stream_chat()` + 消费 `MultiTurnStreamItem` 流 |
| `core/agent/mod.rs` | Agent/AgentBuilder、AgentHook（CompletionCall/Response、ToolCall、InvalidToolCall、RequestPatch）、openai::CompletionModel | **Agent 构造**：build_rag_agent/build_chat_agent + 6 类 Hook |
| `core/agent/tools/mod.rs`（29 处 DynamicTool::new） | DynamicTool、ToolContext、ToolOutput | **工具运行时**：全部 30+ 工具闭包 |
| `core/agent/tool_registry.rs`、`external_tools.rs` | DynamicTool | 工具注册表、外部 HTTP 工具 |
| `core/mcp/mod.rs` | DynamicTool | MCP 工具适配 |
| `core/approval/hook.rs` | AgentHook、HookContext、ToolCall、ToolCallAction | 审批 Hook |
| `core/subagent/mod.rs` | MultiTurnStreamItem、StreamingChat、Message、openai | 子代理（嵌套 rig Agent） |

### 1.2 rig 内部做了、mdgo 没感知的事（自研时必须自己实现）

这是评估的关键——**rig 替 mdgo 藏起了哪些复杂度**：

1. **多轮循环驱动**（rig-agent `drive_tool_calls`）：发起请求 → 解析 assistant 消息中的 tool_calls → 逐个执行工具 → 把工具结果按 OpenAI 协议格式化为 tool 消息 → **追加进 run history** → 再次发起请求。mdgo 只看到最终流，循环是 rig 的。
2. **OpenAI 流式 SSE 解析**：`delta.tool_calls` 增量（`index` + `function.name` + `function.arguments` 分片拼接）、`finish_reason`（`tool_calls`/`length`/`stop`）、`stream_options.include_usage` 用量块、推理模型的 `reasoning_content` 等——当前全部藏在 rig 里。
3. **工具结果回填协议**：工具结果必须以 `role: "tool"` + `tool_call_id` 紧跟对应 assistant(tool_calls) 消息重放；rig 处理了配对与格式。
4. **Hook 调度顺序**：`on_completion_call` → 请求 → `on_completion_response`；`on_tool_call`（执行前，Skip/Run 短路）；`on_invalid_tool_call`（未知工具）；`RequestPatch`（每轮改写 preamble/active_tools/additional_params）。
5. **max_turns 执行**：轮次耗尽抛 `MaxTurnsError`（mdgo 已显式识别，llm.rs:2112-2116）。
6. **http_client 连接池与超时**：rig 内部 reqwest（0.13 实例）注入；mdgo 用 `rig_core::http_client::ReqwestClient::builder()`。

### 1.3 关键结论

- **依赖面是"窄而深"**：文件不多（9 个），但 `MultiTurnStreamItem`/`DynamicTool`/`AgentBuilder` 三个类型横跨主循环、全部工具与子代理。
- **工具执行本身是 rig 黑盒的最后一个部分**：`commands/llm.rs` 消费流时并不知道工具在流内何时被执行、结果如何回填——自研 loop 将把这一环显式化（也正是 DSH tool-calls.ts 做的事）。

---

## 2. 可行性分析

### 2.1 有利条件（为什么现在做正合适）

| 条件 | 证据 |
|---|---|
| 手写流式客户端先例 | `services/anthropic.rs`：reqwest + `find_frame_end`/`parse_data_line`/`handle_sse_line` + select! 取消（anthropic.rs:210-318） |
| 类型层已解耦 | `core/chat_types.rs`：`ToolCallDto` + `group_tool_units` + `paired_tool_call_ids`（chat_types.rs:5-60） |
| 循环外围已就绪 | 溢出重试（llm.rs:2084-2109）、MaxTurns 区分（:2112）、防幻觉改用 ToolCallBus 成功集（:2131-2138）——这些逻辑全部在 rig 之外，自研 loop 原样搬入 |
| 工具已统一轨迹 | 全部工具经 `record_tool_call`/`record_tool_result` 写 ToolCallBus，与 rig 的 ToolOutput 只差一层薄壳 |
| 业务层不碰 rig | 检索/技能/记忆/规划/审批/压缩/子代理白名单全部只依赖 `KbSearchConfig` + 自有类型 |
| DSH 设计基准完整 | `docs/deepseek-harness-architecture-report.md` §2-§4 有逐行号的设计可映射 |

### 2.2 必须自研的 4 项（按难度排序）

| # | 能力 | 难度 | 说明 |
|---|---|---|---|
| 1 | **LLM 协议层**（OpenAI 兼容） | 中（1-2 周） | 流式 SSE（text + tool_calls 增量 + usage + finish_reason）+ 非流式 + output_schema + 指数退避（复用现有 `retry_loop`/`is_retryable_completion_error`，仅换掉 rig 的调用点） |
| 2 | **Agent 循环**（turn/step） | **高（4-6 周）** | 见 §3.2 设计；本质是把 rig 的 `drive_tool_calls` 显式化 + 仿 DSH 的 turn/step 语义 + Hook 等价物 + 并行调度 |
| 3 | **工具运行时**（Tool trait） | 中（2-3 周） | 33 个工具构造器机械迁移（见 §4 Phase 3） |
| 4 | **消息/历史类型收敛** | 低（3-5 天） | `chat_types.rs` 已有 90%；补 OpenAI 视图转换（`chat_turns_to_history` 从 rig Message 改为自有类型） |

### 2.3 难点与不确定性（诚实评估）

1. **OpenAI 流式 tool_calls 增量解析**：不同推理模型对 `delta.tool_calls` 的分片方式有差异（index 乱序、arguments 按块到达、finish_reason 缺失等）。当前 rig 版本行为不可直接移植（rig 内部实现），需对照 OpenAI 协议文档 + 用真实模型回归。→ 对策：mock SSE 服务器集成测试（现有 eval 框架可扩展）+ 只支持 OpenAI 兼容协议（与现状一致）。
2. **并行调度正确性**：DSH 的"exclusive barrier + 有界滚动池 + 模型序提交"在 tokio 下可实现，但**写工具（edit/write/delete/git_commit）必须串行**（副作用不可重叠），读工具才可并行。→ 对策：仿 DSH `isConcurrencySafe` 契约——工具声明并发安全，调度器只并行安全子集（§3.2）。
3. **Hook 语义等价**：rig 的 `on_tool_call` Skip 短路 + `InvalidToolCallAction::Skip` 回填是 mdgo 防死循环/自纠的根基，自研 loop 必须 1:1 保留（技能门禁 + 审批门 + 无效工具恢复 + 重复调用熔断）。
4. **子代理迁移**：子代理用 `build_rag_agent`（嵌套 rig），去 rig 后子代理必须跑在自研 loop 上（复用同一 Tool trait + 白名单，工作量小，但需保证取消级联与事件隔离语义不变）。

---

## 3. 目标架构（仿 DSH 核心，去 Cordis）

### 3.1 分层设计（映射 DSH 分层 → mdgo 模块）

```
┌─ 命令层（保持现有 IPC/事件协议不变，前端零改动）─────────────┐
│  commands/llm.rs（agent_query/kb_llm_query 编排）            │
├─ Agent 内核（新，替代 rig）───────────────────────────────────┤
│  core/loop/                                                    │
│   ├─ llm_seam.rs     LLM 适配器抽象（DSH ctx.llm 等价物）      │
│   │                   OpenAI 实现（自研）+ Anthropic 实现（已有）│
│   ├─ session.rs      事件溯源会话（DSH Session 等价物，Phase 4）│
│   ├─ loop.rs         turn/step 状态机（DSH ReactLoopAgent）     │
│   ├─ tool_calls.rs   并行调度器（DSH tool-calls.ts）            │
│   ├─ hooks.rs        pre-step/request/tool 三组 waterfall 等价物│
│   └─ error.rs        StreamingError 等价物（溢出/MaxTurns/...） │
├─ 工具系统（新，替代 DynamicTool）──────────────────────────────┤
│  core/agent/tool.rs  Tool trait（schema+output+execute+timeout+ │
│                       concurrency_safe+present）                │
│  core/agent/tools/*  33 个工具迁移（闭包 → 实现 Tool）          │
├─ 业务层（全部保留，零改动）────────────────────────────────────┤
│  core/search|skill|memory|planner|approval|context|subagent    │
│  services/chat.rs|llm.rs(去 rig 后仅剩业务调用)                │
└────────────────────────────────────────────────────────────────┘
```

### 3.2 四个内核的设计要点（逐条映射 DSH 报告章节）

**① LLM seam（DSH §llm-streaming / report §3.4）**
- trait `LlmAdapter { async fn stream(&self, req: LlmRequest, cancel) -> Result<StreamHandle> }`，`StreamHandle` 产出 `StreamEvent`（TextDelta / ReasoningDelta / ToolCallDelta / Usage / Finish(reason)）。
- 两个实现：`OpenAiAdapter`（自研 SSE 解析）、`AnthropicAdapter`（现有 anthropic.rs 包装）。
- **直接收益：Agent 模式支持 Anthropic**（当前硬限制，llm.rs:824-834 直接拒绝），对齐 DSH"adapter 注册表"。
- 非流式补全（规划/扩展/摘要/评审）走同一 adapter 的 `complete()`，现有 retry_loop/output_schema/校验逻辑原样搬入。

**② 事件溯源会话（DSH report §4.1，Phase 4）**
- 新增 `session_events` 表（append-only：`seq, session_id, event_type, payload_json, created_at`），最小事件子集：`turn/start,end`、`step/start,end`、`user/message`、`assistant/message`（含 usage）、`tool/call`（原始参数）、`tool/result`、`compaction/summary`。
- LLM 历史由事件派生（`derive_history(session_id, budget)`，复用现有压缩器）；`chat_messages` 保留为兼容读路径（双写迁移期）。
- 收益：原始 chunk 保真回放、任意点 fork、压缩 shadowed 语义、"模型可见即已记录"不变式。
- **与去 rig 解耦**：事件溯源是独立于循环的存储改造，可在 loop 落地后再做（两件事不互相阻塞）。

**③ turn/step 循环（DSH report §2.2-2.3）**
- `LoopAgent::turn()` 语义：claim 输入 → pre-step hooks（改写/拒绝）→ step（模型请求 + 工具批次）→ turn-stopping 检查点。
- step 内部：组装请求（preamble 由现有 SkillInstructionHook 逻辑生成）→ `adapter.stream()` → 组装 assistant 消息（含 tool_calls）→ 若 finish=tool_calls → 调度工具 → 结果以 tool 消息回填历史 → 下一 step；直到 finish=stop 或 max_turns。
- **Hook 等价物**（把 rig AgentHook 六类 1:1 迁移）：
  - `pre_request(ctx, messages) -> Patch`（LlmTrace / SkillInstruction / ReasoningEffort）
  - `on_tool_call(name, args) -> Run|Skip(reason)`（SkillGate + 重复调用熔断 + ApprovalGate，保持"先技能门禁、后审批"短路序）
  - `on_invalid_tool_call(name, available) -> Skip(reason)`（InvalidToolCallHook）
  - `on_request_error(err) -> Retry|Abort`（溢出压缩重试、MaxTurns 归类）
- 取消：`CancellationToken` + 每处检查点，工具层 `ABORTED/ABORTED_BEFORE_DISPATCH` 语义（合成结果保回放）。
- max_turns 预算 + 剩余 3 轮预警注入（现有逻辑搬入 pre_request）。

**④ 工具系统 + 并行调度（DSH report §3.3-3.4、§3.5）**
- `trait Tool { fn schema(&self) -> JsonSchema; fn output_schema(&self) -> Option<JsonSchema>; fn timeout_ms(&self) -> Option<u64>; fn concurrency_safe(&self) -> bool; async fn execute(&self, args, ctx) -> ToolResult; }`（schemars 已是依赖，输出契约对齐 DSH `output`）。
- 注册表：保留 `ToolRegistry` + `KbSearchConfig` 上下文（skill 门禁/审批/轨迹全复用）。
- 流水线（对齐 DSH 五段）：pre（技能门禁 + 审批）→ execute（timeout 包装 + 并行池）→ post（结果规范化 + record_tool_result）→ finalize（结构化输出投影）。
- **并行调度器**（对齐 DSH tool-calls.ts）：同一 assistant 消息的多个 tool_calls 按 `concurrency_safe` 分组——exclusive 串行成 barrier，parallel 走 `buffer_unordered(n)` 有界池；结果按**模型序**提交（写工具绝不并行）。
- 现有 `ToolCallBus`/`record_tool_call`/`record_tool_result` 语义不变（前端工具卡片零改动）。

### 3.3 明确"不仿"的部分（避免过度设计）

| DSH 特性 | 是否仿 | 理由 |
|---|---|---|
| Cordis 插件框架（ctx/scope/waterfall 派发） | **不仿** | 单体桌面应用不需要插件热插拔；用 Rust 模块 + trait 即可达到同等开闭性 |
| 事件溯源 Session 全词汇表 | 仿最小子集 | 只取 7 类事件（§3.2②），不仿 request/header、todo/write 等日志事件 |
| rpcId 回显 + 基线回放重连传输 | 暂不仿 | 无 headless/SDK 需求前，现有 Tauri IPC + 事件协议足够；Phase 6 做 headless 时再评估 |
| workflow/ralph/agent-team | 不仿 | 子代理扩展 + 目标体系（goal）已够用，workflow 编排脚本超出知识库工具定位 |
| 逐文件 100% 覆盖率门 | 部分仿 | 对 loop/调度器/SSE 解析设覆盖率门，业务层维持现状 |

### 3.4 保留的业务层（零改动清单）

`core/search/*`（混合检索/精排/聚簇）、`core/skill/*`（激活/注入/指标/门禁）、`core/memory/*`、`core/agent/planner.rs`、`core/approval/*`、`core/context/*`（压缩/检查点，仅转换层换类型）、`core/trace.rs`、`core/bridge/*`（WebSocket 工具桥）、`core/mcp/*`（仅工具适配层换 Tool trait）、`services/chat.rs`、`services/ai_history.rs`、全部前端（事件协议不变）。

---

## 4. 工作量与分期（每期可交付、可回滚）

| Phase | 内容 | 产出 | 工作量（单人全职） | 验收 |
|---|---|---|---|---|
| **0** | LLM 协议层：OpenAI SSE 客户端（流式+非流式+usage+output_schema）+ 适配器 trait | `core/loop/llm_seam.rs` + `services/openai.rs` | 1-2 周 | mock SSE 服务器单测；与真实模型联调（text/tool_calls/reasoning/溢出） |
| **1** | 消息类型收敛：自有 `LlmMessage`/`ToolCall`/`AssistantContent`，`chat_turns_to_history` 改输出自有类型 | `core/loop/types.rs` | 3-5 天 | 单测：工具单元分组/孤儿过滤/协议消息装配 |
| **2** | Agent 循环核心：turn/step 状态机 + 顺序工具执行 + Hook 等价物 + 取消 + max_turns + 溢出重试 | `core/loop/{loop,hooks,error}.rs` | 3-4 周 | 与 rig 版**并行跑**（feature flag 对比输出）；工具历史/预算/取消集成测试 |
| **3** | 并行调度器 + 工具契约迁移：33 个工具改 Tool trait，MCP/外部工具适配 | `core/loop/tool_calls.rs` + `core/agent/tool.rs` | 3-4 周 | 读工具并行、写工具串行；工具卡片/审批/技能门禁回归全绿 |
| **4** | 事件溯源会话（可选但推荐）：session_events 表 + derive_history + 双写迁移 | `core/loop/session.rs` + `services/chat.rs` 扩展 | 4-6 周 | fork/回放/压缩检查点集成测试；`chat_messages` 兼容读 |
| **5** | 子代理迁移 + 收尾：子代理跑自研 loop；移除 rig 依赖（Cargo.toml 删 rig-core/rig-agent） | 全量 | 1-2 周 | `cargo tree` 无 rig；全部工具/子代理/技能回归 |
| **6** | 平台化（按需）：Anthropic Agent 模式、headless CLI、eval 入 CI、精确 token 计量 | — | 2-3 周 | — |

**合计约 14-21 周（4-6 个月）**；Phase 0-3 完成即可摘掉 rig（Phase 5 收尾），Phase 4/6 可按需裁剪。

### 关键里程碑

- **M1（Phase 0-2 结束）**：自研 loop 在 feature flag 后与 rig 版并行运行，同一请求两种实现输出 diff 对比——这是风险最低的验证方式。
- **M2（Phase 3 结束）**：rig 依赖移除，`cargo tree` 干净，功能回归全绿。
- **M3（Phase 4 结束）**：事件溯源上线，fork/回放能力解锁。

---

## 5. 风险与对策

| 风险 | 等级 | 对策 |
|---|---|---|
| OpenAI 流式 tool_calls 增量解析差异（推理模型分片/ finish_reason 缺失） | 中 | 只支持 OpenAI 兼容协议（与现状一致）；mock SSE 集成测试覆盖 text/tool_calls/usage/溢出四类帧；真实模型回归清单 |
| 并行调度副作用重叠（写工具并发） | 中 | 仿 DSH `isConcurrencySafe` 契约：默认 exclusive，仅显式声明的读工具并行；结果严格按模型序提交 |
| Hook 语义漂移（技能门禁/审批/无效工具自纠丢失） | 中 | 把 rig 版 6 类 Hook 行为写成行为契约单测，自研 loop 逐条对照（M1 diff 对比兜底） |
| 子代理取消级联/事件隔离回归 | 低 | 复用现有 ToolBusGuard + 独立 request_id 语义；子代理集成测试（含父链取消） |
| 事件溯源迁移期间一致性 | 中 | 双写 + `chat_messages` 兼容读 + 一次性 backfill；Phase 4 独立于 loop 可后置 |
| 团队对 rig 内部行为的隐性依赖（如工具结果回填格式） | 中 | 先写"rig 行为快照测试"（当前输出固化为 golden 数据），自研 loop 对齐后再删 |
| 6 个月长周期投入 | 中 | 分期 + feature flag 并行运行，任何一期可暂停回滚，rig 版始终可用 |

---

## 6. 收益分析（去 rig 换来了什么）

| 收益 | 说明 |
|---|---|
| **并行工具执行** | rig 顺序执行 → 自研调度器读工具并行（对齐 DSH），多文件读/多检索耗时显著下降 |
| **Agent 模式支持 Anthropic** | 现硬限制（llm.rs:824）随 LLM seam 消除 |
| **事件溯源会话** | 原始 chunk 回放、任意点 fork、压缩 shadowed 语义、"模型可见即已记录"不变式 |
| **精确 token 计量** | 可用真实 tokenizer（tokenizers 已依赖），替换字符估算（P1-2 遗留） |
| **循环可控** | 溢出重试/MaxTurns/预算预警/注入防护从"rig 事件里捞"变为"自己的代码"；未来升级模型协议不再受 rig 版本约束 |
| **依赖面收窄** | 移除 rig-core/rig-agent（含其 reqwest 0.13 双实例问题，llm.rs:390 注释的坑消失） |
| **工具契约化** | output schema + timeout + concurrency 声明，前端结构化卡片从 git_diff 硬编码走向通用投影 |

**代价**：6 个月投入 + 上述风险；期间 rig 版与自研版并行维护的短期成本。

---

## 7. 决策建议

1. **结论：做。** 技术风险可控、业务层零改动、先例（anthropic.rs）与基础（chat_types.rs）已具备；不做的话，rig 0.41 的顺序执行、单协议、升级锁死会持续成为瓶颈。
2. **范围：仿四内核，不仿 Cordis/TS/传输细节**（§3.3）。"完全仿照 DeepSeek Harness 的核心"应理解为**架构原则**（事件溯源、turn/step、流水线、并行调度、契约化），而非代码级照搬。
3. **顺序：Phase 0→1→2 先做"最小 loop"，M1 diff 验证后再进 3；Phase 4（事件溯源）独立后置；Phase 6 按需。**
4. **护栏：** 开工前先提交当前基线（工作区当前干净）；为 rig 版写"行为快照测试"再动手；每个 Phase 结束跑 `cargo test --lib` + 手动验收清单（沿用 `docs/agent_capability_testing.md` 风格）。
5. **参考实现顺序**（自研 loop 与 DSH 报告的对应）：loop 状态机 ← report §2.2-2.3；并行调度 ← §3.4 tool-calls.ts；工具契约 ← §3.1 ToolDefinition；事件溯源 ← §4.1 Session；溢出恢复 ← §4.3 compaction-basic 的 request-error 分支。

---

## 附：自研 loop 的模块清单（建议目录）

```
tauri/src-tauri/src/core/loop/
├── mod.rs            // pub use 聚合
├── types.rs          // LlmMessage/ToolCall/ToolResult/StreamEvent（自有，Phase 1）
├── llm_seam.rs       // LlmAdapter trait + OpenAI/Anthropic 实现注册（Phase 0）
├── openai.rs         // OpenAI 兼容 SSE 客户端（Phase 0，参照 anthropic.rs 模式）
├── loop.rs           // LoopAgent：turn/step 状态机 + 输入 claim + 取消（Phase 2）
├── hooks.rs          // pre_request/on_tool_call/on_invalid_tool_call/on_request_error（Phase 2）
├── tool_calls.rs     // 并行调度器：exclusive barrier + 有界池 + 模型序提交（Phase 3）
├── session.rs        // 事件溯源 Session（Phase 4，可选）
└── error.rs          // LoopError：ContextOverflow/MaxTurns/StreamFailed（Phase 2）
```
