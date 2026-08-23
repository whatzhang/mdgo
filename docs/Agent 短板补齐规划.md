# mdgo Agent 短板分析与补齐规划（对比 Reasonix / Pi Coding Agent）

> 最后更新：2026-08-23（原版本：2026-08-11）
> 分析基线：当前工作区代码（含上轮未提交的取消传播/子代理/Planner/Trace/审批/上下文压缩交付）
> 对比对象：
> - **Reasonix**（本机 Agent 平台，内嵌文档 v1.23.0）
> - **Pi Coding Agent**（`@earendil-works/pi-coding-agent`，pi.dev 官方文档 + pi-mono 源码）
> 用途：为"补齐 Agent 短板、达到开源框架与商用 Agent 能力"提供分阶段、可执行的详细规划。
>
> **基线说明（2026-08-23 更新）**：本文档 §0/§1 的能力基线为 **rig 时代**（rig 0.41）快照；现架构为**自研 `core/loop` 内核**（commit `7278d50`「自研 Agent 内核替代 rig」起，现 HEAD=`edab77e`）。原规划中的**架构类短板（顺序工具执行、非事件溯源、工具契约弱）已由重构直接解决**；各 P0/P1/P2 项已标注交付状态。**交付状态总览：P0 全部兑现、P1 基本兑现（个别子项为剩余工作）、P2 部分兑现**。`cargo test --lib` 现为 321 passed / 0 failed。

---

## 0. 基线：mdgo 当前 Agent 已具备能力（勿重复实现）——历史基线（rig 时代）

基于 rig 0.41 的单体 RAG Agent，上轮已交付（`docs/Agent 能力建设归档.md`，未提交 git）：

| 能力 | 现状（rig 时代描述） | 位置 | v3 现状（2026-08-23） |
|---|---|---|---|
| 流式请求真正可取消 | `next_or_cancel` biased select! + drop 级联 | `commands/llm.rs:172-181` | **已随 v3 更新**：`core/loop/loop.rs` 的 `tokio::select!` 检查点 + `CancellationToken`；`next_or_cancel` 已移除 |
| 子代理（只读深度调研） | `deep_research` + `read_subagent_result` 分页 + LRU(16) | `core/subagent/mod.rs`、`tools/mod.rs:1924-2047` | 工具迁至 `core/agent/loop_tools.rs`；`SubagentRunner` 跑自研 `LoopAgent`；新增 `spawn_subagent`（写型）/`parallel_research` |
| Planner（规则路由+用户确认） | `should_plan` 纯规则 + 单模型 plan JSON + 60s 挂起表 | `core/agent/planner.rs`、`commands/llm.rs:631-795` | 保留；full plan 字段（touchpoints/risks/non_goals/rollback）已扩展；规划模型路由生效 |
| Trace（五阶段可观测） | TraceBus 按 request_id 分桶 + 前端面板 | `core/trace.rs`、`index.html:54800` | TraceBus 保留；前端面板逻辑迁至 `css_js/modules/agent.js`；`LlmTraceHook`/`[llm_trace]` 已移除 |
| 工具审批门控 | ApprovalGate(edit/delete) + IPC transport + fail-closed | `core/approval/*` | 保留；`ApprovalHook` 挂 loop 层；`approval.yaml` 配置驱动（P2-19） |
| 上下文压缩 | 摘要恒保留 + 滑窗（30k 字符预算） | `core/context/mod.rs` | 保留；`compaction_state` 落库 + token 精确计量（TokenizerBackedEstimator） |
| Skill 体系 | L1 目录 + L2 动态注入 + active_tools 窄化 + 指标 | `core/skill*.rs` | 保留；`SkillInstructionHook`/`SkillGateHook` 迁至 `core/agent/loop_hooks.rs`；技能正文内存直读（activation.rs） |
| RAG 混合检索 | query_plan 规则路由 + RRF + bge rerank + 聚簇 | `core/search/*`、`core/indexer.rs:802` | 保留；预检索优化器（P0）+ token 预算分块 + embedding 缓存 + 标签检索 + 证据校验 + 检索 benchmark 已落地 |

---

## 1. 三方能力对比矩阵——历史基线（rig 时代）

> 下表 mdgo 列全部为 **rig 时代**快照；✅/🟡/❌ 分级与 Reasonix/Pi 对比结论不变，但 mdgo 列的多个 ❌ 已由 v3 内核与后续批次解决（逐行附 v3 状态）。

