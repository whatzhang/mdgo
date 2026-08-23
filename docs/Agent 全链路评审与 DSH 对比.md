# mdgo Agent 全链路 Code Review 与 DeepSeek Harness 对比及优化方案

> 最后更新：2026-08-23（原版本：2026-08）
> 分析基线：**rig 时代 commit `a19581c`** + 未提交的 `agent.js`/`main.html`/`canvas SKILL.md` 改动（评审当时工作区）。
> 对比对象：**DeepSeek Harness（DSH）**（`G:\gitProject\deepseek-harness`，Cordis 插件化 Agent 平台，当前会话即运行于其上）
> 方法：入口→出口逐环节走查 mdgo Agent 链路；对 DSH 按官方文档与关键源码剖析；最后给出基于 DSH 机制的优化方案。
>
> **基线说明（2026-08-23 更新，重要）**：本评审的问题清单与 DSH 对比结论**仍有价值**，但本文 §1/§2 的架构描述为 **rig 时代**快照，**已过时**。自 commit `7278d50`（「refactor: 自研 Agent 内核替代 rig」）起，mdgo 已按本文 §5.3 的架构方向重构为**自研 `core/loop` 内核**（现 HEAD=`edab77e`）：`LoopAgent` turn/step 状态机、`LoopHook` 钩子、`LlmAdapter` 双协议（OpenAI/Anthropic）、事件溯源 `Session` + `session_events` 表、并行工具调度器、`Tool` trait 契约。原 §2 中**架构类问题（顺序执行、非事件溯源、工具契约弱、rig 锁定）已由重构解决**；§2 其余问题绝大多数已在 rig 时代修复并随 v3 保留（见文末「实施记录」与各节标注）。`cargo test --lib` 现为 **321 passed / 0 failed**；前端已模块化（`css_js/modules/*.js`）。

---

## 0. 执行摘要

- mdgo Agent 是一条**工程素养很高**的链路：取消传播、fail-open/fail-closed、部分结果保留、防幻觉守卫、注入防护、工具轨迹可视化等生产级细节均已到位，**未发现 P0 级崩溃/数据丢失缺陷**（评审结论不变，v3 内核延续了这些防护）。
- 评审时主要短板集中在**架构形态**：① 依赖 rig 0.41 的顺序工具执行 + 有限 Hook 面；② 会话历史是消息表 + 手工配对；③ 工具系统无输出契约/超时/并发标记；④ 上下文预算字符估算；⑤ 前端单文件、双流式路径重复；⑥ 无事件溯源。—— **①③⑥已由 v3 自研内核（core/loop）直接解决；②由 `core/chat_types.rs` + 事件溯源 `derive_history` 单一化；④由 token 精确计量解决；⑤由前端模块化缓解**。
- **P1 级问题 8 条**（见 §2.1/§2.2）：**全部已修复**（凭据脱敏、字节/字符统一、防幻觉守卫、MaxTurnsError 显式暴露、取消感知工具、软门禁语义、git .mdgo 防护、write 上限与校验顺序）。
- DSH 在**事件溯源会话、并行工具调度、工具契约、插件化组合、多代理/工作流/目标体系、精确 token 计量**等方面提供可直接借鉴的范式——mdgo 已采纳前四项中的核心（事件溯源/并行调度/工具契约），插件化/工作流/目标体系按产品定位未采纳（见 §3.3）。
- 优化路线（评审时给出）：**P0（正确性）** §5.1 十项、**P1（架构）** §5.2 九项、**P2（平台）** §5.3 八项。—— **P0/P1 全部落地，P2 部分落地**（事件溯源、并行调度、子代理扩展、commands/llm.rs 拆分中前端收敛已完成；headless CLI、eval 入 CI、会话树 UI 未做）。

---

## 1. mdgo Agent 链路总览（入口 → 出口）——rig 时代描述 + v3 现状

### 1.1 双入口

| 模式 | 前端入口 | IPC 命令 | 后端 |
|---|---|---|---|
| Agent / RAG | `css_js/modules/agent.js` → `sendRagQuery()` | `agent_query` | `commands/llm.rs`（Stage 0-3 业务管线 + v3 `agent_generate_loop_v2` 生成路径） |
| 纯对话 | `main.html` → `sendChatMessage()` → `sendLlmQuery()` | `kb_llm_query` | `commands/llm.rs` v3 `kb_llm_query_loop_v2`（LoopAgent 无工具）；Anthropic 协议走 `kb_llm_query_anthropic`（services/anthropic.rs） |

> v3 现状：前端统一流程仍为 `chatMessages` → `expandToolHistory()`（css_js/modules/chat-history.js）→ `trimChatHistory()` → `invoke('agent_query'|'kb_llm_query', …)`；AbortController → `kb_cancel_task`。Agent/RAG 前端逻辑已从 main.html 迁至 `css_js/modules/agent.js`。

### 1.2 后端 `agent_query` 执行链（Stage 0 → 3 → v3 LoopAgent 生成）

```
agent_query (commands/llm.rs:671)
├─ 任务注册：TaskRegistry(cancel token) + agent_tasks 状态中心 + 同会话旧任务替换
├─ LLM 客户端：get_or_create_llm_client（配置指纹缓存）→ build_adapter（LlmConfig.protocol 选 OpenAiAdapter/AnthropicAdapter）
├─ Stage 0  技能预激活：手动触发/会话挂载 → resolve_preactivated（spawn_blocking）
├─ Stage 0.5 轻量规划：should_plan 规则门 → generate_plan_json（结构化校验 ≤3 次修正，full plan 字段）
│            → plan:request 前端确认（oneshot + 60s 超时 fail-closed；取消/拒绝/超时三态收尾）
├─ Stage 1-3 预检索（仅当预激活技能声明检索工具）：
│   1. 原始查询嵌入 + LLM 查询扩展（tokio::join! 并行，扩展带 10s 独立超时 fail-open）
│   2. 扩展批量向量化 + embedding 语义去重 + 符号实体发现（代码意图时）
│   3. 多查询 hybrid_recall（buffer_unordered 并行）→ 候选池统一精排（rerank_pool）
│      → aggregate_hits（文档级聚合 + 跨查询一致性加成）→ build_context_text
│      → wrap_suspicious（注入防护包裹）→ build_sources（引用去重）
├─ Stage 4  生成（v3：agent_generate_loop_v2，commands/llm.rs:1991）：
│   LoopAgent::turn()（core/loop/loop.rs）
│   ├─ 任务计划注入 preamble → 记忆注入（search_hybrid 关键词∪向量 RRF，top-3，两级作用域）
│   ├─ 组装：LlmAdapter（双协议）+ loop_tools.rs 注册表（HashMapToolRegistry，26+ 工具）
│   │        + loop_hooks.rs 业务 Hook（SkillInstruction/SkillGate/Approval）+ BusToolEventSink
│   ├─ 压缩检查点应用（CompactionState：摘要 + 检查点）→ seed_session_from_messages 播种 Session
│   ├─ turn 循环 step：derive_history（事件溯源投影）→ pre_request patch → adapter.stream()
│   │   → LoopEvent（Delta/ReasoningDelta/ToolCall/ToolResult/Usage）→ 命令层转发 rag:delta
│   │   → finish=ToolCalls → tool_calls.rs 并行调度（exclusive barrier + 有界池 + 模型序提交）
│   │   → tool/result 事件回填 → 下一 step；直到 stop/max_turns/取消
│   └─ 会话事件落库：session_events（upsert_session_events，幂等覆盖）
├─ 出口：rag:done{content, sources(预检索∪工具检索 merge_search_sink), token 用量}
│   apply_anti_hallucination_guard（ACTION_CLAIMS 声明表）→ apply_grounding_validator（证据校验 C2 可选）
│   取消/失败路径：保留部分内容 → rag:done/rag:error；任务状态中心收尾；trace 收尾
└─ 收尾：tool_call_bus().clear + skill_metrics 记录 + TaskRegistry.unregister
```

