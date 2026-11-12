---
title: 可观测性与评测：Trace、质量指标与检索基准
date: 2026-11-10
tags: [Agent, Rust, 可观测性, 评测, 指标]
summary: 从日志分级、TraceBus 阶段事件到质量计数、技能指标与 Recall/MRR/nDCG 基准，梳理 mdgo Agent 的可观测体系及其口径陷阱。
---

# 可观测性与评测：Trace、质量指标与检索基准

> Agent 的失败往往不是"报错"，而是"看起来正常但结果不对"：检索没命中、工具没执行、
> 模型声称做了但没做。这类问题只能靠埋点和指标发现。本文讲 mdgo 的四层可观测性
> （日志、Trace、计数、指标库）与两套评测（场景断言、检索基准），以及几个我在读代码时
> 发现的"注释与实现不一致"的口径陷阱。

## 1. 四层可观测性

| 层次 | 实现 | 粒度 | 生命周期 |
|---|---|---|---|
| 日志 | `tracing` + `log` 桥接 | 行级 | 文件 + 终端 |
| Trace | `core/trace.rs::TraceBus` | 请求 × 阶段 | 内存，按 request_id 分桶 |
| 质量计数 | `AgentQualityCounters` | 进程级累计 | 进程生命周期 |
| 业务指标 | `core/skill/metrics.rs` + SQLite | 技能 × 执行 | 落库，重启不丢 |

四层的分工很明确：日志回答"发生了什么"，Trace 回答"慢在哪一步"，计数回答"整体健康度"，
指标库回答"哪个技能在什么条件下更容易失败"。

## 2. 日志：为什么用 tracing 包 log

`init_logging`（`lib.rs:802`）做了三件事：

1. `tracing_log::LogTracer::init()` 把现有的 `log::info!` 宏桥接进 tracing subscriber，
   这样几十个模块里既有的 `log::` 调用不需要改；
2. 构建两个 `fmt` layer：终端（ANSI、带线程名/线程 ID）和文件（无 ANSI、同样带线程信息），
   文件创建失败时降级为 `std::io::sink()`，只保留终端输出；
3. 用 `tracing_subscriber::reload::Layer` 包住过滤器，把句柄存进 `LOG_LEVEL_HANDLE`，
   供 `set_log_level` 命令热切换级别。

过滤策略在 `log_filter_targets`（`lib.rs:774`）：

```rust
Targets::new()
    .with_target("mdgo_lib", level)                       // 自己的代码按 level
    .with_default(tracing::level_filters::LevelFilter::WARN) // 第三方统一 WARN
```

默认级别 dev=DEBUG、release=WARN（`lib.rs:810-814`），同时 `log::set_max_level` 保持同步，
否则 `log::` 宏侧仍会在编译期被裁掉。

> **口径陷阱**：函数注释写着"高频噪音 target（lance/tantivy/datafusion/sqlparser/tao/ort）
> 直接 OFF"，但实现里只有 `mdgo_lib` 与 `with_default(WARN)` 两条规则，并没有逐 target
> 的 OFF 列表。第三方库的 DEBUG/INFO 确实被 WARN 挡住，但 WARN 级噪音仍会进来。要真正
> 屏蔽需要显式 `.with_target("lance", LevelFilter::OFF)`。

## 3. TraceBus：按请求分桶的阶段事件

`TraceEvent`（`core/trace.rs:19-32`）的字段就是前端阶段面板需要的全部信息：

```rust
pub struct TraceEvent {
    pub seq: u64,          // 全局单调递增
    pub stage: String,     // planning / expanding / searching / aggregating / generating
    pub status: String,    // start / ok / error / cancelled / denied
    pub duration_ms: u64,  // start 事件为 0
    pub detail: String,    // 命中数、token、拒绝原因…
    pub ts_ms: u64,        // Unix 毫秒
}
```

`TraceBus` 的结构是 `Mutex<HashMap<String, Vec<TraceEvent>>>` + 全局 `AtomicU64` 序号。
写入走 `record`，读取走消费式 `drain`：取出该请求的全部事件并清空桶
（`core/trace.rs:83-94`）。命令层的 `emit_pending_trace_events`
（`commands/llm.rs:413-433`）负责：

1. `drain` 出事件；
2. 逐条写入 `AgentTaskStore` 的 `trace_events` 快照（切回页面时恢复阶段面板）；
3. 一次性 emit `trace:event`，载荷是 `{request_id, events:[...]}`。

### 3.1 一次 RAG 请求的 stage 与耗时口径

