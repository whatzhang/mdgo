# mdgo 去 rig、自研 Agent Loop 可行性评估（仿 DeepSeek Harness 核心 + 自研知识库业务）

> 最后更新：2026-08-23（原版本：2026-08）
> 基线：git HEAD `874bd5f`（评估当时；P1/P2 已修复：统一工具配对 `chat-history.js`/`core/chat_types.rs`、澄清提问 `question.rs`、软门禁/记忆/重试/日志安全修复）
> 目标（原文）：**移除 rig-core / rig-agent 依赖**，按 DeepSeek Harness（DSH）核心范式自研 Agent 循环，知识库业务逻辑（检索/技能/记忆/规划/审批/压缩）全部保留。
> 参考：DSH 架构调研报告（原 `docs/deepseek-harness-architecture-report.md` 已移出 docs/；以下称「DSH 设计基准」）§2-§4 为自研设计的事实基准。
>
> **落地状态（2026-08-23，重要）**：本评估为**决策蓝图**，提议的架构已 **1:1 落地**——commit `7278d50`（「refactor: 自研 Agent 内核替代 rig」）起，现 HEAD = `edab77e`。`core/loop` 内核（11 个源文件）、`session_events` 事件溯源表、`derive_history` 增量投影、并行工具调度器、`Tool` trait 契约、双协议 `LlmAdapter`（OpenAI/Anthropic）均已实现；Cargo.toml 已无 rig 依赖。§1 的 rig 依赖盘点为**历史快照**；§4 各 Phase 已勾选完成项。`cargo test --lib` 现为 **321 passed / 0 failed**。

---

## 0. 结论摘要

**可行，且当前正是最佳时机。** 理由：

1. **rig 只提供了 4 样东西**：① OpenAI 兼容 HTTP 客户端（流式 SSE 解析）；② 消息/历史类型；③ 多轮 Agent 循环（`drive_tool_calls`：调 LLM → 执行工具 → 回填结果 → 再调 LLM）；④ 工具运行时（`DynamicTool`）。**mdgo 的全部业务逻辑（检索、技能、记忆、规划、审批、压缩、子代理白名单）都在这 4 样之外**，由 mdgo 自己的代码编排。
2. **团队已证明能脱离 rig 手写流式客户端**：`services/anthropic.rs` 是纯 reqwest + SSE 帧解析 + select! 取消的独立流式实现（anthropic.rs:149-255），与 rig 零依赖。OpenAI 协议只是同一套技能的另一个变体。
3. **类型层已解耦 90%**：`core/chat_types.rs` 已定义 `ToolCallDto` 与统一配对语义（`group_tool_units`/`paired_tool_call_ids`），注释明确"与 rig 的 ToolCall 解耦（依赖倒置）"——正是为去 rig 铺的路。
4. **真正的重活只有一件**：多轮 Agent 循环（turn/step 状态机 + 工具调度 + Hook 等价物 + 并行执行 + 取消）。其余（客户端、类型、工具迁移）是机械工作量。

**总工作量估算（评估原文）**：单人全职约 4-6 个月，分期 6 个 Phase，每期可独立交付、可回滚。风险集中在两处：OpenAI 流式 tool_calls 增量解析（推理模型差异）、并行工具调度的"模型序提交 + 副作用串行化"正确性；均有成熟对策（见 §5）。

**建议（评估原文）**：分期做，先"最小自研 loop"（Phase 0-3，摘掉 rig 依赖），验证一个迭代后再做事件溯源会话（Phase 4）。**仿 DSH 的四个内核（LLM seam / 事件溯源 / turn-step 循环 / 工具流水线+并行调度），不仿 Cordis 插件框架与 TS 栈**——插件化对单体桌面应用是过度设计。

> **落地回顾（2026-08-23）**：按此路线执行完毕——Phase 0-3 摘 rig（7278d50），Phase 4 事件溯源与 Phase 6 部分（Anthropic Agent 模式）随后落地；"不仿 Cordis/TS/传输细节"的判断被验证正确。