| 维度 | mdgo（rig 时代） | v3 状态 | Reasonix | Pi Coding Agent |
|---|---|---|---|---|
| **规划** | 🟡 规则路由 + 单模型 plan JSON + 用户确认（`planner.rs:37-51`） | ✅ full plan + 规划模型路由 | ✅ 规则路由 + 可选独立 `planner_model` + light plan/full plan | ❌ 不内置，官方 `plan-mode` 扩展 |
| **子代理** | 🟡 只读 `deep_research`（12 轮）+ 分页读 | ✅ `spawn_subagent`（读/写）+ `parallel_research` + `SubagentSpec` | ✅ `task`/`read_only_task`/`parallel_tasks`/`fleet` + write_paths 隔离 + profiles + depth≤2 | ❌ 不内置，`subagent` 示例扩展 |
| **长期记忆** | ❌ 无 memory 抽象 | ✅ `MemoryStore` + `remember`/`forget`/`search_memory`（两级作用域 + revision 审计链） | ✅ `memory`/`remember`/`forget` 结构化记忆 + revision 审计链 | 🟡 无独立记忆层 |
| **上下文工程** | 🟡 摘要+滑窗压缩（30k 字符估算，不落库） | ✅ `compaction_state` 落库 + token 精确计量 + 溢出重试 | ✅ token 经济 + 按需加载 + compact | ✅ compaction（token 精确 + 自包含检查点） |
| **工具系统** | 🟡 20+ 工具，rig `DynamicTool` | ✅ 30+ 工具实现 `Tool` trait（ToolSpec 契约） | ✅ 全工具面 | 🟡 7 个内置 + TypeBox schema |
| **工具执行** | 🟡 顺序执行（rig poll 栈内） | ✅ 并行调度器（exclusive barrier + 有界池 + 模型序提交） | ✅ 并行 | ✅ 默认并行 + `sequential` 可选 |
| **工具调用历史回流** | ❌ `ChatMessage{role,content}` 丢弃 tool_calls | ✅ 事件溯源 `session_events` + `derive_history`（配对/孤儿剔除） | ✅ 完整 tool 消息回传 | ✅ AgentMessage 完整保留 |
| **结构化输出** | ❌ 三处 `output_schema: None` | ✅ `output_schema` + `JsonSchemaValidator` + 校验重试 | ✅ schema/契约化 | ✅ TypeBox 严格校验 |
| **重试/容错** | ❌ 无重试 | ✅ `retry_loop` 指数退避（429/5xx/超时，取消优先）+ `LlmError::is_retryable` | ✅ 错误处理 + 工具契约 | ✅ provider retry（3 次指数退避） |
| **反思/自我批评** | ❌ 仅重复工具调用熔断 | ✅ `self_review` 工具（反思质量门） | ✅ review/security_review 审查层 | ❌ 不内置 |
| **安全/权限** | 🟡 审批门(edit/delete) + SkillGate + 只读子代理 | ✅ `approval.yaml` 配置驱动 + 只读模式 + 注入防护 + git .mdgo 防护 | ✅ 沙箱/权限分级 + 审批分级 | 🟡 无沙箱 + project trust 门 |
| **多模型** | ❌ 单一 `LlmConfig` | ✅ `planner_model`/`summary_model` 路由 + OpenAI/Anthropic 双协议 | ✅ 多 provider + `planner_model` | ✅ 15+ provider、会话内切换 |
| **会话管理** | 🟡 SQLite 线性会话 | ✅ 事件溯源（`session_events`）+ `chat_fork` 分支 | ✅ session 存档 + BM25 检索 | ✅ JSONL **树形**会话 + 分支/fork |
| **可观测** | 🟡 TraceBus 五阶段 + 前端面板 | ✅ 保留 + tracing 双输出 + 全局任务状态条 | ✅ 步骤记录 + trace | ✅ 完整事件流 |
| **集成模式** | 🟡 仅 Tauri IPC | 🟡 仍以 Tauri IPC 为主（RPC/SDK 未做，P2-16 用户跳过） | ✅ CLI + Desktop | ✅ Interactive / Print / JSON / RPC / SDK |
| **扩展生态** | ❌ 工具硬编码 | ✅ 外部 HTTP 工具（agent_tools.yaml）+ MCP 工具（`McpTool`/`register_mcp_tools`） | ✅ Skills + MCP + 插件安装 | ✅ Extensions/Skills/Templates/Packages |
| **评测** | ❌ 无 LLM 评测 | 🟡 `core/eval` 断言/报告 + YAML 场景（真实执行器待 CLI）；检索 benchmark（`src/bin/benchmark.rs`） | ✅ Delivery 验收标准 + review | ❌ 无 agent 级评测 |
| **前端 Agent UI** | 🟡 工具卡片/计划卡片/审批弹窗/trace 面板 | ✅ 模块化（`css_js/modules/*.js`）+ 工具卡片折叠/耗时徽标 | ✅ 桌面原生 UI | ✅ TUI |

---

## 2. 短板清单（按影响分级）——含交付状态

### P0 — 正确性 / 体验根基（先做）——**P0 全部兑现 ✅**

