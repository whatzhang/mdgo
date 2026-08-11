# mdgo Agent 短板分析与补齐规划（对比 Reasonix / Pi Coding Agent）

> 版本：2026-08-11 · 分析基线：当前工作区代码（含上轮未提交的取消传播/子代理/Planner/Trace/审批/上下文压缩交付）
> 对比对象：
> - **Reasonix**（本机 Agent 平台，内嵌文档 v1.23.0）
> - **Pi Coding Agent**（`@earendil-works/pi-coding-agent`，pi.dev 官方文档 + pi-mono 源码）
> 用途：为"补齐 Agent 短板、达到开源框架与商用 Agent 能力"提供分阶段、可执行的详细规划。

---

## 0. 基线：mdgo 当前 Agent 已具备能力（勿重复实现）

基于 rig 0.41 的单体 RAG Agent，上轮已交付（`docs/agent_capability_archive.md`，未提交 git）：

| 能力 | 现状 | 位置 |
|---|---|---|
| 流式请求真正可取消 | `next_or_cancel` biased select! + drop 级联 | `commands/llm.rs:172-181` |
| 子代理（只读深度调研） | `deep_research` + `read_subagent_result` 分页 + LRU(16) | `core/subagent/mod.rs`、`tools/mod.rs:1924-2047` |
| Planner（规则路由+用户确认） | `should_plan` 纯规则 + 单模型 plan JSON + 60s 挂起表 | `core/agent/planner.rs`、`commands/llm.rs:631-795` |
| Trace（五阶段可观测） | TraceBus 按 request_id 分桶 + 前端面板 | `core/trace.rs`、`index.html:54800` |
| 工具审批门控 | ApprovalGate(edit/delete) + IPC transport + fail-closed | `core/approval/*` |
| 上下文压缩 | 摘要恒保留 + 滑窗（30k 字符预算） | `core/context/mod.rs` |
| Skill 体系 | L1 目录 + L2 动态注入 + active_tools 窄化 + 指标 | `core/skill*.rs` |
| RAG 混合检索 | query_plan 规则路由 + RRF + bge rerank + 聚簇 | `core/search/*`、`core/indexer.rs:802` |

---

## 1. 三方能力对比矩阵

> ✅ 完整能力 / 🟡 部分或基础 / ❌ 缺失 · mdgo 依据为当前代码调查，Reasonix 依据内嵌文档，Pi 依据官方文档。

