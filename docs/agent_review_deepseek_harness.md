# mdgo Agent 全链路 Code Review 与 DeepSeek Harness 对比及优化方案

> 版本：2026-08 · 分析基线：当前工作区代码（`main.html` / `css_js/modules/*.js` / `tauri/src-tauri/src`，git HEAD `a19581c` + 未提交的 `agent.js`/`main.html`/`canvas SKILL.md` 改动）
> 对比对象：**DeepSeek Harness（DSH）**（`G:\gitProject\deepseek-harness`，Cordis 插件化 Agent 平台，当前会话即运行于其上）
> 方法：入口→出口逐环节走查 mdgo Agent 链路；对 DSH 按官方文档（docs/architecture、agent-lifecycle、tool-execution-pipeline、tool-catalog、各 subsystems/*）与关键源码（`packages/core/agent-loop`、`packages/core/tools` 等）剖析；最后给出基于 DSH 机制的优化方案。

---

## 0. 执行摘要

- mdgo Agent 是一条**工程素养很高**的链路：取消传播、fail-open/fail-closed、部分结果保留、防幻觉守卫、注入防护、工具轨迹可视化等生产级细节均已到位，**未发现 P0 级崩溃/数据丢失缺陷**。
- 主要短板集中在**架构形态**而非单点 bug：① 依赖 rig 0.41 的「顺序工具执行 + 有限 Hook 面」；② 会话历史是「消息表 + 手工配对」，工具调用配对逻辑在前后端存在 4 份近似实现（易漂移）；③ 工具系统无输出契约/超时/并发标记；④ 上下文预算是字符估算而非精确 token；⑤ 前端 2.6MB 单文件、聊天与 RAG 两条流式路径重复；⑥ 无事件溯源，回放/分支/恢复能力有限。
- **P1 级问题 8 条**（见 §2）：API Key 明文日志、压缩预算字节/字符混用、防幻觉守卫只看工具名不看执行成败、MaxTurnsError 静默截断、取消后阻塞工具重建总线桶、软门禁语义反转、git 工具缺 .mdgo 防护、write 上限按字符计 + 校验顺序错误。
- DSH 在**事件溯源会话、并行工具调度、工具契约、插件化组合、多代理/工作流/目标体系、精确 token 计量**等方面提供可直接借鉴的范式。
- 优化路线：**P0（正确性）**：§5.1 十项（凭据脱敏、字节/字符统一、守卫修正、取消感知工具等）；**P1（架构）**：工具历史配对单一化、精确 token 计量、上下文溢出重试、`ask_user_question`、工具输出契约；**P2（平台）**：事件溯源会话、并行工具调度、可续子代理 + 控制工具、目标体系、`commands/llm.rs` 拆分、JSON 事件流 + headless CLI、eval 入 CI。

---

## 1. mdgo Agent 链路总览（入口 → 出口）

### 1.1 双入口

| 模式 | 前端入口 | IPC 命令 | 后端 |
|---|---|---|---|
| Agent / RAG | `css_js/modules/agent.js` → `sendRagQuery()`（agent.js:304） | `agent_query` | `commands/llm.rs:768`（约 1400 行，链路主战场） |
| 纯对话 | `main.html` → `sendChatMessage()` → `sendLlmQuery()`（main.html:51332） | `kb_llm_query` | `commands/llm.rs:2183`；Anthropic 协议走 `kb_llm_query_anthropic`（:2431） |

前端统一流程：`chatMessages`（内存）→ `expandToolHistory()`（agent.js:186，把 assistant.toolCalls 还原为协议消息 + 配对 tool 结果）→ `trimChatHistory()`（main.html:51204，按 token 预算裁剪，工具单元成组）→ `estimateMessagesTokens()` → `invoke('agent_query'|'kb_llm_query', …)`；AbortController → `kb_cancel_task`。

### 1.2 后端 `agent_query` 执行链（Stage 0 → 4 → 出口）

```
agent_query (llm.rs:768)
├─ 任务注册：TaskRegistry(cancel token) + agent_tasks 状态中心 + 同会话旧任务替换
├─ LLM 客户端：get_or_create_llm_client（配置指纹缓存）；Anthropic 协议直接拒绝（Agent 模式仅 OpenAI 兼容）
├─ Stage 0  技能预激活：手动触发/会话挂载 → resolve_preactivated（spawn_blocking）
├─ Stage 0.5 轻量规划：should_plan 规则门 → generate_plan_json（结构化校验 ≤3 次修正）
│            → plan:request 前端确认（oneshot + 60s 超时 fail-closed；取消/拒绝/超时三态收尾）
├─ Stage 1-3 预检索（仅当预激活技能声明检索工具）：
│   1. 原始查询嵌入 + LLM 查询扩展（tokio::join! 并行，扩展带 10s 独立超时 fail-open）
│   2. 扩展批量向量化 + embedding 语义去重 + 符号实体发现（代码意图时）
│   3. 多查询 hybrid_recall（buffer_unordered 并行）→ 候选池统一精排（rerank_pool）
│      → aggregate_hits（文档级聚合 + 跨查询一致性加成）→ build_context_text
│      → wrap_suspicious（注入防护包裹）→ build_sources（引用去重）
├─ Stage 4  生成：
│   任务计划注入 preamble → 记忆注入（search_hybrid 关键词∪向量 RRF，top-3，两级作用域）
│   → build_rag_agent（rig AgentBuilder + 6 类 Hook，见 1.3）
│   → 压缩检查点应用（CompactionState：摘要 + cutoff_msg_id）→ prepare_history（摘要+滑窗）
│   → 写回新检查点（仅 summarize+window 成功时）→ chat_turns_to_history（工具消息配对还原）
│   → agent.stream_chat(...).into_future() → 流式消费循环（next_or_cancel 偏置 select!）
│       文本增量 → rag:delta（同时写 agent_tasks.append_text 供切页恢复）
│       工具调用 → 日志 + tools_called 收集（防幻觉校验输入）
│       每循环 emit_pending_tool_events（ToolCallBus 消费式转发 agent:tool_call/result）
├─ 出口：rag:done{content, sources(预检索∪工具检索 merge_search_sink), token 用量}
│   apply_anti_hallucination_guard（ACTION_CLAIMS 声明表）→ apply_grounding_validator
│   取消/失败路径：保留部分内容 → rag:done/rag:error；任务状态中心收尾；trace 收尾
└─ 收尾：tool_call_bus().clear + skill_metrics 记录 + TaskRegistry.unregister
```

### 1.3 Agent 构造与 Hook 链（`core/agent/mod.rs:1173` build_rag_agent）

| Hook | 职责 | 位置 |
|---|---|---|
| `LlmTraceHook` | 每轮 LLM 请求/响应体 Debug 日志（含 run_id/turn） | mod.rs:295 |
| `SkillInstructionHook` | 每轮 preamble 注入（基础规约 + L1 技能目录 + 技能约束摘要 ≤800 字符 + 轮次预算预警 ≤3 轮）；`RequestPatch::active_tools` 窄化可见工具 | mod.rs:480 |
| `SkillGateHook` | `on_tool_call` 兜底拦截（BASE_TOOLS 放行 / 技能声明放行 / allow_extra）+ **重复调用熔断** `guard_duplicate_call`（连续相同 (工具,参数) ≥2 次后 Skip 引导） | mod.rs:368 |
| `InvalidToolCallHook` | 无效工具名恢复：Skip + 可用工具提示回填，模型下轮自纠（rig 默认 fail-fast） | mod.rs:440 |
| `ApprovalGateHook` | 破坏性写操作审批（edit/delete/write/multi_edit/git_commit/git_checkout/mcp_*/open-ui） | approval/hook.rs |
| `ReasoningEffortHook` | `reasoning_effort` 透传（OpenAI 兼容顶层字段） | mod.rs:677 |