---

## 1. rig 依赖面盘点（当前基线，已量化）——**历史快照（评估时基线 `874bd5f`；现均已移除）**

> ⚠ 本节为评估当时的盘点快照，仅用于记录决策依据；自 commit `7278d50` 起，下表全部 rig 使用点已由 `core/loop` 自研实现替代，Cargo.toml 已无 rig-core/rig-agent。

### 1.1 按文件（`rg "use rig"` 全量 40 处，评估时）

| 文件 | rig 使用 | 用途 | v3 替代 |
|---|---|---|---|
| `services/llm.rs` | rig_core openai CompletionClient/CompletionModel、CompletionRequest、CompletionError、http_client（reqwest 0.13）、OneOrMany | **LLM 客户端**：非流式补全（规划/扩展/摘要/评审）+ 重试判定 | `core/loop/openai.rs` `OpenAiAdapter` + `services/llm.rs` `retry_loop` |
| `commands/llm.rs` | MultiTurnStreamItem、StreamingChat、Message、AssistantContent、ToolCall、ToolFunction、StreamedAssistantContent、StreamingError | **Agent 主循环**：`agent.stream_chat()` + 消费流 | `core/loop/loop.rs` `LoopAgent::turn()` + `agent_generate_loop_v2` |
| `core/agent/mod.rs` | Agent/AgentBuilder、AgentHook、openai::CompletionModel | **Agent 构造**：build_rag_agent + 6 类 Hook | `core/agent/loop_hooks.rs`（LoopHook 三业务 Hook）+ `build_loop_adapter` |
| `core/agent/tools/mod.rs`（29 处 DynamicTool::new） | DynamicTool、ToolContext、ToolOutput | **工具运行时**：全部 30+ 工具闭包 | `core/loop/tool.rs` `Tool` trait + `core/agent/loop_tools.rs`（26+ 工具） |
| `core/agent/tool_registry.rs`、`external_tools.rs` | DynamicTool | 工具注册表、外部 HTTP 工具 | **`tool_registry.rs` 已删除**；`HashMapToolRegistry`（core/loop/tool.rs）+ `build_loop_tool_registry`/`register_external_tools`（loop_tools.rs） |
| `core/mcp/mod.rs` | DynamicTool | MCP 工具适配 | `core/agent/loop_tools.rs` `McpTool` + `register_mcp_tools`（MCP 客户端保留） |
| `core/approval/hook.rs` | AgentHook、HookContext、ToolCall、ToolCallAction | 审批 Hook | **`approval/hook.rs` 已删除**；`core/agent/loop_hooks.rs` `ApprovalHook` |
| `core/subagent/mod.rs` | MultiTurnStreamItem、StreamingChat、Message、openai | 子代理（嵌套 rig Agent） | `SubagentRunner::run` 接收 `Arc<dyn LlmAdapter>`，跑自研 LoopAgent |

### 1.2 rig 内部做了、mdgo 没感知的事（自研时必须自己实现）——评估原文

这是评估的关键——**rig 替 mdgo 藏起了哪些复杂度**：

1. **多轮循环驱动**（rig-agent `drive_tool_calls`）：发起请求 → 解析 assistant 消息中的 tool_calls → 逐个执行工具 → 把工具结果按 OpenAI 协议格式化为 tool 消息 → **追加进 run history** → 再次发起请求。mdgo 只看到最终流，循环是 rig 的。—— v3：`LoopAgent::turn()` step 循环显式化。
2. **OpenAI 流式 SSE 解析**：`delta.tool_calls` 增量（`index` + `function.name` + `function.arguments` 分片拼接）、`finish_reason`（`tool_calls`/`length`/`stop`）、`stream_options.include_usage` 用量块、推理模型的 `reasoning_content` 等。—— v3：`core/loop/openai.rs` 自研。
3. **工具结果回填协议**：工具结果必须以 `role: "tool"` + `tool_call_id` 紧跟对应 assistant(tool_calls) 消息重放。—— v3：`Session::derive_history` 配对/孤儿剔除。
4. **Hook 调度顺序**：`on_completion_call` → 请求 → `on_completion_response`；`on_tool_call`（执行前，Skip/Run 短路）；`on_invalid_tool_call`（未知工具）；`RequestPatch`。—— v3：`LoopHook` 四组钩子（pre_request/on_tool_call/on_invalid_tool_call/on_request_error）。
5. **max_turns 执行**：轮次耗尽抛 `MaxTurnsError`。—— v3：`LoopConfig.max_turns` + `LoopError::MaxTurns` + `TurnOutcome::MaxTurns`。
6. **http_client 连接池与超时**：rig 内部 reqwest（0.13 实例）注入。—— v3：`OpenAiAdapter` 内置 reqwest 客户端（双 reqwest 实例问题消失）。