| 维度 | mdgo（现状） | Reasonix | Pi Coding Agent |
|---|---|---|---|
| **规划** | 🟡 规则路由 + 单模型 plan JSON + 用户确认（`planner.rs:37-51`） | ✅ 规则路由 + 可选独立 `planner_model` + light plan(≤4 步)/full plan（touchpoints、非目标、风险、验收、回滚） | ❌ 不内置，官方 `plan-mode` 扩展 |
| **子代理** | 🟡 只读 `deep_research`（12 轮）+ 分页读 | ✅ `task`/`read_only_task`/`parallel_tasks`/`fleet`(2-64 并行) + write_paths 隔离 + profiles + depth≤2 | ❌ 不内置，`subagent` 示例扩展（exec 派生进程） |
| **长期记忆** | ❌ 无 memory 抽象；仅 SQLite 会话存储 | ✅ `memory`/`remember`/`forget` 结构化记忆 + revision 审计链 + 自动写入授权 | 🟡 无独立记忆层；会话文件 + compaction + branch summary |
| **上下文工程** | 🟡 摘要+滑窗压缩（30k **字符**估算，不落库，每次重算） | ✅ token 经济 boot surface、按需加载、compact | ✅ compaction（**token 精确**、`keepRecentTokens`、split turn、`retainedTail` 自包含检查点、`/compact` 手动） |
| **工具系统** | 🟡 20+ 工具，rig `DynamicTool`，硬编码 Rust | ✅ 全工具面（文件/搜索/LSP/内存/技能/子代理/安装/审查/工作流） | 🟡 7 个内置 + TypeBox schema + 扩展 `registerTool` |
| **工具执行** | 🟡 顺序执行（rig poll 栈内）；仅检索并行 | ✅ 并行（multi_edit、parallel_tasks） | ✅ 默认并行 + `sequential` 可选 + `terminate` + `onUpdate` 流式部分结果 |
| **工具调用历史回流** | ❌ `ChatMessage{role,content}` 丢弃 tool_calls（`llm.rs:145-150`） | ✅ 完整 tool 消息回传 | ✅ AgentMessage 完整保留 |
| **结构化输出** | ❌ 三处 `output_schema: None`（`services/llm.rs:187/343/421`） | ✅ schema/契约化 | ✅ TypeBox 工具参数 + 严格校验 |
| **重试/容错** | ❌ 无重试；失败即 `rag:error` | ✅ 错误处理 + 工具契约 | ✅ provider retry（3 次指数退避）+ `auto_retry` + `stopReason=length` 处理 |
| **反思/自我批评** | ❌ 仅重复工具调用熔断 | ✅ review/security_review 审查层 | ❌ 不内置 |
| **安全/权限** | 🟡 审批门(edit/delete) + SkillGate + 只读子代理 | ✅ 沙箱/权限（allow_write/forbid_read）+ 审批分级 + YOLO | 🟡 无沙箱（设计决定）+ project trust 门 + 容器隔离方案 |
| **多模型** | ❌ 单一 `LlmConfig`（`lib.rs:33-47`），全链路同模型 | ✅ 多 provider + `planner_model` | ✅ 15+ provider、会话内切换、跨 provider 上下文交接、7 级 thinking |
| **会话管理** | 🟡 SQLite 线性会话（create/delete/clear/favorite） | ✅ session 存档 + BM25 检索（`history`/`read_session`） | ✅ JSONL **树形**会话 + `/tree` 分支 + `/fork`/`/clone` + label/export/share |
| **可观测** | 🟡 TraceBus 五阶段 + 前端面板 | ✅ 步骤记录 + trace | ✅ 完整事件流（`turn_start`/`tool_execution_*`/`compaction_*`）+ JSON/RPC 事件流 |
| **集成模式** | 🟡 仅 Tauri IPC（前端 invoke） | ✅ CLI + Desktop | ✅ Interactive / Print / JSON / RPC / **SDK** |
| **扩展生态** | ❌ 工具硬编码；无动态注册/MCP | ✅ Skills + MCP + 插件安装 | ✅ Extensions/Skills/Prompt Templates/Themes/Packages |
| **评测** | ❌ 无 LLM 评测（仅 skill_metrics） | ✅ Delivery 验收标准 + review | ❌ 无 agent 级评测 |
| **前端 Agent UI** | 🟡 工具卡片/计划卡片/审批弹窗/trace 面板（通用弹窗） | ✅ 桌面原生 UI | ✅ TUI（差分渲染、编辑器 autocomplete） |

---

## 2. 短板清单（按影响分级）

### P0 — 正确性 / 体验根基（先做）

| # | 短板 | 现象与证据 | 影响 |
|---|---|---|---|
| P0-1 | **工具调用历史不回流 LLM** | `ChatMessage`/`ChatTurn` 仅 `role+content`（`services/llm.rs:17-20`、`commands/llm.rs:145-150`）；前端 `trimChatHistory` 丢弃 tool_calls（`index.html:54086`）；rig 收到的历史只有文本消息 | 多轮工具任务中模型"失忆"——看不到自己调过什么工具、结果如何，重复调用或逻辑断裂，Agent 正确性根本缺陷 |
| P0-2 | **无跨会话长期记忆** | 全仓无 memory 抽象/向量记忆检索；仅 SQLite 会话存储 + 会话可索引进知识库 | 无法沉淀用户偏好、项目约定、已验证结论；每次会话从零开始 |
| P0-3 | **无结构化输出/校验** | 三处 `output_schema: None`（`services/llm.rs:187/343/421`）；planner 靠宽松 `parse_plan` | 规划/查询扩展/摘要输出不可靠；无 schema 约束的 JSON 解析易碎 |
| P0-4 | **无 LLM 调用重试** | 无重试逻辑；失败直接 `rag:error`；LLM 300s 超时硬断 | 瞬时网络抖动/限流即失败，体验脆弱 |
| P0-5 | **上下文压缩不落库、字符估算** | 压缩每次请求重算；`MAX_MESSAGE_CHARS=30000` 按字符非 token（`llm.rs:1074`）；无检查点 | 长会话成本高、不精确；无 compaction 自包含检查点语义 |
| P0-6 | **单一模型无路由** | 单一 `LlmConfig{endpoint,model,api_key}`（`lib.rs:33-47`）；规划/扩展/摘要/生成全同模型 | 无法用小模型做规划/摘要、强模型做生成；成本与质量不可调 |