| # | 短板 | 现象与证据（rig 时代） | 影响 | 交付状态 |
|---|---|---|---|---|
| P0-1 | **工具调用历史不回流 LLM** | `ChatMessage`/`ChatTurn` 仅 `role+content`；前端 `trimChatHistory` 丢弃 tool_calls | 多轮工具任务中模型"失忆" | ✅ 已兑现：`core/chat_types.rs`（`group_tool_units`/`paired_tool_call_ids`）+ 事件溯源 `session_events`/`derive_history`；前端 `css_js/modules/chat-history.js` |
| P0-2 | **无跨会话长期记忆** | 全仓无 memory 抽象 | 无法沉淀用户偏好 | ✅ 已兑现：`core/memory` `MemoryStore` + `remember`/`forget`/`search_memory` + 两级作用域（dir_path）+ 向量删除路径 |
| P0-3 | **无结构化输出/校验** | 三处 `output_schema: None`；planner 靠宽松 `parse_plan` | 输出不可靠 | ✅ 已兑现：`output_schema` + `core/validation` `JsonSchemaValidator` + 校验修正重试（≤3 次） |
| P0-4 | **无 LLM 调用重试** | 无重试逻辑；失败直接 `rag:error` | 体验脆弱 | ✅ 已兑现：`retry_loop`（基 2s、上限 120s、最多 5 次尝试，取消优先）+ `is_retryable`（401/403/400/溢出不重试） |
| P0-5 | **上下文压缩不落库、字符估算** | 压缩每次重算；`MAX_MESSAGE_CHARS=30000` 按字符 | 长会话成本高 | ✅ 已兑现：`chat_sessions.compaction_state` 落库（摘要+检查点）+ token 精确计量（`TokenizerBackedEstimator`/`estimate_turns_tokens` 预算门）+ cut point 工具单元成组 |
| P0-6 | **单一模型无路由** | 单一 `LlmConfig` | 成本与质量不可调 | ✅ 已兑现（原暂缓，后恢复实现）：`model_for_role` 按角色路由（planner/summary 缺省回退主模型）+ OpenAI/Anthropic 双协议 |

### P1 — 智能度 / 能力扩展——**P1 基本兑现 ✅（个别子项为剩余工作）**

| # | 短板 | 现象与证据（rig 时代） | 影响 | 交付状态 |
|---|---|---|---|---|
| P1-7 | **无并行工具执行** | rig 0.41 工具顺序执行 | 多文件读/多检索串行 | ✅ 已兑现（架构级）：方案 A（read 多路径并行）→ 方案 B 落地为 `core/loop/tool_calls.rs` 并行调度器（exclusive barrier + 有界池 + 模型序提交） |
| P1-8 | **无反思/自我批评/自动重试** | 仅 `guard_duplicate_call` 熔断 | 生成质量无自检 | ✅ 已兑现：`self_review` 工具；`on_request_error` → 压缩后重试（溢出恢复） |
| P1-9 | **子代理能力单一** | 仅只读 `deep_research` | 无法并行调研/写型执行 | ✅ 基本兑现：`SubagentSpec{max_turns, mode}` + `spawn_subagent`（写型强制审批门）+ `parallel_research`（JoinSet 并行）；**剩余工作**：命名 profiles（`subagent_profiles/*.md`）未实现 |
| P1-10 | **Planner 粗糙** | 规则启发式；单模型；plan 无扩展字段 | 误判率高 | ✅ 已兑现：full plan（touchpoints/risks/non_goals/rollback 字段最小化）+ 独立规划模型 + `should_plan` 误报源清理（移除「先/再」、疑问句抑制） |
| P1-11 | **无会话分支/回滚** | chat 命令仅线性操作 | 无法回退到决策点 | ✅ 已兑现：`chat_fork` + `fork_session`（parent 挂接，分支点快照） |
| P1-12 | **无工具结果缓存** | 无缓存 | 重复执行浪费 token | ✅ 已兑现：`core/agent/tools/cache.rs` `ToolResultCache`（LRU 256，文件 mtime 失效） |
| P1-13 | **提示注入防护薄弱** | 仅子代理排除技能激活工具 | 文档可注入指令 | ✅ 已兑现：`core/security` `scan_injection`/`wrap_suspicious`（不静默丢弃，包裹提示）；检索与子代理回传均过防护 |
| P1-14 | **前端 Agent UI 粗糙** | 工具卡片无折叠；通用 confirm 弹窗 | 过程不可检视 | 🟡 基本兑现：工具卡片折叠/耗时徽标、审批/计划弹窗、trace 面板已模块化；**剩余工作**：会话树 UI（fork 可视化）、记忆面板独立入口 |

### P2 — 工程化 / 可扩展性——**P2 部分兑现**

