# Agent 能力建设交付归档（取消传播/子代理/优化/Planner/Trace）

> 最后更新：2026-08-23（原归档日期：2026-08）
> 适用范围：`tauri/src-tauri`（Rust 后端）+ 前端模块（`css_js/modules/*.js`，原 `index.html` 内联已迁移；Tauri 主入口 `main.html`）
> 用途：为后续 Agent 相关操作提供设计依据、实现定位与已知取舍。
>
> **归档说明（重要）**：本文档为**机制归档**——各机制的描述为 **rig 时代版本**（rig 0.41），实现已随 **v3 自研内核重构**（commit `7278d50` 起，现 HEAD=`edab77e`）更新。原机制（取消/子代理/Planner/Trace/压缩/审批）的**行为契约全部保留**，承载它们的底层从 rig 换为 `core/loop` 自研内核（LoopAgent/LoopHook/LlmAdapter/事件溯源 Session）。凡实现位置与符号已变化处，本归档逐节标注「v3 现状」；已随 v3 移除的能力标注「已移除」。`cargo test --lib` 现为 **321 passed / 0 failed**。

---

## 0. 交付总览

| 批次 | 内容 | 核心文件 |
|---|---|---|
| 取消传播 + 子代理 | 流式请求真正可取消、隔离只读调研 | `commands/llm.rs`、`core/subagent/`、`core/agent/`、`core/agent/tools/mod.rs` |
| 优化修复批次 | 6 项 review 建议(除会话存储) | `tools/mod.rs`、`core/subagent/`、`services/llm.rs`、`.cargo/config.toml` |
| planner + trace | 规则路由+用户确认;tracing 基础设施+TraceBus 全链路+前端面板 | `core/agent/planner.rs`、`core/trace.rs`、`lib.rs`、`commands/plan.rs`、`commands/system.rs`、`core/skill/metrics.rs` |
| CRT 根治 + 测试修复 | tokenizers 裁剪 esaxx_fast;测试真实运行修复 2 bug | `Cargo.toml`、`core/context/mod.rs`、`core/approval/mod.rs` |

> v3 现状：上述交付的机制在 v3 内核上继续生效（详见各节）；「会话存储」一项已由事件溯源（`session_events` 表 + `Session::derive_history`）超越原「跳过」取舍。

---

## 1. 取消传播

### 设计（历史机制，rig 时代）
流式 LLM 请求的取消此前只在"chunk 间隙"轮询 `is_cancelled()`,服务端暂停流时取消永不生效。改造为 `tokio::select!` 同时等待流事件与取消信号,取消分支 return 时 **drop stream future → rig 惰性流 → 尽力断开底层 reqwest 连接**。

### 实现要点（历史实现，`commands/llm.rs`）
- `next_or_cancel<T>(stream, cancel)`：封装 `select! { biased; _ = cancel.cancelled() => Err(()), item = stream.next() => Ok(item) }`；`biased` 保证取消与事件同时就绪时取消优先。**【已随 v3 移除】**——v3 取消检查点内置于 `LoopAgent::turn`。
- `agent_query` 与 `kb_llm_query` 两处流式主循环：`while let` → `loop { match next_or_cancel(...) }`，取消分支保留原清理(已生成内容发 `rag:done`/`llm:done`、工具事件补发、技能指标记 cancelled、unregister、return)。
- **压缩后快速取消检查**：`prepare_history` 之后、`stream_chat` 之前 `if cancel.is_cancelled()` → 不发起 HTTP 请求。

### v3 现状（`core/loop` 自研内核）
- 取消统一为 **`tokio::select!` + `CancellationToken` 检查点模式**：`LoopAgent::turn`（`core/loop/loop.rs`）在请求循环每处检查点 `select!` 优先响应取消（loop.rs:258），工具调度器（`core/loop/tool_calls.rs`）同样感知 cancel token，未启动调用产出合成错误结果（保回放），已启动 drain 到 quiescence。
- 子代理取消：`SubagentRunner::run` 内偏置 select! 优先响应父链取消（`KbSearchConfig.cancel` 透传），取消失败标记 + break；父链取消后子代理不会成为孤儿任务。
- 日志文案更新：取消日志现为 `对话取消，直接结束 request_id=...`（不再使用旧文案「立即断开请求」）。