### P1 — 智能度 / 能力扩展

| # | 短板 | 现象与证据 | 影响 |
|---|---|---|---|
| P1-7 | **无并行工具执行** | rig 0.41 工具在 poll 栈内顺序执行（`subagent/mod.rs:98-100` 注释）；仅检索 `buffer_unordered(4)` | 多文件读/多检索串行，任务耗时长 |
| P1-8 | **无反思/自我批评/自动重试** | 仅 `guard_duplicate_call` 熔断（`tools/mod.rs:238`） | 生成质量无自检；失败任务无"再试一次"路径 |
| P1-9 | **子代理能力单一** | 仅只读 `deep_research`；无写型子代理、无并行派发、无 profiles、无深度限制配置 | 无法"并行调研多主题/让子代理执行实现" |
| P1-10 | **Planner 粗糙** | 规则启发式（120 字符阈值）；单模型；plan 无 touchpoints/非目标/回滚 | 误判率高；计划质量依赖主模型；无回滚路径 |
| P1-11 | **无会话分支/回滚** | chat 命令仅 create/delete/rename/clear/favorite/set_last | 错误方向的任务无法回退到决策点重来 |
| P1-12 | **无工具结果缓存** | 仅审批已决缓存/规约 mtime 缓存/子代理 LRU | 相同 read/grep 反复执行，浪费 token 与时间 |
| P1-13 | **提示注入防护薄弱** | 仅子代理排除技能激活工具（`agent/mod.rs:768`）；无通用注入检测 | 知识库文档/网页内容可注入恶意指令 |
| P1-14 | **前端 Agent UI 粗糙** | 工具卡片无折叠（`index.html:52930`）；审批/计划共用通用 confirm 弹窗（`54731/54758`）；无会话树/记忆面板 | 复杂任务过程不可检视，信任感弱 |

### P2 — 工程化 / 可扩展性

| # | 短板 | 现象与证据 | 影响 |
|---|---|---|---|
| P2-15 | **工具系统不可扩展** | 工具全部硬编码 Rust `DynamicTool`；无动态注册/MCP | 无法按需装载第三方能力（如网页抓取、数据库） |
| P2-16 | **无 RPC/SDK 集成模式** | 仅 Tauri IPC；无事件流/进程间协议 | 无法被外部程序驱动/嵌入 |
| P2-17 | **无评测框架** | 无 LLM 评测/回归；仅 skill_metrics | Agent 改动无法量化回归验证 |
| P2-18 | **无多 provider/thinking 支持** | 单 provider；无 reasoning effort 控制 | 无法对接订阅类/OAuth/多厂商 |
| P2-19 | **安全加固缺失** | 审批策略硬编码两种工具；无用户级策略配置；无沙箱概念 | 权限模型不可定制 |

---

## 3. 路线图总览

```
Phase 0（正确性根基）────── P0-1 → P0-2 → P0-3 → P0-4 → P0-5 → P0-6
Phase 1（智能度扩展）────── P1-7/8/9（并行+反思+子代理）→ P1-10（planner 增强）→ P1-11/12/13/14
Phase 2（工程化平台化）──── P2-15/16/17/18/19（按需交叉进行）
```

依赖关系：
- P0-1 是全部 Agent 增强的前提（工具执行/子代理/规划都依赖完整消息历史）。
- P0-6（多模型）为 P1-10（独立 planner 模型）铺路。
- P0-3（结构化输出）是 P1-8（反思质量门）与 P2-17（评测）的输入。
- P2-15（动态工具）依赖 P0-1 的工具消息规范化。

