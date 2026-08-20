# DeepSeek Harness (DSH) 架构深度报告

> 基于仓库 `G:\gitProject\deepseek-harness`（`@deepseek-ai/dsh-root`，版本 `0.1.0-rc.8`）源码逐文件阅读整理。所有路径相对仓库根目录；所有行号均对应所读源码文件。只读源码，未读 node_modules/dist 等构建产物。

---

## 一、总体架构（分层图：packages/apps 划分，依赖方向）

### 1.1 顶层形态

DSH 是一个 **pnpm monorepo**（根 `package.json:11-18` 的 `workspaces` 覆盖 `packages/*/*`、`apps/*`、`vendor/*`、`native/*`、`website`），约 **220 个 `@deepseek-ai/dsh-*` 包**（逐包读取 `packages/**/package.json` 的 `name`/`description` 统计），外加 2 个应用（`apps/cli`、`apps/web`）与 1 个 vendored 框架（`vendor/`，即 Cordis）。

核心设计声明见 `README.md:5-7`：**"everything is a plugin"**，底层由 **Cordis**（vendored 的插件框架）驱动。`docs/cordis-primer.md:9-13` 给出五个核心概念：

- 插件是实现了 `Service` 的对象（函数或 `Service` 子类），带可选的 `inject` 与 `apply(ctx)`；
- **Context 是服务的仓库**，服务认领 `ctx.<key>`（如 `ctx.tools`、`ctx.llm`、`ctx.sessions`），其他插件按 key 查找，不 import 具体实现；
- 依赖通过 `inject` 声明，加载顺序由服务依赖决定而非手工排序；
- 服务通过 **TS 声明合并** 声明事件，再以 `emit`/`waterfall`/`parallel`/`serial` 四种模式派发（`cordis-primer.md:19-26` 表格）；
- **注册都是可逆副作用**（`ctx.effect()` / `ctx.on()`），卸载时按序回滚。

### 1.2 分层图

```
┌────────────────────────────────────────────────────────────────┐
│ apps/cli (bin.ts)  apps/web (Vite 壳)  website (文档)          │  应用层
├────────────────────────────────────────────────────────────────┤
│ bundle/  profile+bundle 组合层：base / web-app / headless       │  组合层
│   dsh.profile / dsh.bundle 清单 + cordis.patch.yml 逐行覆盖      │
├────────────────────────────────────────────────────────────────┤
│ host/      webserver, apiproxy(网关), frontend-static,          │  宿主/传输层
│            plugin-inventory, directory-picker                   │
│ api/       gateway(Typert Remote 分发器), remotes(BFF 组装)      │
│ client/    connection(HTTP-up/WS-down), modules, runtime,       │
│            ui-* (浏览器 UI 插件)                                 │
│ sdk/       protocol(换行 JSON-RPC), server(stdin/stdout 插件),   │
│            client(TS SDK)                                       │
├────────────────────────────────────────────────────────────────┤
│ 面向模型的能力层（seam）：fs/, shell/, subprocess/, terminal/,   │  能力 seam 层
│   web/, lsp/, sandbox/, jobs/, spill/, skill/, mcp/,            │
│   code-runtime/, attachment/, storage/, settings/, credentials/ │
├────────────────────────────────────────────────────────────────┤
│ 核心 spine（core/）：session → system-prompt → tools → agent →   │  核心层
│   agent-loop → scope；llm/ (LLM seam + token-meter + retry)；    │
│   compaction/；subagent/, goal/, workflow/, interaction/,        │
│   plan/, hooks/, guard/, preset/, session-*/ , schedule/ ...     │
├────────────────────────────────────────────────────────────────┤
│ util/ (零依赖小工具) · typert/ (RPC 类型生成) · vendor/ (Cordis)  │  底座
└────────────────────────────────────────────────────────────────┘
```

依赖方向（由 `docs/module-graph.md` 生成图与源码 import 验证）：

- **底座 → 核心**：`scope` 是唯一无依赖的"库"（`docs/subsystems/core.md:20`："dependency-free library … sits below session/ and system-prompt/"），session/system-prompt/tools 都消费它做 per-agent 作用域。
- **核心 spine 内部单向**：`session`（事件日志）→ `system-prompt`（提示词组装）→ `tools`（工具注册表）→ `agent`（接口/注册表）→ `agent-loop`（唯一具体驱动实现，`docs/subsystems/core.md:20`："agent-loop is the one concrete implementation of the public Agent contract"）。`agent-loop` 是唯一具体驱动，扩展插件只依赖 `agent` 接口、绝不依赖 `agent-loop`，因此 loop 可整体替换（`docs/architecture.md:48-49`）。
- **能力 seam 层**：`fs`、`shell`、`subprocess`、`web`、`lsp`、`sandbox` 等每个都是三件套——Service Definition（抽象接口包）、Service Provider（实现包）、Consumer（模型可见工具包）。见 `docs/architecture.md:99-104`："a **seam** is a swappable capability with three roles"。例：`fs/fs`（定义）→ `fs/fs-local`、`fs/fs-sandbox`、`fs/fs-e2b`（实现）→ `fs/tool-fs`（消费工具）。
- **传输层依赖核心**：`host/apiproxy` 实现 `ctx.apiProxy`，把 `ctx.agents`/`ctx.sessions` 投影成 RPC/事件帧；`api/gateway` 在共享 `/api` RPC 通道上做 Typert Remote 分发；`sdk/server` 通过 stdio JSON-RPC 桥接 `ctx.agents`。浏览器端 `client/connection` 是纯消费者。

### 1.3 Profile / Bundle 组合模型

- **Profile**：Harness home 里命名的一组 bundle 栈 + 用户 `cordis.patch.yml`；`web`、`headless` 是自带模板（`docs/architecture.md:19`）。
- **Bundle**：Cordis 配置行 + 代码的分发格式，在各自 `package.json` 的 `dsh` 字段声明（`docs/architecture.md:23-24`）。
- 应用顺序：profile 列出的每个 bundle → profile 的 patch → home 级 patch → `--patch` 覆盖；**patch 按行 id 整行替换配置**（`docs/architecture.md:27`）。可用 `dsh --profile web --dump-config` 查看实际启动树（`:31-33`）。
- `packages/bundle/base/README.md`：`dsh-base` 是每个 profile 的第一层，插入模型适配器、工具、持久化、沙箱与审批策略、设置/凭据、遥测、spawn/fork 子代理 provider；同一 patch 文件内用 `disabled: !!js process.platform === 'win32'` 按平台开关 bash/pwsh 两条 shell 栈。

---

## 二、Agent 运行时（loop、turn、streaming、cancellation）

### 2.1 包与角色

| 包 | 职责 | ctx key |
|---|---|---|
| `packages/core/agent` | `Agent` 接口、实时注册表（`AgentRegistry`）、initiator 作用域、`agent/*` 事件词汇 | `ctx.agents` |
| `packages/core/agent-loop` | 具体驱动 `ReactLoopAgent` + 工厂 `AgentLoop` | `ctx.agentLoop` |
| `packages/core/session` | 追加式 `SessionEvent` 日志与内存存储 | `ctx.sessions` |