### 行为契约（不变）
- 取消即断连(尽力而为:连接可能被连接池复用,但不再依赖下一个 chunk)。
- 取消保留已生成部分内容（`TurnOutcome::Cancelled`/`Failed` 均携带 content）。

---

## 2. 子代理(隔离只读深度调研)

### 架构
```
主对话 agent_query
  └─ deep_research 工具(模型自主触发)
       └─ SubagentRunner::run:独立 request_id(sub-{uuid})/空 ActiveSkillState/独立 search_sink
            ├─ 只读工具子集(白名单 6 个)
            ├─ 有界摘要 → 返回父链(父链上下文不被污染)
            └─ 全量输出 → AppState.subagent_results(LruResultStore) → read_subagent_result 分页读取
```

### 文件与职责
- `core/subagent/mod.rs`:
  - `read_only_tool_set()`:kb_search/code_lookup/read/grep/ls/git_status;**明确不含** edit/delete(写)、activate/deactivate_skill(注入面)、pomodoro(交互)、deep_research/read_subagent_result(防递归)。
  - **`SUBAGENT_MAX_TURNS` 常量已更名为 `SubagentSpec.max_turns` 字段**（v3：`core/subagent/mod.rs:73`，轮次上限由调用方经 `SubagentSpec` 传入，工具侧默认 12、上限 30）；`SUBAGENT_SUMMARY_CHARS=4000` 保留（limits.rs）。
  - `SubagentRunner::run(adapter, search_config, base_rules, spec)`（v3 签名）：构造**自研 LoopAgent**（`LoopConfig::new(spec.max_turns, base_rules)` + 白名单过滤注册表 + 独立 request_id），流式收集；`select!` 监听父链取消；`ToolBusGuard`(RAII) 兜底清理工具总线；结束清 trace 总线。**`build_rag_agent`（rig 版构造）已移除**。
  - `LruResultStore`:AtomicU64 访问序 LRU(insert 满淘汰最旧、get 刷新访问序、已有 id 更新不淘汰)，**替代"满则清空"**——保留。
- `core/agent/loop_tools.rs`（原 `core/agent/tools/mod.rs` 的工具构造已迁移/拆分）：`deep_research`(schema: task 必填、max_turns 1-30) → 返回 `{summary, subagent_id, max_turns, failed}`；`read_subagent_result`(subagent_id 必填、offset、max_chars 1-60000 clamp) 按字符分页；`spawn_subagent`（mode=read_only|write，写型强制审批门）、`parallel_research`（`JoinSet` 并行派发 2-N 个只读子代理）。
- `core/agent/loop_tools.rs::filter_registry`：按白名单过滤注册表（子代理只读/写型工具集；白名单外工具不注册 → 模型不可见不可调）。
- `lib.rs`:`AppState.subagent_results: Arc<LruResultStore>`(组装 `new(16)`)；`llm_client_for` 公共工厂（配置指纹缓存复用 reqwest 连接池，commands 与 tools 共用）。

### 安全边界(已 review 确认)
- 只读白名单在注册表层过滤,`approval_gate=None` 无审批绕过面;技能激活工具被白名单排除(堵提示注入);递归工具被白名单排除。
- 写型子代理（mode=write）：白名单含 edit/delete/write/multi_edit/git_*，**强制挂载审批门**（v3：ApprovalHook 在 loop 层裁决，审批事件冒泡父链前端）；门缺失回退只读。
- 子代理结果经 LRU 有界(16 条),`read_subagent_result` 是唯一读取入口。