| # | 短板 | 现象与证据（rig 时代） | 影响 | 交付状态 |
|---|---|---|---|---|
| P2-15 | **工具系统不可扩展** | 工具硬编码 `DynamicTool` | 无法装载第三方能力 | ✅ 已兑现：外部 HTTP 工具（`%APPDATA%/com.mdgo/agent_tools.yaml` 配置驱动）+ MCP 工具（`McpTool` + `register_mcp_tools`，`mcp_<server>_<tool>`）+ `BridgeTool`（前端桥） |
| P2-16 | **无 RPC/SDK 集成模式** | 仅 Tauri IPC | 无法外部驱动 | ❌ **未兑现（用户明确跳过）**：方案保留在 §4 P2-16，列为剩余工作/可选 |
| P2-17 | **无评测框架** | 无 LLM 评测 | 改动无法量化回归 | 🟡 部分兑现：`core/eval`（EvalScenario/`evaluate_scenario`/YAML 加载，断言与报告可单测）；**真实 LLM 执行器待 CLI/headless 接入**；检索侧 `src/bin/benchmark.rs`（Retrieval Benchmark）已落地 |
| P2-18 | **无多 provider/thinking 支持** | 单 provider | 无法对接订阅类/多厂商 | 🟡 部分兑现：`reasoning_effort` 透传（low/medium/high）+ OpenAI/Anthropic 双协议（LlmAdapter seam）；**剩余工作**：订阅类/OAuth、15+ provider、Anthropic Agent 模式工具协议面 |
| P2-19 | **安全加固缺失** | 审批策略硬编码 | 权限模型不可定制 | 🟡 基本兑现：`approval.yaml` 配置驱动（`{tool, match_args, action: allow|ask|deny}`）+ 只读模式（策略表达）+ git 工具 `.mdgo` 防护 + 凭据脱敏；**剩余工作**：进程沙箱概念（对齐 DSH read-only/workspace-write/danger-full-access）未做 |

---

## 3. 路线图总览

```
Phase 0（正确性根基）────── P0-1 → P0-2 → P0-3 → P0-4 → P0-5 → P0-6   ✅ 全部兑现
Phase 1（智能度扩展）────── P1-7/8/9（并行+反思+子代理）→ P1-10（planner 增强）→ P1-11/12/13/14   ✅ 基本兑现
Phase 2（工程化平台化）──── P2-15/16/17/18/19（按需交叉进行）   🟡 部分兑现（P2-16 用户跳过）
```

依赖关系（rig 时代规划，落地时已按序兑现）：
- P0-1 是全部 Agent 增强的前提（工具执行/子代理/规划都依赖完整消息历史）——v3 事件溯源即此前提的实现。
- P0-6（多模型）为 P1-10（独立 planner 模型）铺路——均已兑现。
- P0-3（结构化输出）是 P1-8（反思质量门）与 P2-17（评测）的输入——已兑现。
- P2-15（动态工具）依赖 P0-1 的工具消息规范化——已兑现（v3 Tool trait 契约）。

每 Phase 结束验收标准：`cargo test --lib` 全绿 + 新增用例 + 手动验收清单（沿用 `docs/Agent 能力验收清单.md` 风格）+ 前端回归。

---

## 4. 分项详细方案——含交付状态与实现名更正

> 所有改动遵循 SOLID（原设计原则，落地时贯彻）：单一职责、开闭、里氏替换、接口隔离、依赖倒置。
> **实现名更正说明**：原方案中的设计名（如 `RetryClient`、`SqliteMemoryStore`、`services/openai.rs`、`chat_turns_to_history`、`DynamicTool`、`tool_registry.rs`、`SUBAGENT_MAX_TURNS`、`index.html` 内联）与最终实现名存在漂移，本节约对当前代码核实后的实际符号。

### P0-1 工具调用历史回流 LLM（最高优先级）——✅ 已兑现

**目标**：模型在每轮能看到完整的 assistant tool_calls + tool 消息结果，agent loop 语义正确。

**v3 实现**：
1. 数据层：`core/chat_types.rs` 定义统一配对语义（`group_tool_units`/`paired_tool_call_ids`，对齐 OpenAI 协议 cut point 规则）；`ChatMessage` 含 `tool_calls`/`tool_call_id`。
2. 会话层：**事件溯源** `core/loop/session.rs` `Session`（append-only 事件日志 + `derive_history` 增量投影，call_id 配对、孤儿剔除）；`session_events` SQLite 表持久化（`commands/llm.rs` `upsert_session_events`/`load_session_events`）。
3. 转换层：`seed_session_from_messages`（`ChatMessage` → `SessionEvent`，纯函数可单测）；原 `chat_turns_to_history` 函数**已移除**，其职责由 `derive_history` 承担。
4. 前端：`css_js/modules/chat-history.js`（groupToolUnits/expandToolHistory/trimChatHistory，main.html/agent.js 薄包装委托）。
5. 兼容：老会话无 tool_calls 字段 → `None`，行为不变。