### 2.2 驱动状态机（`packages/core/agent-loop/src/agent.ts`）

`ReactLoopAgent`（`agent.ts:64`）内部是一个四态 Phase（`agent.ts:38-46`）：

```ts
type Phase =
  | { kind: 'idle'; lastTurn: number }
  | { kind: 'maintenance'; abort: AbortController; lastTurn: number; wakeRequested: boolean }
  | { kind: 'running'; abort: AbortController; turn: number; step: number; wakeRequested: boolean }
```

对外只暴露两种 `AgentStatus`：`'idle' | 'running'`（`runtime-types.ts:50`，`agent.ts:99-101`）。`setPhase` 在状态翻转时发 `agent/status`（`agent.ts:104-111`）。

**驱动循环**：`wakeDriver()`（`agent.ts:172-193`）在 idle 时启动一次 `kick()`，`kick()`（`agent.ts:210-223`）反复 `while (await this.turn()) {}`，直到 `turn()` 返回 false；所有错误与取消都被 containment 在 driver 边界。每次驱动运行包在 `ctx.agents.withInitiator(this, …)` 里（`agent.ts:192`），使整条异步链可被 `requireInitiator()` 因果归因（`packages/core/agent/src/index.ts:322-326`，基于 `AsyncLocalStorage`，`index.ts:259-260`）。

### 2.3 Turn / Step 语义

文档层定义（`docs/architecture.md:65-66`）：**step = 一次模型请求 + 其调用的工具；turn = 零或多个 step**。`docs/architecture.md:67-82` 给出完整 turn 流程；`docs/agent-lifecycle.md:8-72` 是 Mermaid 时序图。

实现（`agent.ts`）：

- `turn()`（`agent.ts:246-330`）：先 `session.append('turn/start', { turn })`（`:255`），然后循环：
  1. `preStep(target, {turn, step})`（`agent.ts:225-243`）：`inbox.claim(target, turn)` 领取消息（`next-step` 全部 + turn 边界时 1 条 `next-turn`），`systemPrompt.assemble()` 组装提示词，把运行期上下文投影 `runtimeContext.project(...)` 追加到消息尾，再跑 `agent/pre-step` **waterfall**（`:234-240`）。默认决策 `{kind:'enter'}`；监听器可改写消息或 `{kind:'reject'}` 拒绝。
  2. reject → `turnEnds = {kind:'blocked'}`（`:267-270`）；首步 enter 被改空 → turn 以 `completed` 结束但**不花模型调用**（`:274-277`）。
  3. `session.append('step/start', ...)`（`:279`）→ 逐条 `user/message`（`:282-284`）→ `await this.step(assembly)`（`:287`）→ `finally` 里 `step/end`（`:292`）。
  4. 模型不再欠响应且 `nextStep` 空 → `agent/turn-stopping` **serial** 检查点（`:295-298`，监听器可以 `steer()` 让 turn 再开一步，`runtime-types.ts:262-277`）。
  5. `finally` 里 `session.append('turn/end', { turn, reason })`（`:319`）——reason 可能是 `completed|blocked|aborted|error|max-tokens|interrupted`（`packages/core/session/src/types.ts:155-177`）。
  6. `max-tokens` 是"粘性"的：后完成的 step 不能把 turn 结果降级（`:290`，`:410`）。
- `step()`（`agent.ts:332-420`）：内部 `while(true)`：
  1. `buildRequest()`（`:426-514`）：从 session 的 `requestHeader()` 恢复配置，跑 `agent/request` waterfall（`:457-460`），`ctx.llm.prepareCall()` 绑定 adapter 注册（`:468`），`canonicalHeader()` 组 header，首请求/变更时追加 `request/header` 事件（`:484-489`），provider/model 变更时追加 `request/context`（`:497-502`）；最后 `markAgentLoopRequest(deepFreeze({...}))` 冻结请求（`:505-512`）。
  2. 流式消费：`for await (const chunk of stream)`，每个 chunk 先 `session.append('assistant/chunk', …)`（**:350**，可回放）再喂给 `BlockAssembler`（`:343-352`）。
  3. 流失败：若 signal 已中止且有中断内容，先补一条 `assistant/message {interrupted:true}`（`:354-369`）；否则跑 `agent/request-error` waterfall（`:374-384`），监听器（llm-retry、compaction-basic）可返回 `{kind:'retry'}` 重试（`:386-389`）。
  4. 组装完整 `assistant/message` 事件（`:400-409`），`sourceEventSeqs: chunkSeqs` 引用全部 chunk seq——**这是"模型可见即已记录"不变量的实现载体**（`docs/architecture.md:96`）。
  5. `finish.kind === 'max-tokens'` → 返回 `{kind:'max-tokens'}`；否则过滤 `tool-call` 块，交给 `executeToolCalls()`（`:412-418`）。

### 2.4 工具调度并发模型（`packages/core/agent-loop/src/tool-calls.ts`）

`executeToolCalls()`（`tool-calls.ts:59-101`）按 **model 顺序** 分组调度：

- 每个调用块先 `parseArguments()`（`:104-110`，非法 JSON 保留为文本）构造 `PlannedCall`（`:71-80`）。
- 取第一个未开始调用的 `ctx.tools.executionMode(exec).kind`：`exclusive` 单独成组（形成屏障），`parallel` 取剩余全部成组（`:87-89`）。
- `runGroup()`（`:121-246`）维护 **有界滚动池**：`fillPool()`（`:198-213`）在 `inFlight.size < maxParallelToolCalls`（默认 10，`constants.ts:6`，可配置 `agent-loop.config.maxParallelToolCalls`，`index.ts:250-252`、`tool-calls.ts:131`）内启动调用；每个 parallel 调用**启动前重新分类**（`:203-204`，注册表变更可制造新屏障）。
- **dispatch 可以重叠，policy/结果/上下文按 model 顺序提交**：`commitReady()`（`:146-160`）只沿连续 model 序提交已 settled 的 slot，先 `finalize`（走 post-execute）或 `finish`，再 `appendToolResult()`（`:268-289`），`additionalContexts` 进 `acceptContext`（进入下一 step 的 next-step inbox，`:156`）。
- **取消**：abort 后停止补池、drain 已启动调用、为未启动调用补 `tool/call`+合成 `tool/result`（`appendSkippedToolCall`，`:249-259`，错误码 `TOOL_ABORTED_BEFORE_DISPATCH`），保证**回放完整**；调度器内部失败则停止新派发、drain 已启动的、抛出首个错误且**不伪造结果**（`:140-143, 231-235`）。

### 2.5 流式与事件（agent → UI/RPC 的路径）

模型流在 loop 内：adapter → `StreamChunk`（`packages/llm/llm/src/types.ts:312-324`：`block-start/text-delta/reasoning-delta/tool-call-delta/block-end/usage/finish`）→ 逐 chunk `assistant/chunk` 会话事件 → UI/RPC 通过 `session/event` 火线拿到。三域事件分工（`docs/architecture.md:56-61`）：