### 已知取舍
- 子代理不注册 TaskRegistry(无独立取消命令),取消依赖"父链 drop 传播 + `KbSearchConfig.cancel` 透传 select!"——保留（v3 同）。
- subagent 的 trace 事件写入 `sub-*` 桶、不转发前端(设计取舍,容量 64 兜底)——保留。

---

## 3. 优化修复批次(6 项,除"结果入会话存储")

| # | 优化 | 实现 |
|---|---|---|
| 1 | `read_subagent_result.max_chars` 未 clamp | `.map(...).unwrap_or(8192).clamp(1, 60_000)` |
| 2 | `ToolCallBus` 无容量治理 | `MAX_TRACKED_REQUESTS=64`,`record_call/record_result` 满则清空 |
| 3 | `subagent_results` 满 16 清空 | 改 `LruResultStore`(LRU 淘汰,见上) |
| 4 | `.cargo/config.toml` 硬编码 lld-link 绝对路径 | 注释化 + 文档说明(默认回退 MSVC link.exe,加速选项保留),并修 jobs 注释表述 |
| 5 | LLM 请求无超时 | `LLMClient::new` 注入 300s 超时 client（v3：超时策略已并入 `LlmAdapter` + `retry_loop`，rig reqwest 双实例问题随去 rig 消失） |
| 7 | `create_tool_registry` 缩进异常 | 手动对齐(不跑全局 cargo fmt,避免无关大 diff) |

> 注:编号 6(子代理结果入会话存储)为用户明确跳过的设计取舍项；v3 事件溯源（session_events）后，子代理结果仍走 LRU，未并入会话事件流（取舍保持）。

---

## 4. Planner(规则路由 + 用户确认)

### 触发规则(`core/agent/planner.rs::should_plan`,纯规则零 LLM 开销)
- 长度 ≥ `LONG_QUERY_CHARS=120` → 必规划;
- ≤ `SHORT_QUERY_CHARS=40` → 仅含任务动词(`PLAN_VERBS`:重构/设计/分析/总结/迁移/调研/实现/搭建/规划/计划/优化/评估/对比/架构/方案/步骤/分步/详细说明/研究/改进)才规划;
- 中段:任务动词 或 多意图连接词(`并且/同时/以及/然后/还要/先/再`)。
- v3 增补（P1-10）：移除「先/再」误报源，疑问句/轻量查看类抑制；`Plan` 扩展 full plan 字段 `touchpoints[]`/`non_goals[]`/`risks[]`/`rollback[]`（字段最小化输出，services/llm.rs generate_plan_json 结构化约束）。

### 链路(agent_query Stage 0.5,技能解析后、检索前)
```
should_plan 命中 → rag:status "planning" → generate_plan_json(非流式 completion,不占 DEFAULT_MAX_TURNS)
  → parse_plan(宽松 JSON,容忍 ```json 围栏/杂质;失败 fail-open 降级不规划)
  → plan:request 事件(60s)→ 用户批准 → plan 注入 preamble(每轮可见)
  → 拒绝/取消/超时 → rag:done(content 置空防污染会话,fail-closed)