每 Phase 结束验收标准：`cargo test --lib` 全绿 + 新增用例 + 手动验收清单（沿用 `docs/agent_capability_testing.md` 风格）+ 前端回归。

---

## 4. 分项详细方案

> 所有改动遵循 SOLID：单一职责（每个新模块只做一件事）、开闭（扩展工具/策略/压缩器通过 trait/注册表，不修改核心 loop）、里氏替换（统一 trait 接口）、接口隔离（调用方依赖最小接口）、依赖倒置（高层依赖抽象，不依赖具体实现）。

### P0-1 工具调用历史回流 LLM（最高优先级）

**目标**：模型在每轮能看到完整的 assistant tool_calls + tool 消息结果，agent loop 语义正确。

**现状问题**：
- `services/llm.rs:17-20` `ChatMessage` 只有 `role/content`；
- `commands/llm.rs:145-150` `prepare_history` 映射丢弃工具字段；
- `commands/llm.rs:153-162` `chat_turns_to_history` 只生成 system/assistant/user 文本；
- 前端 `index.html:54086` `trimChatHistory` 只保留 role/content；
- 数据库 `chat_messages.tool_calls` 列已存在（存 JSON 字符串，`services/chat.rs:74`），只是读取时不解析回传。

**设计（SOLID）**：
1. 数据层：`ChatMessage` 扩展 `tool_calls: Option<Vec<ToolCall>>`、`tool_call_id: Option<String>`（新增 `core/chat_types.rs` 定义 `ToolCall{id,name,arguments}`，对 rig 的 `ToolCall` 做 DTO 隔离——依赖倒置）。
2. 存储层：`services/chat.rs` `save_message` 已存 tool_calls JSON；`get_session_messages` 解析回 `ChatMessage.tool_calls`（开闭：仅改读取侧）。
3. 转换层：`commands/llm.rs` 新增 `chat_turns_to_history` 分支——assistant 消息带 tool_calls 时生成 `Message::assistant` + `ToolCall` 数组，随后追加 `Message::tool()` 结果消息（对齐 OpenAI 协议：tool 消息 role=tool + tool_call_id）。压缩器 `core/context/mod.rs` 需保证"tool 结果与 tool_call 同侧切分"（对齐 Pi 的 cut point 规则），否则工具消息孤儿化。
4. 前端：`trimChatHistory` 保留 tool_calls 字段（`index.html:54086`），`histMessages` 透传。
5. 兼容：老会话无 tool_calls 字段 → `None`，行为不变（向后兼容）。

**涉及文件**：`core/chat_types.rs`、`services/llm.rs`、`services/chat.rs`、`commands/llm.rs`、`core/context/mod.rs`、`index.html`。

**验收**：① 单测：历史含 assistant tool_call + tool 消息时转换产物包含 `Message::tool`；② 集成：模型多轮工具任务（先 grep 再 read 再 edit）不再重复无谓工具调用；③ `cargo test --lib` 全绿。

---

### P0-2 跨会话长期记忆层

**目标**：`MemoryStore` 抽象 + `remember`/`forget`/`search_memory` 工具 + 每轮相关记忆注入，对齐 Reasonix `memory/remember/forget`。

**设计（SOLID）**：
1. 新模块 `core/memory/`（单一职责）：
   - `MemoryStore` trait（`save/get/search/delete/list`）——接口隔离；
   - `SqliteMemoryStore` 实现（复用 `%APPDATA%/com.mdgo` SQLite 连接池模式，参照 `services/chat.rs`）；
   - schema：`memory_items(id, scope, kind, title, body, keywords, source_ref, expires_at, created_at, updated_at, revision)`；revision 单调递增（对齐 Reasonix 审计链）。
2. 检索：`search_memory(query)` 用关键词 + 可选向量（复用 `chat_vectors` 的 embedding 管线，`indexer.rs:1391`），RAG 打分取 top-k。
3. 注入：`agent_query` 生成阶段前，把 top-k 记忆以 `【记忆】` 块注入 preamble（对齐 planner 注入模式 `llm.rs:1020-1024`）。
4. 工具：`remember`（创建/更新，含敏感信息过滤 + 显式确认，对齐 Reasonix"安全写入与确认"）、`forget`、`search_memory`；注册进 `BASE_TOOLS` 与子代理白名单（只读侧仅 search_memory）。
5. 挂起表确认模式复用 `core/approval` 的 oneshot 样板。