- **Session 事件**（`session/event`）：持久事实，`turn/*`、`step/*`、`user/message`、`assistant/*`、`tool/*`、`request/*`、`agent/inbox/spliced` 等（`packages/core/session/src/types.ts:236-336`）；
- **Agent 事件**（`agent/*`）：实时句柄，`agent/status`、`agent/inbox/inserted|claimed|discarded`、`agent/pre-step`(waterfall)、`agent/request`(waterfall)、`agent/request-error`(waterfall)、`agent/turn-stopping`(serial)、`agent/error`（`packages/core/agent/src/runtime-types.ts:146-291`）；
- **能力事件**：`fs/*`、`tools/*`、`telemetry/*` 等附加策略。

完整的生产者/消费者矩阵见 `docs/event-producer-consumer.md:8-65`（生成自 TS Program）。

### 2.6 取消语义

- `Agent.cancel(cause, options)`（`agent.ts:134-140`）：默认清 inbox（`keepInbox` 可保留），再 `phase.abort.abort(cause)`——**首个 cause 获胜**；`AgentCancelCause = {kind:'user'}|{kind:'parent'}|{kind:'hook',reason}|{kind:'disposed'}`（`docs/subsystems/core.md:196-200`）。
- 取消在 loop 的每处 `signal.throwIfAborted()` 检查点生效（`agent.ts:231,241,253,264,278,294,297,336,347,353,385` 等）；abort 的 turn 以 `{kind:'aborted', reason}` 结束（`agent.ts:303-305`）。
- 工具层取消见 §3.5：body 已启动 → `ABORTED`，未启动 → `ABORTED_BEFORE_DISPATCH`；调度器先 drain 已启动调用再提交合成结果（`tool-calls.ts:237-242`）。
- 取消后唤醒：`send()` 在已中止 activity 后到达的唤醒消息会重分类为 `next-turn`（`agent.ts:113-120`），`wakeDriver` 用 `wakeRequested` 闩锁保证"取消收敛后补一次唤醒"（`agent.ts:172-181`；对应 Agent Note `2026-08-07-cancel-convergence-wake-latch`）。
- 维护任务：`runMaintenance()`（`agent.ts:142-162`）从真正 idle 启动非 turn 任务（compactNow 用它）；`whenIdle()`（`:195-200`）跟随整个 activity 收敛。

### 2.7 工厂与所有权（`packages/core/agent-loop/src/index.ts`）

- `AgentLoop extends Service implements AgentFactory`（`index.ts:296`），构造时 `ctx.agents.setFactory(this)`（`:350`），注册 `{{provider}}`/`{{model}}`/`{{cwd}}` 提示词变量（`:351-353`）。
- `create()`/`createAgent()`/`resume()`（`:589-710`）：`SessionPreparation.create(sessions.prepare(...))` 准备未发布 session → `prepare()`（`:459-578`）构造 `ReactLoopAgent`、融合三路 abort（调用方 signal + owner fiber unload + factory teardown，`:479-487`）→ 可选 `setup` 回调在**未发布**状态下组装 agent 作用域（`:638-640`）→ `publish(source)`（`:556-570`）按序 `sessions.enter → agents.enter → sessions.announce → agents.announce → agent/session-start`，任一步失败整体回滚。
- 配置式 agent（`config.agents`，`:355-381`）：启动时 create 或按 `resumeSessionId` 从持久化恢复；launcher 可用 `ctx.provide(CONFIGURED_AGENT_IDENTITIES_KEY, ...)` 固定身份（`:205-211`）。
- 生命周期顺序：`dispose()` = disposed 取消 → `whenIdle()` → `scope.dispose()` → 注销 → 摘 session（`index.ts:497-520`）。

---

## 三、工具系统（接口、注册、schema、并发、权限/沙箱、重试）

### 3.1 包与核心类型（`packages/core/tools/src/index.ts`）

- `ToolDefinition extends ToolSchema`（`:222-288`）：必填 `output: ToolOutputDefinition`（`:212-219`：canonical `schema` + 纯函数 `render` + 可选 `presentationMeta`）、`execute(args, exec)`（`:235`）、可选 `finalizeContent`（`:247`，同步 last-mile 内容变换，执行开始时快照）、`timeoutMs`（`:255`，**永不上模型**，`schemas()` 只白名单 name/description/parameters）、`isConcurrencySafe(args)`（`:269`，纯同步分类器，**只有精确 `true` 才并行**，抛异常/缺省都按 exclusive 处理）、`presentCall`/`presentResult`（`:279-287`，纯 UI 渲染意图）。
- 执行输入/上下文：`ToolExecutionInput`（`:314-338`：`callId`、`rootCallId`、`name`、`arguments`、`agent`、`parent` token、`signal`）→ 注册表补 `token` 后成 `ToolExecution`（`:379-384`）；`ToolRunContext`（`:404-421`）额外给 `deferContext()` 与 `concludeTurn()`。
- 结果：`ToolExecutionResult = ToolExecutionSuccess | ToolExecutionFailure`（`:555-580`），成功必须是无损 JSON 的 canonical `value`（`createSuccessResult`，`:1793-1823`，先 `snapshotJsonValue` 再 `validateJsonSchemaValue` 校验 output schema，`render` 后 `snapshotProjection`）。

### 3.2 注册 / 作用域 / 限制

- `register(definition)`（`:1037-1062`）：校验 output、timeoutMs；`run_code` 名保留（`:1054-1056`）；返回精确 disposer。注册按 scope 分层：`ToolLayer`（`:714-754`）含 `NamedEntries<ToolDefinition>`、`AnonymousEntries` 的 restrictions 与 guards、`mode`。
- `view(scope)`（`:1152-1193`）：一次层遍历解析可见工具——继承面（global + 祖先链，近者遮蔽远者）→ 应用整条链的 restrictions（`:1174`）→ 自身层注册最后插入（shadowing）→ 非 native 模式追加保留的 `run_code` 传输（`:1189-1191`）。
- `restrict(filter)`（`:1071-1098`）：只允许 `agent.ctx` 作用域内调用，allow/deny 交集生效，未知名/保留名抛错。
- `guard(guard)`（`:1110-1116`）：**单调守卫**，同步返回字符串即拒绝；多个守卫只能拒绝、不能强放行（`ToolGuard` 注释 `:703-711`）。`guardReason` 先 global 再 scope 链（`:1119-1128`）。
- `presentAs(mode)`（`:946-974`）：per-scope 声明 `'native' | 'code' | 'both'` 展示模式（`:651`）。Code Mode 下模型只能直接调 `run_code`，其它工具经 SDK 子派发；`collapses()`（`:1324-1326`）是"模式折叠"安全谓词，`resolveExecution`（`:1221-1226`）与 `createExecution`（`:1380-1381`）共享，避免"提示词宣布一套、执行另一套"的绕过。