### 1.3 关键结论（评估原文）

- **依赖面是"窄而深"**：文件不多（9 个），但 `MultiTurnStreamItem`/`DynamicTool`/`AgentBuilder` 三个类型横跨主循环、全部工具与子代理。
- **工具执行本身是 rig 黑盒的最后一个部分**——自研 loop 将把这一环显式化（也正是 DSH tool-calls.ts 做的事）。—— v3 已显式化（`core/loop/tool_calls.rs`）。

---

## 2. 可行性分析

### 2.1 有利条件（为什么现在做正合适）——评估原文，落地已验证

| 条件 | 证据 | 落地验证 |
|---|---|---|
| 手写流式客户端先例 | `services/anthropic.rs`：reqwest + `find_frame_end`/`parse_data_line`/`handle_sse_line` + select! 取消 | ✅ 该模式迁入 `core/loop/openai.rs`/`core/loop/anthropic.rs` |
| 类型层已解耦 | `core/chat_types.rs`：`ToolCallDto` + `group_tool_units` + `paired_tool_call_ids` | ✅ 保留并成为事件溯源投影的配对依据 |
| 循环外围已就绪 | 溢出重试、MaxTurns 区分、防幻觉成功集 | ✅ 全部迁入 loop 语义（on_request_error/retry_prepare/TurnOutcome） |
| 工具已统一轨迹 | 全部工具经 `record_tool_call`/`record_tool_result` 写 ToolCallBus | ✅ `BusToolEventSink` 保持前端协议零改动 |
| 业务层不碰 rig | 检索/技能/记忆/规划/审批/压缩/子代理白名单只依赖 `KbSearchConfig` + 自有类型 | ✅ 业务层零改动（§3.4 保留清单） |
| DSH 设计基准（已移出 docs/） | 评估时依据逐行号设计映射；结论已内化于 core/loop 落地 | ✅ 事件溯源/turn-step/并行调度/工具契约逐项映射落地 |

### 2.2 必须自研的 4 项（按难度排序）——评估原文 + 落地

| # | 能力 | 难度（评估） | 说明 | 落地 |
|---|---|---|---|---|
| 1 | **LLM 协议层**（OpenAI 兼容） | 中（1-2 周） | 流式 SSE + 非流式 + output_schema + 指数退避 | ✅ `core/loop/{openai,anthropic,llm_seam}.rs` |
| 2 | **Agent 循环**（turn/step） | **高（4-6 周）** | rig `drive_tool_calls` 显式化 + turn/step 语义 + Hook 等价物 + 并行调度 | ✅ `core/loop/loop.rs` |
| 3 | **工具运行时**（Tool trait） | 中（2-3 周） | 33 个工具构造器机械迁移 | ✅ `core/loop/tool.rs` + `core/agent/loop_tools.rs`（26+ 工具） |
| 4 | **消息/历史类型收敛** | 低（3-5 天） | `chat_types.rs` 已有 90%；补 OpenAI 视图转换 | ✅ `derive_history` 输出自有 `LlmMessage` |

### 2.3 难点与不确定性（诚实评估）——评估原文 + 落地验证