工具注册：`ToolRegistry`（tool_registry.rs）+ `create_tool_registry`（mod.rs:1310）——30+ 内置工具按技能分组一行一注册；`BASE_TOOLS`（mod.rs:133）常驻可见；`SKILL_GATED_VISIBLE_TOOLS`（mod.rs:146）软门禁（可见可调、未激活时 Skip 引导）；外部 HTTP 工具（external_tools.rs）+ MCP 工具（`mcp:<server>:<tool>`）并入放行集。

### 1.4 工具执行模型

- 全部为 rig `DynamicTool`（内联手写 JSON Schema + `Box::pin` 闭包），**在 rig poll 栈内顺序执行**（唯一并行点：工具内部 `buffer_unordered(4)`，如 read 多文件）。
- 每次调用经 `record_tool_call`（生成 `call_{uuid}`、参数截断 12k、技能归属解析）与 `record_tool_result(_structured)`（结果截断 12k、ok/失败、质量计数）写入全局 `ToolCallBus`（tools/mod.rs:74，64 请求桶上限、逐桶淘汰、drain 消费、RAII 清理）。
- 失败路径统一 `tool_error` 返回 rig Err（被 rig 记录为工具失败，不影响流继续）。

### 1.5 支撑层（详见各章节）

- **上下文工程**：`SummarizeThenWindowCompressor`（按工具单元 2/3 切分旧段 → LLM 摘要（6000 字符预算）→ 滑窗 recent）；检查点 `CompactionState` 落库 `chat_sessions.compaction_state`。
- **可观测**：`TraceBus` 五阶段（planning/expanding/searching/aggregating/generating）→ `trace:event` 前端渲染阶段耗时面板。
- **子代理**：同进程 rig Agent，独立 request_id/技能态/检索收集器；只读/写型白名单；写型强制审批门、门缺失回退只读；结果 LRU(16) + `read_subagent_result` 分页。
- **记忆**：`memory_items` + FTS5 + 向量（RRF 融合），两级作用域（当前库 ∪ 全局），注入点生成前 preamble。
- **安全**：检索/子代理回传 `wrap_suspicious` 注入包裹；Action Claim 防幻觉；grounding 校验。
- **传输**：Tauri IPC（invoke + 事件）；`FrontendBridge`（core/bridge，WebSocket，工具闭包→前端 handler，DashMap + 5s 超时）供 pomodoro/raw-parse/open-ui 等交互工具。

---

## 2. mdgo Agent 代码走查发现（问题清单）

> 分级：P0 正确性/数据丢失；P1 影响体验/安全，建议尽快；P2 应当修复；P3 可择机。全部带 `文件:行号` 证据。

### 2.1 服务层与支撑层（services/llm.rs、services/chat.rs、core/context、core/trace、core/approval、core/subagent、core/memory、core/validation、core/mcp、core/eval、core/security、lib.rs）

#### P1
| # | 问题 | 证据 |
|---|---|---|
| 1 | **API Key 明文写入 INFO 日志**（随文件日志落盘，任何能读日志者可窃取） | `services/llm.rs:319` |
| 2 | **压缩预算字符/字节混用**：预算按字符（`tokens_to_chars_budget = token*2`），成本按 `content.len()`（**字节**）比较；中文 1 字≈3 字节 → 中文历史被过早压缩、保留过少、摘要调用过频（测试只用 ASCII 掩盖） | `core/context/mod.rs:49-51` vs `:202, :216, :264` |