### 3.3 Schema 与提示词集成

- `schemas(scope)`（`:1234-1236`）只投影三个字段；`schemaOf`（`:1256-1267`）用 `snapshotJsonValue` 脱附参数。
- tools 服务构造时把 schema 提供者挂到 `ctx.systemPrompt.tools()`（`:832`），并在 code 模式注册 `tools:code-only`（order 99）与 `tools:sdk`（order 100+，`:855-892`）两个提示词段。
- 系统提示词组装在 `packages/core/system-prompt/src/index.ts`：`PromptSection`（`:53-75`，`order` 升序拼接，persona=0、工具指引 100-199）、`PromptContext`（`:78-85`，动态上下文）、`PromptAssembly`（`:115-120`）、`renderPrompt`（`:212-217`，`{{variable}}` 严格插值）、`renderContextSnapshot`/`joinContextSections`（`:224-240`，运行期上下文快照，即本会话当前使用的 "Current runtime context" 段落）。

### 3.4 执行流水线（pre → guard → around → body → post → finalize → result）

流水线全景见 `docs/tool-execution-pipeline.md:8-58`（Mermaid）。代码路径：

- `execute()`（`:1342-1344`）→ `prepareExecution()`（`:1463-1507`）：
  1. `createExecution()`（`:1364-1451`）：materialize 参数（无损 JSON + deepFreeze），`run_code` 折叠调用在**策略之前**确定性拒绝为 `UNKNOWN_TOOL`（`:1423-1444`）。
  2. caller 已取消 → `final-result` + `ABORTED_BEFORE_DISPATCH`（`:1470-1472`）。
  3. `tools/pre-execute` **waterfall**（`:1475-1478`）：`{kind:'allow'|'deny'|'ask'}`（`:588-591`）。
  4. `ask` → `serviceAsk()`（`:1689-1729`）：经 `ctx.get('approval')` 一次性审批，`allowed-once` 才放行，`rejected/cancelled/unavailable` 一律 deny（无 approval 服务默认 deny，**fail-closed**）。
  5. 单调 guards（`:1486-1488`）→ denial 物化为错误结果（`:1489-1499`）。
- dispatch 阶段（`:1569-1599`）：`tools/execute` **waterfall**（`:1573-1576`）包住 `dispatchToolBody()`（`:1532-1560`）——wrapper 可替换 `exec.signal`，注册表把 caller signal 与 wrapper signal **融合**（`fuseToolSignals`，`:1889-1916`），body 只被调用一次，取消不 abandon 已启动 promise（drain 到 quiescence）。
- 结果规范化：`normalizeDispatchResult`（`:1826-1844`）；post 阶段 `tools/post-execute` **waterfall**（`:1742-1781`）：`accept`（可替换 content/value、附加 `additionalContexts`）或 `block`（矫正反馈转 isError）。
- `finishScheduledExecution`（`:1631-1646`）：materialize（`materializeFinalResult`，`:1847-1862`，无损 JSON + deepFreeze）→ `finalizeContent` 快照回调（`:1649-1654`）→ `notifyResult`（`:1657-1676`，冻结 exec 后发 `tools/result` emit，监听器失败 containment）。
- 调度器专用符号入口：`TOOL_RUNTIME_SCHEDULER`（`:466`），`prepare/dispatch/finalize/finish` 四段（`:451-460`），供 agent-loop 的滚动池在"有序 policy、重叠 dispatch"下使用。

### 3.5 超时 / 重试 / 沙箱

- **超时**（`packages/guard/timeout-policy/src/index.ts`）：`tools/execute` wrapper（`:56`）读工具声明的 `timeoutMs`（`:57`），用 `@deepseek-ai/dsh-timeout` 的 `deadline()`（`:61`）在 `exec.signal` 上换装 deadline，`next()` 返回后 `timeoutOf()` 区分"自己的定时器"与"外层取消"（`:73`）；自己的超时把结果替换为 `TOOL_TIMEOUT` 结构化错误（`:41-48`）。设计是**协作式**：工具承诺遵守 `exec.signal`，wrapper 绝不 race/abandon 工具 promise（`:2-4`）。
- **LLM 重试**（`packages/llm/llm-retry/src/index.ts`）：监听 `agent/request-error`（`:210-219`）。`recover()`（`:156-208`）：无策略 → 放行；`mode:'normal'` 时 `failure.code` 不在 `retryableCodes` 则放行；重试次数**基于回放**——在 session 日志里 `findLast` 匹配 turn+step+provider+policyKey 的 `llm/retry` 事件（`:182-188`）；退避为指数 + 对称抖动（`:58-63`，默认 initialDelayMs 500 / maxDelayMs 10000 / jitter 0.1 / maxRetries 5 / retryableCodes `[EMPTY_RESPONSE, RATE_LIMIT, SERVER, TIMEOUT, TRANSPORT]`，`packages/llm/llm/src/retry-policy.ts:14-24`）；provider 的 `Retry-After` 优先（`:194-205`）。**先落盘 `llm/retry` 事件再等待**（`:150`，"durable before its cancellable wait"）。
- **沙箱**（`packages/sandbox/sandbox/src/index.ts`）：`SandboxMode = 'read-only' | 'workspace-write' | 'danger-full-access'`（`:29`）；`SandboxProvider.confine(argv, policy)`（`:158-176`）返回 `ConfinedArgv`（`:95-116`，含 `enforcement: 'full'|'partial'`、denialSignatures、runnerFailureRules），**fail-closed**（`:152-157`）。策略按调用携带（`:61-68`）。`packages/sandbox/sandbox-policy/src/index.ts` 的 `resolve(request)`（`:135-142`）优先级：显式批准的 mode > session 覆盖 > 部署默认（默认 `'read-only'`，`:62-65`）；session 覆盖来自日志中最后一个 `sandbox/mode` 事件（`session-mode.ts:52-58`，log-only 不污染模型历史）。沙箱策略还渲染为 `sandbox:policy` 上下文段（order 110，`index.ts:112-123`），随运行期上下文进日志。
- **文件系统门**：`fs/write-intent`、`fs/edit-intent` waterfall（`packages/fs/fs/src/index.ts:58,66`）由 `tool-fs`/`tool-str-replace-editor` 派发、`fs-observation-policy` 监听做 read-before-edit 与版本守卫（`docs/event-producer-consumer.md:34-36`）。

---

## 四、上下文与状态管理（历史、token 预算、压缩/compact）

### 4.1 会话日志 = 唯一事实源（`packages/core/session/src/index.ts`）