### 1.3 Agent 构造与 Hook 链——v3 现状（core/agent/loop_hooks.rs，替代 rig build_rag_agent + AgentHook）

| Hook（v3：`LoopHook`） | 职责 | 位置 |
|---|---|---|
| `SkillInstructionHook` | `pre_request`：每轮 preamble 注入（基础规约 + L1 技能目录 + 技能约束摘要 ≤800 字符 + 轮次预算预警 ≤3 轮）；`RequestPatch::active_tools` 窄化可见工具（含 mcp_tool_names 并入） | loop_hooks.rs:28 |
| `SkillGateHook` | `on_tool_call` 兜底拦截（BASE_TOOLS 放行 / 技能声明放行 / allow_extra）+ **重复调用熔断** | loop_hooks.rs:116 |
| `ApprovalHook` | `on_tool_call`：破坏性写操作审批（edit/delete/write/multi_edit/git_*/mcp_*/open-ui 等），`ApprovalGate::check` + DenialCategory 反馈 | loop_hooks.rs:150 |
| `on_invalid_tool_call` | 无效工具名恢复：Skip + 可用工具提示回填，模型下轮自纠 | hooks.rs trait 默认 + loop 内实现 |
| `on_request_error` | 请求失败（如上下文溢出）→ `RetryAction`（压缩后重发 ≤1 次）/ MaxTurns 归类 | hooks.rs trait + loop try_recover |

> rig 时代独立的 `LlmTraceHook`/`ReasoningEffortHook` 已并入 `pre_request` 语义（`[llm_trace]` 日志已移除；reasoning_effort 由 `LlmAdapter` 透传）。

工具注册（v3）：`build_loop_tool_registry`（`core/agent/loop_tools.rs:598`，`HashMapToolRegistry`）——26+ 内置工具按组注册（只读组 concurrency_safe=true、写组 exclusive、记忆/子代理/反思/澄清/日程/书签组）；`register_bridge_tools`（pomodoro/raw-parse/open-ui）、`register_external_tools`（agent_tools.yaml 配置驱动 HTTP）、`register_mcp_tools`（连接中 MCP 服务器，`mcp_<server>_<tool>`）。**原 `tool_registry.rs` 已删除**（grep 确认不存在）；`BASE_TOOLS`/技能门禁语义迁移至 `SkillGateHook`。`filter_registry`（白名单）供子代理注册表过滤。

### 1.4 工具执行模型——v3 现状（替代 rig DynamicTool 顺序执行）

- 全部工具实现 `core/loop/tool.rs` 的 **`Tool` trait**（`ToolSpec`：name/description/parameters JSON Schema/output_schema/timeout_ms/concurrency_safe；`execute(args, ctx)` 返回规范化 JSON 值）。
- **并行调度器** `core/loop/tool_calls.rs`：同一 assistant 消息的多个 tool_calls 按 `concurrency_safe` 分组——exclusive 串行成 barrier，concurrency_safe 走有界池（默认 4）；**结果严格按模型序提交**；取消产合成结果保回放。
- 每次调用经 `ToolEventSink`（`BusToolEventSink` 写入全局 `ToolCallBus`，参数/结果截断 12k、64 请求桶上限、drain 消费、RAII 清理）→ 命令层 `emit_pending_tool_events` 转发前端 `agent:tool_call/result`（协议零改动）。
- 失败路径统一 `ToolError`（NotFound/InvalidArgs/Failed/Timeout/Cancelled/Internal），错误文本回填模型。

### 1.5 支撑层（v3 现状）

- **上下文工程**：`SummarizeThenWindowCompressor`（按工具单元 2/3 切分旧段 → LLM 摘要（6000 字符预算）→ 滑窗 recent；摘要恒保留）；检查点 `CompactionState` 落库 `chat_sessions.compaction_state`；token 精确计量（`TokenizerBackedEstimator`）。
- **可观测**：`TraceBus` 五阶段（planning/expanding/searching/aggregating/generating）→ `trace:event` 前端渲染阶段耗时面板；tracing 双输出（文件+终端）+ `LogTracer` 桥接。
- **会话**：**事件溯源** `core/loop/session.rs` `Session`（9 类 `SessionEvent` + `derive_history` 增量投影，配对/孤儿剔除）+ `session_events` SQLite 表（`services/chat.rs` 建表，`commands/llm.rs` 读写）；`chat_messages` 保留兼容读。
- **子代理**：同进程自研 `LoopAgent`（`SubagentRunner::run` 接收 `Arc<dyn LlmAdapter>`），独立 request_id/技能态/检索收集器；只读/写型白名单；写型强制审批门；结果 LRU(16) + `read_subagent_result` 分页；`spawn_subagent`/`parallel_research`（JoinSet）。
- **记忆**：`MemoryStore` + `memory_items` + FTS5 + 向量（RRF 融合），两级作用域（当前库 ∪ 全局），注入点生成前 preamble。
- **安全**：检索/子代理回传 `wrap_suspicious` 注入包裹；Action Claim 防幻觉；grounding 校验（证据校验 C2 可选开关）；`approval.yaml` 配置驱动审批策略；git 工具 `.mdgo` 防护；凭据脱敏。
- **传输**：Tauri IPC（invoke + 事件）；`FrontendBridge`（core/bridge，WebSocket，工具闭包→前端 handler，DashMap + 5s 超时）供 pomodoro/raw-parse/open-ui 等交互工具。

---

## 2. mdgo Agent 代码走查发现（问题清单）——rig 时代证据 + 修复状态

> 分级：P0 正确性/数据丢失；P1 影响体验/安全；P2 应当修复；P3 可择机。**修复状态列**为 2026-08-23 核对：✅=已修复并保留（rig 时代实施，v3 内核延续）；🔧=随 v3 重构解决；⬜=未修复（剩余工作）；❌=已随 v3 移除（问题消失）。

### 2.1 服务层与支撑层

#### P1
| # | 问题 | 证据（rig 时代） | 状态 |
|---|---|---|---|
| 1 | **API Key 明文写入 INFO 日志** | `services/llm.rs:319` | ✅ 已修复：`mask_secret`（FNV-1a + 长度不可逆掩码）+ 单测；v3 保留 |
| 2 | **压缩预算字符/字节混用** | `core/context/mod.rs:49-51` vs `:202,:216,:264` | ✅ 已修复：预算比较全部 `chars().count()` + 中文回归测试；后升级为 token 精确计量 |