```
- `commands/plan.rs::plan_respond`:从 `AppState.plan_pending` 挂起表取 oneshot sender 回传。
- 等待确认期间 `select!` 监听 cancel(点"停止"立即中止,不必等 60s)。
- 前端：`css_js/modules/agent.js` 等模块监听 `plan:request` 计划卡片(目标/步骤/验收 + touchpoints/risks/non_goals/rollback)+ `invoke('plan_respond', {planId, approved, reason})`（原 index.html 内联已迁移）。

### 单测(`core/agent/planner.rs`)
`should_plan` 触发/不触发边界、`parse_plan` 围栏 JSON/无效输入、`to_preamble_text` 结构、full plan 字段清洗。

---

## 5. Trace(可观测全链路)

### tracing 基础设施(`lib.rs::init_logging` 重构)——保留（v3 同）
- `simplelog` → `tracing-subscriber`(文件 + 终端双输出;文件失败降级 sink)。
- `tracing_log::LogTracer` 桥接现有 `log::` 宏（rig 内部 span 此前 100% 丢失,现进入同一输出）→ **v3 后 rig 已移除，`log::` 宏与自研代码 tracing 事件经同一桥接**。
- `Targets` 过滤:5 个高频 target OFF(lance/tantivy/datafusion/sqlparser/tao::platform_imp)+ default dev=DEBUG/release=WARN。
- **`reload::Layer` 热重载**:`LOG_LEVEL_HANDLE`(OnceLock<Handle<Targets, Registry>>)+ `log_filter_targets(level)` 单一构造源;`set_log_level`(commands/system.rs)双侧同步(`log::set_max_level` + `handle.reload`)。
- 日志路径:Windows `%APPDATA%/com.mdgo/logs/`。

### TraceBus(`core/trace.rs`)——保留
- `TraceEvent{seq, stage, status, duration_ms, detail, ts_ms}`;`TraceBus` 按 request_id 分桶(容量 64/drain 消费式/clear)。
- `stage_start/stage_end` 便捷辅助。

### 埋点覆盖(全部 LLM 链路)——保留
- `agent_query`:planning/expanding/searching/aggregating/generating 五阶段 start/end,状态 ok/error/cancelled/denied + 耗时 + detail(查询数/命中数/字符数/token)。
- `kb_llm_query`:generating(start/cancelled/error/ok)。
- `subagent`:subagent(start/end)。
- 转发:`emit_pending_trace_events` drain 后 emit `trace:event`(前端按 request_id 过滤)。

### 关联修复
- **`LlmTraceHook` 已移除**（rig 时代独立 struct）：v3 中每轮请求的日志由 `LoopHook::pre_request` 与 loop 内部请求/响应日志承担（hooks.rs 注释保留映射说明），`[llm_trace]` 日志标签不复存在。
- **metrics.rs bug 修复**（保留）：`record_execution_batch` 含 `request_id` 参数,INSERT 占位符 9 列对齐,4 处调用点(`spawn_blocking` 内 clone)。

### 前端面板
- `window.__chatTraceMap` 按 request_id 收集 `trace:event`（现位于 `css_js/modules/agent.js`/`agent_global.js`，原 index.html 内联已迁移）；
- `renderTracePanel(events)` 可折叠"阶段耗时"面板(状态图标/耗时/详情),`rag:done` 时渲染到消息卡片并在完成后清理。

---

## 6. CRT 根治与测试真实运行(附)

### 根因链(已实证,历史记录)
```
mdgo Cargo.toml: tokenizers = "0.22"(默认 features)
  └─ default 含 "esaxx_fast" = ["esaxx-rs/cpp"]
       └─ esaxx-rs 0.1.10 build.rs:.static_crt(true) 硬编码编译静态 CRT C++
            └─ 与动态 CRT 的 ort_sys(ONNX Runtime)链接 → LNK2038(772 处)
