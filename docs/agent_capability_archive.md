# Agent 能力建设交付归档(取消传播/子代理/优化/Planner/Trace)

> 归档日期:2026-08 · 适用范围:`tauri/src-tauri`(Rust 后端)+ 根 `index.html`(前端内联)
> 用途:为后续 Agent 相关操作提供设计依据、实现定位与已知取舍。
> 状态:全部交付并通过 `cargo test --lib`(31/31 通过)+ 多轮 review。

---

## 0. 交付总览

| 批次 | 内容 | 核心文件 |
|---|---|---|
| 取消传播 + 子代理 | 流式请求真正可取消、隔离只读调研 | `commands/llm.rs`、`core/subagent/`、`core/agent/`、`core/agent/tools/mod.rs` |
| 优化修复批次 | 6 项 review 建议(除会话存储) | `tools/mod.rs`、`core/subagent/`、`services/llm.rs`、`.cargo/config.toml` |
| planner + trace | 规则路由+用户确认;tracing 基础设施+TraceBus 全链路+前端面板 | `core/agent/planner.rs`、`core/trace.rs`、`lib.rs`、`commands/plan.rs`、`commands/system.rs`、`core/skill/metrics.rs` |
| CRT 根治 + 测试修复 | tokenizers 裁剪 esaxx_fast;测试真实运行修复 2 bug | `Cargo.toml`、`core/context/mod.rs`、`core/approval/mod.rs` |

---

## 1. 取消传播

### 设计
流式 LLM 请求的取消此前只在"chunk 间隙"轮询 `is_cancelled()`,服务端暂停流时取消永不生效。改造为 `tokio::select!` 同时等待流事件与取消信号,取消分支 return 时 **drop stream future → rig 惰性流 → 尽力断开底层 reqwest 连接**。

### 实现要点(`commands/llm.rs`)
- `next_or_cancel<T>(stream, cancel) -> Result<Option<T>, ()>`:封装 `select! { biased; _ = cancel.cancelled() => Err(()), item = stream.next() => Ok(item) }`;`biased` 保证取消与事件同时就绪时取消优先。
- `agent_query` 与 `kb_llm_query` 两处流式主循环:`while let` → `loop { match next_or_cancel(...) }`,取消分支保留原清理(已生成内容发 `rag:done`/`llm:done`、工具事件补发、技能指标记 cancelled、unregister、return)。
- **压缩后快速取消检查**:`prepare_history` 之后、`stream_chat` 之前 `if cancel.is_cancelled()` → 不发起 HTTP 请求。
- 取消分支 debug 日志(记录取消时机与已消费字符数)。

### 行为契约
- 取消即断连(尽力而为:连接可能被连接池复用,但不再依赖下一个 chunk)。
- 依赖确认:rig 0.41 的 agent 工具在**流式 poll 栈内顺序执行**(不 spawn 独立 task,唯一 spawn 在测试代码),父链取消 drop stream 会**级联 drop 正在 await 的工具闭包**——这是子代理取消的基础。

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
  - `read_only_tool_set()`:kb_search/code_lookup/read/grep/list_files/git_status;**明确不含** edit/delete(写)、activate/deactivate_skill(注入面)、pomodoro(交互)、deep_research/read_subagent_result(防递归)。
  - `SUBAGENT_MAX_TURNS=12`、`SUBAGENT_SUMMARY_CHARS=4000`。
  - `SubagentRunner::run(model, search_config, skill_registry, base_rules, spec)`:构造只读 Agent(白名单+`approval_gate=None`+max_turns+`narrow_tools=false`),流式收集;`select!` 监听父链取消(`KbSearchConfig.cancel` 透传,取消失败标记+break);`ToolBusGuard`(RAII)兜底清理工具总线;结束清 trace 总线。
  - `LruResultStore`:AtomicU64 访问序 LRU(insert 满淘汰最旧、get 刷新访问序、已有 id 更新不淘汰),**替代"满则清空"**。