- `Session`（`:425-758`）：追加式 `log: SessionEvent[]`，`append()`（`:604-655`）做无损 JSON 快照 + `surfaceManager.validateNext` + 入 log 后同步通知 `session/event` 观察者（**热路径不阻塞 IO**，持久化插件异步缓冲，`:570-575`）；事件深度冻结（`:627-633`）。seq = log.length（`:565-567`）。
- `SessionEventMap` 持久事件词汇（`packages/core/session/src/types.ts:236-336`）：`turn/start`、`turn/end`（含 reason）、`step/start`、`step/end`、`user/message`、`assistant/chunk`、`assistant/message`（含 usage/interrupted）、`tool/call`、`tool/result`（含 meta/sourceEventSeqs）、`request/header`、`request/context`、`session/end-seed`；插件用声明合并扩展（goal 的 `goal/change`、agent 的 `agent/inbox/spliced` 等）。
- 事件信封：`{type, seq, time, data}` + 可选 `surfaceOp`/`sourceEventSeqs`/`ignorable`（`docs/persistence-catalog.md:58-90`）。**surface** 类型只有三种：`user/message`、`assistant/message`、`tool/result`（`docs/persistence-catalog.md:23-27`），它们决定如何进入"模型可见表面"。
- `deriveMessages()`（`:726-747`）：从 surface 节点增量投影 LLM 历史，**缓存 + 增量**（`derivedNodes`/`derivedGeneration` 游标）；`surfaceOp: {op:'replace'}`（compaction 用）会使 `replaceGeneration` 递增并重建缓存。空内容的 assistant/message（纯 max-tokens）不进入历史（`:740-743`）。
- 运行时不变式："**Model-visible means logged**"（`docs/architecture.md:96`）——任何进入模型请求的内容必须能从日志重建，由 runtime invariant 断言（`docs/architecture.md:96`；`agent-loop/src/invariant.ts`）。

### 4.2 Token 计量（`packages/llm/token-meter/src/index.ts`）

- `ctx.tokenMeter.measure(session, requestHeader?)`（`:116-147`）：**回放折叠**（per-session `ReplayState` 游标，`:160-181`）。基线只取"最新一次成功调用的 usage"当且仅当该调用信封与当前 requestHeader 匹配 **且 provider 总 token 不低于全量启发式价格**（`:125-127, 246-248`，保守锚点）；否则用启发式估算（`estimate.ts`：`CHARS_PER_TOKEN=4`、`BLOCK_OVERHEAD=4`、`ROLE_OVERHEAD=4`，`:13-19`）。`totalTokens = max(0, baseline + surfaceDelta)`（`:143`）。
- 表面折叠（`surface-fold.ts:42-65`）：append 推节点 `{seq, tokens}`，replace 用 `deltaTokens = tokens - removed` 拼接。
- 辅助调用分类 `purpose?: 'compaction' | 'session-title'`（`llm/src/types.ts:376`），适配器可据此区分计费/策略。

### 4.3 压缩（`packages/compaction/compaction-basic/src/index.ts`）

- 两个触发点（`_registerAutomaticCompaction`，`:137-224`）：
  1. `agent/pre-step` 上做 **step 压力检查**（`:147-165`）：`compactIfNeeded(agent, 'pressure', signal)`；
  2. `agent/request-error` 上做 **context-overflow 恢复**（`:179-223`）：仅当 `failure.code === CONTEXT_WINDOW_EXCEEDED_CODE`，按模型策略 `maxOverflowRetries` 限次，成功压缩且 surface generation 前进才返回 `{kind:'retry'}`。
- `compactIfNeeded()`（`:258-332`）：先解析路由目标模型策略（`resolveTargetPolicy`，config.ts）→ 压力触发先做**无模型剪枝**（`ctx.get('toolResultPruner')`，可选，`:281`）→ 重测 → `selectCompactableRange(session, measurement, retainTokens)` 选区间 → `compactRegion()` 循环直到低于阈值或达到 `compactionRetries`。
- `compactSurfaceRegion`（region.ts，`:349-357`）：对表面区间做**替换事务**——用 LLM 摘要（`summarizeWithLlm`，summarizer.ts）生成 summary，追加新节点 `surfaceOp:{op:'replace'}`，`sourceEventSeqs` 覆盖被遮蔽节点，可回放；`agent/pre-step` 决策与 `request-error` 恢复之间"在失败的 step 关闭与 turn 关闭之间"运作（`docs/agent-lifecycle.md:76`）。
- 手动压缩：`compactNow()`（`:368-420`）经 `agent.runMaintenance` 从 idle 执行，完成后强制 `sessions.flush` 检查点（`:395-397`）。`command-compact` 是 `/compact` 人类命令。
- 无模型剪枝器（`packages/compaction/compaction-tool-result-pruner/src/index.ts`）：`pruneContent`（`:83-122`）超阈值 → 保 headChars + 一个 `PRUNE_MARKER` + 保 tailChars；`pruneSession`（`:136-184`）对每个被剪的 `tool/result` 节点先追加 `compaction/prune` **shadow-price** 事件（用 tokenMeter.estimateMessage 定价，`:162-166`），再追加 `tool/result` 替换（`:167-173`），保证回放可恢复替换输入。

### 4.4 持久化格式

- 抽象 seam（`packages/session/session-persistence/src/index.ts`）：`append` 要求"首个事件的 seq 必须等于存储的 next-seq"（`:136-143`）；`load` 返回以平衡 `turn/end` 结尾的日志（`:181-183`）。`coordinator.ts` 的 `PersistenceCoordinator`（`:588`）：per-id promise 链串行化、`appendCore` 续性强制（`:698-709`）、崩溃修复（`prepareCore`/`commitPrepared`，`:892-963`）、`SessionWriteBehind` 200ms 批量窗口（`:30`，write-behind.ts:22-159）、事件钩子 `session/created→init`、`session/event→enqueue`、`session/flush→flush`、`session/disposed→retire`（`:1118-1132`）。
- JSONL 后端（`packages/session/session-persistence-jsonl/src/index.ts`）：目录 `root/<projectKey>/<~XXXX 编码 id>/session{,.jsonl.zstd}`（format.ts:121-179）；header 行 `type:'session'` 带格式版本（format.ts:33-108，`SESSION_FORMAT_VERSION = 0`，`docs/persistence-catalog.md:10`）；**原子落盘 = 临时文件 + fsync + `link()` 发布**（`:513-569`）；zstd 帧撕裂恢复（`:348-419`）；chunk 行打包（`packages/core/session/src/chunk-rows.ts`：连续 `assistant/chunk` 的 delta 序列压成 `text-chunks`/`reasoning-chunks`/`tool-call-chunks` 行，`:64-67`，`MIN_RUN = 3`，`:77`）。

---

## 五、多代理机制（goal、subagent、workflow、ralph）

### 5.1 Subagent（`packages/subagent/`）