#### P2
| # | 问题 | 证据 |
|---|---|---|
| 1 | `planner_model`/`summary_model` 用户可配置但**不生效**：`model_for_role` 忽略 role 恒返主模型 | `lib.rs:88-90`；`commands/llm.rs:1001-1013` |
| 2 | LLM 客户端缓存满 8 **全清**（非逐条淘汰，高频切换配置命中率骤降） | `lib.rs:291-294` |
| 3 | TraceBus 桶数 ≥64 时 `map.clear()` **清空全部在途桶**（并发 >64 时前面请求 trace 被抹掉） | `core/trace.rs:62-64` |
| 4 | 记忆检索/向量仅覆盖**最近 100 条**（`list(...,100)`）；向量索引**无删除路径**，删除后陈旧向量累积 | `core/memory/mod.rs:414,480,489`；`memory/vector.rs:67-100` |
| 5 | `fork_session` 后 `token_usage` 归零、`compaction_state` 不复制（统计失真、分支从全量重压缩） | `services/chat.rs:732` |
| 6 | `search_sessions` 用 `LOWER(content) LIKE` **全表扫描**（索引失效、无 LIMIT） | `services/chat.rs:1041-1043` |
| 7 | `chat_session_skills` 表不在 `ChatStore::init_tables` 创建（跨模块隐式依赖，skill schema 未初始化即报 no such table） | `services/chat.rs:1401` vs `core/db/schema.rs:105` |
| 8 | `summarize_bookmark` 文档（声称走网关 output_schema）与实现（实为本地解析）不符；`enable_thinking` 硬编码 | `llm.rs:823-827` vs `:890-903` |
| 9 | `retry_loop` 开头 `if cancel.is_cancelled()` 为**空块死代码**（取消后首次调用仍会发出） | `services/llm.rs:254-256` |
| 10 | `ProviderError(_)` **一律重试**（401/403 永久错误也退避 3 次，最长 ~14s 延迟） | `services/llm.rs:229` |
| 11 | 子代理忽略 `FinalResponse` 变体（若 rig 0.41 最终答案只经 FinalResponse 送达则成功调研也会空输出，需按版本验证） | `core/subagent/mod.rs:236` |
| 12 | MCP 传输故障判定用**中文错误子串**匹配（改文案即失效）；工具名规范化 `server.replace([' ',':'],"_")` 可能碰撞 | `core/mcp/mod.rs:863-879, :1008-1012` |
| 13 | 审批已决缓存满 256 全清 | `core/approval/mod.rs:179-184` |
| 14 | `WRITE_STATS` 每累计 10 次写 dump 全部统计（高频对话日志刷屏） | `services/chat.rs:36-49` |

#### P3
| # | 问题 | 证据 |
|---|---|---|
| 1 | `CompactionState.tokens_before` 恒写 0，从未累积（死数据） | `commands/llm.rs:1847` |
| 2 | MCP HTTP SSE 帧缓冲无上限（异常服务端可致内存膨胀） | `core/mcp/http.rs:107-118` |
| 3 | eval 框架无真实 LLM 执行器，仅测试覆盖 | `core/eval/mod.rs:16-18` |
| 4 | 注入扫描误报率高（"你是一个"等普通文本命中）；仅启发式非安全边界 | `core/security/mod.rs:18-49,102-104` |
| 5 | `JsonSchemaValidator` 每次调用重编译 schema | `services/llm.rs:810-811` |
| 6 | 用户同文案连发被幂等去重（误伤场景未文档化） | `services/chat.rs:535-564` |

#### 值得肯定
- 审批 fail-closed 完备（超时/通道不可用/策略拒绝三分，带差异化模型反馈）；写型子代理强制审批门、门缺失回退只读。
- 压缩按工具调用单元切分杜绝孤儿 tool 消息；检查点应用失败安全降级全量压缩。
- MCP 凭据脱敏（URL userinfo + 命令行敏感参数）收敛于单一写入点；自动重连耗尽保护。
- 子代理只读白名单显式排除记忆写、技能激活与递归工具（防污染与无限递归）。

### 2.2 Agent 核心与工具系统（core/agent/*，tools/mod.rs）

> 依据：通读 `core/agent/mod.rs`（1470 行）、`tools/mod.rs`（5289 行）、`commands/llm.rs`（2670 行）、`tool_registry/external_tools/limits/planner/task_store/cache/canvas`，并交叉核对 rig-agent 0.41.0 源码（hook 链短路语义、工具顺序执行、ToolCall 事件在 hook 前发出）。

**执行模型**：mdgo 无自写 agent 循环——rig 0.41 内部驱动（`drive_tool_calls`），mdgo 只消费 `MultiTurnStreamItem` 流做旁路转发。工具**默认顺序执行**（全工程无 `tool_concurrency` 覆盖）；工具结果由 rig 提交进 run history 逐轮回传 LLM（工具结果**会**回上下文，不存在"结果不进模型"问题）；取消走 `TaskRegistry` + `next_or_cancel` 偏置 select；`DEFAULT_MAX_TURNS=20` + 剩余 3 轮预算预警注入 preamble。

**工具全清单（32 个内置 DynamicTool）**：BASE_TOOLS 24 个（activate_skill/deactivate_skill/read/ls/glob/grep/write/edit/multi_edit/delete/git_status/git_diff/git_commit/git_checkout/webfetch/deep_research/read_subagent_result/remember/forget/search_memory/todo_write/spawn_subagent/parallel_research/self_review）+ 软门禁 8 个（kb_search/code_lookup/schedule/pomodoro/raw-parse/open-ui/search_bookmarks/get_bookmark）+ 外部 HTTP 工具 + MCP 工具（`mcp_<server>_<tool>`），全部经 `ToolRegistry` 一行注册。

**关键机制**：`ToolCallBus`（12K 截断、64 桶、call_seq 配对）→ `emit_pending_tool_events` 转发前端；`ApprovalGateHook` 挂在 `SkillGateHook` 之后（rig hook 链首个非 Run 动作短路：先技能门禁、后审批），60s 超时 fail-closed；技能三层披露 + `active_tools` 窄化 + 软门禁；防幻觉 = Mutation Verification 回读（前置）+ Action Claim 声明表守卫 + Grounding Validator（后置）+ Loop Guard 熔断。

#### P1
| # | 问题 | 证据 |
|---|---|---|
| 1 | **防幻觉守卫只看工具名、不看执行成功**：`tools_called` 只在工具调用事件出现时记录（含失败/被审批拒绝的调用），后续 Action Claim 守卫据此"豁免"——被拒的写操作也能掩护"声称已写入" | `commands/llm.rs:1992-1994`（收集）+ rig `streaming.rs:57-59`（ToolCall 事件在 hook 前发出，不区分成败） |
| 2 | **MaxTurnsError 有部分内容时静默按成功收尾**：流式循环把错误统一置 `stream_failed` 后 break，若已有部分内容则继续走正常 `rag:done`，用户无感知回答被轮次预算截断 | `commands/llm.rs:2026-2030` 与 `:2052`（仅空内容报错） |
| 3 | **取消后阻塞工具闭包无取消且重建总线桶**：取消后 `tool_call_bus().clear(request_id)` 已清桶，但 grep 等 `spawn_blocking` 工具无取消信号，完成后 `record_tool_result` 重建已清空的桶（轨迹残留/内存不释放） | `tools/mod.rs:149`（grep spawn_blocking）vs `commands/llm.rs:1963`（clear） |
| 4 | **软门禁语义反转**：`allowed_tools()==None`（无激活技能）时门禁**全放行**，与"未激活 → 温和引导"的文档语义相反（模型可在无技能激活时调用 kb_search 等） | `core/agent/mod.rs:838-841` |
| 5 | **git_checkout/git_commit 无 `.mdgo` 目录防护**：与 edit/write/delete 的 `.mdgo` 内部数据保护不对称，模型可经 git 工具触碰知识库内部状态 | `tools/mod.rs:1541-1554` |
| 6 | **write 1MB 上限按字符数非字节** + `create_dir_all` 先于最终安全校验（越权路径检查之后才建目录，失败会残留目录） | `tools/mod.rs:1837`、`:1858-1863` |