**涉及文件**：`core/memory/`（新）、`commands/memory.rs`（新，注册 IPC）、`core/agent/mod.rs`、`core/agent/tools/mod.rs`、`core/subagent/mod.rs`、`lib.rs`。

**验收**：① 单测：写入→检索召回、revision 冲突拒绝覆盖；② 集成：会话 A 记住偏好 → 会话 B 提问自动注入；③ 手动：记忆面板展示/删除。

---

### P0-3 结构化输出与校验

**目标**：LLM 输出用 JSON Schema 约束 + 失败重试，替代宽松文本解析。

**设计**：
1. `services/llm.rs`：`CompletionRequest` 增加 `output_schema: Option<serde_json::Value>`（OpenAI `response_format.json_schema`），三处调用点（规划 `llm.rs:293-379`、查询扩展 `llm.rs:187`、摘要 `llm.rs:421`）按需传入。
2. 校验层：`core/validation/`（单一职责）——`JsonSchemaValidator`（可先用 `jsonschema` crate）+ `parse_or_retry(json, schema, max_retries)` 失败用错误信息引导模型重发（对齐 Pi `stopReason=length` 重发语义）。
3. `parse_plan` 改为先 schema 校验再宽松解析（开闭：保留旧解析为 fallback）。

**涉及文件**：`services/llm.rs`、`core/validation/`（新）、`core/agent/planner.rs`、`Cargo.toml`。

**验收**：① 单测：非法 JSON → 重试 → 合法；schema 拒绝缺字段；② 规划输出 100% 可解析。

---

### P0-4 LLM 调用重试与容错

**目标**：provider 层指数退避重试 + 明确失败语义。

**设计**：
- `services/llm.rs` 的 client 构造处包一层 `RetryClient`（单一职责）：对 429/5xx/超时做指数退避（`maxRetries=3, baseDelayMs=2000, maxDelayMs=60000`，对齐 Pi），业务错误（4xx 非 429、schema 校验失败）不重试；
- 流式请求重试仅在"首 chunk 前失败"时重试（已消费部分不重发，对齐 rig 流语义）；
- 重试事件进 TraceBus（`trace:event` 新增 `retry` detail）。

**涉及文件**：`services/llm.rs`、`commands/llm.rs`、`core/trace.rs`。

**验收**：单测用 mock 抛 429 两次后成功；集成：日志出现 retry 埋点，无重复内容。

---

### P0-5 上下文压缩落库与 token 精确预算

**目标**：压缩结果持久化 + token 计数 + 检查点语义（对齐 Pi compaction）。

**设计**：
1. token 估算：用项目已依赖的 `tokenizers`（`Tokenizer::from_bytes`，BERT 类）统一 `count_tokens`，替代字符估算（`core/context/mod.rs` 预算参数改为 token）。
2. 落库：`chat_sessions` 新增 `compaction_state` 列（JSON：`{summary, kept_from_seq, first_kept_id, tokens_before}`），`prepare_history` 先读检查点再压缩增量（对齐 Pi `firstKeptEntryId`/`retainedTail`）。
3. cut point 规则：压缩切分只在 user/assistant 边界，绝不在 tool 消息中间切（P0-1 之后 tool 消息成对出现）。

**涉及文件**：`core/context/mod.rs`、`services/chat.rs`、`commands/llm.rs`、`core/db/schema.rs`（迁移）。

**验收**：① 单测：摘要+滑窗 token 预算精确、tool 对不跨切分；② 集成：长会话第二次请求直接读检查点，token 成本下降且上下文一致。

---

### P0-6 多模型配置与路由

**目标**：支持"规划/摘要用小模型、生成用主模型"配置（对齐 Reasonix `planner_model`）。

**设计**：
- `LlmConfig` 扩展 `planner_model: Option<String>`、`summary_model: Option<String>`（`lib.rs:33-47`，向后兼容缺省=主模型）；
- `services/llm.rs` 缓存改为按模型指纹多 key（现有 `llm_client_cache` 指纹机制扩展）；
- 路由点：`generate_plan_json` 用 planner_model、压缩摘要用 summary_model、生成用主模型（依赖倒置：`LlmClientProvider::model_for(role)`）。

