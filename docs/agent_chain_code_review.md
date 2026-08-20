# mdgo Agent 链路 Code Review 报告（后端入口/出口闭合 + 前端 UI 适配）

> 审查范围：
> - 后端：`src-tauri/src/commands/{llm,chat,approval,plan,question,bridge,skill,schedule}.rs`、`core/loop/*`（loop/tool_calls/session/hooks）、`core/agent/{loop_tools,loop_hooks,tools,task_store,planner}.rs`、`core/approval/*`、`core/bridge/*`、`core/subagent/*`、`core/skill.rs`、`services/{llm,chat}.rs`
> - 前端：`tauri/dist/main.html`（主脚本 5.3 万行）+ `dist/css_js/modules/{agent,agent_global,frontend-bridge,chat-history,skill,mcp,schedule}.js`
> - 方法：逐链路核对 invoke 参数 ↔ 命令签名、事件 payload ↔ 监听器、回传命令 ↔ 挂起表；对照 rig 时代（git HEAD）行为确认 v3 回归。
> - 日期：本轮重构（rig 移除 + core/loop 自研内核）完成后。

---

## ✅ 修复状态（全部已实施并验证）

| # | 问题 | 状态 | 修复位置 |
|---|---|---|---|
| B1 | 工具轨迹不再实时转发 | ✅ | llm.rs turn 回调处理 `LoopEvent::ToolResult` → 实时 drain；另修复**防幻觉证据收集顺序 bug**（successful_tool_names 原在 drain 后恒为空 → 改为回调内收集 evidence） |
| B2 | 取消/错误早退任务不闭合 | ✅ | agent_query 4 处早退 + kb_llm_query_loop_v2 2 处 + anthropic 2 处 + agent_generate_loop_v2 1 处补 `agent_tasks.finish`；`kb_cancel_task` 补 finish |
| B3 | generating 阶段不闭合 | ✅ | RAG 各 outcome 补 stage_end；chat 补 stage_start + 各 outcome stage_end |
| B4 | 事件溯源：chat 不写 + upsert 残留 | ✅ | upsert 改全量替换（delete+insert 事务，防裁剪窗口残留）；kb_llm_query 加 dir_path/session_id 参数 + loop_v2 写事件；clear_messages 清事件；读路径语义注释（与前端裁剪契约冲突，保持 legacy 数据源） |
| B5 | 推理过程不可见 | ✅ | 后端转发 `rag:thinking`/`llm:thinking`；前端可折叠思考过程展示（agent.js + main.html） |
| B6 | question 弹窗无超时 | ✅ | 前端 120s 自动关闭（提交 null） |
| B7 | 冗余 drain | ✅ | 删除 rag:done 后第二次调用 |
| B8 | 配对语义三份实现 | ✅ | 双端注释互指 + 镜像副本语义说明 |
| B9 | 白名单缺 ask_user_question | ✅ | 补入 + 修正过时注释 + 新增 `allowed_tools_cover_registry` 断言测试 |
| B10 | Anthropic 取消无终态 | ✅ | 取消→llm:done(空)+finish(Cancelled)；正常/取消统一 llm:done+finish |
| H1 | 停止后秒发新消息竞态 | ✅ | agent.js / main.html finally 加 `activeRequestId` 守卫（旧请求只清自己的监听/帧，不动共享状态） |
| H2 | 页面重入停止失效 | ✅ | 重入恢复路径重建 AbortController + abort→kb_cancel_task + chatStreaming=true |
| H3 | 任务条滞留 running | ✅ | 前端 10 分钟陈旧 running 兜底隐藏 |
| M1 | 打字动画残留 | ✅ | finally + stopChatGeneration 补 removeChatTyping |
| M2 | 耗时徽标 0.0s / 批量闪现 | ✅ | B1 实时转发后自然解决（call/result 分帧到达） |
| M3 | usage null 清缓存记录 | ✅ | llm:done 时 usageData 非空才 updateCacheRate |
| L1 | 桥重连旧 port / 退避不重置 | ✅ | 重连前重查 get_bridge_port；onopen 重置退避 |
| L2 | trace map 泄漏 | ✅ | finally 中 delete |
| L3 | llm:done 无兜底渲染 | ✅ | 补 streamingDiv 兜底创建（对齐 rag 路径） |
| L4 | 工具卡片永久"执行中…" | ✅ | rag:done 对 ok===null 卡片标记"未完成" |
| L5 | 审批多弹窗无去重 | ✅ | 审批单例弹窗 + 队列串行处理；plan request_id 注释说明 |
| L6 | 终态任务不展示 | ✅ | 任务条展示最近 1 条终态（带状态色） |
| C1 | **历史压缩业务逻辑回归（构建警告暴露）** | ✅ | `prepare_history` 在两个 v3 入口重新接线：RAG（agent_generate_loop_v2：检查点加载 → apply_compaction_checkpoint → 预算压缩 → 摘要成功写回 CompactionState）+ chat（LLMClient 摘要器，失败降级纯滑窗）；`rag:status` 增加 compressing 阶段提示 |
| C2 | **技能执行统计业务逻辑回归** | ✅ | `collect_skill_exec_inputs` + `record_skill_execution` 在 agent_generate_loop_v2 三个终态点接线（成功/失败/取消 → 技能面板执行统计恢复） |
| C3 | 编译警告清零 | ✅ | 删除 6 个死常量/5 个死函数/2 个冗余方法 + 清理 unused imports；测试专用项（is_retryable_status_code / format_skill_instructions）标注 `#[allow(dead_code)]` 保留 |