- `core/agent/tools/mod.rs`:`deep_research`(schema: task 必填、max_turns 1-30)→ 返回 `{summary, subagent_id, max_turns, failed}` 并提示可分页;`read_subagent_result`(schema: subagent_id 必填、offset、max_chars 1-60000 clamp)按字符 skip/take 分页,offset 越界明确提示。
- `core/agent/mod.rs` 参数化(开闭原则):
  - `create_tool_registry(only: Option<&HashSet<String>>)` 白名单过滤(默认 None=全量)。
  - `build_rag_agent(...)` 新增 `max_turns: usize`、`tool_whitelist: Option<&HashSet<String>>`、`narrow_tools: bool`。
  - `BASE_TOOLS` 加入 `deep_research`/`read_subagent_result`(通用能力,主对话始终可见)。
  - `SkillInstructionHook` 加 `narrow_tools`(false 时不设 active_tools,模型可见全部已注册工具);`SkillGateHook` 加 `allow_all`(子代理放行白名单内工具)。
- `lib.rs`:`AppState.subagent_results: Arc<LruResultStore>`(组装 `new(16)`);`llm_client_for` 公共工厂(配置指纹缓存复用 reqwest 连接池,commands 与 tools 共用)。

### 安全边界(已 review 确认)
- 只读白名单在注册表层过滤,`approval_gate=None` 无审批绕过面;技能激活工具被白名单排除(堵提示注入);递归工具被白名单排除。
- 子代理结果经 LRU 有界(16 条),`read_subagent_result` 是唯一读取入口。

### 已知取舍
- 子代理不注册 TaskRegistry(无独立取消命令),取消依赖"父链 drop 传播 + `KbSearchConfig.cancel` 透传 select!"。
- subagent 的 trace 事件写入 `sub-*` 桶、不转发前端(设计取舍,容量 64 兜底)。

---

## 3. 优化修复批次(6 项,除"结果入会话存储")

| # | 优化 | 实现 |
|---|---|---|
| 1 | `read_subagent_result.max_chars` 未 clamp | `.map(...).unwrap_or(8192).clamp(1, 60_000)` |
| 2 | `ToolCallBus` 无容量治理 | `MAX_TRACKED_REQUESTS=64`,`record_call/record_result` 满则清空 |
| 3 | `subagent_results` 满 16 清空 | 改 `LruResultStore`(LRU 淘汰,见上) |
| 4 | `.cargo/config.toml` 硬编码 lld-link 绝对路径 | 注释化 + 文档说明(默认回退 MSVC link.exe,加速选项保留),并修 jobs 注释表述 |
| 5 | LLM 请求无超时 | `LLMClient::new` 注入 300s 超时 client,用 `rig_core::http_client::ReqwestClient`(**reqwest 0.13**;mdgo 直接依赖的 0.12 是不同 crate 实例,`HttpClientExt` 只对 0.13 实现,直接注入 0.12 类型不满足约束) |
| 7 | `create_tool_registry` 缩进异常 | 手动对齐(不跑全局 cargo fmt,避免无关大 diff) |

> 注:编号 6(子代理结果入会话存储)为用户明确跳过的设计取舍项。

---

## 4. Planner(规则路由 + 用户确认)

### 触发规则(`core/agent/planner.rs::should_plan`,纯规则零 LLM 开销)
- 长度 ≥ `LONG_QUERY_CHARS=120` → 必规划;
- ≤ `SHORT_QUERY_CHARS=40` → 仅含任务动词(`PLAN_VERBS`:重构/设计/分析/总结/迁移/调研/实现/搭建/规划/计划/优化/评估/对比/架构/方案/步骤/分步/详细说明/研究/改进)才规划;
- 中段:任务动词 或 多意图连接词(`并且/同时/以及/然后/还要/先/再`)。

### 链路(agent_query Stage 0.5,技能解析后、检索前)
```
should_plan 命中 → rag:status "planning" → generate_plan_json(非流式 completion,不占 DEFAULT_MAX_TURNS)
  → parse_plan(宽松 JSON,容忍 ```json 围栏/杂质;失败 fail-open 降级不规划)
  → plan:request 事件(60s)→ 用户批准 → plan 注入 preamble(每轮可见)
  → 拒绝/取消/超时 → rag:done(content 置空防污染会话,fail-closed)