**涉及文件（实现）**：`core/chat_types.rs`、`core/loop/session.rs`、`services/chat.rs`、`services/llm.rs`、`commands/llm.rs`、`css_js/modules/chat-history.js`。

**验收**：① 单测：`derive_history` 配对/孤儿剔除；② 集成：模型多轮工具任务不再重复无谓工具调用；③ `cargo test --lib` 全绿（321/321）。

---

### P0-2 跨会话长期记忆层——✅ 已兑现

**目标**：`MemoryStore` 抽象 + `remember`/`forget`/`search_memory` 工具 + 每轮相关记忆注入。

**v3 实现**：
1. `core/memory/mod.rs`：`MemoryStore`（struct，`open_at(db_path)`）——原设计名 `SqliteMemoryStore` **实现为 `MemoryStore`**；schema `memory_items(id, scope, dir_path, kind, title, body, keywords, source_ref, expires_at, created_at, updated_at, revision)`；revision 单调递增。
2. 检索：`search_hybrid`（关键词 FTS5 ∪ 向量，RRF 融合，两级作用域：当前库 ∪ 全局）。
3. 注入：生成前把 top-k 记忆注入 preamble（`【长期记忆（与本问题相关）】` 块）。
4. 工具：`remember`/`forget`/`search_memory`（loop_tools.rs）；子代理只读白名单仅含 `search_memory`。
5. 向量索引删除路径：`MemoryVectorIndex::prune` + 全量可见（10k 上限替代 100 条）。

**涉及文件（实现）**：`core/memory/{mod,vector}.rs`、`core/agent/loop_tools.rs`、`core/subagent/mod.rs`、`commands/memory.rs`（IPC）、`lib.rs`。

**验收**：① 单测：写入→检索召回、revision 递增；② 集成：会话 A 记住偏好 → 会话 B 提问自动注入；③ 手动：记忆更新/删除/跨会话（`docs/Agent 能力验收清单.md` 用例 22-24）。

---

### P0-3 结构化输出与校验——✅ 已兑现

**目标**：LLM 输出用 JSON Schema 约束 + 失败重试。

**v3 实现**：
1. `CompletionRequest.output_schema`（OpenAI `response_format.json_schema`），规划/查询扩展/摘要/评审按需传入。
2. `core/validation/mod.rs`：`JsonSchemaValidator`（`validate`/`validate_json_text`）+ `build_fix_prompt`（错误信息引导模型重发）。
3. `parse_plan` 先 schema 校验再宽松解析（旧解析保留为 fallback）。

**涉及文件（实现）**：`services/llm.rs`、`core/validation/mod.rs`、`core/agent/planner.rs`、`core/loop/llm_seam.rs`。

**验收**：① 单测：非法 JSON → 重试 → 合法；schema 拒绝缺字段；② 规划输出 100% 可解析（`docs/Agent 能力验收清单.md` 用例 19）。

---

### P0-4 LLM 调用重试与容错——✅ 已兑现

**目标**：provider 层指数退避重试 + 明确失败语义。

**v3 实现**：原设计名 `RetryClient` **实现为 `retry_loop`**（`services/llm.rs:215`）——泛型重试函数（对 429/408/5xx/连接/超时指数退避，基 2s、上限 120s、最多 5 次尝试，取消优先）；业务错误（401/403/400/ContextOverflow/InvalidRequest）不重试（`is_retryable_llm_error`/`LlmError::is_retryable`）。流式请求重试语义由 `LoopAgent` 的 `on_request_error`（溢出压缩后重发 ≤1 次）承担。

**涉及文件（实现）**：`services/llm.rs`、`core/loop/{types,error,loop}.rs`、`core/agent/loop_hooks.rs`。

**验收**：单测 mock 抛 429 两次后成功（`retry_loop_*` 系列）；集成：日志出现重试埋点，无重复内容（`docs/Agent 能力验收清单.md` 用例 20）。

---

### P0-5 上下文压缩落库与 token 精确预算——✅ 已兑现

**目标**：压缩结果持久化 + token 计数 + 检查点语义。

**v3 实现**：
1. token 计量：`TokenizerBackedEstimator`/`embedding::estimate_tokens`（BGE WordPiece）+ `estimate_turns_tokens` 预算门（未超预算零压缩），替代字符估算（`MAX_MESSAGE_CHARS` 字符口径退役）。
2. 落库：`chat_sessions.compaction_state` 列（JSON：summary + 检查点），`load_compaction_checkpoint` 先读检查点再压缩增量。
3. cut point：压缩切分只在 user/assistant 边界，工具消息成对（`group_tool_units` 同侧切分）。

**涉及文件（实现）**：`core/context/mod.rs`、`core/embedding.rs`、`services/chat.rs`、`commands/llm.rs`。