**验证**：`cargo check --lib` ✅ **0 warnings**；`cargo test --lib` **272 passed / 0 failed**（含新增 B9 断言 + upsert 语义变更 + 压缩/统计接线后既有测试）；全部前端模块 + main.html 内联脚本 `node --check` ✅。

---

## 一、总体结论

**业务链路入口→出口基本闭合**：`agent_query` / `kb_llm_query` 的 invoke 参数与后端签名完全一致；`rag:*`/`llm:*`/`agent:tool_*`/`trace:event`/`approval:request`/`plan:request`/`question:request`/FrontendBridge 事件协议与前端监听器逐字段核对一致；审批（IpcApprovalTransport 60s fail-closed）、计划（plan:request 60s）、澄清（120s）、桥（5s）、子代理（ToolBusGuard 独立桶清理）、MCP/外部工具（注册+放行集）闭环均成立。

**但存在 3 组实质问题（均为本轮重构引入或暴露）**：
1. **v3 回归——实时性**：工具轨迹/阶段事件不再逐步转发（LoopEvent::ToolCall/ToolResult 在命令层被丢弃），全部在请求结束批量出现；RAG "generating" 阶段永不闭合。
2. **状态不闭合**：多条取消/错误早退路径不 `agent_tasks.finish` → 全局任务条滞留幽灵 running；Anthropic 通道取消不发任何终态事件。
3. **前端竞态**：停止后秒发新消息会清空新请求状态；页面切走再切回后"停止"按钮失效（取消通道未重建）。

---

## 二、后端问题清单

### B1【高】工具轨迹/阶段事件不再实时转发（v3 回归）
- **证据**：`commands/llm.rs:1968-1983` 与 `1816-1841` 的 turn 回调只处理 `LoopEvent::Delta/Usage`；`LoopEvent::ToolCall`（core/loop/loop.rs:272 派发）与 `ToolResult`（loop.rs:343 派发）被丢弃。工具事件改走 `BusToolEventSink → ToolCallBus`，仅在 turn() 结束后 `emit_pending_tool_events`（llm.rs:2001/2057）一次性转发。rig 时代（git HEAD）每流式迭代调用 `emit_pending_tool_events`。
- **影响**：多轮工具任务中前端工具卡片全部在结束瞬间闪现，"执行中…"状态不可见；耗时徽标恒 ≈0.0s（见前端 M2）；trace 阶段事件同理。
- **方案**：turn 回调中处理 `LoopEvent::ToolResult`（每批工具执行完）时调用 `emit_pending_tool_events(app, request_id)` 增量 drain（此时总线已含本批 call+result）；`ToolCall` 事件可忽略（总线在 run_one 内已写 call）。同时把 `llm.rs:2057` 的第二次调用删除（drain 后必为空）。