#### P2
| # | 问题 | 证据 |
|---|---|---|
| 1 | `kb_search` 工具输出未过 `wrap_suspicious` 注入防护（仅主链路预检索过） | `core/agent/mod.rs:752-798` |
| 2 | `should_plan` 的"先/再"等动词启发式误报率高（与真实规划意图相关性弱） | `core/agent/planner.rs:37-51` |
| 3 | 孤儿 tool result 消息处理仅靠前端/后端各自防御，无单一校验（P0-1 语义在 4 处重复实现） | `main.html:51204`、`agent.js:186`、`commands/llm.rs:215`、`core/context/mod.rs:125` |
| 4 | 工具结果缓存（`tools/cache.rs`）命中/失效时全量克隆结果（性能与内存） | `core/agent/tools/cache.rs` |
| 5 | 大量 magic number / 重复代码（各工具 schema 手写、限额散落） | 多个 `build_*_tool` 函数 |

#### P3
- 工具 schema 与限额未从 `limits.rs` 单一引用（部分内联）；`canvas` 工具复杂度高但无独立测试；`guard_duplicate_call` 的 canonical 参数序列化对超大参数开销未评估。

#### 值得肯定
- 无效工具调用恢复（InvalidToolCallHook）对齐主流 Agent 自纠模式；防重复调用熔断（连续相同调用 ≥2 次后 Skip 引导）有效防死循环；技能门禁 + 软门禁 + 工具闭包守卫三层防御；MCP 工具默认审批（对齐 Claude Code 默认 prompt 权限）。

### 2.3 前端（main.html / agent.js / agent_global.js）

#### P2
| # | 问题 | 证据 |
|---|---|---|
| 1 | **两条流式路径重复**：`sendRagQuery`（agent.js:304）与 `sendLlmQuery`（main.html:51332）各自实现事件监听/节流渲染/落库/部分内容保存（约 60% 代码同构），改一处漏一处 | agent.js:344-646 vs main.html:51332-51567 |
| 2 | **工具配对语义前后端 4 处实现**：前端 `trimChatHistory`（main.html:51204，按单元成组）、`expandToolHistory`（agent.js:186，还原协议消息）；后端 `chat_turns_to_history`（llm.rs:215，过滤孤儿）+ `group_turns`（context/mod.rs:125，压缩切分）——同一「assistant(tool_calls)+tool 结果成组」规则四处复制，任一漂移即产生孤儿 tool 消息或历史失真 | 见 §2.2 P2-3 |
| 3 | **单文件 2.6MB main.html**：聊天/Agent/AI 相关逻辑全部内联在 5 万+ 行单文件（聊天区段约 main.html:49000-53245），构建无拆分、无类型检查、无单测，回归风险集中在手测 | — |
| 4 | **前端 token 估算与后端压缩双轨**：`estimateTokenCount`/`trimChatHistory`（前端，字符/4 近似）与后端 `tokens_to_chars_budget`（chars/2）口径不一致，前端 UI 显示的上下文占用率与实际发送给 LLM 的不完全一致 | main.html:51153 vs core/context/mod.rs:49 |

#### P3
- `agent_global.js` 全局任务状态条每 30s 轮询 + 事件驱动双路径（成本低但可收敛）；`OBSERVED_CHANNELS` 事件驱动的任务条刷新与 30s 兜底轮询并存，网络波动时可能有瞬时双刷。
- `main.html` 中 `chatStreaming`/`_chatStreamingDiv`/`_streamingToolCalls` 等流式状态为全局变量，模块化边界靠注释约定（agent.js 顶部依赖清单），无编译期约束。

#### 值得肯定
- 取消链路完整：AbortController → `kb_cancel_task` → 后端偏置 select 断开；断联/失败保留部分内容落库（`savePartialAssistantMessage` + finally 兜底，防竞态双存）。
- 流式渲染 rAF 节流 + done 后完整 sanitize 覆盖；`expandToolHistory` 对老数据（无 call_id/result）降级为文本消息，向后兼容。
- `handleAgentToolEvent` 工具卡片（耗时徽标/点击展开/结构化卡片）与 trace 阶段面板，可检视性在同类本地工具中属上乘。

---

## 3. DeepSeek Harness 架构剖析（对 mdgo 有借鉴意义的部分）

> 完整行号级报告见附录 `docs/deepseek-harness-architecture-report.md`（约 220 个包逐包调研 + 传输层/策略层两个子代理并入）。本节约取其与 mdgo 优化直接相关的要点。

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

### 4.1 架构对比表