#### P2
| # | 问题 | 证据（rig 时代） | 状态 |
|---|---|---|---|
| 1 | `planner_model`/`summary_model` 用户可配置但**不生效** | `lib.rs:88-90` | ✅ 已修复：`model_for_role` 按角色路由 |
| 2 | LLM 客户端缓存满 8 **全清** | `lib.rs:291-294` | ✅ 已修复：改逐条淘汰 |
| 3 | TraceBus 桶数 ≥64 时 `map.clear()` 清空全部在途桶 | `core/trace.rs:62-64` | ⬜ 未修复（低影响，剩余工作） |
| 4 | 记忆检索/向量仅覆盖最近 100 条；向量索引无删除路径 | `core/memory/mod.rs:414,480,489` | ✅ 已修复：`MemoryVectorIndex::prune` + 全量可见（10k 上限） |
| 5 | `fork_session` 后 `token_usage` 归零、`compaction_state` 不复制 | `services/chat.rs:732` | ⬜ 未修复（剩余工作；v3 事件溯源下 fork 语义待复核） |
| 6 | `search_sessions` 用 `LOWER(content) LIKE` 全表扫描 | `services/chat.rs:1041-1043` | ⬜ 未修复（剩余工作） |
| 7 | `chat_session_skills` 表不在 `ChatStore::init_tables` 创建 | `services/chat.rs:1401` | ⬜ 未修复（剩余工作，需复核 v3 后是否仍成立） |
| 8 | `summarize_bookmark` 文档与实现不符；`enable_thinking` 硬编码 | `llm.rs:823-827` vs `:890-903` | ⬜ 未修复（剩余工作，书签重构后需复核） |
| 9 | `retry_loop` 开头 `if cancel.is_cancelled()` 空块死代码 | `services/llm.rs:254-256` | ✅ 已修复：删除空块；重试条件加 `cancel.is_cancelled()` |
| 10 | `ProviderError(_)` 一律重试（401/403 也退避） | `services/llm.rs:229` | ✅ 已修复：`is_retryable` 收窄（401/403/400/溢出不重试） |
| 11 | 子代理忽略 `FinalResponse` 变体 | `core/subagent/mod.rs:236` | ❌ 已随 v3 移除：子代理跑自研 LoopAgent，无 rig 流变体问题 |
| 12 | MCP 传输故障判定用中文错误子串匹配 | `core/mcp/mod.rs:863-879` | ⬜ 未修复（剩余工作） |
| 13 | 审批已决缓存满 256 全清 | `core/approval/mod.rs:179-184` | ⬜ 未修复（低影响） |
| 14 | `WRITE_STATS` 每累计 10 次写 dump 全部统计 | `services/chat.rs:36-49` | ⬜ 未修复（低影响） |

#### P3
| # | 问题 | 证据 | 状态 |
|---|---|---|---|
| 1 | `CompactionState.tokens_before` 恒写 0 | `commands/llm.rs:1847` | ✅ 已修复：token 精确计量后 `tokens_before` 实值化（需复核） |
| 2 | MCP HTTP SSE 帧缓冲无上限 | `core/mcp/http.rs:107-118` | ⬜ 未修复（剩余工作） |
| 3 | eval 框架无真实 LLM 执行器 | `core/eval/mod.rs:16-18` | 🟡 部分：断言/报告 + YAML 场景已建；真实执行器待 CLI |
| 4 | 注入扫描误报率高 | `core/security/mod.rs:18-49,102-104` | ⬜ 未修复（启发式边界，误报可接受） |
| 5 | `JsonSchemaValidator` 每次调用重编译 schema | `services/llm.rs:810-811` | ⬜ 未修复（低影响） |
| 6 | 用户同文案连发被幂等去重 | `services/chat.rs:535-564` | ⬜ 未修复（行为取舍） |

#### 值得肯定（v3 延续）
- 审批 fail-closed 完备（超时/通道不可用/策略拒绝三分，带差异化模型反馈）；写型子代理强制审批门、门缺失回退只读。
- 压缩按工具调用单元切分杜绝孤儿 tool 消息；检查点应用失败安全降级全量压缩。
- MCP 凭据脱敏收敛于单一写入点；自动重连耗尽保护。
- 子代理只读白名单显式排除记忆写、技能激活与递归工具（防污染与无限递归）。

### 2.2 Agent 核心与工具系统——rig 时代依据 + v3 重构说明

> 评审依据（rig 时代）：通读 `core/agent/mod.rs`（1470 行）、`tools/mod.rs`（5289 行）、`commands/llm.rs`（2670 行）、`tool_registry/external_tools/limits/planner/task_store/cache/canvas`，并交叉核对 rig-agent 0.41.0 源码。

**执行模型（rig 时代）**：mdgo 无自写 agent 循环——rig 0.41 内部驱动（`drive_tool_calls`），mdgo 只消费 `MultiTurnStreamItem` 流做旁路转发。工具默认顺序执行；取消走 `TaskRegistry` + `next_or_cancel` 偏置 select；`DEFAULT_MAX_TURNS=20` + 剩余 3 轮预算预警。
**v3 现状**：以上全部由自研内核取代——`LoopAgent::turn()` turn/step 状态机（`core/loop/loop.rs`）、并行工具调度（`tool_calls.rs`）、`LoopConfig`（max_turns/预算预警阈值/重试预算/并行上限）。

**工具全清单（v3，30+ 内置 `Tool`）**：kb_search / code_lookup / read / grep / ls / glob / write / edit / multi_edit / delete / git_status / git_diff / git_commit / git_checkout / remember / forget / search_memory / todo_write / deep_research / read_subagent_result / spawn_subagent / parallel_research / webfetch / self_review / ask_user_question / schedule / search_bookmarks / get_bookmark / activate_skill / deactivate_skill + BridgeTool（pomodoro/raw-parse/open-ui）+ ExternalHttpTool + McpTool（`mcp_<server>_<tool>`）。

**关键机制（v3）**：`ToolCallBus`（12K 截断、64 桶）→ `emit_pending_tool_events` 转发前端；`ApprovalHook` 挂在 `SkillGateHook` 之后（短路序：先技能门禁、后审批），60s 超时 fail-closed；技能三层披露 + `active_tools` 窄化 + 软门禁；防幻觉 = Mutation Verification 回读（前置）+ Action Claim 声明表守卫 + Grounding Validator（后置）+ Loop Guard 熔断。

#### P1
| # | 问题 | 证据（rig 时代） | 状态 |
|---|---|---|---|
| 1 | **防幻觉守卫只看工具名、不看执行成功** | `commands/llm.rs:1992-1994` | ✅ 已修复：`ToolCallBus::successful_tool_names`（ok=true 才计） |
| 2 | **MaxTurnsError 有部分内容时静默按成功收尾** | `commands/llm.rs:2026-2030` 与 `:2052` | ✅ 已修复：`StreamingError::Prompt(MaxTurnsError)` 检测 + 截断提示追加；v3 `TurnOutcome::MaxTurns` 显式携带内容 |
| 3 | **取消后阻塞工具闭包无取消且重建总线桶** | `tools/mod.rs:149` vs `commands/llm.rs:1963` | ✅ 已修复：`record_tool_call/result` 取消后跳过（桶不再重建）；v3 工具调度器感知 cancel token |
| 4 | **软门禁语义反转**（无激活技能时全放行） | `core/agent/mod.rs:838-841` | ✅ 已修复：`KbSearchConfig.skill_gating` 统一（主对话 true/子代理 false），4 处门禁一致 |
| 5 | **git_checkout/git_commit 无 `.mdgo` 目录防护** | `tools/mod.rs:1541-1554` | ✅ 已修复：git_commit 拒绝暂存区含 .mdgo；git_checkout 拒绝 .mdgo 路径 |
| 6 | **write 1MB 上限按字符数 + 校验顺序错误** | `tools/mod.rs:1837,1858-1863` | ✅ 已修复：按字节计 + 词法校验全部前置后再建目录 |