### B2【高】取消/错误早退路径任务状态不闭合
- **证据**：`agent_query` 早退 `llm.rs:760/768`（LLM 初始化失败/未配置）与取消路径 `1242/1456-1459/1564` 只 `task_registry.unregister` 不 `agent_tasks.finish`；`kb_llm_query_loop_v2` 早退 `1788/1794`、`kb_llm_query_anthropic` `2190` 同样缺失。规划取消路径（`1004-1006`，注释"问题3修复"）有 finish，Stage1-3 取消未同步。
- **影响**：`agent_global.js:43` 按 `status==='running'` 渲染全局任务条 → 幽灵任务 + 停止按钮 no-op（前端 H3）。
- **方案**：所有早退路径统一 `agent_tasks.finish(request_id, Failed|Cancelled)`；建议抽一个 `finish_task(&state, request_id, status)` 辅助并覆盖全部 return 点；`kb_cancel_task` 也补 finish（或断言生成路径必收尾）。

### B3【中】RAG generating 阶段事件不闭合（chat 有 end 无 start）
- **证据**：`llm.rs:1559` `stage_start("generating")` 后，v3 路径成功/取消均无 `stage_end("generating")`；`llm.rs:1848`（chat 错误路径）`stage_end("generating","error")` 但 chat 从未 stage_start。
- **影响**：agent.js 阶段面板显示未闭合的 "▶ generating" 条目；切回恢复视图同样停留 generating。
- **方案**：`agent_generate_loop_v2` 结束处按 outcome（Done/Cancelled/Failed）补 `stage_end("generating", ok|cancelled|error, duration, detail)`；`kb_llm_query_loop_v2` 开始处补 `stage_start("generating")`。

### B4【中】事件溯源会话读路径缺失 + chat 模式不写事件
- **证据**：`upsert_session_events` 仅在 `agent_generate_loop_v2`（llm.rs:1995）调用；`load_session_events`（services/chat.rs:820）生产代码零调用（仅测试）；`kb_llm_query_loop_v2` 不持久化。
- **影响**：会话恢复仍走 legacy `chat_messages` → `seed_session_from_messages` 重建，事件溯源（工具配对/孤儿剔除）仅在当次请求生效；RAG 写入的事件无人消费（写多读零）。
- **方案**：① `kb_llm_query_loop_v2` 补 upsert（与 RAG 对齐）；② `chat_session_messages` 读路径优先 `load_session_events` 回放、legacy 表兜底迁移（或在蓝图中明确"读路径切换"的排期，避免半成品状态长期存在）。

### B5【低】LoopEvent::ReasoningDelta 未转发（推理过程不可见）
- **证据**：openai.rs 解析 `reasoning_content` → `StreamEvent::ReasoningDelta` → loop.rs:269 派发；命令层回调无对应处理；前端无 thinking 事件监听（前端无从适配，属后端缺口）。
- **方案**：命令层转发 `rag:thinking`/`llm:thinking` 事件（增量 + 完成态），前端可折叠展示；Anthropic 通道（thinking blocks）同步接入。

### B6【低】ask_user_question 前端弹窗无超时联动
- **证据**：AskUserQuestionTool 120s 超时（limits.rs:112）；前端 `showQuestionModalAsync` 无超时；后端超时后弹窗仍开，用户后续回答命中 `question_respond` → "未知的提问请求"（仅 console.warn）。
- **方案**：前端弹窗加 120s 倒计时自动关闭（提交 null）；或后端超时前发 `question:cancel` 事件由前端关闭。