| stage | 何时 start | 何时 end | detail |
|---|---|---|---|
| `planning` | 规则判定需要规划后 | 批准/拒绝/取消 | `query_len`、批准原因 |
| `expanding` | 开始查询扩展 | 扩展完成（含超时） | `expanded=N` |
| `searching` | 开始多查询召回 | 精排+收尾完成 | `hits=N` |
| `aggregating` | 文档级聚合开始 | 上下文构建完成 | `docs=N chars=N` |
| `generating` | 进入 LoopAgent 前 | turn 结束 | `docs=N`、错误/取消原因 |

耗时是"阶段内墙钟时间"，包含该阶段内的 I/O 与 LLM 调用。`stage_end` 的 `status` 直接
复用了 ok/error/cancelled/denied 四种字符串，前端不需要额外枚举。

### 3.2 容量治理的实际行为

`MAX_TRACKED_TRACES = 64`（`core/trace.rs:15`）。注释说"超过后清空最旧桶"，实现是：

```rust
if map.len() >= MAX_TRACKED_TRACES {
    map.clear();
}
```

即**清空所有桶**，而不是最旧的一个。对 trace 这种"辅助展示、丢失无害"的数据可以接受，
但如果你在排查一个长请求时恰好并发了很多请求，阶段面板可能整段消失。要改成 LRU 需要
记录每个桶的 `last_active`。

## 4. 质量计数：三个比值

`AgentQualityCounters`（`core/agent/mod.rs:231-251`）是四个 `AtomicU64`：

| 计数 | 埋点 | 语义 |
|---|---|---|
| `requests` | `agent_query` 入口（`commands/llm.rs:940`） | 请求总数 |
| `hallucination_warnings` | 两个守卫命中时 +1（`commands/llm.rs:688`、`757`） | 防幻觉提醒触发次数 |
| `tool_successes` | `record_tool_result(ok=true)`（`tools/mod.rs:275-280`） | 工具成功次数 |
| `tool_failures` | `record_tool_result(ok=false)` | 工具失败次数 |

派生的两个比值写在 `log_quality_summary`（`core/agent/mod.rs:262-288`）：

```text
Hallucination Rate ≈ hallucination_warnings / requests
工具失败率         ≈ tool_failures / (tool_successes + tool_failures)
```

几个必须知道的口径细节：

- **只统计进程生命周期**，重启归零，不落库。它是"迭代时观察"的工具，不是报表；
- **取消不算失败**：`record_tool_result_structured` 在请求已取消时直接 return
  （`tools/mod.rs:266-270`），避免把用户点停止产生的 `Cancelled` 计成工具故障；
- **失败率的分母只有工具调用**，不含"模型没调工具"的情况。所以"失败率低"不等于
  "Agent 可靠"，还要看 Hallucination Rate；
- Loop 工具同样计入：新内核的 `BusToolEventSink::on_result` 会转调
  `record_tool_result`（`core/agent/loop_tools.rs:3442-3443`），两代工具共用同一计数。

## 5. 技能指标：落库的那一层

技能指标是唯一落库的 Agent 指标，设计目标是"回答哪个技能在什么来源、什么耗时下更容易
失败"。核心结构在 `core/skill/metrics.rs`：

```text
skill_dispatch_stats    单行表（id=1）：total_dispatches / matched_dispatches
skill_exec_metrics      明细表：request_id / scope / skill_id / match_level /
                        score / state / duration_ms / error_code / created_at
```

### 5.1 两段式命中率

调度计数不是"命中就 +1"，而是**起始计总数、结束补命中**（`metrics.rs:171-203`）：

- 请求开始 `record_dispatch(dir, false)`：只 `total += 1`；
- 请求结束若有技能实际激活，`record_dispatch_matched(dir)`：`matched += 1`。

原因是技能既可能被规则预激活，也可能被 LLM 在对话中途动态激活。如果只在开始时判断，
LLM 激活会被记成 miss，命中率被系统性低估。

### 5.2 写放大治理

高频路径不直接写库：`pending: HashMap<dir, PendingDispatch>` 先累加内存，满足任一条件才
落库（`metrics.rs:105-123`）：

- 累计 `total >= DISPATCH_FLUSH_THRESHOLD`（16）；
- 请求结束 `record_execution_batch` 时无条件冲刷一次（阈值传 1）。