**涉及文件**：`lib.rs`、`services/llm.rs`、`commands/llm.rs`、前端配置面板。

**验收**：配置 planner_model 后规划请求日志显示走该模型；未配置行为不变。

---

### P1-7 并行工具执行

**目标**：同轮多个工具调用可并行（对齐 Pi 默认 parallel）。

**设计（重要约束：rig 0.41 工具在 poll 栈内顺序执行）**：
- 方案 A（优先，侵入小）：不在 rig 内改，而是在**工具内部**并行——把 `read`（多文件）、`grep`（多 pattern）等工具参数 schema 扩展为数组，内部 `buffer_unordered`；对"检索类"已有并行先例（`llm.rs:897-901`）。
- 方案 B（架构级）：升级/自研 agent loop 或对 rig 的 `Agent` 做并行工具调度层，需评估 rig 0.41 `MultiTurnStreamItem` 是否暴露整批 tool_calls（上轮已确认 rig 工具顺序执行，风险高，放 Phase 2 评估）。
- 验收以方案 A 落地为先：单次工具调用内部并行，行为契约不变。

**验收**：read 3 个文件耗时 ≈ 单个文件耗时（日志验证）；回归全绿。

---

### P1-8 反思/自我批评质量门

**目标**：关键任务生成后增加"检查-修正"循环（Reasonix review 机制的轻量版）。

**设计**：
- 新工具 `self_review`（模型自主触发或规则触发：长答案/多轮工具任务后）：
  - 输入：目标 + 已生成答案 + 工具执行摘要；输出：`{issues[], fixes[]}` JSON（用 P0-3 schema）；
  - 有 issues → 追加一轮修正（preamble 注入"根据以下问题修正"）；无 issues → 结束；
- 预算：`SELF_REVIEW_MAX_ROUNDS=2`、仅主模型、计入 max_turns（改 `DEFAULT_MAX_TURNS` 为可配置）；
- 事件：`rag:status "reviewing"` + TraceBus `reviewing` 阶段。

**涉及文件**：`core/agent/tools/mod.rs`、`core/agent/mod.rs`、`commands/llm.rs`、`core/trace.rs`。

**验收**：触发反思任务日志显示 reviewing→修正轮；无问题答案零额外轮次。

---

### P1-9 子代理体系扩展

**目标**：写型子代理 + 并行派发 + profiles（对齐 Reasonix task/fleet/parallel_tasks）。

**设计（开闭）**：
- `core/subagent/mod.rs` 泛化：`SubagentSpec{tools: 白名单, max_turns, mode: ReadOnly|Write, summary_chars, model}`；
- 新工具 `spawn_subagent`（可并行多次调用，各自独立 request_id；父链等待 `JoinSet` 全部完成或首个失败）与 `parallel_research`（一次调起 2-N 个只读子代理，结果分别入 `LruResultStore`，`read_subagent_result` 已有分页）；
- 写型子代理：`Write` 模式白名单加入 edit/delete + **审批门强制挂载**（复用 `ApprovalGate`，子代理审批事件冒泡到父链前端）；
- profiles：`resources/agent/subagent_profiles/*.md` 定义命名角色（reviewer/implementer/researcher），模型按名调用（对齐 Reasonix SUBAGENT_PROFILES）。

**涉及文件**：`core/subagent/mod.rs`、`core/agent/tools/mod.rs`、`core/agent/mod.rs`、`lib.rs`、`resources/agent/`。

**验收**：① 并行调研 3 主题耗时≈单主题（日志）；② 写型子代理的 edit 触发审批弹窗；③ 无权限越界（白名单+审批双保险）。

---

### P1-10 Planner 增强

**目标**：full plan 结构 + 独立模型 + 回滚指引（对齐 Reasonix GUIDE.md:1168-1191）。