1. **OpenAI 流式 tool_calls 增量解析**：不同推理模型分片方式差异（index 乱序、arguments 分块、finish_reason 缺失）。→ 对策：mock SSE 服务器集成测试 + 只支持 OpenAI 兼容协议。—— ✅ 落地：SSE 解析纯函数单测 + 真实模型联调；另支持 Anthropic Messages 协议（评估时列为 Phase 6，提前到 Phase 0 一并落地）。
2. **并行调度正确性**：DSH 的"exclusive barrier + 有界滚动池 + 模型序提交"在 tokio 下可实现，但**写工具必须串行**（副作用不可重叠），读工具才可并行。→ 对策：仿 DSH `isConcurrencySafe` 契约（= `ToolSpec.concurrency_safe`）。—— ✅ 落地：`core/loop/tool_calls.rs`，写工具一律 exclusive，结果按模型序提交。
3. **Hook 语义等价**：rig 的 `on_tool_call` Skip 短路 + `InvalidToolCallAction::Skip` 回填是防死循环/自纠的根基，必须 1:1 保留（技能门禁 + 审批门 + 无效工具恢复 + 重复调用熔断）。—— ✅ 落地：`core/agent/loop_hooks.rs`（SkillGate/Approval）+ loop 内 on_invalid_tool_call 自纠。
4. **子代理迁移**：去 rig 后子代理必须跑在自研 loop 上（复用同一 Tool trait + 白名单，保证取消级联与事件隔离语义不变）。—— ✅ 落地：`SubagentRunner::run` 接收 `Arc<dyn LlmAdapter>` + `LoopConfig`，偏置 select! 优先响应父链取消。

---

## 3. 目标架构（仿 DSH 核心，去 Cordis）——**已 1:1 落地**

### 3.1 分层设计（映射 DSH 分层 → mdgo 模块）

```
┌─ 命令层（保持现有 IPC/事件协议不变，前端零改动）─────────────┐
│  commands/llm.rs（agent_query/kb_llm_query 编排）            │
├─ Agent 内核 core/loop/（新，替代 rig）───────────────────────┤
│  ├─ llm_seam.rs     LlmAdapter 抽象（DSH ctx.llm 等价物）      │
│  │                   OpenAI 实现（openai.rs）+ Anthropic（anthropic.rs）│
│  ├─ session.rs      事件溯源会话（DSH Session 等价物）         │
│  ├─ loop.rs         turn/step 状态机（DSH ReactLoopAgent）     │
│  ├─ tool_calls.rs   并行调度器（DSH tool-calls.ts）            │
│  ├─ hooks.rs        pre_request/on_tool_call/on_invalid_tool_call/on_request_error │
│  └─ error.rs        LoopError（溢出/MaxTurns/...）             │
├─ 工具系统（新，替代 DynamicTool）────────────────────────────┤
│  core/loop/tool.rs  Tool trait + ToolSpec（schema/output/timeout/concurrency_safe）│
│  core/agent/loop_tools.rs  26+ 工具迁移（闭包 → 实现 Tool）   │
├─ 业务层（全部保留，零改动）───────────────────────────────────┤
│  core/search|skill|memory|planner|approval|context|subagent  │
│  services/chat.rs|llm.rs(去 rig 后仅剩业务调用)              │
└──────────────────────────────────────────────────────────────┘
```

### 3.2 四个内核的设计要点（逐条映射 DSH 报告章节）——评估原文 + 落地核对

**① LLM seam（DSH §llm-streaming / report §3.4）**
- trait `LlmAdapter { async fn stream(&self, req, cancel) -> Result<StreamHandle> }`，`StreamHandle` 产出 `StreamEvent`（TextDelta / ReasoningDelta / ToolCall / Usage / Finish(reason)）；`complete()` 非流式。
- 两个实现：`OpenAiAdapter`（自研 SSE 解析）、`AnthropicAdapter`（现有 anthropic.rs 模式迁入）。
- **直接收益：Agent 模式支持 Anthropic**（评估时硬限制 llm.rs:824-834 直接拒绝）→ **✅ 已兑现**：`build_loop_adapter` 按 `LlmConfig.protocol` 选择；纯对话另有 `kb_llm_query_anthropic` 专用通道。⚠ 剩余：Anthropic Agent 模式暂为纯对话语义（工具协议面后续扩展）。
- 非流式补全（规划/扩展/摘要/评审）走同一 adapter 的 `complete()`，现有 retry_loop/output_schema/校验逻辑原样搬入 → ✅ 已兑现（`services/llm.rs` `LLMClient.adapter` + `retry_loop`）。