| 维度 | mdgo（现状） | DeepSeek Harness | 结论 |
|---|---|---|---|
| 技术栈 | Rust + Tauri 2 + rig 0.41 + SQLite/LanceDB/Tantivy + ONNX 本地嵌入 | TypeScript monorepo（~220 包）+ vendored Cordis 插件 + Node | 各有所长：mdgo 单二进制桌面、本地优先；DSH 插件化、可组合 |
| Agent 循环 | rig Agent（固定循环 + Hook 补丁，`stream_chat` 旁路消费） | 自研 `ReactLoopAgent`（turn/step 状态机 + waterfall 事件，loop 可整体替换） | DSH 可控性/可替换性显著优 |
| 输入通道 | 单请求（用户消息一次一请求）；整体取消 | inbox 模型：`followup/steer/inject` 三通道 + 中途引导 + 维护任务 | DSH 优（可中途注入/引导） |
| 工具执行 | **顺序**（rig poll 栈内；仅工具内部 buffer_unordered 并行） | **并行**（exclusive barrier + 有界滚动池默认 10，model 序提交） | DSH 显著优 |
| 工具契约 | DynamicTool + 手写内联 JSON schema；无 output schema/超时/并发标记 | ToolDefinition：output schema + render + finalizeContent + timeoutMs + isConcurrencySafe + presentCall/presentResult | DSH 显著优 |
| 会话历史 | SQLite 消息表（role/content/tool_calls JSON 列），原始 chunk 不保留 | 事件溯源日志（`assistant/chunk` 原始保真），历史由 `deriveMessages` 派生 | DSH 显著优（可回放/可恢复） |
| 工具配对 | 前后端 4 处近似实现（trim/expand/chat_turns_to_history/group_turns） | 单一事件对（tool/call + tool/result）+ surface 投影 + 配对平衡校验 | DSH 显著优 |
| 上下文工程 | 摘要+滑窗（**字符估算** token、检查点落库）；无溢出重试 | tokenMeter（回放折叠 + 启发式锚点）+ 压力/溢出双触发 + 工具结果剪枝 + request-error 重试 | DSH 优 |
| RAG/检索 | 混合检索（BM25+向量+精排+聚簇+查询扩展+符号实体+一致性加成） | 无内置知识库 | **mdgo 显著优**（产品根基） |
| 技能体系 | SKILL.md 三作用域 + 渐进披露 + 动态注入/窄化/门禁 + 指标闭环 | skill 工具（指令目录注入，`agent.inject()` 目录替换） | mdgo 更完整 |
| 多代理 | 同进程一次性子代理（只读/写型白名单 + 审批门 + LRU 结果分页） | subagent seam（多 provider、continuable、控制工具 list/send/interrupt + report）+ workflow + goal + ralph | DSH 显著优 |
| 长期目标 | 无（仅 todo_write） | goal 事件源 + revision CAS + round driver | DSH 优 |
| 审批/安全 | ApprovalGate（策略 + IPC 弹窗 + 60s fail-closed）+ 注入防护 + 写路径 .mdgo 防护（git 工具缺失） | ctx.approval（ask/never、audit 事件、callId 关联）+ 沙箱 seam（默认 read-only）+ fs 意图门 | DSH 沙箱维度优，mdgo 策略面已完备 |
| 重试 | 非流式指数退避（3 次）；流式不重试（副作用风险）；ProviderError 一律重试 | 基于日志回放计数的重试（先落盘再等待）+ Retry-After 优先 + retryableCodes 白名单 | DSH 优 |
| 集成/传输 | Tauri IPC + 事件 + WebSocket 工具桥；桌面应用 | HTTP/SSE + WebSocket + SDK JSON-RPC + ACP + headless | DSH 集成面显著广 |
| 可观测 | TraceBus 五阶段（内存桶）+ 工具卡片 + 质量计数（内存原子） | session/event 持久事实 + agent/* 实时事件 + 生成目录 CI 门 | DSH 持久化优 |
| 测试/工程 | 少量单测（context/planner 等），无覆盖率门/e2e | 逐文件 100% 覆盖率门 + 真 API e2e + keyless 快照 + 浏览器快照 | DSH 显著优 |
| 前端形态 | 2.6MB 单文件 main.html + 少量模块 js | 多包 web UI（client/ui-* 插件），事件驱动渲染 | DSH 可维护性优 |

### 4.2 mdgo 的优点（相对 DSH）
1. **RAG/知识库深度**：混合检索、查询扩展去重、符号实体发现、rerank + 聚簇 + 跨查询一致性加成——DSH 无此能力，这是 mdgo 的产品根基（DSH 是编码 harness，mdgo 是知识库工具，定位不同）。
2. **工程防护细节**：取消传播（偏置 select!）、部分结果保留与落库、防幻觉 Action Claim 守卫 + Mutation Verification + Grounding Validator 三层、注入包裹、技能门禁 + 软门禁 + 工具闭包守卫三层防御、子代理防递归/防污染白名单、MCP 凭据脱敏——"生产级"打磨，非 demo 级。
3. **本地优先**：ONNX/DirectML 本地嵌入与精排、离线可用、单二进制桌面分发（Tauri）。
4. **领域工具丰富**：schedule/pomodoro/canvas/bookmark/raw-photo/open-ui + SKILL.md 技能生态，与知识库深度耦合；技能激活由 LLM 决策 + 预激活规则双路径。
5. **成本透明**：token 用量、缓存命中率、上下文占用率 UI 可见（DSH 无此 UI 口径）；历史压缩自动降级（摘要失败回滑窗，永不失败）。

### 4.3 mdgo 的不足（相对 DSH）
1. **rig 0.41 依赖锁定**：顺序工具执行、Hook 面有限、Agent 模式不支持 Anthropic、升级破坏性大（工具 schema/事件类型强绑定）。
2. **会话非事件溯源**：消息表 + tool_calls JSON，原始 chunk 不保留；配对语义 4 处重复；fork/恢复/回放能力有限；取消不产合成结果，回放不完整。
3. **工具系统契约弱**：无 output schema、无超时声明、无并发安全标记、无回放级 UI 投影；前端结构化卡片仅 git_diff 硬编码；结果规范/校验缺失。
4. **上下文预算不精确**：字符估算（chars/2 或 /4）非真实 token；前端 trim 与后端 compress 双轨策略（漂移风险）；无上下文溢出重试（context_length_exceeded 直接失败）。
5. **交互模型单向**：无 steer（中途引导）、无 ask_user_question（澄清提问）、无后台可续子代理控制（list/send/interrupt）；任务只能整体取消。
6. **单一巨型文件**：`commands/llm.rs` 121KB（agent_query 1400 行）、`tools/mod.rs` 267KB、前端 `main.html` 2.6MB——维护与测试成本高。
7. **可观测不持久**：质量计数/TraceBus 内存态，进程重启即失；无逐文件覆盖率门/真 e2e。
8. **多代理能力单一**：子代理同进程、一次性、无 profiles/深度限制语义；无 workflow 编排；无目标（goal）体系。

### 4.4 双方共性
- 都强调 **fail-closed 安全默认**（审批/门禁/未知工具）、**取消优先于一切**（mdgo biased select / DSH throwIfAborted 检查点）、**工具结果完整回传模型**（不存在"工具结果不进上下文"的旧问题）。
- 都把 **LLM 调用与工具执行分离**（mdgo 靠 rig hook，DSH 靠 waterfall 事件），把策略挂在执行管道上而非工具内部。
- 都在向"多模型/多 provider + 思考程度控制 + 上下文预算可配置"收敛（mdgo 已有 reasoning_effort/max_tokens/planner_model 雏形；DSH 有 provider route + retryableCodes）。

---

## 5. 优化方向与方案（借鉴 DSH）

> **实施状态（2026-08 本轮）**：§5.1 全部 10 项与 §5.2 全部 9 项已落地实现（✅），
> 详见 `docs/agent_review_deepseek_harness.md` 底部「实施记录」。§5.3（P2 架构形态）
> 按路线图推进中。

### 5.0 原则
- 不重写：保留 rig + Tauri 架构，按「能力 seam」思路增量迁移 DSH 范式（每项可独立裁剪、可回滚）。
- 优先级：正确性（P0）→ 契约与语义（P1）→ 架构形态（P2）。

### 5.1 P0 —— 正确性修复（对照 §2.1 + §2.2）
1. **凭据脱敏**：`services/llm.rs:319` 移除 api_key 明文，改为 `api_key_sha256[..8]` 或 `***`；对 MCP 凭据复用现有脱敏函数（§2.1 P1-1）。
2. **压缩预算字节/字符统一**：`core/context/mod.rs` 全部成本改为 `chars().count()`（或接入 P1-2 精确 token 计量）；新增中文混合单测（§2.1 P1-2）。
3. **retry_loop 取消死代码**：`services/llm.rs:254-256` 改为循环首部 `select!` 取消即返回（§2.1 P2-9）。
4. **ProviderError 重试收窄**：401/403/400（业务 4xx 非 429/408）不重试，仅 429/5xx/连接类；对齐 DSH retryableCodes 白名单（§2.1 P2-10）。
5. **防幻觉守卫看执行成败**：`tools_called` 改为从 `ToolCallBus` 取「成功 result」的工具名集合（`ok=true` 且含 result），失败/被审批拒绝的调用不得豁免 Action Claim 判定（§2.2 P1-1）。
6. **MaxTurnsError 显式暴露**：消费循环区分 `MaxTurnsError` 与普通错误——有部分内容时补发 `rag:status`/`rag:error` 提示"已达轮次预算上限，回答可能不完整"，不静默按成功收尾（§2.2 P1-2）。
7. **取消感知的阻塞工具**：grep/read 等 `spawn_blocking` 工具传入 cancel token，取消后不再写回总线（`record_tool_result` 前检查 `cancel.is_cancelled()`）；或 ToolCallBus 对已 clear 的桶拒绝重建（§2.2 P1-3）。
8. **软门禁语义修正**：`allowed_tools()==None` 时按"无已激活技能"处理（仅 BASE_TOOLS + 软门禁清单放行），与文档语义一致（§2.2 P1-4）。
9. **git 工具 .mdgo 防护**：`git_checkout`/`git_commit` 增加与 edit/write/delete 一致的 `.mdgo` 内部目录防护（§2.2 P1-5）。
10. **write 上限与校验顺序**：1MB 上限按**字节**计；`create_dir_all` 移到路径安全校验之后（§2.2 P1-6）。

### 5.2 P1 —— 契约与语义（借鉴 DSH 工具契约 / token 计量 / request-error）
1. **工具历史配对单一化**：新增 `core/chat_types.rs` 作为唯一配对语义源，`chat_turns_to_history`（后端）、`trimChatHistory`/`expandToolHistory`（前端）改为纯数据转换引用同一分组规则（前端以共享 JS 模块 `css_js/modules/chat-history.js` 收敛，main.html 内联改为外链）。对齐 DSH「工具调用单元成组 + cut point 规则」。
2. **精确 token 计量**：启用 `tokenizers`（已依赖）按模型 tokenizer 计 token，替换 `ApproxTokenEstimator`（chars/2）与前端 `estimateTokenCount`；对齐 DSH `ctx.tokenMeter`（计量与回放单例）。
3. **上下文溢出重试**：捕获 provider 的 context_length_exceeded（400 或错误码）→ 强制压缩（更紧预算 + 工具结果剪枝）→ 重发一次；对齐 DSH `agent/request-error` 语义（仅在压缩推进时重试）。
4. **`ask_user_question` 工具**：复用 approval/plan 的 oneshot + 前端弹窗样板（agent_global.js），注册为 BASE_TOOLS；模型在信息不足时澄清而非猜测（对齐 DSH ask_user_question seam）。
5. **工具输出契约（轻量版 ToolDefinition）**：为 read/grep/ls/git_diff/kb_search 等声明 `output`（JSON Schema + 投影），后端 `record_tool_result_structured` 已有结构化槽位；前端按 `tool_output` 类型化渲染卡片（替换 git_diff 硬编码）；工具声明 `timeout_ms`（超过则取消并记录失败）——对齐 DSH `finalizeContent`/`presentResult` 的纯函数投影思想。
6. **planner_model 真正生效或下线配置**：`lib.rs:88-90` 实现按 role 路由（Planner/Summary/Main），或从 UI 移除死配置（P2-1 修复）。
7. **memory 索引去删除 + 取消 100 条上限**（P2-4）：`MemoryVectorIndex::sync` 增加删除路径；关键词/向量路径全量可见集。
8. **kb_search 输出注入防护补齐**：工具输出统一过 `wrap_suspicious`（与主链路预检索一致，§2.2 P2-1）。
9. **should_plan 触发规则增强**：动词表扩充 + 意图感知（对齐 P1-10 规划增强）；`tools/cache.rs` 命中返回改为引用/克隆成本评估（§2.2 P2-2/P2-4）。

### 5.3 P2 —— 架构形态（借鉴 DSH 事件溯源 / 并行调度 / 多代理 / 平台化）
1. **事件溯源会话（最值得投入，量最大）**：
   - 新增 `session_events` 表（append-only：`seq, session_id, event_type, payload_json, created_at`），按 DSH SessionEventMap 的**最小子集**建模：`turn/start,end`、`step/start,end`、`user/message`、`assistant/chunk`、`assistant/message`（含 usage）、`tool/call`（原始参数）、`tool/result`、`todo/write`、`compaction/summary`（shadowed seqs）。
   - LLM 历史改为从事件日志**派生**（`derive_history(session_id, budget)`），现有 `chat_messages` 作为兼容读路径保留（迁移期双写）。
   - 收益：原始 chunk 保真回放、任意点 fork、压缩 shadowed 语义、UI 重放一致性、取消合成结果保持回放有效（对齐 DSH「模型可见即已记录」不变量）。
   - 风险与对策：迁移量中等；先行在 `agent_query` 写路径落事件，读路径逐步切换。
2. **并行工具执行**：
   - 短期（方案 A，侵入小）：延续「工具内部并行」路线——read/grep/kb_search 已并行；扩展多文件 edit（multi_edit 已批量）、并行检索工具调用。
   - 中期（方案 B）：评估 rig 升级（0.42+ 的批处理工具调用语义）或自研轻量调度层：在 `agent_query` 消费循环中拦截整批 `ToolCall` 事件，按 DSH tool-calls.ts 模式（exclusive barrier + 有界滚动池 + 模型序提交）执行后**单次回填 tool 结果并续跑**——需验证 rig 是否暴露批工具调用（风险高，先做可行性 spike）。
   - 兜底：至少实现 `isConcurrencySafe` 等价标记（工具声明 + 调度分组），为未来自研 loop 预留。
3. **可续子代理 + 控制工具**：
   - `spawn_subagent` 结果入持久化存储（SQLite，替代 LRU(16)）；新增 `list_agents`/`send_message`/`interrupt_agent` 工具（对齐 DSH subagent-control），支持「后台调研 → 继续追问」。
   - `parallel_research` 用 `JoinSet` 真正并行（当前实现核查是否串行 await）。
   - 写型子代理审批事件冒泡已具备（复用 ApprovalGate），补齐子代理 `report` 语义（子代理内可直接汇报而非仅最终摘要）。
4. **目标（goal）体系**：按 DSH goal seam 的轻量版：`core/goal/` + `create_goal/get_goal/update_goal` 工具（revision CAS、phase、blocked 需连续 3 轮 + blocked_reason）；事件溯源复用 P2-1 的 session_events。
5. **`commands/llm.rs` 拆分**：按阶段拆为 `agent/query.rs`（编排）、`agent/retrieval.rs`（Stage1-3）、`agent/history.rs`（压缩/转换）、`agent/events.rs`（事件发射）、`agent/generate.rs`（流式循环）；单一职责 + 可单测。
6. **传输与集成**：`agent_query_json`（事件流复用现有协议）+ `mdgo-agent` headless 二进制（stdin JSONL / stdout 事件流，复用 trace 事件结构）——对齐 DSH headless + JSON/RPC 模式，为自动化与 eval 铺路。
7. **eval 入 CI**：`core/eval` 挂真实执行器（mock 场景 CI 常规跑、LLM 场景 `--ignored` 手动跑），纳入 `cargo test`；质量计数落库（SQLite 指标表）替代内存原子。
8. **前端收敛**：流式渲染/保存逻辑在 `agent.js` 与 main.html 的 llm 路径去重（提取 `chat-stream.js` 模块）；`trimChatHistory` 与后端预算策略对齐单一来源；会话树 UI（fork 可视化，对齐 DSH /tree）。

### 5.4 执行顺序与验收
```
Phase A（正确性）   5.1 全部 → cargo test --lib 全绿 + 手动验收（日志无明文、中文长会话压缩正常）
Phase B（契约语义） 5.2.1→5.2.2→5.2.3→5.2.4→5.2.5 → 工具历史单测 + 溢出重试集成测试
Phase C（架构形态） 5.3.1（事件溯源，最大项，可独立成 sprint）→5.3.2→5.3.3→5.3.4→5.3.5
Phase D（平台化）   5.3.6→5.3.7→5.3.8（按需裁剪）
```
每项完成更新 `docs/agent_capability_testing.md` 验收清单；Phase C 前先提交基线（当前工作区有未提交改动）。

---

## 6. 风险与取舍

| 项 | 说明 | 对策 |
|---|---|---|
| rig 0.41 限制 | 并行工具调度需验证 rig 是否暴露整批 tool_calls | 先 spike；不可行则工具内部并行 + 自研调度留接口 |
| 事件溯源迁移 | 双写期一致性与历史数据兼容 | 迁移期 `chat_messages` 兼容读路径 + 一次性 backfill 脚本 |
| 单文件 2.6MB index.html/main.html | 前端改动风险 | 改动收敛独立模块 + 手动验收清单 |
| token 计量精确化 | 不同模型 tokenizer 差异 | 按模型指纹缓存 tokenizer；未知模型回退近似估算 |
| 多代理/记忆 token 成本 | 注入与并行增加消耗 | 预算参数可配置；记忆 top-k 收敛；子代理并行上限 |

---

## 附：实施记录（2026-08 本轮，§5.1 + §5.2 全部落地）

| 项 | 改动 | 文件 |
|---|---|---|
| P0-1 凭据脱敏 | `api_key` 日志改不可逆掩码（FNV-1a + 长度），新增 `mask_secret` + 单测 | `services/llm.rs` |
| P0-2 压缩预算字符/字节统一 | 预算比较全部改 `chars().count()`（`char_len`/`unit_char_len`），新增中文回归测试 | `core/context/mod.rs` |
| P0-3 retry_loop 取消死代码 | 删除空块；重试条件加 `cancel.is_cancelled()`（取消优先） | `services/llm.rs` |
| P0-4 ProviderError 重试收窄 | `status_from_provider_message`（显式/裸状态码）+ `is_permanent_provider_message`（401/403/context 溢出等不重试） | `services/llm.rs` |
| P0-5 防幻觉守卫看执行成败 | `ToolCallBus::successful_tool_names`（ok=true 才计），循环内/后合并；不再从 ToolCall 事件收集 | `core/agent/tools/mod.rs`、`commands/llm.rs` |
| P0-6 MaxTurnsError 显式暴露 | `StreamingError::Prompt(MaxTurnsError)` 检测 + 截断提示追加 + 区分错误文案/指标码 | `commands/llm.rs` |
| P0-7 取消感知工具 | `record_tool_call/result_structured` 取消后跳过（总线桶不再重建） | `core/agent/tools/mod.rs` |
| P0-8 软门禁语义修正 | `KbSearchConfig.skill_gating`：主对话 true（None → 引导激活，与 Hook 一致）、子代理 false；4 处门禁统一 | `core/agent/mod.rs`、`tools/mod.rs`、`commands/llm.rs`、`core/subagent/mod.rs` |
| P0-9 git .mdgo 防护 | `git_commit` 拒绝暂存区含 .mdgo；`git_checkout` 拒绝 .mdgo 路径 | `core/agent/tools/mod.rs` |
| P0-10 write 上限与校验顺序 | 1MB 按字节；词法校验（绝对路径/.. /父目录 .mdgo 组件）全部前置后再建目录 | `core/agent/tools/mod.rs` |
| P1-1 工具配对单一化 | 后端 `core/chat_types::{group_tool_units, paired_tool_call_ids}`（压缩切分 + 消息转换共用）；前端 `css_js/modules/chat-history.js`（groupToolUnits/expandToolHistory/trimChatHistory），main.html/agent.js 改薄包装委托 | `core/chat_types.rs`、`core/context/mod.rs`、`commands/llm.rs`、`css_js/modules/chat-history.js`、`main.html`、`css_js/modules/agent.js` |
| P1-2 精确 token 计量 | `embedding::estimate_tokens`（BGE WordPiece）+ `TokenizerBackedEstimator` + `estimate_turns_tokens` 预算门（未超预算零压缩） | `core/embedding.rs`、`core/context/mod.rs`、`commands/llm.rs` |
| P1-3 上下文溢出重试 | `is_context_overflow_error` 检测 → 预算收紧 60% 重新压缩 → 重试一次（≤1 次，对齐 DSH request-error） | `commands/llm.rs` |
| P1-4 ask_user_question | 新工具（BASE_TOOLS）+ oneshot 挂起表 + `question:request` 事件 + `question_respond` IPC + 前端弹窗 | `core/agent/tools/mod.rs`、`core/agent/mod.rs`、`commands/question.rs`、`lib.rs`、`css_js/modules/agent_global.js` |
| P1-5 工具输出契约 | read/kb_search/code_lookup 输出结构化（files/sources），前端通用结构化卡片渲染器 | `core/agent/mod.rs`、`core/agent/tools/mod.rs`、`css_js/modules/agent.js` |
| P1-6 planner_model 生效 | `model_for_role` 按角色路由（planner/summary 缺省回退主模型）；客户端缓存满 8 改逐条淘汰 | `lib.rs` |
| P1-7 memory 索引 | `MemoryVectorIndex::prune`（删除路径）+ 全量可见（10k 上限替代 100） | `core/memory/vector.rs`、`core/memory/mod.rs` |
| P1-8 检索工具注入防护 | kb_search/code_lookup 输出过 `wrap_suspicious` | `core/agent/mod.rs` |
| P1-9 should_plan 增强 | 移除"先/再"误报源；疑问句/轻量查看类抑制；新增回归测试 | `core/agent/planner.rs` |

> 验收：`cargo check --lib` 通过；`cargo test --lib` 全绿（含新增中文压缩、重试分类、掩码、should_plan、向量 prune 等用例）；JS 模块 `node --check` 通过。

### 追加修复（2026-08-20 用户报告）：BM25 删除返回 Opstamp 导致元数据被清零

- **现象**：删除单个文件（`requirements_copy.txt`）后，`chunk_count/vector_count` 元数据被扣成 0，整个知识库显示被清空。
- **根因**：tantivy `IndexWriter::delete_term()` 返回 **Opstamp（操作戳）**而非删除文档数；`Bm25Index::try_delete_document` 误把操作戳当 `deleted_count` 返回，`Indexer::remove_file` 据此把 `chunk_delta` 扣成整个索引的操作戳数量（50534/50535），元数据直接归零（watcher 双触发又扣一次）。
- **修复**：删除前用 `Count` collector 统计 doc_name 精确匹配的真实文档数，删除后返回真实计数；日志同步修正；新增 2 个回归测试（`delete_document_returns_real_count_not_opstamp`、`delete_document_leaves_other_docs_searchable`）。
- **恢复**：索引数据本身完好（term 删除语义一直正确），元数据归零后执行一次全量重建（`kb_index`）即可恢复计数，不会重复写入（`replace_document_chunks` 先删后写幂等）。

---

## 附：评审范围与证据索引

- **附录**：DSH 架构行号级报告见 [`docs/deepseek-harness-architecture-report.md`](deepseek-harness-architecture-report.md)（约 220 包调研 + 传输层/策略层两个子代理并入）。
- 前端入口：`css_js/modules/agent.js`、`css_js/modules/agent_global.js`、`css_js/modules/frontend-bridge.js`、`main.html`（聊天/AI 相关区段）
- 后端链路：`tauri/src-tauri/src/commands/llm.rs`、`core/agent/mod.rs`、`core/agent/tools/mod.rs`、`core/agent/{tool_registry,external_tools,limits,task_store,planner}.rs`、`services/llm.rs`、`services/chat.rs`、`core/{context,trace,subagent,memory,validation,mcp,eval,security,bridge,approval}/*`
- 规约与资源：`resources/agent/rag_agent.md`、`resources/agent/chat_agent.md`、`resources/skills/*/SKILL.md`
- DSH（本文 §3 直接引用）：`docs/architecture.zh.md`、`docs/agent-lifecycle.zh.md`、`docs/tool-execution-pipeline.zh.md`、`docs/tool-catalog.md`、`docs/subsystems/{session,core,tools,system-prompt,compaction,approval,subagent,goal,workflow,web-server,scope}.md`、`packages/core/agent-loop/src/{agent,tool-calls}.ts`、`packages/core/tools/src/index.ts`