**设计**：
- `Plan` 扩展：`touchpoints[]`（涉及文件/知识域）、`non_goals[]`、`risks[]`、`rollback`（失败回滚步骤）、`est_turns`（`core/agent/planner.rs:64-74`）；
- 模型：`generate_plan_json` 改用 `planner_model`（P0-6 之后可行）；
- 触发规则优化：动词表扩充分词感知（jieba 已依赖），降低 120 字符阈值误判；
- 拒绝/失败路径已有 fail-closed（`llm.rs:752-763`），补"计划执行中失败 → 提示回滚"。

**涉及文件**：`core/agent/planner.rs`、`services/llm.rs`、`commands/llm.rs`、`index.html`（计划卡片渲染 touchpoints/risks/rollback）。

**验收**：复杂任务计划卡片展示非目标/风险/回滚；单测更新 `should_plan` 边界。

---

### P1-11 会话分支/回滚

**目标**：从历史决策点 fork 新会话继续（对齐 Pi `/tree`/`/fork`/`/clone` 的轻量版）。

**设计**：
- `services/chat.rs`：`chat_sessions` 加 `parent_id`/`branch_point` 列；`fork_session(session_id, message_seq)` 命令：复制该点之前消息 + 挂 parent；
- `commands/chat.rs` 新增 `chat_fork` IPC；前端消息卡右键"从此继续"；
- 迁移：`core/db/schema.rs` 兼容旧库。

**涉及文件**：`services/chat.rs`、`commands/chat.rs`、`core/db/schema.rs`、`index.html`。

**验收**：fork 后新会话上下文=分支点快照；原会话不受影响。

---

### P1-12 工具结果缓存

**目标**：read/grep/kb_search 等纯函数结果按"工具+规范化参数+内容指纹"缓存。

**设计**：
- `core/agent/tools/cache.rs`（新）：`ToolResultCache`（LRU，参照 `LruResultStore` 实现）+ 失效策略（文件 mtime 变化即失效——复用 `core/watcher.rs` 事件）；
- 只对只读工具启用（edit/delete/审批类绝不缓存）；缓存命中发 `trace:event` detail=cache_hit。

**涉及文件**：`core/agent/tools/cache.rs`（新）、`core/agent/tools/mod.rs`、`core/trace.rs`。

**验收**：重复 read 同一文件第二次不发 LLM 工具结果（日志 cache_hit）；文件修改后缓存失效。

---

### P1-13 提示注入防护

**目标**：检测并抑制来自检索内容/文件的注入指令。

**设计**：
- 检索上下文注入前的静态扫描（`core/security/injection.rs` 新）：关键词启发（"ignore previous instructions / 忽略以上 / system prompt"）+ 高亮包裹（不静默丢弃，用 `【注意:以下内容含可疑指令】` 包裹并提示模型忽略——可审计）；
- 子代理输出返回父链前同样扫描；
- 策略可开关（配置项 `security.injection_guard: bool`）。

**涉及文件**：`core/security/`（新）、`commands/llm.rs`（context 拼装点 `llm.rs:1020`）、`core/subagent/mod.rs`。

**验收**：构造含注入指令的文档 → 回答不受影响且日志记录告警；误报率人工抽查。

---

### P1-14 前端 Agent UI 增强

**目标**：可检视的 Agent 过程 UI（对齐 Reasonix 桌面 / Pi TUI 的可视化标准）。

**设计**：
- 工具卡片折叠/展开 + 耗时徽标（`index.html:52930` `handleAgentToolEvent` 扩展）；
- 审批专用弹窗（区分计划/审批/确认三态，替代通用 confirm，`54731-54784` 拆分）；
- 会话记忆面板（列出注入的记忆项 + 删除入口）；
- 计划卡片渲染 touchpoints/risks/rollback（P1-10）。

**涉及文件**：`index.html`（CSS + JS，2.6MB 单体内内联模块化函数）。

**验收**：手动用例清单（沿用 `docs/agent_capability_testing.md` 风格逐项过）。

---

### P2-15 动态工具注册 / MCP

**目标**：工具可插拔（对齐 Pi extensions / Reasonix MCP）。

**设计**：
- `core/agent/tool_registry.rs` 扩展 `register_dynamic(name, ToolBuilder)` 公开 API + 配置驱动的外部工具（HTTP JSON-RPC 或 stdio 子进程）适配器；
- MCP 客户端（`rmcp`/`mcp` crate 评估）或先做最小 stdio 协议；
- 前端工具市场面板（后续）。