**验收**：① 单测：token 预算精确、tool 对不跨切分、compaction_state 往返；② 集成：长会话第二次请求直接读检查点（`docs/Agent 能力验收清单.md` 用例 21）。

---

### P0-6 多模型配置与路由——✅ 已兑现（原暂缓，后恢复实现）

**目标**：支持"规划/摘要用小模型、生成用主模型"配置。

**v3 实现**：`LlmConfig` 含 `planner_model`/`summary_model`（缺省=主模型）；`model_for_role` 按角色路由（planner/summary/generate）；客户端缓存按模型指纹多 key + 满 8 逐条淘汰；`reasoning_effort` 透传；OpenAI/Anthropic 双协议经 `build_loop_adapter` 选择。

**涉及文件（实现）**：`lib.rs`、`services/llm.rs`、`commands/llm.rs`、前端配置面板。

**验收**：`docs/Agent 能力验收清单.md` §11（用例 40-45）。

---

### P1-7 并行工具执行——✅ 已兑现（架构级）

**目标**：同轮多个工具调用可并行。

**v3 实现**：原方案 A（工具内部并行：read 多路径/grep 多 pattern）已落地；方案 B 落地为**自研并行调度器** `core/loop/tool_calls.rs`——按 `ToolSpec.concurrency_safe` 分组：exclusive 串行成 barrier，concurrency_safe 走有界池（`LoopConfig.max_parallel_tools` 默认 4），结果严格按模型序提交；写工具（edit/write/delete/git_commit/multi_edit）一律 exclusive。

**涉及文件（实现）**：`core/loop/{tool,tool_calls,loop}.rs`、`core/agent/loop_tools.rs`。

**验收**：read 3 个文件耗时 ≈ 单个文件耗时；写工具绝不并行（`docs/Agent 能力验收清单.md` 用例 27 + 单测）。

---

### P1-8 反思/自我批评质量门——✅ 已兑现

**目标**：关键任务生成后增加"检查-修正"循环。

**v3 实现**：`self_review` 工具（loop_tools.rs）：输入目标 + 已生成答案 + 工具执行摘要 → `{issues[], fixes[]}`；有 issues 追加修正轮；计入 max_turns；`rag:status "reviewing"` + TraceBus `reviewing` 阶段。

**涉及文件（实现）**：`core/agent/loop_tools.rs`、`commands/llm.rs`、`core/trace.rs`。

**验收**：`docs/Agent 能力验收清单.md` 用例 29。

---

### P1-9 子代理体系扩展——✅ 基本兑现（profiles 为剩余工作）

**目标**：写型子代理 + 并行派发 + profiles。

**v3 实现**：`core/subagent/mod.rs` `SubagentSpec{tools 白名单, max_turns, mode: ReadOnly|Write, summary_chars}`（原 `SUBAGENT_MAX_TURNS` 常量 → `SubagentSpec.max_turns` 字段）；`spawn_subagent`（mode=read_only|write，写型强制审批门）、`parallel_research`（`JoinSet` 并行派发 2-N 个只读子代理，结果分别入 `LruResultStore`）；`filter_registry` 白名单过滤。

**剩余工作**：命名 profiles（`resources/agent/subagent_profiles/*.md`，reviewer/implementer/researcher 角色）未实现。

**涉及文件（实现）**：`core/subagent/mod.rs`、`core/agent/loop_tools.rs`、`lib.rs`。

**验收**：① 并行调研 3 主题耗时≈单主题；② 写型子代理的 edit 触发审批弹窗；③ 无权限越界（`docs/Agent 能力验收清单.md` 用例 25/26）。

---

### P1-10 Planner 增强——✅ 已兑现

**目标**：full plan 结构 + 独立模型 + 回滚指引。

**v3 实现**：`Plan` 扩展 `touchpoints[]`/`non_goals[]`/`risks[]`/`rollback[]`（字段最小化输出，`services/llm.rs` generate_plan_json 结构化约束 + 空数组省略）；`generate_plan_json` 走 planner_model（P0-6）；`should_plan` 移除「先/再」误报源、疑问句/轻量查看类抑制；拒绝/失败 fail-closed 保留。

**涉及文件（实现）**：`core/agent/planner.rs`、`services/llm.rs`、`commands/llm.rs`、`css_js/modules/agent.js`（计划卡片渲染 touchpoints/risks/rollback）。

**验收**：`docs/Agent 能力验收清单.md` 用例 7/28。

---

### P1-11 会话分支/回滚——✅ 已兑现

**目标**：从历史决策点 fork 新会话继续。

**v3 实现**：`services/chat.rs` `fork_session(session_id, message_seq)`（分支点前消息快照 + parent 挂接）；`commands/chat.rs` `chat_fork` IPC；前端消息卡"分支"按钮。

**涉及文件（实现）**：`services/chat.rs`、`commands/chat.rs`、前端（main.html + 模块）。