**② 事件溯源会话（DSH report §4.1）**
- 新增 `session_events` 表（append-only：`session_id, seq, event_type, payload, created_at`，主键 (session_id, seq) 幂等覆盖）→ **✅ 已实现**（services/chat.rs 建表 + commands/llm.rs 读写）。
- LLM 历史由事件派生（`derive_history(session_id, budget)`，复用现有压缩器）→ **✅ 已实现**（`core/loop/session.rs`，增量缓存 + 配对/孤儿剔除）。
- 收益：原始 chunk 保真回放、任意点 fork、压缩 shadowed 语义、"模型可见即已记录"不变式 → ✅ 事件级回放与 fork 就绪；⚠ 原始逐 chunk 增量未逐条保留（事件子集 9 类）。
- **与去 rig 解耦**：事件溯源独立于循环的存储改造，可在 loop 落地后再做 → ✅ 按序落地（Phase 4 于内核后接入）。

**③ turn/step 循环（DSH report §2.2-2.3）**
- `LoopAgent::turn()` 语义：claim 输入 → pre-step hooks（改写/拒绝）→ step（模型请求 + 工具批次）→ turn-stopping 检查点 → **✅ 已实现**。
- step 内部：组装请求（preamble 由 SkillInstructionHook 逻辑生成）→ `adapter.stream()` → 组装 assistant 消息（含 tool_calls）→ finish=tool_calls → 调度工具 → 结果回填 → 下一 step；直到 stop/max_turns → **✅ 已实现**。
- **Hook 等价物**：`pre_request`（SkillInstruction）/ `on_tool_call`（SkillGate + 重复调用熔断 + ApprovalGate，短路序：先技能门禁、后审批）/ `on_invalid_tool_call`（无效工具恢复）/ `on_request_error`（溢出压缩重试、MaxTurns 归类）→ **✅ 已实现**（`core/loop/hooks.rs` + `core/agent/loop_hooks.rs`）。
- 取消：`CancellationToken` + 每处检查点，工具层取消产合成结果保回放 → **✅ 已实现**（`tokio::select!` 检查点 + 调度器取消语义）。
- max_turns 预算 + 剩余 3 轮预警注入（pre_request）→ ✅ 已实现（`LoopConfig.budget_warning_threshold`）。

**④ 工具系统 + 并行调度（DSH report §3.3-3.4、§3.5）**
- `trait Tool { fn spec(&self) -> &ToolSpec; async fn execute(&self, args, ctx) -> Result<Value, ToolError> }`（ToolSpec 含 parameters/output_schema/timeout_ms/concurrency_safe）→ **✅ 已实现**（`core/loop/tool.rs`）。
- 注册表：`HashMapToolRegistry` + `KbSearchConfig` 上下文（技能门禁/审批/轨迹全复用）→ **✅ 已实现**。
- 流水线：pre（技能门禁 + 审批，loop 层 Hook）→ execute（timeout 包装 + 并行池）→ post（结果规范化 + ToolEventSink）→ **✅ 已实现**。
- **并行调度器**：exclusive 串行成 barrier，concurrency_safe 走有界池；结果按模型序提交（写工具绝不并行）→ **✅ 已实现**（`core/loop/tool_calls.rs`）。
- 现有 `ToolCallBus`/`record_tool_call`/`record_tool_result` 语义不变（前端工具卡片零改动）→ ✅ `BusToolEventSink` 适配。