```
- esaxx 由 **tokenizers** 引入(非 tantivy);项目实际用不到 esaxx(BM25 用 jieba;embedding/rerank 用 `Tokenizer::from_bytes` 加载 BERT 类 tokenizer.json,不走 Unigram 路径)。
- `onig_sys` 无 `static_crt` 硬编码,受 `CFLAGS=-MD` 控制 → 无冲突。

### 根治(方案:关闭 tokenizers esaxx_fast feature)——现仍生效
```toml
# Cargo.toml（现状）
tokenizers = { version = "0.22.2", default-features = false, features = ["onig"] }
```
- 保留 onig(模型 pretokenizer 可能用 Regex);**不要写 `rayon` feature**(tokenizers 0.22.2 无该 feature,rayon 是非可选依赖);progressbar 可关(项目不用训练 CLI)。
- 效果:esaxx-rs 仍在依赖树,但 cpp feature 关闭 → build.rs 空实现 → 无 C 编译 → 无静态 CRT → **LNK2038 消除**;`cargo test --lib` 首次真实运行。

### 测试运行暴露并修复的 2 个 bug（保留）
1. **`SummarizeThenWindowCompressor` 摘要被滑窗砍掉**(core/context/mod.rs):摘要作为最旧消息,滑窗超预算优先保 recent → 摘要丢失,压缩形同虚设。修复:**摘要恒保留**(若摘要本身超预算则降级纯滑窗),滑窗只作用于 recent(`budget - summary.len()` 预算分配;recent 滑窗为纯内存操作用 `CancellationToken::new()` 规避 move)。
2. **审批测试 `MockTransport::clone` 计数不共享**(core/approval/mod.rs tests):`AtomicUsize::new(calls.load())` 拷贝而非共享,断言永远失败。修复:`Arc<AtomicUsize>` 共享。

### 验证
`cargo test --lib`（现）:**321 passed / 0 failed**（2026-08-23 实测；原 31 → 51 → 58 → 67 → 321，随能力批次与 v3 内核单测增长）。

---

## 7. 关键设计决策与模式(后续复用)

1. **确认通道模式(approval/plan/question 通用样板)**:`AppState.pending` 挂起表(HashMap<id, oneshot::Sender>)+ 事件 `xxx:request` → 前端弹窗/卡片 → `invoke('xxx_respond')` 命令回传 → `tokio::time::timeout` 超时 fail-closed;future 被父链取消 drop 时用 **RAII guard** 兜底清理挂起条目(`RemovePendingOnDrop` / `ToolBusGuard` 同款)。v3 增补：`ask_user_question` 工具 + `question:request` 事件 + `question_respond` IPC（commands/question.rs）。
2. **注入方式**:planner 产物注入 preamble(每轮可见,约束最强);取消/拒绝的 `rag:done.content` 置空,依赖前端 `if (fullContent)` 跳过 push 与落库,避免污染会话历史。
3. **取消统一模式**:rig 时代为 `next_or_cancel`(biased select!)+ 压缩后快速检查 + `KbSearchConfig.cancel` 透传;v3 为 `core/loop` 的 `tokio::select!` 检查点 + `CancellationToken` 透传（loop.rs / tool_calls.rs / SubagentRunner）。
4. **有界存储**:内存结果一律容量治理(`ToolCallBus`/`TraceBus` 64 桶清空、`LruResultStore` 16 条 LRU、审批缓存 256 条)。
5. **依赖陷阱**:rig 时代的 `rig-core` 用 reqwest 0.13、mdgo 直接依赖 0.12(两个 crate 实例)问题——**随 v3 移除 rig 依赖已消失**；Windows native 依赖统一 `/MD`(`.cargo/config.toml` 的 `CFLAGS/CXXFLAGS=-MD` + `-crt-static`),但 build.rs 硬编码 `.static_crt(true)` 的 crate(如 esaxx-rs)不受 env 控制,优先通过 feature 裁剪或 patch 处理（tokenizers esaxx_fast 方案仍适用）。

---

## 8. 已知遗留(非阻塞,后续可做)

- 五个"阶段间隙取消"分支(expanding 后/searching 后/生成前)为既有行为,无 rag:done/cancelled 埋点(前端本地收敛,未统一补发)。
- 前端 `__chatTraceMap` 在无 rag:done 的中断请求下残留(量小)。
- `simplelog` 依赖已不再使用(Cargo.toml 保留待清理)——v3 后依然成立。
- `.cargo/config.toml` 的 lld-link 加速已注释化;如需加速改本机路径。
- 若未来加载 Unigram/sentencepiece 模型,esaxx_fast 关闭后走纯 Rust 回退(稍慢,功能正常)。
- v3 新增遗留:Anthropic Agent 模式暂为纯对话语义（工具协议面映射后续扩展）；`core/eval` 框架无真实 LLM 执行器（仅单测覆盖断言与报告，待 CLI/headless 接入）。