这样单请求的 2~3 次调度写被合并成 1 次，减少与 `ChatStore`/`AiHistoryStore` 争同一把
SQLite 写锁。落库用 `TransactionBehavior::Immediate`（`metrics.rs:225`），注释解释了原因：
WAL 下 `DEFERRED` 读快照升级可能报 `SQLITE_BUSY_SNAPSHOT`。

### 5.3 聚合窗口与公式

`get_summary`（`metrics.rs:325`）不是全表聚合，而是每个 `(scope, skill_id)` 分区取
**最近 50 条**（`ROW_NUMBER() OVER (PARTITION BY ...)`），理由是看板只需要近期窗口，
且按技能独立取窗能避免高频技能把低频技能挤出样本。

- 成功率 = 成功条数 / 窗口内条数；
- 全局平均耗时 = 全部明细的 `sum/count`，**不是各技能均值的平均**（否则低频技能被稀释）；
- 调度命中率 = `matched / total`（来自单行表，重启后仍准确）。

明细还会做 90 天清理，但做了节流：每目录每小时最多一次 `DELETE`（`metrics.rs:269-279`），
避免每次写入都附带范围删除。

## 6. 用量统计：一个 30s/1h 的口径矛盾

`stats_ai_usage`（`commands/stats.rs:63`）为 dashboard 热力图提供按天聚合：

- 数据源：`ai_history` 的 `daily_usage_since` + chat 的 `daily_messages_since` /
  `session_count_since`，全部在 `spawn_blocking` 里跑，不阻塞 tokio；
- 缓存键 `"{dir}|{days}"`，默认窗口 365 天，`days` clamp 到 1..3650；
- 合并时同一日期可能同时有 AI 调用与消息，两个维度分别累加。

> **口径陷阱**：模块注释写"内存缓存 30s"，常量却是
> `STATS_CACHE_TTL_SECS = 60 * 60`（`stats.rs:19`），即 1 小时。实际行为是 1 小时内
> 看不到新数据。对热力图这种低频页面可以接受，但如果产品要求"准实时"，应改回 30s 或
> 在写入路径主动失效缓存。

## 7. 过程摘要：让下一轮"记住上一轮怎么失败的"

`write_process_summary`（`commands/llm.rs:499-642`）在每个请求结束时，从任务快照里
生成一段精简文本，写入 `last_process_summary` 的 **one-shot 槽**；下一个请求组装 system 时
`take_process_summary` 取走并清除（`commands/llm.rs:436-442`），只影响紧邻的下一轮。

摘要内容按"可执行信号"组织：

- 阶段耗时：`planning=ok 120ms, searching=ok 480ms...`；
- 工具结果：`tool(ok/FAIL/running) summary`，摘要截断 60 字符；
- 失败明细：每条带参数预览（50 字符）、原因（80 字符）与分类；
- 同工具成败统计；
- 判断指引：只有**全部失败都是环境问题**时才建议换方案，否则明确要求"先修正参数或调用
  方式后重试"。

失败分类 `classify_failure`（`commands/llm.rs:458-481`）是关键词规则，顺序是
Env（连接/超时/服务不可用/网络）→ Usage（权限/锁定/未初始化/必须先执行）→
Param（默认）。默认归 Param 是刻意的：绝大多数工具失败确实是路径/参数问题，默认导向
"修正重试"而不是"换工具"，能显著减少模型无谓地放弃可用工具。

## 8. 评测一：场景断言框架

`core/eval` 是一个**纯断言**框架，把"执行"与"判断"拆开（依赖倒置）：

```rust
pub async fn evaluate_all<F, Fut>(scenarios: &[EvalScenario], mut run: F) -> EvalReport
where F: FnMut(&EvalScenario) -> Fut,
      Fut: Future<Output = (Vec<String>, String)>;
```

- `EvalScenario`：`name` / `query` / `expected_tools` / `expected_outcome_regex`，
  可从 YAML 加载（`load_scenarios_yaml`）；
- `assert_tool_sequence`：期望工具按序**作为子序列**出现即可，允许中间夹额外调用
  （`eval/mod.rs:65-80`）。这是务实的：Agent 多调一个 `read` 不应判定失败；
- `assert_outcome`：可选正则，未配置视为通过；
- `evaluate_scenario` 把工具与输出两类失败合并到 `detail`，报告给出 `passed_rate`。

内置 3 个场景（`multi_tool_task` / `plan_then_execute` / `memory_recall`，
`eval/mod.rs:160-181`），但**当前只有测试在跑**：模块头注释明确"真实执行器待 CLI 接入"，
且整个模块 `#![allow(dead_code)]`。也就是说，这是一套"可用的骨架"，还没有成为 CI 的
回归门禁。要真用起来，需要提供一个执行器：起一个 headless Agent，把
`LoopEvent::ToolCall` 收集成 `Vec<String>`，把最终文本作为 output。