### 3.3 明确"不仿"的部分（避免过度设计）——评估原文，落地验证

| DSH 特性 | 是否仿 | 理由 | 落地验证 |
|---|---|---|---|
| Cordis 插件框架 | **不仿** | 单体桌面应用不需要插件热插拔 | ✅ 正确：Rust 模块 + trait 达到同等开闭性 |
| 事件溯源 Session 全词汇表 | 仿最小子集 | 只取 9 类事件 | ✅ 正确 |
| rpcId 回显 + 基线回放重连传输 | 暂不仿 | 无 headless/SDK 需求前 Tauri IPC 足够 | ✅ 正确（P2-16 用户跳过） |
| workflow/ralph/agent-team | 不仿 | 子代理扩展 + 目标体系已够用 | ✅ 正确（未做 workflow/goal） |
| 逐文件 100% 覆盖率门 | 部分仿 | 对 loop/调度器/SSE 解析设覆盖率门 | 🟡 部分：loop/调度器/SSE 均有单测，未设覆盖率门 |

### 3.4 保留的业务层（零改动清单）——✅ 落地验证

`core/search/*`（混合检索/精排/聚簇）、`core/skill/*`（激活/注入/指标/门禁 + 技能正文内存直读）、`core/memory/*`、`core/agent/planner.rs`、`core/approval/*`、`core/context/*`（压缩/检查点，仅转换层换类型）、`core/trace.rs`、`core/bridge/*`（WebSocket 工具桥）、`core/mcp/*`（仅工具适配层换 Tool trait）、`services/chat.rs`、`services/ai_history.rs`、全部前端（事件协议不变）——**业务层未重构，前端 `agent:tool_call/result` 等协议零改动**。

---

## 4. 工作量与分期（每期可交付、可回滚）——含完成勾选

> ✅ = 已落地（commit `7278d50` 起分批）；🟡 = 部分；⬜ = 未做。

| Phase | 内容 | 产出 | 工作量（评估） | 验收 | 状态 |
|---|---|---|---|---|---|
| **0** | LLM 协议层：OpenAI SSE 客户端（流式+非流式+usage+output_schema）+ 适配器 trait | `core/loop/llm_seam.rs` + **`core/loop/openai.rs`**（评估原文误写为 `services/openai.rs`，已更正） | 1-2 周 | mock SSE 服务器单测；与真实模型联调 | ✅ 已落地（另含 anthropic.rs 双协议） |
| **1** | 消息类型收敛：自有 `LlmMessage`/`ToolCall`，历史转换改输出自有类型 | `core/loop/types.rs` | 3-5 天 | 单测：工具单元分组/孤儿过滤/协议消息装配 | ✅ 已落地（`derive_history` 投影自有类型） |
| **2** | Agent 循环核心：turn/step 状态机 + 顺序工具执行 + Hook 等价物 + 取消 + max_turns + 溢出重试 | `core/loop/{loop,hooks,error}.rs` | 3-4 周 | 与 rig 版并行跑（feature flag 对比输出）；工具历史/预算/取消集成测试 | ✅ 已落地（M1 并行验证后切流） |
| **3** | 并行调度器 + 工具契约迁移：33 个工具改 Tool trait，MCP/外部工具适配 | `core/loop/tool_calls.rs` + `core/loop/tool.rs` + `core/agent/loop_tools.rs` | 3-4 周 | 读工具并行、写工具串行；工具卡片/审批/技能门禁回归全绿 | ✅ 已落地（26+ 工具 + BridgeTool/ExternalHttpTool/McpTool） |
| **4** | 事件溯源会话：session_events 表 + derive_history + 双写迁移 | `core/loop/session.rs` + `services/chat.rs` 扩展 | 4-6 周 | fork/回放/压缩检查点集成测试；`chat_messages` 兼容读 | ✅ 已落地 |
| **5** | 子代理迁移 + 收尾：子代理跑自研 loop；移除 rig 依赖（Cargo.toml 删 rig-core/rig-agent） | 全量 | 1-2 周 | `cargo tree` 无 rig；全部工具/子代理/技能回归 | ✅ 已落地（`cargo tree` 确认无 rig） |
| **6** | 平台化（按需）：Anthropic Agent 模式、headless CLI、eval 入 CI、精确 token 计量 | — | 2-3 周 | — | 🟡 部分：Anthropic 双协议 ✅（Agent 模式纯对话语义）、精确 token 计量 ✅；headless CLI ⬜、eval 入 CI 🟡（框架已建待执行器） |