- 抽象 seam `ctx.subagents`（`packages/subagent/subagent/src/index.ts`）：**命名 provider 注册表**（多 provider 共存，`registerProvider`，`:385-401`），仿 LLM adapter 注册表而非单例 executor（`:8-10`）。
- `start(name, request)`（`:430-442`）：能力校验（`assertCapabilities`，`:497-512`：outputSchema/depthLimit/toolFilter/persona）→ 深度断言 → `provider.start(resolved)`，通过 `observeRun` 发 `subagent/start`/`subagent/end` 生命周期事件（`:157-166`，按父 agent scope 过滤）。
- 连续型子代理：`startContinuable`（`:212-214`）、`followup`（`:231-238`）、`interrupt`（`:255-257`）、`reportFrom`（`:270-276`）；`SubagentContinuationManager`（continuation.ts）持有子 AgentHandle，每轮都走子自己的 inbox，provider 只提供 detached creation spec（`:16-25`）。
- 提供者实现：in-process `subagent-spawn-in-process`（全新 child）与 `subagent-fork-in-process`（以父日志前缀为种子）；跨进程 `subagent-acp`（Agent Client Protocol stdio）、`subagent-dsh-sdk`（stdio JSON-RPC）、以及官方 `subagent-claude-code` / `subagent-codex`。共享驱动 `subagent-in-process-driver`（`startInProcessRun`，`:102-148`：`parent.ctx.agents.create({sessionId, meta: childSessionMeta, seed, setup})`，`:132-139`；`drivePublishedRun` 恰好驱动一个 turn：`child.followup(...)` + `whenIdle`，`:154-205`）。
- 模型可见工具：`tool-subagent`（`description`/`prompt`/`run_in_background`，输出 `background jobId | continuable subagentId | foreground runId+output`，`:316-374`）、`tool-subagent-control`（send_message/interrupt_agent/list_agents）、`tool-subagent-report`（child 内 report）。

### 5.2 Goal（`packages/goal/`）

- 领域（`goal/src/domain.ts`）：`GoalOperation = create|edit|pause|resume|complete|block|clear`（`:14-22`）；每次变更写一个 `goal/change` session 事件（全量快照或 clear tombstone，`:23-44`）；`FoldedGoal`（`:71-82`）是纯回放折叠。工具执行时的权威性检查（`tool-goal/src/authority.ts`）：`goalToolExecution`（`:50-63`）要求 agent 在注册表、running、是当前 initiator、turn 打开；`requireDirectHuman`（`:90-93`）要求本 turn 有 `source.kind === 'user'` 的直接人类消息；`completionAuthority`（`:101-108`）允许"直接人类或恰好被接纳的 goal round"。
- 模型可见工具（`tool-goal/src/index.ts`）：`get_goal`（`:195-205`）、`create_goal`（`:207-232`，`objective` 必填 + `max_goal_rounds` 可选）、`update_goal`（`:234-337`，`goal_id`+`revision` 必填，`action` 枚举 `edit|pause|resume|complete|blocked`）；`blocked` 有连续轮数阈值（默认 3，`:32-34`）。
- 轮次驱动（`goal-round-driver/src/index.ts`）：只在**静止点**接纳下一轮（`readyToDrive`，`:103-109`）；`drive`（`:138-205`）先 `sessions.flush` 持久化检查点，再 `agent.followup` 排队 `<goal_round>` 提示（source `{kind:'goal', goalId, revision, round}`，`:176-179`）；**竞态栅栏** `validReservation`（`:334-347`）：要求精确 live goal revision、`activation==='armed'`、`source.round === goal.roundsStarted + 1`、claim 的消息内容一致，在 `agent/pre-step` 上 enforce（`:349-414`）。

### 5.3 Workflow（`packages/workflow/`）

- 抽象 seam `ctx.workflowEngine`（`workflow/src/index.ts`）：`start(request)` 返回 `WorkflowRun`，result 永不复现 rejection（`:157-168`）；事件 `workflow/start|phase|log|agent-start|agent-end|end`（`:43-89`）；`WorkflowError` 带 `fatal` 标志——fatal 错误必须杀死脚本，普通 child 失败只是该项置 `null`（`:130-148`）。
- 引擎实现 `workflow-worker-thread`：在 worker 线程执行模型写的编排脚本，`agent()` 桥回 `ctx.subagents`（包描述）。
- 模型工具 `tool-workflow`（`tool-workflow/src/index.ts`）：参数 `script`（纯 JS body）+ `meta{name,description}` + `phases[]` + `args`（`:220-256`）；工具把自身 abort signal 桥到 `run.cancel('parent step aborted')`（`:299-300`），`finally` dispose（`:316-330`）；`WorkflowRecorder`（`:73-131`）把 `workflow/*` 事件投影为 `tool-workflow/run-start|agent-start|agent-end|run-end` 会话事件。

### 5.4 Ralph（`packages/workflow/tool-ralph/src/index.ts`）

- 工具 `ralph`（`:412-478`）：参数 `objective` + 可选 `maxRounds`（默认 256，部署上限可配，`:37`）。**固定编排脚本** `RALPH_SCRIPT`（`:90-177`）：每轮用 `agent(prompt, {label, phase, schema: reportSchema})` 开一个**全新 child**（无父会话种子、无先前 child 会话），只携带不可变 objective + 上一轮的**有界结构化 handoff**（`maxHandoffChars` 默认 16384，`:38`）；报告校验（`validateReport`，`:112-149`）强制 `continue|complete|blocked` 三态的字段约束（continue 需 nextSteps 且 blocker 空；complete 需 evidence 且无 nextSteps；blocked 需具体 blocker）。
- 执行（`:437-475`）：`requireFreshProvider`（`:220-232`）要求 provider 支持 structured output 且**不继承父上下文**（`inheritsParentContext === false`）；`ctx.workflowEngine.start({script: RALPH_SCRIPT, maxTotalAgents: maxRounds, ...})`；abort 桥接 `run.cancel`；终止 reason 非 completed 视为错误（`stopReasonError`，`:336-349`）；结果 `{runId, agentsStarted, result}`。
- 提示词段 `tool:ralph`（order 116，`:407-411`）明确限定使用场景："ONLY when the direct human explicitly asks for a Ralph loop…；普通长期任务用 goal 工具，有界委托用 subagent/workflow"。

---

## 六、事件/传输模型（UI、RPC、JSON 流）

### 6.1 三套传输面

| 面 | 客户端 | 传输 | 代码位置 |
|---|---|---|---|
| 浏览器 Web UI | `client/connection` + `client/ui-*` | HTTP-up (POST `/api/<method>`) / WebSocket-down (`/api/events.mux`、`/api/events.host`) | `packages/client/connection/src/index.ts:161-195` |
| SDK（跨进程） | `sdk/client` | stdio **换行分隔 JSON-RPC 2.0** | `packages/sdk/protocol/src/transport.ts:62-269` |
| 自动化 ACP | `acp/acp` | JSON-RPC stdio | `packages/acp/acp` |

### 6.2 SDK JSON-RPC（`packages/sdk/`）