#### P2
| # | 问题 | 证据 | 状态 |
|---|---|---|---|
| 1 | `kb_search` 工具输出未过 `wrap_suspicious` | `core/agent/mod.rs:752-798` | ✅ 已修复：kb_search/code_lookup 输出过防护 |
| 2 | `should_plan` 的"先/再"等动词启发式误报率高 | `core/agent/planner.rs:37-51` | ✅ 已修复：移除误报源 + 疑问句/轻量查看抑制 |
| 3 | 孤儿 tool result 消息处理 4 处重复实现 | `main.html:51204`、`agent.js:186`、`commands/llm.rs:215`、`core/context/mod.rs:125` | ✅ 已修复：`core/chat_types.rs` 单一配对源 + 前端 `chat-history.js` 委托；v3 事件溯源 `derive_history` 兜底 |
| 4 | 工具结果缓存命中/失效时全量克隆 | `core/agent/tools/cache.rs` | ⬜ 未修复（低影响） |
| 5 | magic number / 重复代码 | 多个 `build_*_tool` 函数 | 🟡 部分：参数解析/软门禁抽为纯函数；schema 手写仍存在 |

#### P3
- 工具 schema 与限额未从 `limits.rs` 单一引用（部分内联）；`canvas` 工具复杂度高但无独立测试——✅ 已部分修复：canvas 工具现带 10 个 benchmark 用例（docs/canvas-benchmark-cases/）；`guard_duplicate_call` canonical 参数序列化开销未评估（⬜）。

#### 值得肯定（v3 延续）
- 无效工具调用恢复（on_invalid_tool_call）对齐主流 Agent 自纠模式；防重复调用熔断有效防死循环；技能门禁 + 软门禁 + 工具闭包守卫三层防御；MCP 工具默认审批（对齐 Claude Code 默认 prompt 权限）。

### 2.3 前端（main.html / css_js/modules/*.js）——rig 时代证据 + 模块化现状

> 评审时前端为 2.6MB 单文件 main.html + 少量模块 js；**v3 现状**：Agent/RAG 逻辑已模块化至 `css_js/modules/agent.js`、`chat-history.js`、`agent_global.js`、`frontend-bridge.js`、`canvas.js`、`schedule.js`、`skill.js`、`mcp.js`（main.html 体积下降但仍有 2.3MB，聊天与 RAG 渲染逻辑部分内联）。

#### P2
| # | 问题 | 证据（rig 时代） | 状态 |
|---|---|---|---|
| 1 | **两条流式路径重复**（sendRagQuery vs sendLlmQuery 约 60% 同构） | agent.js:344-646 vs main.html:51332-51567 | 🟡 部分：sendRagQuery 已模块化至 agent.js；sendLlmQuery 仍在 main.html（去重为剩余工作） |
| 2 | **工具配对语义 4 处实现** | trimChatHistory/expandToolHistory/chat_turns_to_history/group_turns | ✅ 已修复：`chat-history.js`（groupToolUnits/expandToolHistory/trimChatHistory）单一实现，main.html/agent.js 薄包装委托 |
| 3 | **单文件 2.6MB main.html** | 全部内联 5 万+ 行 | 🟡 部分：Agent 模块已外链；main.html 体积仍大（聊天/渲染区段），拆分进行中 |
| 4 | **前端 token 估算与后端压缩双轨** | estimateTokenCount（/4）vs tokens_to_chars_budget（/2） | ✅ 已修复：后端 token 精确计量 + `estimate_turns_tokens` 预算门；前端口径对齐（chat-history.js） |

#### P3
- `agent_global.js` 全局任务状态条每 30s 轮询 + 事件驱动双路径（⬜ 低影响）；`chatStreaming` 等流式状态为全局变量（🟡 模块化后部分改善）。

#### 值得肯定（v3 延续）
- 取消链路完整：AbortController → `kb_cancel_task` → 后端偏置 select 断开；断联/失败保留部分内容落库。
- 流式渲染 rAF 节流 + done 后完整 sanitize 覆盖；`expandToolHistory` 对老数据降级为文本消息，向后兼容。
- `handleAgentToolEvent` 工具卡片（耗时徽标/点击展开/结构化卡片）与 trace 阶段面板，可检视性在同类本地工具中属上乘。

---

## 3. DeepSeek Harness 架构剖析（对 mdgo 有借鉴意义的部分）

> 原 DSH 架构行号级调研报告（`docs/deepseek-harness-architecture-report.md`，约 220 个包逐包调研 + 传输层/策略层两个子代理并入）已移出 docs/；本节约取其评审当时与 mdgo 优化直接相关的要点。**本节为 DSH 侧描述，不受 mdgo 重构影响，保持原文。**

### 3.1 总体形态

- **pnpm monorepo，约 220 个 `@deepseek-ai/dsh-*` 包 + apps/cli + apps/web**；底层是 vendored **Cordis 插件框架**："everything is a plugin"，Context 是服务仓库（`ctx.tools`/`ctx.llm`/`ctx.sessions`…），注册都是可逆副作用，四种事件派发模式（emit/waterfall/parallel/serial）。
- **分层**：util/typert 底座 → 核心 spine（session → system-prompt → tools → agent → agent-loop + scope，依赖单向）→ 能力 seam 层（fs/shell/sandbox/web/lsp/subagent 等，每个是 Service Definition/Provider/Consumer 三件套）→ host/api/client/sdk 传输层 → bundle/profile 组合层 → apps。`agent-loop` 是唯一具体驱动，扩展插件只依赖 `agent` 接口——**loop 可整体替换**。

### 3.2 Agent 运行时（ReactLoopAgent）

- 四态机（idle/maintenance/running，对外只暴露 idle/running）；`kick()` 反复 `while(await turn())`；每 turn 内含 0..n 个 step（step = 一次模型请求 + 其工具）。
- 每 step：`inbox.claim`（next-step 全部 + turn 边界 1 条 next-turn）→ `agent/pre-step` **waterfall**（改写/拒绝消息）→ `step/start` → 逐条 `user/message` → `agent/request` waterfall → `llm/stream`（**逐 chunk 落 `assistant/chunk` 事件**，可回放）→ BlockAssembler → `assistant/message`（`sourceEventSeqs` 引用全部 chunk）→ `executeToolCalls` → `step/end` → `agent/turn-stopping`（serial 终点检查点，可 steer 再开一步）→ `turn/end`（reason：completed/blocked/aborted/error/max-tokens/interrupted）。
- **取消**：`cancel(cause)` 清 inbox（可 keepInbox）+ abort；loop 每处 `throwIfAborted` 检查点生效；工具取消分 `ABORTED`/`ABORTED_BEFORE_DISPATCH` 且**补齐合成 tool/result 保回放完整**；唤醒闩锁保证取消收敛后补跑。`inject()`/`steer()`/`followup()` 三通道输入模型（idle 注入留 inbox 直到被唤醒）。
- 工厂 `AgentLoop`：`setup` 在**未发布**状态组装 agent 作用域，`publish` 按序 sessions.enter→agents.enter→announce，失败整体回滚；`dispose` 有序收敛。

### 3.3 工具系统（ToolDefinition 契约 + 五段流水线）