### B7【低】冗余调用 `emit_pending_tool_events`（llm.rs:2001 与 2057）
- 第二次在 rag:done 之后调用，drain 必为空；建议删除并注明顺序依赖（工具事件必须先于 rag:done）。

### B8【低】工具配对语义三份实现
- 后端 `chat_types::group_tool_units`（P1-1 宣称的单一来源）↔ 前端 `chat-history.js groupToolUnits`（历史裁剪用）↔ 压缩器。前端副本无测试覆盖，语义漂移风险。
- **方案**：前端补单测；或后端把配对结果（tool_calls 是否可回放）随消息下发，前端直接消费。

### B9【低】技能工具白名单缺 `ask_user_question`
- **证据**：`core/skill.rs:32-38 ALLOWED_TOOLS` 未含 `ask_user_question`（技能表单无法声明；运行时因属 BASE_TOOLS 仍可用）。注释仍写"与 Rig Agent 注册的内置工具一致"（过时）。
- **方案**：补入白名单 + 更新注释；顺带核对与 v3 注册表全量一致（可加编译期断言：ALLOWED_TOOLS ⊆ 注册表名）。

### B10【中】Anthropic 纯对话通道取消无终态事件
- **证据**：`kb_llm_query_anthropic`（llm.rs:2173-2188）：成功→llm:done；失败→仅 `if !cancel.is_cancelled()` 发 llm:error → **取消时既无 done 也无 error**。
- **影响**：前端依赖 finally 兜底复位，但打字动画残留（前端 M1）；部分内容无法经 llm:done 落库。
- **方案**：取消时发 `llm:done`（携带已生成部分内容，与 openai 通道 Cancelled 语义一致）。

---

## 三、前端问题清单

### H1【高】停止后秒发新消息竞态：旧请求 finally 清空新请求状态
- **证据**：`agent.js:636-655` 与 `main.html:51525-51543` 的 finally 无 requestId 守卫，会清空共享全局态（chatStreaming/_chatStreamingFullContent/_chatStreamingDiv/_streamingToolCalls/chatAbortController）。用户点停止后立即发新消息 → 旧请求 finally 到达时把新请求的流式中断、停止按钮失效。
- **方案**：模块级 `activeRequestId`，finally 中 `if (requestId !== activeRequestId) return`；或把流式状态封装为请求实例对象。

### H2【高】页面重入后"停止"无法取消后端
- **证据**：切走时 `cleanupChatState` 置 `chatAbortController=null`（main.html:49743）；重入恢复路径（main.html:49837-49885）只重建流式 DOM，不重建 AbortController/abort 监听 → 重入后点停止仅本地复位（`controller?.abort()` 为 undefined），后端任务继续跑。
- **方案**：恢复 running 任务（agent_task_get 返回 running）时重建 AbortController + abort→`kb_cancel_task` 监听，并置 chatStreaming=true。

### H3【高】全局任务条滞留 running 无兜底（与后端 B2 联动）
- **证据**：`agent_global.js:43-61` 仅过滤 running，无陈旧清理；后端 B2 路径不 finish → 任务条永久显示幽灵任务。
- **方案**：后端修复 B2 为主；前端兜底：对"收到 rag:done/llm:done/error 后仍 running"或"running 超过 N 分钟"的任务本地隐藏/提供"取消并清除"。

### M1【中】取消路径打字动画残留
- **证据**：cancel-without-done 路径（后端 B2/B10）下，`agent.js` finally（636-655）、`main.html` finally（51525-51543）、`stopChatGeneration`（50324-50337）均未调 `removeChatTyping()` → `#chat-typing-indicator` 残留 DOM；下次发送会 append 重复 id 节点。
- **方案**：finally 与 stopChatGeneration 补 `removeChatTyping()`（幂等）。

### M2【中】工具耗时徽标恒 0.0s + 轨迹批量出现（与后端 B1 联动）
- **证据**：`agent.js:96` 用 `Date.now()` 记 startTs，call/result 同帧到达（后端批量 flush）→ 差值≈0；`124-131` 徽标失真。
- **方案**：后端 B1 修复后自然解决；期间可隐藏耗时徽标或后端在事件携带 ts。