- 帧分类（`protocol/src/transport.ts:2-4`）：带 `id`+`method` = request；仅 `id` = response；仅 `method` = notification；坏行忽略。`JsonRpcLineTransport`（`:62-269`）UTF-8 缓冲、`\n` 分帧、`req_<uuid>` 请求 id、AbortSignal 弃请求、`flush()` 写屏障。
- 命名类型（`protocol/src/types.ts:93-105`）：请求 `initialize`（cwd/provider/model/maxTokens）、`session/prompt`（sessionId + ContentBlock[] → messageId）、`shutdown`；通知 `session.event`、`session.status`、`subagent.started`、`subagent.finished`。
- 服务端（`sdk/server/src/server.ts:53-240`）：订阅 `session/event→session.event`、`agent/status→session.status`、`session/created→subagent.started`（有 parentSession 时）、`subagent/end→subagent.finished`（仅 in-process child，`:87-103`）；`session/prompt` 懒建 session（`ctx.agents.create({sessionId, meta:{cwd}, agentOptions})`，`:223-231`）后 `agent.followup`（`:140-141`）；stdout 专用于协议帧（`index.ts:2-4,53-59`）；shutdown 后 flush → dispose 根 fiber → `process.exit(0)`（`index.ts:67-74`）。
- 客户端（`sdk/client/src/client.ts:184-458`）：`HarnessClient` 自己 spawn 子进程（`start`，`:203-261`）、请求超时用 AbortController 放弃（**:318-321**，**无 wire 级取消**，`run_in_background` 语义下服务端继续跑，`:181-183`）；`subscribeSessionTree()`（`:361-372`）用 `subagent.started` 构建父子映射做树内过滤。高层 `DeepSeekHarness.run()`（`api.ts:146-194`）：订阅会话树 → `session/prompt` → 等到 durable 入队回执（`agent/inbox/spliced` 含该 messageId，`:225-229`）→ 等到 `session.status = idle`（`:171-183`）。进程回收阶梯：协议 shutdown → stdin EOF → SIGTERM → SIGKILL（`dispose.ts:82-99`）。

### 6.3 Web 传输（HTTP-up / WebSocket-down，`packages/client/connection/`）

- 三条路径（`api-path.ts:8-14`）：`API_PATH='/api'`、`MUX_EVENTS_PATH='/api/events.mux'`、`HOST_EVENTS_PATH='/api/events.host'`。
- **上行**：浏览器 `createWebConnectionRpc`（`client/rpc.ts:19-49`）POST `/<channel>/<endpoint>`，信封 `{type:'client-request', rpcId, method, payload}`；服务端 `rpcFetchHandler`（`rpc-host.ts:144-188`）解码校验 `clientRequestSchema`，`method===endpoint` 后调 handler，回 `ServerResponse`。`HostConnectionService`（`rpc-host.ts:43-142`）提供 `ctx.connection.rpc.handle/intercept`；`PRIVILEGED_METHODS`（`index.ts:89-119`）把设置/凭据写、目录选择、模型发现等**钉在 loopback**。
- **下行**：`WebSocketDownlinks`（`websocket-downlink.ts:51-138`）`noServer:true` WSS；客户端向 WS 发任何消息都会被 1008 关闭（`downlink only`，`:109-111`，上游只有 HTTP）；`pump()` 把帧流逐条 `send` 成完整 `ServerRequest`（`:118-137`）。
- **重连**：`ConnectionController`（`client/connection.ts:61-202`）：每一代同时开两条流 → 严格就绪握手（`host.describe` 可达 + 两个 onOpen，`:132-151`）→ `onConnected`；任一流结束 → `reconnecting` + 指数抖动退避（base 500ms ×2 cap 10s，`:19-24,91-95`）。
- 信任栅栏（`api-request-trust.ts:96-123`）：host 权威（loopback 或声明 trustedHosts）+ 拒绝 `sec-fetch-site: cross-site` + Origin 匹配；`assertTrustedAuthority` 配置错误直接拒绝插件加载（`:54-58`）。
- SSE 回退：`toFetchHandler`（`packages/host/apiproxy/src/fetch/handler.ts:243-319`）对两条 events 路径提供 SSE（`data: <JSON ServerRequest>\n\n`，`:203-236`），供非浏览器/进程内客户端（`InProcessApiClient`，`fetch/client.ts:520-541`）。

### 6.4 ApiProxy 与事件帧（`packages/host/apiproxy/`）

- `ApiProxy` 契约（`api/index.ts:22-42`）：sessions/subagents/host/workspace/skills/agentPresets/events/goals/settings/credentials/llm/downloads/respond 十二组方法。
- **四象限消息模型**（`api/rpc.ts:1-187`）：`ClientRequest` / `ServerResponse` / `ServerRequest`（可应答帧如 `approval/requested` 与纯推送共用）/ `ClientResponse`；`rpcId` 由发起方铸造、应答方回显（`api/rpc.ts:14-29,137-140`）。边界一律 Zod 双层校验（`api/rpc.schema.ts`）。
- **mux 帧**（`api/events.ts:69-108`）：`session/event`（裸 SessionEvent + 可选 ToolEventView）、`session/subscribed`（基线 lastSeq）、`approval/requested`（稳定 rpcId）、`approval/resolved`、`question/requested|resolved`、`session/queue`（完整 inbox 快照）、`session/jobs`（完整任务快照）、`session/projection`、`stream/error`。**host 帧**（`:127-155`）：`host/session-added|removed|status`、`host/agent-error`、workspace 帧、`host/remote-event`（按白名单 `API_REMOTE_FORWARDED_EVENTS` 原样转发 host 事件，`packages/api/remotes/src/remote-events.ts:17-29`）。
- **重连收敛 = 基线回放**（`api-proxy.ts:3344-3446`）：流打开时回放 `session/subscribed`（每个已附加 session）、仍挂起的 `approval/requested`/`question/requested`（**带稳定 rpcId**，刷新恢复，`:3350-3361`）、`session/queue`、`session/jobs` 快照；瞬态状态走**整快照而非增量**。
- 应答路由：`respond()`（`api-proxy.ts:3610-3656`）按回显的 rpcId 找到 `PendingApproval`/`PendingQuestion`，解析 promise 并广播 resolved 帧。背压：`FrameQueue` 是 pull-mode 单等待者异步迭代器（`:359-390`）；浏览器端观察缓冲是 microtask 批处理（`fetch/client.ts:271-290`）。
- 网关（`packages/api/gateway/src/index.ts`）：`TypertGatewayService` 在共享 `/api` RPC 通道上 `intercept('/api', claimsEndpoint, dispatchRpc, {authority:'trusted-host'})`（`:104-111`），把 `<namespace>/<method>` 分派到活着的 Cordis Service（严格生成描述符或保守 SRC 派生，`:224-263`），参数/结果边界用 Zod + `assertJsonValue`（`:614-673`）；client 侧 `remote.<namespace>` 类型化服务走 `connection.rpc.call('/api', endpoint, {args}, signal)`（`api/gateway/src/client/index.ts:399-414`）。

### 6.5 会话事件 → UI 的完整链路

`Session.append`（`packages/core/session/src/index.ts:604-655`）→ `session/event` emit（`:641-647`）→ `api-proxy.ts:3387-3408` 为每个事件经 per-stream presenter 表加 `view` 并 push 进 mux 的 FrameQueue → `WebSocketDownlinks.pump` 逐帧 `ServerRequest` → 浏览器 `AbstractApiClient.readSse`/WS pump → microtask 批处理 → UI 插件渲染。工具卡片的 pending/completed 渲染来自 `presentCall/presentResult`（§3.1），`tool/result` 事件携带 `meta` 供回放时复现卡片（`tool-calls.ts:286-288`）。