**合计约 14-21 周（4-6 个月）**；Phase 0-3 完成即可摘掉 rig（Phase 5 收尾），Phase 4/6 可按需裁剪。—— ✅ 实际按此节奏：Phase 0-3 + 5 于 `7278d50` 摘 rig；Phase 4 与 Phase 6 部分随后落地。

### 关键里程碑——全部达成

- **M1（Phase 0-2 结束）**：自研 loop 在 feature flag 后与 rig 版并行运行，同一请求两种实现输出 diff 对比——✅ 达成（M1 并行验证，收敛后切流）。
- **M2（Phase 3 结束）**：rig 依赖移除，`cargo tree` 干净，功能回归全绿——✅ 达成（`7278d50`）。
- **M3（Phase 4 结束）**：事件溯源上线，fork/回放能力解锁——✅ 达成（session_events + derive_history）。

---

## 5. 风险与对策——含落地验证

| 风险 | 等级 | 对策 | 落地验证 |
|---|---|---|---|
| OpenAI 流式 tool_calls 增量解析差异 | 中 | 只支持 OpenAI 兼容协议；mock SSE 集成测试；真实模型回归清单 | ✅ 落地；并扩展 Anthropic Messages 协议 |
| 并行调度副作用重叠（写工具并发） | 中 | 仿 DSH `isConcurrencySafe` 契约：默认 exclusive，仅显式声明的读工具并行；结果严格按模型序提交 | ✅ 落地（`ToolSpec.concurrency_safe` + 调度器） |
| Hook 语义漂移 | 中 | 把 rig 版 6 类 Hook 行为写成行为契约单测，自研 loop 逐条对照（M1 diff 兜底） | ✅ 落地（loop_hooks.rs 单测 + 手动验收） |
| 子代理取消级联/事件隔离回归 | 低 | 复用现有 ToolBusGuard + 独立 request_id 语义；子代理集成测试 | ✅ 落地 |
| 事件溯源迁移期间一致性 | 中 | 双写 + `chat_messages` 兼容读 + 一次性 backfill；Phase 4 独立后置 | ✅ 落地（session_events 幂等覆盖 + 兼容读） |
| 团队对 rig 内部行为的隐性依赖 | 中 | 先写"rig 行为快照测试"（golden 数据），自研 loop 对齐后再删 | ✅ 落地（M1 diff + 单测固化） |
| 6 个月长周期投入 | 中 | 分期 + feature flag 并行运行，任何一期可暂停回滚，rig 版始终可用 | ✅ 落地（回滚 = git revert；v3 为唯一实现） |

---

## 6. 收益分析（去 rig 换来了什么）——含兑现验证

| 收益 | 说明 | 兑现验证 |
|---|---|---|
| **并行工具执行** | rig 顺序执行 → 自研调度器读工具并行 | ✅ 兑现（多文件读/多检索耗时显著下降） |
| **Agent 模式支持 Anthropic** | 评估时硬限制随 LLM seam 消除 | ✅ 兑现（双协议；Agent 模式暂为纯对话语义） |
| **事件溯源会话** | 原始 chunk 回放、任意点 fork、压缩 shadowed 语义、"模型可见即已记录"不变式 | ✅ 兑现（事件级回放/fork 就绪） |
| **精确 token 计量** | 真实 tokenizer（tokenizers 已依赖），替换字符估算 | ✅ 兑现（`TokenizerBackedEstimator`） |
| **循环可控** | 溢出重试/MaxTurns/预算预警/注入防护从"rig 事件里捞"变为"自己的代码" | ✅ 兑现（loop.rs 全量自有） |
| **依赖面收窄** | 移除 rig-core/rig-agent（含 reqwest 0.13 双实例问题） | ✅ 兑现（`cargo tree` 无 rig；双实例坑消失） |
| **工具契约化** | output schema + timeout + concurrency 声明，前端结构化卡片走向通用投影 | ✅ 兑现（ToolSpec 契约；结构化卡片已通用化） |