```
- `commands/plan.rs::plan_respond`:从 `AppState.plan_pending` 挂起表取 oneshot sender 回传。
- 等待确认期间 `select!` 监听 cancel(点"停止"立即中止,不必等 60s)。
- 前端 `index.html`:`listen('plan:request')` 计划卡片(目标/步骤/验收列表)+ `invoke('plan_respond', {planId, approved, reason})`。

### 单测(`core/agent/planner.rs`)
`should_plan` 触发/不触发边界、`parse_plan` 围栏 JSON/无效输入、`to_preamble_text` 结构。

---

## 5. Trace(可观测全链路)

### tracing 基础设施(`lib.rs::init_logging` 重构)
- `simplelog` → `tracing-subscriber`(文件 + 终端双输出;文件失败降级 sink)。
- `tracing_log::LogTracer` 桥接现有 `log::` 宏(rig 内部 span 此前 100% 丢失,现进入同一输出)。
- `Targets` 过滤:5 个高频 target OFF(lance/tantivy/datafusion/sqlparser/tao::platform_imp)+ default dev=DEBUG/release=WARN。
- **`reload::Layer` 热重载**:`LOG_LEVEL_HANDLE`(OnceLock<Handle<Targets, Registry>>)+ `log_filter_targets(level)` 单一构造源;`set_log_level`(commands/system.rs)双侧同步(`log::set_max_level` + `handle.reload`)。
- 日志路径:Windows `%APPDATA%/com.mdgo/logs/`(实现与注释已对齐)。

### TraceBus(`core/trace.rs`)
- `TraceEvent{seq, stage, status, duration_ms, detail, ts_ms}`;`TraceBus` 按 request_id 分桶(容量 64/drain 消费式/clear)。
- `stage_start/stage_end` 便捷辅助。

### 埋点覆盖(全部 LLM 链路)
- `agent_query`:planning/expanding/searching/aggregating/generating 五阶段 start/end,状态 ok/error/cancelled/denied + 耗时 + detail(查询数/命中数/字符数/token)。
- `kb_llm_query`:generating(start/cancelled/error/ok)。
- `subagent`:subagent(start/end)。
- 转发:`emit_pending_trace_events` drain 后 emit `trace:event`(前端按 request_id 过滤)。

### 关联修复
- `LlmTraceHook` 加 `request_id` 字段(`build_rag_agent` 传 Some,`build_chat_agent` 传 None)。
- **metrics.rs bug 修复**:`record_execution_batch` 新增 `request_id` 参数,INSERT 占位符从硬编码 `''` 改为 `?1`(9 列对齐),4 处调用点(`spawn_blocking` 内 clone)。

### 前端面板(`index.html`)
- `window.__chatTraceMap` 按 request_id 收集 `trace:event`;
- `renderTracePanel(events)` 可折叠"阶段耗时"面板(状态图标/耗时/详情),`rag:done` 时渲染到消息卡片并在完成后清理。

---

## 6. CRT 根治与测试真实运行(附)

### 根因链(已实证)
```
mdgo Cargo.toml: tokenizers = "0.22"(默认 features)
  └─ default 含 "esaxx_fast" = ["esaxx-rs/cpp"]
       └─ esaxx-rs 0.1.10 build.rs:.static_crt(true) 硬编码编译静态 CRT C++
            └─ 与动态 CRT 的 ort_sys(ONNX Runtime)链接 → LNK2038(772 处)