- **`ToolDefinition extends ToolSchema`**：必填 `output`（canonical JSON Schema + 纯函数 render + presentationMeta）、`execute(args, exec)`、可选 `finalizeContent`（同步 last-mile 内容变换）、`timeoutMs`（**永不上模型**）、`isConcurrencySafe(args)`（**只有精确 true 才并行**，抛异常/缺省按 exclusive）、`presentCall/presentResult`（纯 UI 渲染意图，可回放）。`schemas()` 白名单只投影 name/description/parameters。
- **注册/作用域**：按 scope 分层（global + agent.ctx）；`restrict` allow/deny 交集；`guard` 单调守卫（只能拒绝不能强放行）。
- **执行流水线**：`tools/pre-execute` waterfall（allow/deny/ask）→ `ctx.approval` 一次性审批（默认 deny，fail-closed）→ 单调守卫 → `tools/execute` waterfall（超时/重试包装，可换 signal 融合）→ body → `fs/write-intent|edit-intent` 门 → 工具自有事件（todo/write、fs/observed…）→ `tools/post-execute`（accept/block/replace/add context）→ 注册表无损快照规范化 → `finalizeContent` → `tools/result`（冻结权威结果）。`run_code` 折叠在策略前拒绝为 UNKNOWN_TOOL。
- **并行调度**（tool-calls.ts）：exclusive 调用形成 barrier，parallel 调用走**有界滚动池**（默认 10 并发）；dispatch 可重叠，policy/结果/上下文按 **model 顺序**提交；每调用启动前重新分类。
- **超时/重试**：超时是协作式（工具承诺遵守 exec.signal）；LLM 重试基于**日志回放计数**（findLast 匹配 turn+step+provider 的 llm/retry 事件）+ 指数退避 + 抖动 + Retry-After 优先，**先落盘再等待**。
- **沙箱**：`read-only | workspace-write | danger-full-access`，默认 **read-only**（fail-closed），策略按调用携带，session 覆盖来自 `sandbox/mode` 日志事件。

### 3.4 上下文与状态（事件溯源 + token 计量 + 压缩）

- **会话日志 = 唯一事实源**：`Session` 追加式 `SessionEvent[]`（无损 JSON 快照 + surfaceManager 校验）；事件词汇可声明合并扩展；surface 类型仅 3 种（user/message、assistant/message、tool/result）决定"模型可见表面"；`deriveMessages()` 从 surface 节点**增量投影** LLM 历史（缓存 + 游标）。不变式：**"模型可见即已记录"**（runtime invariant 断言）。
- **token 计量**：`ctx.tokenMeter` 回放折叠（基线 = 最新成功 usage 当且仅当信封匹配且不低于启发式价格），否则启发式（CHARS_PER_TOKEN=4 + 块/角色开销）。
- **压缩**（compaction seam）：双触发——`agent/pre-step` **压力检查** + `agent/request-error` **context-overflow 恢复**（仅 CONTEXT_WINDOW_EXCEEDED，成功压缩且 surface generation 前进才 retry）；压缩前可先做**无模型工具结果剪枝**（保 head/tail + 剪枝标记）；surface replace 事务（`user/message` + `surfaceOp.replace(start,end)`），`compaction/start|summary|end` 全落日志；手动 `/compact` 走 `runMaintenance`。
- **持久化**：JSONL(zstd) + chunk 行打包（连续 assistant/chunk delta 压行）+ 原子发布（临时文件 + fsync + link()）+ 崩溃修复（prepareCore/commitPrepared）+ write-behind 200ms 批量。

### 3.5 多代理（subagent / workflow / goal / ralph）

- **subagent seam**：`ctx.subagents` **命名 provider 注册表**（多 provider 共存：in-process spawn/fork、ACP、SDK、Claude Code、Codex）；能力声明（outputSchema/depthLimit/toolFilter/persona）**事前校验、fail loud**；**continuable** 子代理经子自己的 inbox 续跑（`startContinuable`/`followup`/`interrupt`）；控制工具 `list_agents`/`send_message`/`interrupt_agent` + 子代理 `report` 回传通道；`tool-subagent` 支持 `run_in_background`（后台 jobId / continuable subagentId / foreground runId+output 三态返回）。
- **workflow seam**：模型写编排脚本（worker_threads 引擎，纯 JS body + meta + args），hooks `agent()/pipeline()/parallel()/phase()`，`maxTotalAgents` 上限，`WorkflowError` 带 fatal 标志（fatal 杀脚本、普通 child 失败该项置 null），stopReason 闭环（completed/cancelled/error）。
- **goal**：事件溯源 `goal/change`（全量快照或 clear tombstone）+ revision CAS；phase active/paused/blocked/complete；blocked 需连续 3 轮 + blocked_reason；round driver 只在静止点接纳下一轮，`agent/pre-step` 上 enforce **竞态栅栏**（精确 live revision + round 连续性）；权威检查（直接人类消息或恰好被接纳的 goal round）。
- **ralph**：固定编排脚本，每轮**全新 child**（无父会话种子）+ 有界结构化 handoff（maxHandoffChars 16384）；报告三态校验（continue/complete/blocked）。

### 3.6 事件/传输

- 三套传输面：浏览器 Web UI（HTTP-up POST `/api/*` + WebSocket-down `/api/events.mux|host`）、SDK（stdio 换行分隔 JSON-RPC 2.0）、ACP（自动化，JSON-RPC stdio）。
- **rpcId 回显 + 基线回放重连**：断线重连后回放 `session/subscribed`（每个已附加 session 的 lastSeq）、挂起的 `approval/requested`/`question/requested`（**稳定 rpcId**，刷新恢复）、`session/queue`、`session/jobs` 整快照——瞬态状态走整快照而非增量，浏览器刷新后 UI 状态收敛。
- 四象限消息模型（ClientRequest/ServerResponse/ServerRequest/ClientResponse）+ Zod 双层边界校验；`PRIVILEGED_METHODS` 把设置/凭据写、目录选择钉在 loopback；`/api` 信任栅栏防 DNS rebinding；SSE 回退供进程内客户端。
- SDK 无 wire 级取消（请求超时用 AbortController 放弃，服务端继续跑）；进程回收阶梯：协议 shutdown → stdin EOF → SIGTERM → SIGKILL。

### 3.7 技术栈与工程实践

- TS 6.0.3 + Node 22.19+/24 + pnpm 11.7 + vendored Cordis + schemastery/Zod + 自有 typert 类型生成（RPC 类型系统）。
- Host/Client **双 tsconfig 聚合**（两侧对 cordis Context 声明合并冲突）；声明合并驱动一切（事件词汇/服务 key/session 事件表/消息来源/内容块）。
- 测试：逐文件 **100% 行覆盖率门**、真 API e2e（自跳过）、keyless 快照回放（ACP 场景 diff JSON-RPC + 重持久化日志）、浏览器快照（Chromium）；原则"prefer the real implementation over a mock""verify the world, not the self-report"。
- 生成目录即 CI 门（gen-*/verify-* 检查文档/矩阵过期）；构建 tsdown+tsc 双阶段 + hygiene 门（publint/knip/verify-runtime-closure 等）。

---

## 4. mdgo vs DeepSeek Harness 对比矩阵

### 4.1 架构对比表（mdgo 列 = v3 现状，2026-08-23）