**验收**：fork 后新会话上下文=分支点快照；原会话不受影响（`docs/Agent 能力验收清单.md` 用例 30）。

---

### P1-12 工具结果缓存——✅ 已兑现

**目标**：read/grep/kb_search 等纯函数结果按"工具+规范化参数+内容指纹"缓存。

**v3 实现**：`core/agent/tools/cache.rs` `ToolResultCache`（LRU 256，访问序淘汰）+ `tool_result_cache()` 全局单例；失效策略=文件 mtime（`get(key, mtime_ns)`）；只对只读工具启用（edit/delete/审批类绝不缓存）。

**涉及文件（实现）**：`core/agent/tools/cache.rs`、`core/agent/tools/mod.rs`。

**验收**：重复 read 同一文件第二次不读盘（缓存透明）；文件修改后缓存失效（`docs/Agent 能力验收清单.md` 用例 31）。

---

### P1-13 提示注入防护——✅ 已兑现

**目标**：检测并抑制来自检索内容/文件的注入指令。

**v3 实现**：`core/security/mod.rs` `scan_injection`（中英文注入模式关键词启发，大小写不敏感）+ `wrap_suspicious`（**不静默丢弃**，`【⚠ 安全提示：检测到...提示注入指令】` 包裹并提示模型忽略——可审计）；检索上下文与 kb_search/code_lookup 工具输出、子代理回传均过防护。

**涉及文件（实现）**：`core/security/mod.rs`、`commands/llm.rs`、`core/agent/loop_tools.rs`。

**验收**：构造含注入指令的文档 → 回答不受影响且日志记录告警（`docs/Agent 能力验收清单.md` 用例 32 + 单测）。

---

### P1-14 前端 Agent UI 增强——🟡 基本兑现（会话树/记忆面板为剩余工作）

**目标**：可检视的 Agent 过程 UI。

**v3 实现**：工具卡片折叠/展开 + 耗时徽标；审批/计划/提问三态弹窗；trace 阶段面板；**前端模块化**（原 `index.html` 内联 → `css_js/modules/*.js`：agent.js / chat-history.js / agent_global.js / frontend-bridge.js / canvas.js / schedule.js / skill.js / mcp.js；Tauri 主入口 `main.html`）。

**剩余工作**：会话树 UI（fork 可视化）、记忆面板独立入口。

**涉及文件（实现）**：`css_js/modules/*.js`、`main.html`。

**验收**：`docs/Agent 能力验收清单.md` 用例 12/13/16/33。

---

### P2-15 动态工具注册 / MCP——✅ 已兑现

**目标**：工具可插拔。

**v3 实现**：`core/agent/external_tools.rs`（`%APPDATA%/com.mdgo/agent_tools.yaml` 配置驱动 HTTP 工具，`ExternalHttpTool`，重名跳过 + 响应截断护栏）；MCP 客户端保留（`core/mcp/*`），工具适配为 `McpTool` + `register_mcp_tools`（`mcp_<server>_<tool>` 命名 + required 校验 + 放行集并入）；`BridgeTool`（pomodoro/raw-parse/open-ui 前端桥，技能声明门控 + 5s 超时）；注册表 `HashMapToolRegistry`（原 `tool_registry.rs` 已删除，注册表迁至 `core/loop/tool.rs` + `core/agent/loop_tools.rs::build_loop_tool_registry`）。

**涉及文件（实现）**：`core/agent/external_tools.rs`、`core/agent/loop_tools.rs`、`core/mcp/*`、`core/loop/tool.rs`。

**验收**：配置文件注册一个外部工具 → 模型可调用（`docs/Agent 能力验收清单.md` 用例 35/36）。

---

### P2-16 RPC / JSON 事件流模式——❌ 未兑现（用户跳过，列为剩余工作/可选）

**目标**：进程外集成。

**设计（保留）**：Tauri 侧暴露 `agent_query_json`（事件流复用现有 `rag:status/rag:done` 协议）；独立 CLI 二进制 `mdgo-agent`（stdin JSONL 请求、stdout JSONL 事件，复用 `core/trace.rs` 事件结构）。

**状态**：用户明确跳过；`core/eval` 真实执行器与检索 benchmark 如需自动化可复用同一事件结构。

---

### P2-17 评测框架——🟡 部分兑现

**目标**：Agent 能力回归。

**v3 实现**：`core/eval/mod.rs`（`EvalScenario` YAML 加载 + `builtin_scenarios` + `evaluate_scenario` 纯断言 + `EvalReport`）；依赖倒置——执行由调用方闭包注入，**真实 LLM 执行器待 CLI/headless 接入**（当前仅单测覆盖断言与报告）。检索侧：`src/bin/benchmark.rs`（Retrieval Benchmark：`cargo run --bin benchmark -- --kb <dir> --queries <queries.jsonl> --expected <expected.jsonl>`，recall@10 + 分阶段耗时）已落地。