```
- esaxx 由 **tokenizers** 引入(非 tantivy);项目实际用不到 esaxx(BM25 用 jieba;embedding/rerank 用 `Tokenizer::from_bytes` 加载 BERT 类 tokenizer.json,不走 Unigram 路径)。
- `onig_sys` 无 `static_crt` 硬编码,受 `CFLAGS=-MD` 控制 → 无冲突。

### 根治(方案:关闭 tokenizers esaxx_fast feature)
```toml
# Cargo.toml
tokenizers = { version = "0.22.2", default-features = false, features = ["onig"] }
```
- 保留 onig(模型 pretokenizer 可能用 Regex);**不要写 `rayon` feature**(tokenizers 0.22.2 无该 feature,rayon 是非可选依赖);progressbar 可关(项目不用训练 CLI)。
- 效果:esaxx-rs 仍在依赖树,但 cpp feature 关闭 → build.rs 空实现 → 无 C 编译 → 无静态 CRT → **LNK2038 消除**;`cargo test --lib` 首次真实运行。

### 测试运行暴露并修复的 2 个 bug
1. **`SummarizeThenWindowCompressor` 摘要被滑窗砍掉**(core/context/mod.rs):摘要作为最旧消息,滑窗超预算优先保 recent → 摘要丢失,压缩形同虚设。修复:**摘要恒保留**(若摘要本身超预算则降级纯滑窗),滑窗只作用于 recent(`budget - summary.len()` 预算分配;recent 滑窗为纯内存操作用 `CancellationToken::new()` 规避 move)。
2. **审批测试 `MockTransport::clone` 计数不共享**(core/approval/mod.rs tests):`AtomicUsize::new(calls.load())` 拷贝而非共享,断言永远失败。修复:`Arc<AtomicUsize>` 共享。

### 验证
`cargo test --lib`:**31 passed / 0 failed**(planner 4、subagent LRU/白名单/截断、context 压缩 4、approval 门控 4、grep glob 系列)。

---

## 7. 关键设计决策与模式(后续复用)

1. **确认通道模式(approval/plan 通用样板)**:`AppState.pending` 挂起表(HashMap<id, oneshot::Sender>)+ 事件 `xxx:request` → 前端弹窗/卡片 → `invoke('xxx_respond')` 命令回传 → `tokio::time::timeout` 超时 fail-closed;future 被父链取消 drop 时用 **RAII guard** 兜底清理挂起条目(`RemovePendingOnDrop` / `ToolBusGuard` 同款)。
2. **注入方式**:planner 产物注入 preamble(每轮可见,约束最强);取消/拒绝的 `rag:done.content` 置空,依赖前端 `if (fullContent)` 跳过 push 与落库,避免污染会话历史。
3. **取消统一模式**:`next_or_cancel`(biased select!)+ 压缩后快速检查 + `KbSearchConfig.cancel` 透传长耗时工具;依赖"rig 工具在 poll 栈内顺序执行、drop 级联中止"。
4. **有界存储**:内存结果一律容量治理(`ToolCallBus`/`TraceBus` 64 桶清空、`LruResultStore` 16 条 LRU、审批缓存 256 条)。
5. **依赖陷阱**:`rig-core` 用 reqwest 0.13、mdgo 直接依赖 0.12(两个 crate 实例),涉及 rig 的 http 注入必须用 `rig_core::http_client::ReqwestClient`;Windows native 依赖统一 `/MD`(`.cargo/config.toml` 的 `CFLAGS/CXXFLAGS=-MD` + `-crt-static`),但 build.rs 硬编码 `.static_crt(true)` 的 crate(如 esaxx-rs)不受 env 控制,优先通过 feature 裁剪或 patch 处理。

---

## 8. 已知遗留(非阻塞,后续可做)

- 五个"阶段间隙取消"分支(expanding 后/searching 后/生成前)为既有行为,无 rag:done/cancelled 埋点(前端本地收敛,未统一补发)。
- 前端 `__chatTraceMap` 在无 rag:done 的中断请求下残留(量小)。
- `simplelog` 依赖已不再使用(Cargo.toml 保留待清理)。
- `.cargo/config.toml` 的 lld-link 加速已注释化;如需加速改本机路径。
- 若未来加载 Unigram/sentencepiece 模型,esaxx_fast 关闭后走纯 Rust 回退(稍慢,功能正常)。