| 维度 | mdgo（v3 现状） | DeepSeek Harness | 结论 |
|---|---|---|---|
| 技术栈 | Rust + Tauri 2 + 自研 `core/loop` 内核 + SQLite/LanceDB/Tantivy + ONNX 本地嵌入 | TypeScript monorepo（~220 包）+ vendored Cordis 插件 + Node | 各有所长：mdgo 单二进制桌面、本地优先；DSH 插件化、可组合 |
| Agent 循环 | 自研 `LoopAgent`（turn/step 状态机 + LoopHook + 取消检查点） | 自研 `ReactLoopAgent`（turn/step 状态机 + waterfall 事件，loop 可整体替换） | 同构（mdgo 后发对齐，去 rig 收益） |
| 输入通道 | 单请求（用户消息一次一请求）；整体取消 | inbox 模型：`followup/steer/inject` 三通道 + 中途引导 | DSH 优（mdgo 有 ask_user_question 澄清工具作部分补偿） |
| 工具执行 | **并行**（exclusive barrier + 有界池 + 模型序提交） | **并行**（exclusive barrier + 有界滚动池默认 10） | 对齐 |
| 工具契约 | `Tool` trait：ToolSpec（output_schema/timeout_ms/concurrency_safe） | ToolDefinition：output schema + render + finalizeContent + timeoutMs + isConcurrencySafe + presentCall/presentResult | DSH 略优（render/present 投影更完整） |
| 会话历史 | **事件溯源**（`Session` + `session_events` 表 + derive_history） | 事件溯源日志（`assistant/chunk` 原始保真 + surface 投影） | 对齐（mdgo 原始 chunk 未逐条保留，事件子集） |
| 工具配对 | 单一实现（`core/chat_types.rs` + `derive_history` 配对/孤儿剔除） | 单一事件对 + surface 投影 + 配对平衡校验 | 对齐 |
| 上下文工程 | 摘要+滑窗（token 精确计量、检查点落库）+ 溢出重试 | tokenMeter（回放折叠）+ 压力/溢出双触发 + 工具结果剪枝 | DSH 优（回放折叠/剪枝） |
| RAG/检索 | 混合检索（BM25+向量+精排+聚簇+查询扩展+预检索优化器+证据校验+benchmark） | 无内置知识库 | **mdgo 显著优**（产品根基） |
| 技能体系 | SKILL.md 三作用域 + 渐进披露 + 动态注入/窄化/门禁 + 内存直读 + 指标闭环 | skill 工具（指令目录注入） | mdgo 更完整 |
| 多代理 | 同进程子代理（只读/写型白名单 + 审批门 + LRU 分页 + spawn/parallel） | subagent seam（多 provider、continuable、控制工具 list/send/interrupt + report）+ workflow + goal + ralph | DSH 显著优 |
| 长期目标 | 无（仅 todo_write） | goal 事件源 + revision CAS + round driver | DSH 优 |
| 审批/安全 | ApprovalGate（approval.yaml 策略 + IPC 弹窗 + 60s fail-closed）+ 注入防护 + .mdgo 防护 + 凭据脱敏 | ctx.approval + 沙箱 seam（默认 read-only）+ fs 意图门 | DSH 沙箱维度优，mdgo 策略面已完备 |
| 重试 | `retry_loop` 指数退避（5 次尝试，取消优先）+ is_retryable 白名单 + 溢出压缩重试 | 基于日志回放计数的重试（先落盘再等待）+ Retry-After 优先 | DSH 优（回放计数） |
| 集成/传输 | Tauri IPC + 事件 + WebSocket 工具桥；桌面应用 | HTTP/SSE + WebSocket + SDK JSON-RPC + ACP + headless | DSH 集成面显著广 |
| 可观测 | TraceBus 五阶段（内存桶）+ 工具卡片 + tracing 双输出 | session/event 持久事实 + agent/* 实时事件 + 生成目录 CI 门 | DSH 持久化优 |
| 测试/工程 | 321 个 lib 单测；无覆盖率门/e2e | 逐文件 100% 覆盖率门 + 真 API e2e + keyless 快照 | DSH 显著优 |
| 前端形态 | 模块化 js（css_js/modules/*.js）+ main.html（2.3MB） | 多包 web UI，事件驱动渲染 | DSH 可维护性优 |

### 4.2 mdgo 的优点（相对 DSH）——不变
1. **RAG/知识库深度**：混合检索、查询扩展去重、符号实体发现、rerank + 聚簇 + 预检索优化器 + 证据校验 + 检索 benchmark——DSH 无此能力，这是 mdgo 的产品根基。
2. **工程防护细节**：取消传播（偏置 select!）、部分结果保留与落库、防幻觉三层（Action Claim + Mutation Verification + Grounding）、注入包裹、技能门禁三层防御、子代理防递归/防污染白名单、MCP 凭据脱敏。
3. **本地优先**：ONNX/DirectML 本地嵌入与精排、离线可用、单二进制桌面分发（Tauri）。
4. **领域工具丰富**：schedule/pomodoro/canvas/bookmark/raw-photo/open-ui + SKILL.md 技能生态；技能激活由 LLM 决策 + 预激活规则双路径 + 技能正文内存直读。
5. **成本透明**：token 用量、上下文占用率 UI 可见；历史压缩自动降级（摘要失败回滑窗，永不失败）。

### 4.3 mdgo 的不足（相对 DSH）——v3 更新
1. ~~rig 0.41 依赖锁定~~ **✅ 已解决**：自研 core/loop 内核，无 rig；工具契约（ToolSpec）、并行调度、双协议（OpenAI/Anthropic）就位。
2. ~~会话非事件溯源~~ **✅ 已解决**：`Session` + `session_events` + `derive_history`；取消合成结果保回放；fork 基于事件流。
3. ~~工具系统契约弱~~ **✅ 已解决**：ToolSpec（output_schema/timeout/concurrency_safe）+ 并行调度；前端结构化卡片仍以文本摘要为主（present 投影未做，🟡）。
4. **上下文预算**：🟡 token 精确计量已上（estimate_turns_tokens 预算门），但无 DSH 的"回放折叠 tokenMeter"与"工具结果剪枝"。
5. **交互模型单向**：🟡 有 ask_user_question 澄清；无 steer（中途引导）、无后台可续子代理控制（list/send/interrupt）。
6. **巨型文件缓解**：🟡 `commands/llm.rs` 仍约 120KB（agent_query 编排 + v3 生成路径同文件）；`css_js/modules/agent.js` 已模块化。
7. **可观测不持久**：🟡 TraceBus/质量计数仍为内存态；无逐文件覆盖率门/真 e2e。
8. **多代理能力单一**：🟡 子代理同进程、一次性；无 workflow 编排；无 goal 体系。

### 4.4 双方共性——不变
- 都强调 **fail-closed 安全默认**（审批/门禁/未知工具）、**取消优先于一切**（mdgo biased select / DSH throwIfAborted 检查点）、**工具结果完整回传模型**。
- 都把 **LLM 调用与工具执行分离**（mdgo LoopHook / DSH waterfall 事件），把策略挂在执行管道上而非工具内部。
- 都在向"多模型/多 provider + 思考程度控制 + 上下文预算可配置"收敛（mdgo 已有 reasoning_effort/双协议/planner_model；DSH 有 provider route + retryableCodes）。

---

## 5. 优化方向与方案（借鉴 DSH）——实施状态更新

> **实施状态（2026-08-23）**：§5.1 全部 10 项与 §5.2 全部 9 项已落地实现（✅，rig 时代实施并随 v3 内核迁移保留，详见文末「实施记录」）；§5.3（P2 架构形态）中**事件溯源、并行工具调度、子代理扩展已由 v3 重构落地**，其余按路线图推进/裁剪。

### 5.0 原则
- 不重写：保留 rig + Tauri 架构，按「能力 seam」思路增量迁移 DSH 范式——**v3 后演进为"自研内核替换 rig"**，原则变为"保留业务层（search/skill/memory/planner/approval/context/subagent），传输/循环/会话层向 core/loop 对齐"。
- 优先级：正确性（P0）→ 契约与语义（P1）→ 架构形态（P2）。

### 5.1 P0 —— 正确性修复（✅ 全部落地）
1. **凭据脱敏**：`mask_secret`（不可逆掩码）+ 单测。
2. **压缩预算字节/字符统一**：`chars().count()` + 中文回归测试；后升级 token 精确计量。
3. **retry_loop 取消死代码**：删除空块；重试条件加 `cancel.is_cancelled()`。
4. **ProviderError 重试收窄**：`is_retryable` 白名单（429/408/5xx/连接/超时；401/403/400/溢出不重试）。
5. **防幻觉守卫看执行成败**：`ToolCallBus::successful_tool_names`。
6. **MaxTurnsError 显式暴露**：截断提示追加 + 区分错误文案/指标码；v3 `TurnOutcome::MaxTurns`。
7. **取消感知的阻塞工具**：取消后 `record_tool_call/result` 跳过（桶不再重建）；v3 调度器感知 cancel。
8. **软门禁语义修正**：`KbSearchConfig.skill_gating` 统一。
9. **git 工具 .mdgo 防护**：git_commit/git_checkout 拒绝 .mdgo。
10. **write 上限与校验顺序**：按字节计 + 词法校验前置。

### 5.2 P1 —— 契约与语义（✅ 全部落地）
1. **工具历史配对单一化**：`core/chat_types.rs`（group_tool_units/paired_tool_call_ids）+ 前端 `css_js/modules/chat-history.js`；v3 事件溯源 `derive_history` 兜底。
2. **精确 token 计量**：`TokenizerBackedEstimator`/`estimate_turns_tokens` 预算门。
3. **上下文溢出重试**：`is_context_overflow_error` 检测 → 预算收紧压缩 → 重试 ≤1 次（v3：`on_request_error` + `retry_prepare`）。
4. **`ask_user_question` 工具**：BASE_TOOLS + oneshot + `question:request` 事件 + `question_respond` IPC + 前端弹窗。
5. **工具输出契约（轻量版 ToolDefinition）**：ToolSpec.output_schema + 结构化结果（read/kb_search/code_lookup 等）。
6. **planner_model 真正生效**：`model_for_role` 按角色路由；缓存满 8 逐条淘汰。
7. **memory 索引去删除 + 取消 100 条上限**：`MemoryVectorIndex::prune` + 全量可见（10k 上限）。
8. **kb_search 输出注入防护补齐**：工具输出统一过 `wrap_suspicious`。
9. **should_plan 触发规则增强**：移除「先/再」误报源 + 疑问句抑制。

### 5.3 P2 —— 架构形态（借鉴 DSH 事件溯源 / 并行调度 / 多代理 / 平台化）
1. **事件溯源会话（最值得投入，量最大）**——✅ **已由 v3 落地**：`session_events` 表（`session_id, seq, event_type, payload, created_at`，主键 (session_id, seq)）+ `Session::derive_history` 增量投影 + "模型可见即已记录"不变式；`chat_messages` 保留兼容读。⚠ 差异：未保留逐 chunk 原始增量（DSH `assistant/chunk`），事件子集为 turn/step/user/assistant/tool/compaction 9 类。
2. **并行工具执行**——✅ **已由 v3 落地（架构级）**：`core/loop/tool_calls.rs`（exclusive barrier + 有界池 + 模型序提交）；`isConcurrencySafe` 等价标记为 `ToolSpec.concurrency_safe`。
3. **可续子代理 + 控制工具**——🟡 部分：`spawn_subagent`（读/写型）+ `parallel_research`（JoinSet）已落地；**剩余工作**：可续子代理（list_agents/send_message/interrupt_agent 控制工具、后台续跑）、子代理 `report` 语义。
4. **目标（goal）体系**——⬜ 未做（按产品定位未采纳；todo_write 已够用）。
5. **`commands/llm.rs` 拆分**——⬜ 未做（agent_query 编排 + v3 生成路径仍同文件；可拆为 agent/query|retrieval|history|events|generate）。
6. **传输与集成**：`agent_query_json` + `mdgo-agent` headless 二进制——⬜ 未做（P2-16 用户跳过）。
7. **eval 入 CI**——🟡 部分：`core/eval` 断言/报告 + YAML 场景已建（单测覆盖）；真实 LLM 执行器 + 质量计数落库待 CLI。
8. **前端收敛**——🟡 部分：流式渲染/历史转换已模块化（chat-history.js/agent.js）；**剩余工作**：sendLlmQuery 路径去重、会话树 UI（fork 可视化）、`trimChatHistory` 与后端预算口径单源。

### 5.4 执行顺序与验收
```
Phase A（正确性）   5.1 全部 → ✅ 落地（rig 时代，v3 延续）
Phase B（契约语义） 5.2.1→5.2.2→5.2.3→5.2.4→5.2.5 → ✅ 落地（rig 时代，v3 延续）
Phase C（架构形态） 5.3.1（事件溯源）→5.3.2→5.3.3→5.3.4→5.3.5 → 🔧 5.3.1/5.3.2 由 v3 重构落地；5.3.3 部分；5.3.4/5.3.5 未做
Phase D（平台化）   5.3.6→5.3.7→5.3.8（按需裁剪） → 🟡 5.3.7 部分；5.3.6 用户跳过；5.3.8 部分
```
每项完成更新 `docs/Agent 能力验收清单.md` 验收清单；Phase C 前先提交基线（已提交：v3 基线 `7278d50`）。

---

## 6. 风险与取舍

| 项 | 说明 | 对策 | v3 状态 |
|---|---|---|---|
| rig 0.41 限制 | 并行工具调度需验证 rig 是否暴露整批 tool_calls | 先 spike；不可行则工具内部并行 + 自研调度留接口 | ✅ 已解决：自研调度器落地，无 rig 约束 |
| 事件溯源迁移 | 双写期一致性与历史数据兼容 | 迁移期 `chat_messages` 兼容读路径 + 一次性 backfill 脚本 | ✅ 已落地：session_events 幂等覆盖 + 兼容读 |
| 单文件 2.6MB index.html/main.html | 前端改动风险 | 改动收敛独立模块 + 手动验收清单 | ✅ 已缓解：模块化（css_js/modules/*.js） |
| token 计量精确化 | 不同模型 tokenizer 差异 | 按模型指纹缓存 tokenizer；未知模型回退近似估算 | ✅ 已落地（BGE WordPiece tokenizer 缓存） |
| 多代理/记忆 token 成本 | 注入与并行增加消耗 | 预算参数可配置；记忆 top-k 收敛；子代理并行上限 | ✅ 持续执行 |

---

## 附：实施记录（rig 时代 §5.1 + §5.2 全部落地；2026-08-23 核对均随 v3 保留）

| 项 | 改动 | 文件 |
|---|---|---|
| P0-1 凭据脱敏 | `api_key` 日志改不可逆掩码（FNV-1a + 长度），新增 `mask_secret` + 单测 | `services/llm.rs` |
| P0-2 压缩预算字符/字节统一 | 预算比较全部改 `chars().count()`（`char_len`/`unit_char_len`），新增中文回归测试 | `core/context/mod.rs` |
| P0-3 retry_loop 取消死代码 | 删除空块；重试条件加 `cancel.is_cancelled()`（取消优先） | `services/llm.rs` |
| P0-4 ProviderError 重试收窄 | `is_retryable_status_code`/`is_retryable_llm_error`（401/403/400/溢出不重试） | `services/llm.rs` |
| P0-5 防幻觉守卫看执行成败 | `ToolCallBus::successful_tool_names`（ok=true 才计） | `core/agent/tools/mod.rs`、`commands/llm.rs` |
| P0-6 MaxTurnsError 显式暴露 | 截断提示追加 + 区分错误文案/指标码（v3：`TurnOutcome::MaxTurns`） | `commands/llm.rs`、`core/loop/error.rs` |
| P0-7 取消感知工具 | `record_tool_call/result_structured` 取消后跳过（总线桶不再重建） | `core/agent/tools/mod.rs` |
| P0-8 软门禁语义修正 | `KbSearchConfig.skill_gating`：主对话 true、子代理 false；4 处门禁统一 | `core/agent/mod.rs`、`tools/mod.rs`、`commands/llm.rs`、`core/subagent/mod.rs` |
| P0-9 git .mdgo 防护 | `git_commit` 拒绝暂存区含 .mdgo；`git_checkout` 拒绝 .mdgo 路径 | `core/agent/tools/mod.rs`（v3：loop_tools.rs） |
| P0-10 write 上限与校验顺序 | 1MB 按字节；词法校验全部前置后再建目录 | `core/agent/tools/mod.rs`（v3：loop_tools.rs） |
| P1-1 工具配对单一化 | 后端 `core/chat_types::{group_tool_units, paired_tool_call_ids}`；前端 `css_js/modules/chat-history.js` | `core/chat_types.rs`、`core/context/mod.rs`、`commands/llm.rs`、`css_js/modules/chat-history.js`、`main.html`、`css_js/modules/agent.js` |
| P1-2 精确 token 计量 | `embedding::estimate_tokens`（BGE WordPiece）+ `TokenizerBackedEstimator` + `estimate_turns_tokens` 预算门 | `core/embedding.rs`、`core/context/mod.rs`、`commands/llm.rs` |
| P1-3 上下文溢出重试 | `is_context_overflow_error` 检测 → 预算收紧 60% 重新压缩 → 重试一次（v3：`on_request_error`/`retry_prepare`） | `commands/llm.rs`、`core/loop/loop.rs` |
| P1-4 ask_user_question | 新工具（BASE_TOOLS）+ oneshot 挂起表 + `question:request` 事件 + `question_respond` IPC + 前端弹窗 | `core/agent/loop_tools.rs`、`core/agent/loop_hooks.rs`、`commands/question.rs`、`lib.rs`、`css_js/modules/agent_global.js` |
| P1-5 工具输出契约 | read/kb_search/code_lookup 输出结构化（files/sources），前端通用结构化卡片渲染器 | `core/agent/loop_tools.rs`、`css_js/modules/agent.js` |
| P1-6 planner_model 生效 | `model_for_role` 按角色路由；客户端缓存满 8 改逐条淘汰 | `lib.rs` |
| P1-7 memory 索引 | `MemoryVectorIndex::prune`（删除路径）+ 全量可见（10k 上限替代 100） | `core/memory/vector.rs`、`core/memory/mod.rs` |
| P1-8 检索工具注入防护 | kb_search/code_lookup 输出过 `wrap_suspicious` | `core/agent/mod.rs`（v3：loop_tools.rs） |
| P1-9 should_plan 增强 | 移除"先/再"误报源；疑问句/轻量查看类抑制；新增回归测试 | `core/agent/planner.rs` |

> 验收（rig 时代）：`cargo check --lib` 通过；`cargo test --lib` 全绿。**v3 现状**：`cargo check --lib` exit 0；`cargo test --lib` **321 passed / 0 failed**（2026-08-23 实测）；JS 模块 `node --check` 通过。

### 追加修复（2026-08-20 用户报告）：BM25 删除返回 Opstamp 导致元数据被清零——保持记录

- **现象**：删除单个文件（`requirements_copy.txt`）后，`chunk_count/vector_count` 元数据被扣成 0，整个知识库显示被清空。
- **根因**：tantivy `IndexWriter::delete_term()` 返回 **Opstamp（操作戳）**而非删除文档数；`Bm25Index::try_delete_document` 误把操作戳当 `deleted_count` 返回。
- **修复**：删除前用 `Count` collector 统计 doc_name 精确匹配的真实文档数，删除后返回真实计数；新增 2 个回归测试。
- **恢复**：索引数据本身完好，元数据归零后执行一次全量重建（`kb_index`）即可恢复计数，不会重复写入（`replace_document_chunks` 先删后写幂等）。

### v3 追加记录（2026-08-23）：自研内核落地后的关键变更

| 项 | 说明 |
|---|---|
| 去 rig | commit `7278d50`：`core/loop` 十一个源文件落地；Cargo.toml 移除 rig-core/rig-agent；`cargo tree` 无 rig；`USE_LOOP_V2` 开关删除（v3 唯一实现） |
| 双协议 | `core/loop/openai.rs`（OpenAiAdapter）+ `core/loop/anthropic.rs`（AnthropicAdapter）；`build_loop_adapter` 按 `LlmConfig.protocol` 选择——原"Agent 模式 Anthropic 直接拒绝"已失效 |
| 工具契约 | `core/loop/tool.rs` ToolSpec（output_schema/timeout_ms/concurrency_safe）+ `HashMapToolRegistry` + `BusToolEventSink`（前端协议零改动） |
| 并行调度 | `core/loop/tool_calls.rs`：exclusive barrier + 有界池（默认 4）+ 模型序提交 + 取消合成结果保回放 |
| 事件溯源 | `core/loop/session.rs`（9 类 SessionEvent + derive_history）+ `services/chat.rs` session_events 表 + `commands/llm.rs` 读写 |
| 业务补全 | loop_tools.rs 26+ 工具（含 BridgeTool/ExternalHttpTool/McpTool/register_mcp_tools/ScheduleTool/SearchBookmarksTool/GetBookmarkTool）；loop_hooks.rs 三业务 Hook |
| 能力迭代 | 知识画布（canvas 技能 + canvas.js）、书签知识资产（core/knowledge/bookmark）、日程管理（core/schedule + ScheduleTool）、RAG P0 批次（token 预算分块/embedding 缓存/标签检索/证据校验/检索 benchmark）、技能正文内存直读 |

---

## 附：评审范围与证据索引

- **附录**：原 DSH 架构行号级调研报告（约 220 包调研 + 传输层/策略层两个子代理并入）已移出 docs/；本文 DSH 侧描述（§3）与核心机制对照以仓库内 `docs/Agent 内核重构蓝图.md`、`docs/去 rig 自研 Agent 内核可行性评估.md` 为准（本仓库以 `core/loop` 现状为准）。
- 前端入口：`css_js/modules/agent.js`、`css_js/modules/agent_global.js`、`css_js/modules/frontend-bridge.js`、`css_js/modules/chat-history.js`、`css_js/modules/canvas.js`、`css_js/modules/schedule.js`、`css_js/modules/skill.js`、`css_js/modules/mcp.js`、`main.html`（聊天区段）
- 后端链路：`tauri/src-tauri/src/commands/llm.rs`、`core/loop/*`（自研内核）、`core/agent/{loop_tools,loop_hooks,planner,limits,external_tools,task_store}.rs`、`core/agent/tools/{mod,cache,canvas}.rs`、`services/llm.rs`、`services/chat.rs`、`core/{context,trace,subagent,memory,validation,mcp,eval,security,bridge,approval,schedule,knowledge}/*`
- 规约与资源：`resources/agent/rag_agent.md`、`resources/agent/chat_agent.md`、`resources/skills/*/SKILL.md`
- DSH（本文 §3 直接引用）：`docs/architecture.zh.md`、`docs/agent-lifecycle.zh.md`、`docs/tool-execution-pipeline.zh.md`、`docs/tool-catalog.md`、`docs/subsystems/{session,core,tools,system-prompt,compaction,approval,subagent,goal,workflow,web-server,scope}.md`、`packages/core/agent-loop/src/{agent,tool-calls}.ts`、`packages/core/tools/src/index.ts`