**代价（评估原文）**：6 个月投入 + 上述风险；期间 rig 版与自研版并行维护的短期成本。—— 实际代价低于评估：M1 并行验证期较短，`7278d50` 一次切换完成。

---

## 7. 决策建议——原文 + 落地回顾

1. **结论：做。** 技术风险可控、业务层零改动、先例（anthropic.rs）与基础（chat_types.rs）已具备；不做的话，rig 0.41 的顺序执行、单协议、升级锁死会持续成为瓶颈。—— ✅ **已执行，收益兑现**。
2. **范围：仿四内核，不仿 Cordis/TS/传输细节**（§3.3）。"完全仿照 DeepSeek Harness 的核心"应理解为**架构原则**（事件溯源、turn/step、流水线、并行调度、契约化），而非代码级照搬。—— ✅ **已执行**（core/loop 四内核 1:1，未引入插件框架）。
3. **顺序：Phase 0→1→2 先做"最小 loop"，M1 diff 验证后再进 3；Phase 4（事件溯源）独立后置；Phase 6 按需。** —— ✅ **已执行**。
4. **护栏：** 开工前先提交当前基线；为 rig 版写"行为快照测试"再动手；每个 Phase 结束跑 `cargo test --lib` + 手动验收清单。—— ✅ **已执行**（基线 `7278d50`；`cargo test --lib` 现 321/321；手动验收见 `docs/Agent 能力验收清单.md`）。
5. **参考实现顺序**（自研 loop 与 DSH 报告的对应）：loop 状态机 ← report §2.2-2.3；并行调度 ← §3.4 tool-calls.ts；工具契约 ← §3.1 ToolDefinition；事件溯源 ← §4.1 Session；溢出恢复 ← §4.3 compaction-basic 的 request-error 分支。—— ✅ **已按此映射落地**。

---

## 附：自研 loop 的模块清单（建议目录）——现状核对

```
tauri/src-tauri/src/core/loop/          ✅ 已全部落地（11 个源文件）
├── mod.rs            // pub use 聚合（✅）
├── types.rs          // LlmMessage/ToolCall/ToolResult/StreamEvent + LlmError（✅，Phase 1）
├── llm_seam.rs       // LlmAdapter trait + CompletionRequest/Response（✅，Phase 0）
├── openai.rs         // OpenAI 兼容 SSE 客户端（✅，Phase 0；评估原文写为 services/openai.rs，已更正）
├── anthropic.rs      // Anthropic Messages 客户端（✅，Phase 0 增补）
├── loop.rs           // LoopAgent：turn/step 状态机 + 取消 + max_turns + LoopConfig（✅，Phase 2）
├── hooks.rs          // LoopHook：pre_request/on_tool_call/on_invalid_tool_call/on_request_error（✅，Phase 2）
├── tool_calls.rs     // 并行调度器：exclusive barrier + 有界池 + 模型序提交（✅，Phase 3）
├── tool.rs           // Tool trait + ToolSpec + ToolRegistry/HashMapToolRegistry + ToolEventSink（✅，Phase 3）
├── session.rs        // 事件溯源 Session：9 类 SessionEvent + derive_history（✅，Phase 4）
└── error.rs          // LoopError：ContextOverflow/MaxTurns/StreamFailed（✅，Phase 2）
```

> 对应落地提交：`7278d50`（内核 + 摘 rig）、`874bd5f` 之后各能力批次（书签/画布/日程/RAG P0/技能内存直读，见 `docs/Agent 内核重构蓝图.md` §6）。