### M3【中】llm:usage 缺失时清空缓存命中率记录
- **证据**：`main.html:51464` 调 `updateCacheRate(usageData,'normal')`；usageData 为 null 时 `51970-51973` 删除该模式历史记录而非占位。
- **方案**：null 时跳过 updateCacheRate。

### L1-L6【低】
- **L1** FrontendBridge 重连复用旧 port + 退避不重置（frontend-bridge.js:92-99）→ 重连前重查 `get_bridge_port`；成功时重置退避。
- **L2** trace:event 不按请求过滤，`__chatTraceMap` 无 done 时泄漏（agent.js:397-405/451）→ finally 中 delete。
- **L3** llm:done 无 streamingDiv 兜底创建（main.html:51381-51469 缺 agent.js:538-545 的兜底）→ 无 delta 直接 done 时回复不渲染。
- **L4** 工具 result 事件丢失时卡片永久"执行中…"（agent.js:112-193 无超时）→ rag:done 时对 ok===null 卡片标记超时态。
- **L5** plan:request 未消费 request_id；并行工具多审批弹窗无去重/排队（agent_global.js:116-172）。
- **L6** 任务条不展示 done/failed/cancelled 终态快照（直接隐藏）。

---

## 四、协议契约核对表（✅ 适配正常，无需改动）

| 链路 | 结论 |
|---|---|
| agent_query / kb_llm_query invoke 参数 ↔ 命令签名 | ✅ 完全一致 |
| messages 数组结构 {id?, role, content, tool_calls?, tool_call_id?} ↔ ChatMessage/ToolCallDto | ✅ 一致（expandToolHistory 生成） |
| rag:delta/done/error/status、llm:delta/usage/done/error 监听 + request_id 过滤 | ✅ 一致 |
| agent:tool_call/result（seq/call_seq/ok/summary/result/structured/skill_id）卡片渲染 | ✅ 字段逐项匹配 |
| approval:request ↔ approval_respond{requestId,approved,reason} | ✅ 一致（60s fail-closed） |
| plan:request ↔ plan_respond{planId,approved,reason} + plan:rejected sticky | ✅ 一致 |
| question:request ↔ question_respond{questionId,answer} + 选项点选 | ✅ 一致 |
| FrontendBridge：get_bridge_port + WS {type:request} → {type:result}，pomodoro/raw-parse/open-ui | ✅ 一致（含 5s 超时兜底） |
| agent_task_list/get + 任务条 + kb_cancel_task | ✅ 一致（除 B2/H3 状态闭合问题） |
| skill_list/allowed_tools/attach/mount、schedule_*、mcp_* 命令 | ✅ 一致 |
| 会话恢复：chat_session_messages 含 tool_calls → expandToolHistory 回放 | ✅ 一致 |
| 事件监听生命周期（finally unlisten、应用级一次性守卫） | ✅ 总体规范 |

---

## 五、修复优先级建议

**P0（本轮重构必须补，影响核心体验）**
1. B1 工具事件实时转发（turn 回调 drain）— 恢复 rig 时代的逐步轨迹
2. B2 + B10 取消/错误/Anthropic 路径任务收尾（agent_tasks.finish + llm:done）— 任务条与打字动画
3. H1 停止后秒发新消息竞态（requestId 守卫）
4. H2 页面重入重建取消通道

**P1（体验/一致性）**
5. B3 generating 阶段闭合（stage_end 补全）
6. B4 会话事件读路径 + chat 写入（或明确排期）
7. M1 typing 残留、M2 耗时徽标、M3 usage null 守卫

**P2（打磨）**
8. B5 thinking 事件、B6 question 超时联动、B7 冗余调用、B8 配对语义收敛、B9 白名单补全
9. L1-L6 前端小项

---

*（本报告未修改任何代码；如需按 P0 顺序实施，可逐项给出补丁。）*