## 9. 评测二：检索基准

`bin/benchmark.rs` 是检索侧的可执行基准，由 `bench` feature 门控
（`Cargo.toml:18-21`），通过 `bench_api` 窄门面访问 `Indexer`。用法：

```text
cargo run --bin benchmark -- --kb <目录> --queries queries.jsonl \
  --expected expected.jsonl [--topk 20] [--reindex --yes-wipe]
```

指标全部是 doc 级（同一文档多个 chunk 去重后只计一次）：

| 指标 | 实现 | 说明 |
|---|---|---|
| Recall@5/10/topk | `recall_at_k` | 命中相关文档数 / 标注相关文档数 |
| MRR | `mrr` | 第一个相关文档排名的倒数 |
| NDCG@5/10 | `ndcg_at_k` | 二值相关性 + log2 折损，除以理想 DCG |
| Latency | 墙钟 | 输出 avg 与 p95 |
| 分阶段耗时 | `last_retrieval_timings` | planner/dense/bm25/symbol/rrf/rerank/finalize |

两个工程细节：

- 汇总分母用**实际参与评测的查询数** `evaluated`，而不是 `queries.len()`。缺标注或标注
  为空的查询会 `continue`，用总查询数当分母会把所有均值系统性稀释（`benchmark.rs:280-284`）；
- `--reindex` 会清空目标目录 `.mdgo` 的索引与 embedding 缓存，因此要求显式 `--yes-wipe`
  确认，并提示先退出正在运行的应用（避免双进程写同一 LanceDB/BM25）。

## 10. 还有一层：真实样本验收

`core/acceptance.rs` 是端到端验收 harness，通过环境变量 `MDGO_ACCEPT_DIR` 启用，
用真实文档样本跑"解析 → 分块 → 索引 → 检索"全链路。它与 Agent 指标互补：benchmark 量
"检索准不准"，acceptance 量"文档进不进得来、保真度够不够"。

## 11. 设计取舍与改进方向

**取舍一：计数不落库。** 好处是零 I/O、零锁竞争；坏处是重启归零，无法做跨版本趋势。
如果要做，最小改动是复用技能指标的批量落库路径，而不是每条计数都写库。

**取舍二：Trace 是"辅助展示"，允许丢。** 所以容量治理用 `clear()` 而不是 LRU。真要做
长链路审计，应该把 trace 事件也落 `session_events` 或独立表，而不是放内存。

**取舍三：评测与执行解耦。** 这让断言可单测，但也导致"没有执行器就没人跑"。建议补一个
`--eval` 子命令，把场景执行接进 CI，否则这套框架会一直停留在"骨架"状态。

**可改进点清单**（按我读代码时的直观判断排序）：

1. `stats_ai_usage` 的 TTL 注释与常量不一致，需统一；
2. `TraceBus` 的注释说淘汰最旧桶，实现是清空全部，需改实现或改注释；
3. `log_filter_targets` 未实现注释承诺的高频 target OFF 列表；
4. `core/eval` 需要真实执行器；
5. 质量计数建议定期 dump 到日志（例如每 N 个请求触发一次 `log_quality_summary`），
   否则只在手动调用时可见。

## 12. 相关文件

| 文件 | 内容 |
|---|---|
| `tauri/src-tauri/src/lib.rs` | 日志初始化与过滤器 |
| `tauri/src-tauri/src/core/trace.rs` | TraceBus 与 stage API |
| `tauri/src-tauri/src/core/agent/mod.rs` | 质量计数与摘要输出 |
| `tauri/src-tauri/src/core/skill/metrics.rs` | 技能指标表、批量落库、聚合 |
| `tauri/src-tauri/src/commands/stats.rs` | AI 用量统计与缓存 |
| `tauri/src-tauri/src/commands/llm.rs` | trace emit、过程摘要、失败分类 |
| `tauri/src-tauri/src/core/eval/mod.rs` | 场景评测框架 |
| `tauri/src-tauri/src/bin/benchmark.rs` | 检索基准 |
| `tauri/src-tauri/src/core/acceptance.rs` | 真实样本验收 harness |

---

**上一篇**：[10 安全防线](10-安全防线-审批-注入防护与防幻觉.md)
**下一篇**：[12 MCP、外部工具与前端事件桥](12-MCP-外部工具与前端事件桥.md)