**涉及文件（实现）**：`core/eval/mod.rs`、`src/bin/benchmark.rs`。

**验收**：`cargo test --lib` 观察 eval 断言用例通过（`docs/Agent 能力验收清单.md` 用例 34）。

---

### P2-18 多 provider / thinking 支持——🟡 部分兑现

**目标**：多 provider 与 thinking level 的简化版。

**v3 实现**：`reasoning_effort: low|medium|high` 透传（`LLMClient.apply_common_params` → `adapter` 附加参数）；**OpenAI 兼容 / Anthropic Messages 双协议**（`build_loop_adapter` 按 `LlmConfig.protocol` 选择 `OpenAiAdapter`/`AnthropicAdapter`）——超过原"单 provider"。

**剩余工作**：订阅类/OAuth provider、15+ provider 多端点路由、Anthropic Agent 模式工具协议面（暂为纯对话语义）、thinking budget 映射（`reasoning_effort` → Anthropic `thinking` 档位暂未接）。

**验收**：`docs/Agent 能力验收清单.md` 用例 42/45。

---

### P2-19 安全加固——🟡 基本兑现（沙箱为剩余工作）

**目标**：可配置审批策略 + 会话级沙箱概念。

**v3 实现**：`core/approval/policy.rs` 配置驱动（`%APPDATA%/com.mdgo/approval.yaml`：`{tool, match_args, action: allow|ask|deny}`）；只读模式（策略表达：edit/delete 均 deny = 只读会话语义）；审批 fail-closed 三分（超时/通道不可用/策略拒绝带差异化模型反馈）；git 工具 `.mdgo` 防护；凭据脱敏（api_key 不可逆掩码）。

**剩余工作**：进程沙箱概念（对齐 DSH read-only/workspace-write/danger-full-access 三档）未做——当前以白名单 + 审批 + 只读模式近似。

**验收**：`docs/Agent 能力验收清单.md` 用例 37/38/39。

---

## 5. 风险与取舍

| 项 | 说明 | 对策 | v3 状态 |
|---|---|---|---|
| rig 0.41 限制 | 工具顺序执行、无原生并行/多 agent；升级 rig 破坏性大 | 工具内部并行（P1-7 方案 A）；Phase 2 评估自研 loop | ✅ **已解除**：自研内核落地，rig 依赖移除 |
| tool 消息协议 | OpenAI 协议要求 tool 消息紧随对应 assistant tool_call | P0-1 转换层保证配对；压缩切分同侧（P0-5） | ✅ 已解决：`derive_history` 配对/孤儿剔除 + `group_tool_units` 同侧切分 |
| 单文件 2.6MB index.html | 前端改动风险高 | 改动收敛到独立函数 + 手动验收清单 | ✅ 已缓解：前端模块化（`css_js/modules/*.js`） |
| 模型幻觉/成本 | 记忆注入、反思轮、并行子代理增加 token | 预算参数全部可配置；记忆 top-k 收敛；反思仅关键任务 | ✅ 持续执行 |
| 数据库迁移 | chat_sessions/chat_messages 加列 | `core/db/schema.rs` 版本化迁移（项目已有 schema 版本重建机制） | ✅ 已执行：compaction_state 列、session_events 表、memory_items dir_path 列均带兼容迁移 |
| 未提交工作区 | 上轮交付未提交 git | 实施前先提交基线，避免混淆 diff | ✅ 已提交：`7278d50` 起为 v3 基线 |

## 6. 建议执行顺序（可独立裁剪）

1. 提交当前工作区基线（上轮交付）——**已完成**（v3 基线 commit `7278d50`）。
2. **P0-1**（工具历史回流）→ **P0-4**（重试）→ **P0-3**（结构化输出）→ **P0-5**（压缩落库）→ **P0-2**（记忆）→ **P0-6**（多模型）——**P0 全部兑现**。
3. **P1-9**（子代理扩展）→ **P1-7**（并行）→ **P1-10**（planner）→ **P1-8**（反思）→ **P1-11~14**——**P1 基本兑现**。
4. Phase 2 按需——**P2 部分兑现**。

**剩余工作汇总**（未兑现/部分兑现项）：
- P1-9：子代理命名 profiles（`subagent_profiles/*.md`）。
- P1-14：会话树 UI（fork 可视化）、记忆面板独立入口。
- P2-16：RPC/SDK 集成模式（用户跳过，可选）。
- P2-17：`core/eval` 真实 LLM 执行器（待 CLI/headless）。
- P2-18：订阅类/OAuth provider、Anthropic Agent 模式工具协议面、thinking budget 映射。
- P2-19：进程沙箱三档（read-only/workspace-write/danger-full-access）。

每完成一项即跑 `cargo test --lib` 并更新 `docs/Agent 能力验收清单.md` 验收清单。