**验收**：配置文件注册一个外部工具（如 web_fetch）→ 模型可调用。

---

### P2-16 RPC / JSON 事件流模式

**目标**：进程外集成（对齐 Pi `--mode json`/`--mode rpc`/SDK）。

**设计**：
- Tauri 侧暴露 `agent_query_json`（事件流复用现有 `rag:status/rag:done` 协议，`commands/llm.rs` 事件已是结构化）；
- 独立 CLI 二进制 `mdgo-agent`（`tauri/src-tauri/src/bin/`）：stdin JSONL 请求、stdout JSONL 事件（复用 `core/trace.rs` 事件结构）。

**验收**：`echo '{"query":"..."}' | mdgo-agent --rpc` 得到完整事件流。

---

### P2-17 评测框架

**目标**：Agent 能力回归（对齐商用 Agent 的 eval 实践）。

**设计**：
- `core/eval/`（新）：场景集（YAML：`{name, setup, query, expected_tools[], expected_outcome_regex}`）+ runner（真实运行 agent_query 非流式 + 断言）+ 报告（对齐 `skill_metrics` 落库）；
- CI：`cargo test --lib eval_scenarios`（标 `#[ignore]` 需要 LLM 的场景，常规跑 mock 场景）。

**验收**：`cargo test --lib eval -- --ignored` 产出场景通过率报告。

---

### P2-18 多 provider / thinking 支持

**目标**：对齐 Pi 15+ provider 与 thinking level 的简化版。

**设计**：`LlmConfig` 支持 `providers[]`（OpenAI 兼容端点列表 + 订阅类暂缓）+ `reasoning_effort: low|medium|high` 透传（rig 0.41 模型参数扩展）。

**验收**：切换 provider 无需重启（配置指纹热重建已有基础）。

---

### P2-19 安全加固

**目标**：可配置审批策略 + 会话级沙箱概念。

**设计**：
- `core/approval/policy.rs` 改为配置驱动（YAML/TOML：`{tool, match_args, action: allow|ask|deny}`，对齐 Reasonix permissions 分级）；
- 审批超时/通道失败文案修正（hook.rs:57-79 已知误导风险）；
- 只读/只写模式（`read_only_session` 全局开关，对齐子代理只读语义）。

**验收**：配置 deny grep → 调用被拒并提示；策略热加载。

---

## 5. 风险与取舍

| 项 | 说明 | 对策 |
|---|---|---|
| rig 0.41 限制 | 工具顺序执行、无原生并行/多 agent；升级 rig 破坏性大 | 工具内部并行（P1-7 方案 A）；Phase 2 评估自研 loop |
| tool 消息协议 | OpenAI 协议要求 tool 消息紧随对应 assistant tool_call | P0-1 转换层保证配对；压缩切分同侧（P0-5） |
| 单文件 2.6MB index.html | 前端改动风险高 | 改动收敛到独立函数 + 手动验收清单 |
| 模型幻觉/成本 | 记忆注入、反思轮、并行子代理增加 token | 预算参数全部可配置；记忆 top-k 收敛；反思仅关键任务 |
| 数据库迁移 | chat_sessions/chat_messages 加列 | `core/db/schema.rs` 版本化迁移（项目已有 schema 版本重建机制） |
| 未提交工作区 | 上轮交付未提交 git | 实施前先提交基线，避免混淆 diff |

## 6. 建议执行顺序（可独立裁剪）

1. 提交当前工作区基线（上轮交付）。
2. **P0-1**（工具历史回流）→ **P0-4**（重试）→ **P0-3**（结构化输出）→ **P0-5**（压缩落库）→ **P0-2**（记忆）→ **P0-6**（多模型）。
3. **P1-9**（子代理扩展）→ **P1-7**（并行）→ **P1-10**（planner）→ **P1-8**（反思）→ **P1-11~14**。
4. Phase 2 按需。

每完成一项即跑 `cargo test --lib` 并更新 `docs/agent_capability_testing.md` 验收清单。