---

## 七、技术栈与工程实践（类型、测试、构建）

### 7.1 技术栈

- **语言/运行时**：TypeScript（devDeps `typescript ^6.0.3`，根 `package.json:183`）；Node `^22.19.0 || >=24.0.0`（`:8-10`）；`type: "module"`，pnpm `11.7.0`（`:6-7`）。
- **框架**：vendored **Cordis**（`vendor/`，`docs/cordis-primer.md:5`）+ `@deepseek-ai/schemastery`（类 Zod 的配置 schema，如 `tools/src/index.ts:8` 的 `z.object` Config）、Zod 用于 RPC 边界校验（`apiproxy/api/rpc.schema.ts`）。
- **自有 RPC 类型系统**：`packages/typert/`（generator/loader/protocol/registry）——TS 工程分析器 + 模型驱动的 Remote 类型生成，`api/gateway` 是它的 host 分发端（§6.4）。
- **LLM 适配器**：抽象 `LlmAdapter`（`packages/llm/llm/src/index.ts:180-233`），唯一必选方法 `stream(options): AsyncIterable<StreamChunk>`；生产适配器 `llm-deepseek`（chat-completions，`llm/src/types.ts` 的 `GenerateOptions` 是其供应商中立请求词汇）。`LlmRuntime.prepareCall`（`:779-814`）把 config 解析与 adapter 注册**绑定**成一个一次性句柄，HMR 不会把 A 适配器的能力结果配到 B 适配器。
- **其他**：`oxlint`（lint）、`vitest`（测试）、`tsdown`（打包）、`tsx`（直接跑 TS）、`lefthook`（git hooks）、`jscpd`（查重）、`knip`（未用依赖检查）、`mermaid`（文档图）。

### 7.2 类型工程

- **Host/Client 双聚合**（`docs/development.md` "TypeScript project layout"）：`tsconfig.host.json`（host 包 + examples + scripts）与 `tsconfig.client.json`（`packages/client/*` + `apps/web`）是两个独立 TS Program，因为**两侧都按相同 key 对 cordis `Context` 接口做声明合并**，合到一个 Program 会冲突；`api/remotes` 是唯一拆分的包（host/client 双 tsconfig）。
- **声明合并驱动一切**：事件词汇（`declare module '@deepseek-ai/cordis' { interface Events {...} }`，如 `tools/src/index.ts:137-209`、`agent/src/runtime-types.ts:146-291`）、服务 key（`interface Context { tools: ToolRuntime }`）、session 事件表（`declare module '@deepseek-ai/dsh-session/types' { interface SessionEventMap {...} }`，如 `goal/src/domain.ts:61-68`）、消息来源（`MessageSourceMap`）、内容块（`ContentBlockMap`）。
- **品牌类型/无损 JSON 纪律**：`SessionId`/`CallId` 等 branded types（`llm/src/brand.ts`）；所有持久数据必须过 `snapshotJsonValue` 无损 JSON 检查（`session/src/json.ts`），BigInt/函数/循环引用在 append 现场抛错（`session/src/index.ts:590-602`）。
- **生成目录即 CI 门**：`gen-cordis-catalog`、`gen-config-catalog`、`gen-tool-catalog`、`gen-persistence-catalog`、`gen-module-graph`、`gen-doc-graphs` 等脚本生成文档/矩阵，`verify-*` 检查过期（根 `package.json:109-130`）。

### 7.3 测试（`docs/testing.md`）

- 分层：单元（vitest，`pnpm test`）；**覆盖率门**（`test:coverage`，`packages/*/*/src` 逐文件 100% 行覆盖，`:10`）；**真 API e2e**（`test:e2e`，有 key 自跳过，`:11`）；**快照**（`test:snapshot`，keyless 回放，ACP 场景 diff JSON-RPC 与重持久化日志，`:12`）；**浏览器快照**（`test:web`，Chromium 对比 `apps/web/tests/snapshots/`，CI 只读 `DSH_SNAPSHOT=replay`，`:13`）。
- 原则：**"prefer the real implementation over a mock"**（`:21-23`，只 mock LLM adapter/网络/时钟）；**"verify the world, not the self-report"**（`:27-29`，e2e 断言从外部重跑命令/重读文件）；**"test the real entry path"**（`:31-35`，`bin` 跑构建产物 `lib/bin.js`，暴露 tsx 掩盖的竞态）；测试解析一律走 `tsconfig.base.json` 的 `paths` 到 `src`，绝不经包 exports 到构建产物（`:37-39`）。

### 7.4 构建

- `pnpm run build` = `tsx scripts/build.ts`；库构建 = `tsc -b tsconfig.host.json && tsdown --env.DSH_BUILD_FACE host` 再 `tsc -b tsconfig.client.json && tsdown --env.DSH_BUILD_FACE client`（`package.json:22-24`，`docs/development.md` 根构建节）；`pnpm dsh web` 用已构建产物直接起 Web UI（`README.md:37`）。
- 发布前门：`publint`、`knip`、`verify-runtime-closure`、`verify-optional-dependency-imports`、`verify-dsh-package-licenses` 等（`package.json:133` 的 `hygiene` 聚合）。

---

## 附：关键设计决策速查

1. **一切皆插件、无特权核心**（`README.md:5-7`；`docs/architecture.md:13`）：模型适配器、工具注册表、会话日志、agent loop 本身都是可替换插件。
2. **持久事实与实时协调分离**：可回放事实只进 `session/event`；实时控制/状态走 `agent/*`；"模型可见即已记录"由运行时不变式强制（`docs/architecture.md:94-96`）。
3. **agent 核心与传输解耦**：loop 只对 `ctx.agents`/`ctx.sessions` 编程；UI/RPC/SDK/ACP 都是旁观者（消费 `session/event`、调用 `agent.followup` 等）。
4. **per-agent 作用域**（`core/scope`）：`agent.ctx` 隔离注册；scope 链"注册向下继承、事件向上接纳"（`scope/src/index.ts:32-39`、`:170-185`）。
5. **事件源会话 + surface 投影**：历史是日志的派生投影（`deriveMessages` 增量缓存）；compaction = surface replace 事务（可回放、可恢复）。
6. **fail-closed 策略面**：沙箱默认 read-only、审批默认 deny、工具并发默认 exclusive、未知工具 UNKNOWN_TOOL、超时协作式、重试先落盘。
7. **传输层"上行 HTTP / 下行 WS" + rpcId 回显 + 基线回放重连**：浏览器刷新/断线后靠整快照 + 稳定 rpcId 收敛，无需增量同步。
8. **事件信封强制无损 JSON + 格式版本 0 + `ignorable` 逃生阀**：未知事件类型默认拒绝重建，显式标记才可跳过（`docs/persistence-catalog.md:67-76`）。
