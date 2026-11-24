# Agent 上下文体积治理方案

> 定位：**问题 3「上下文体积失控」的专项设计**（对比 DeepSeek Harness 与主流 Agent 的实现，给出 mdgo 的改造方案）。
> 状态：设计稿（2026-11-24），待评审；本文不含代码改动。
> 关联：`docs/Agent 内核架构.md`（§2.2.1 前缀缓存布局）、`docs/Agent 时延优化实施记录.md`（P0-2/P0-3/P0-6/P0-7 已落地）、`docs/blog/07-上下文工程-Token 预算与压缩.md`（现有压缩机制详解）。

---

## 0. 摘要

**结论**：mdgo 的上下文预算看起来有四道闸门，实际只有"字符闸门"生效——两个按窗口比例算的预算（前端 40 万 token、后端 80 万 token）在 1M 窗口下等于没有；唯一拦得住的是"单条工具结果 24k 字符 / 累计 120k 字符"。于是每 step 实际 prompt 落在 **60–140k token**（估算，可用 `[loop] usage prompt=` 日志实测），预填充时间 15–45s，正好解释现网"单轮 39.5s / 67.6s / 74.9s"。

**三条改造主线**（对齐 DSH 的成熟做法，并补齐主流 Agent 已普及的机制）：

1. **预算以"延迟预算"为准，不以"窗口比例"为准**：历史硬上限 32k token（可配 16k–48k），其余全部下调一个数量级。
2. **工具结果先"无模型剪枝"再谈摘要**：保头/保尾 + 统计摘要 + 剪枝标记 + 续读句柄（零模型成本、可逆），这是 DSH 的核心做法，mdgo 目前只有"整条清成占位符"。
3. **压缩从"每轮一次"改成"每 step 前的压力检查"**：DSH 是 `pre-step` 压力检查 + `request-error` 溢出恢复双触发；mdgo 只在 turn 开始前压一次，多步任务中途必然胀回去。

预期：每 step prompt 降到 **20–30k token**，TTFT 降 50–65%，输入成本降到约 1/3，且**不牺牲答案质量**（用"摘要 + 句柄续读"换掉"全量塞入"）。

---

## 1. 现状解剖：体积是怎么长出来的

### 1.1 一条请求的完整体积构成

```text
┌ system ────────────────────────────────────────────────┐
│ rag_agent.md 规约 3KB + Hook 技能约束摘要 ≤800 字符        │  ~1–2k token
├ tools（模型可见 schema，排在 messages 之前）─────────────┤
│ 30+ 个工具，描述里带大量"强制规则"，JSON 25–60KB           │  ~10–20k token（估算）
├ 本轮上下文（时间 + 检索片段 12k 字符 + 记忆 + 技能目录）──┤  ~4–8k token
├ history（关键）───────────────────────────────────────┤
│ 工具结果累计 ≤120k 字符（真实生效的闸门）                  │  ~60–80k token
│ 用户/助手正文（实测一个 70 消息会话 42k 字符）             │  ~20–28k token
└ 本轮问题 + (末尾预算提醒) ─────────────────────────────┘  ~0.2k token
```

**估算合计 60–140k token**（中文按 1.5–2 字符/token 折算）。真实值现在可以直接读日志：
`[loop] usage turn=… step=… prompt=… cached=… hit=…`（P0-2 已加）。

### 1.2 四道闸门，只有一道真的会响

| 闸门 | 代码 | 公式 | 1M 窗口下实际值 | 会触发吗 |
|---|---|---|---|---|
| 前端历史裁剪 | `css_js/modules/chat-history.js:123` | `ctx × 0.4` | **400k token** | 否（实测一次请求 517–529 条消息照样全发） |
| 后端压缩门 | `commands/llm.rs:214` `compression_budget_tokens` | `ctx × 0.8` | **800k token** | 否 |
| 压缩器字符预算 | `core/context/mod.rs:85` `tokens_to_chars_budget` | `tokens × 2` | 1.6M 字符 | 否 |
| 会话工具结果 | `core/loop/session.rs:18,24,31` | 单条 24k / 累计 120k 字符 | **24k / 120k 字符** | **是** |
| 工具自身输出 | `core/agent/limits.rs` | grep 60k / git_diff 60k / webfetch 50k / MCP 60k / 外部 100k | 60k–100k 字符/次 | 是（撑满上一层） |

**这就是根因**：`0.4 × 1M` 与 `0.8 × 1M` 在语义上"正确"，在延迟上等于"不设防"。窗口是**能力上限**，不是**使用额度**。

### 1.3 工具结果被放大三次

1. **工具产出端**：`grep` 一次 60k 字符、`git_diff` 60k、`webfetch` 50k、外部 HTTP 100k；
2. **进历史时**：单条截到 24k 字符（`MAX_TOOL_RESULT_CHARS`），累计超 120k 后**最旧的结果整条清成占位符**（`derive_history` 内，`session.rs:250-257`）——「要么 24k 全量、要么 0」，没有中间态；
3. **前端回传时**：`expandToolHistory`（`chat-history.js:71`）把 UI 里的工具记录**再展开成协议消息**（1 条 assistant 挂 N 条 tool），于是 70 条 UI 消息膨胀成 500+ 条协议消息，每轮经 IPC 全量回传、后端再 `to_vec` → `ChatTurn` → `seed_session` 拷贝三次（`llm.rs:151/183/2044`）。

第三层是"看不见的放大器"：即使模型侧只看到 120k 字符，进程内也要为同一份内容付 3 次拷贝 + 一次 IPC 序列化。

### 1.4 压缩路径：能压，但触发太晚、代价太高

- **触发点**：只在 **turn 开始前**（`commands/llm.rs::prepare_history`）——一次多步任务中途涨上去不会压；
- **门槛**：先跑 `estimate_turns_tokens`（tokenizer，对整段历史逐条编码，`core/context/mod.rs:62`），超 `ctx×0.8` 才压；
- **算法**：`SummarizeThenWindowCompressor` 取 **前 2/3 摘要 + 后 1/3 滑窗**（`mod.rs:318`），摘要用主模型（一次非流式调用，`summary_max_chars=6000`）；
- **代价**：摘要调用 600s 上限 ×5 次重试（`services/llm.rs`），慢端点上"为了省 token 先等半分钟"；
- **检查点**：`CompactionState { summary, cutoff_msg_id }` 落库，避免重复摘要（`llm.rs:2096`）。

这套机制本身设计不差（分层、可回退、有检查点），问题在**入口条件与时机**：1M 窗口下几乎永不触发，触发一次又很贵。

### 1.5 工具 schema：每 step 都要付的"固定税"

`SkillInstructionHook::pre_request` 每 step 重算可见工具集合，`assemble_tool_schemas` 每 step 深拷贝 schema 并再序列化一次（`core/loop/loop.rs:544`）。按 30–50 个工具、单 schema 0.5–1.5KB 估，**25–60KB ≈ 10–20k token**，每 step 重付，且排在 messages 之前（前缀缓存一旦工具集变化就整段失效）。

---

## 2. 为什么"窗口大就能填满"是错的

### 2.1 预填充是线性的

```text
TTFT ≈ 预填充(prompt_tokens / 预填充速率) + 首 token 解码
```

| prompt | @3k tok/s | @8k tok/s |
|---|---|---|
| 25k | 8.3s | 3.1s |
| 60k | 20s | 7.5s |
| 120k | 40s | 15s |

窗口 1M 只是"不会报错"，不是"不花钱"。

### 2.2 质量：中间内容会被忽略

长上下文里模型对"中段"的召回显著下降（lost-in-the-middle）。120k 字符的历史里 90% 是几步之前的旧工具输出，真正的依据往往只有最近 2–3 条 + 检索片段。**塞满 ≠ 更强，反而更容易被噪声带偏**。

### 2.3 与前缀缓存的关系

前缀缓存只复用"字节一致的前缀"。历史越长：
- 命中时省得多（好）；
- 但**首次预填充、以及任何一次前缀失效（换工具集、切会话、压缩）都要重付全额**。

所以正确策略是"**小而稳定**"：体积小 → 失效代价小；前缀稳 → 命中率高。

### 2.4 成本

输入 token 按量计费，多步任务里历史被重发 N 次。120k × 4 步 = 48 万输入 token / 轮，是 25k × 4 = 10 万的 **4.8 倍**。

---

## 3. 横向对比：DSH 与主流 Agent 都怎么做

> 说明：DSH 部分依据本仓库 `docs/Agent 全链路评审与 DSH 对比.md` §3.4 的调研结论；其余产品按其公开资料与常见实现归纳，具体细节随版本变化，此处只取**机制**。

### 3.1 DeepSeek Harness（DSH）

| 机制 | 做法 | 对 mdgo 的启示 |
|---|---|---|
| **事实源** | 会话 = 追加式 `SessionEvent[]`，`deriveMessages()` 从 surface 节点**增量投影**（缓存 + 游标），不变式"模型可见即已记录" | mdgo 已有事件溯源（`Session` + `derive_history`），但历史仍由前端每轮重建、全量回传 |
| **预算基准** | `ctx.tokenMeter` **回放折叠**：以上一次成功 usage 为基线（信封匹配时），否则启发式（`CHARS_PER_TOKEN=4` + 块/角色开销） | mdgo 用 tokenizer 估算整段历史，**从不参考 provider 真实 usage** |
| **压缩触发** | **双触发**：`agent/pre-step` 压力检查（每 step 前）+ `agent/request-error` 的 `CONTEXT_WINDOW_EXCEEDED` 恢复（成功压缩且 surface 前进才 retry） | mdgo 只有 "turn 前一次 + 溢出重试一次"，多步中途不压 |
| **剪枝先行** | 压缩前先做**无模型工具结果剪枝**：保 head/tail + 写剪枝标记 | mdgo 只有"整条清成占位符"（信息全丢、不可逆） |
| **替换语义** | `surfaceOp.replace(start,end)` 事务式替换模型可见面，`compaction/start\|summary\|end` 全落日志，可回放、可审计 | mdgo 用 `shadowed_seqs` 近似，粒度是"事件集合"而非"精确区间" |
| **多代理** | subagent 独立上下文；ralph 每轮**全新 child** + 有界 handoff（`maxHandoffChars=16384`） | mdgo 的 `spawn_subagent` 有 4000 字符摘要预算，方向对但缺"父向子有界投喂"的硬约束 |
| **手动入口** | `/compact` 走 `runMaintenance` | mdgo 无手动压缩入口 |

**一句话**：DSH 把上下文当成**有账本、可回放、可外科手术式替换**的资源；mdgo 目前是"估算 + 整条截断/清零"的粗粒度管理。

### 3.2 主流 Agent 的六种模式

| 模式 | 代表 | 核心动作 |
|---|---|---|
| **A. 阈值自动压缩** | Claude Code 自动 compact、Cline/Roo 的 condense、OpenHands condenser | 接近上限时把旧对话总结成一段，保留最近窗口 |
| **B. 工具结果剪枝/遮蔽** | Claude Code 清理旧工具输出、OpenHands `ObservationMaskingCondenser`、DSH prune | 旧 observation 换成短占位/摘要，保留结构 |
| **C. 摘要 + 最近窗口** | LangGraph summarization node、OpenHands `LLMSummarizingCondenser` | 前段摘要、后段原样 |
| **D. 检索式上下文** | Aider repo map（tree-sitter 符号 + 预算内注入）、Cursor codebase 检索 | 不塞历史，按相关性取 |
| **E. 分层记忆** | Letta/MemGPT（core / archival / recall） | 常驻小核心 + 可检索外部记忆 |
| **F. 子代理隔离 + 有界 handoff** | Claude Code subagents、DSH subagent、Codex/Agents SDK handoff | 子任务独立上下文，只回传结论 |
| **G. 硬截断参数** | OpenAI Agents SDK `truncation=auto/last_messages`、`max_turns` | 兜底：显式裁剪策略 + 轮次预算 |

共同点：**没有一家靠"按窗口比例设预算"**；都是"小预算 + 多级裁剪 + 可检索的外部存储"。

### 3.3 对比矩阵

| 能力 | DSH | 主流 | mdgo 现状 | 差距 |
|---|---|---|---|---|
| 预算基准 | 真实 usage 回放折叠 | 多为 token 估算 + 阈值 | tokenizer 估算 × 窗口比例 | ❌ 口径与触发都偏 |
| 触发时机 | 每 step 前 + 溢出恢复 | 接近上限时（多为 step 级） | 仅 turn 前 | ❌ |
| 工具结果 | 无模型头尾剪枝 + 标记 | 遮蔽/清理 | 24k 全量 或 整条清零 | ❌ 缺中间态 |
| 摘要 | 事务化替换 + 全量落日志 | 摘要 + 最近窗口 | 前 2/3 摘要 + 后 1/3 滑窗（有检查点） | 🟡 算法够用，入口不对 |
| 可逆/可审计 | surface replace + 日志 | 部分 | `shadowed_seqs` 遮蔽 | 🟡 |
| 子代理隔离 | 有界 handoff | 独立上下文 | 4000 字符摘要 | 🟡 |
| 手动压缩 | `/compact` | 有 | 无 | ❌ |
| 工具 schema 治理 | schema 白名单投影 | 按需暴露 | 每 step 全量 | ❌ |

---

## 4. 目标与设计原则

### 4.1 目标（可验收）

| 指标 | 现状（估算） | 目标 P50 |
|---|---|---|
| 每 step prompt token | 60–140k | **20–30k** |
| 历史 token 占比 | 70–85% | ≤ 60% |
| 单轮输入成本 | 1× | ≤ 0.35× |
| TTFT（长会话第 N 轮） | 与首轮同量级 | 首轮 × 1.3 以内 |
| 压缩触发时延 | 单次 1–30s（摘要） | 单次 ≤ 5s（无模型剪枝为主） |
| 答案质量（20 题 A/B） | 基线 | 正确率不降、引用数不降 |

### 4.2 五条设计原则

1. **按延迟预算，不按窗口比例**：预算 = `min(窗口 × 0.6, 硬上限)`，硬上限是配置项而非窗口函数。
2. **先便宜后昂贵**：无模型剪枝（0 成本）→ 规则摘要（≤1ms）→ 模型摘要（秒级）；能用前者绝不用后者。
3. **可逆、可续读**：剪掉的不是"丢弃"，而是"换成句柄 + 摘要"，模型想要全文可以显式续读一次。
4. **单元化裁剪**：`assistant(tool_calls) + 其 tool 结果` 永远同进同退（mdgo 已具备，保持）。
5. **可度量**：每次裁剪/压缩都留痕（次数、省了多少 token、耗时），能被日志和面板读到。

---

## 5. 方案设计

### 5.1 预算模型（核心）

引入**单一预算入口** `ContextBudget`，替换现在散落的 4 个比例：

```rust
pub struct ContextBudget {
    pub window: u32,            // 模型窗口（来自 profile）
    pub reserve_output: u32,    // 预留输出 = min(max_tokens, 8192)
    pub safety_ratio: f32,      // 0.6：给工具/上下文/误差留余量
    pub hard_cap_tokens: u32,   // 用户可配：默认 24_000，范围 16_000–48_000
    pub system_cap: u32,        // 静态规约上限 8_000
    pub tools_cap: u32,         // 工具 schema 上限 12_000
    pub context_cap: u32,       // 检索/记忆注入上限 6_000
    pub tool_result_item_cap: u32,   // 单条工具结果 10_000（2026-11-24 由 8_000 上调）
    pub tool_result_total_cap: u32,  // 历史工具结果累计 24_000（由 16_000 上调）
}

impl ContextBudget {
    /// 历史可用预算 = min(窗口×0.6 - 输出预留 - system - tools - context, hard_cap)
    pub fn history_tokens(&self) -> u32 { … }
}
```

**关键变化**：

| 旋钮 | 现值 | 新值 | 说明 |
|---|---|---|---|
| 历史预算 | `ctx × 0.8`（≈800k） | `min(ctx×0.6 − 预留 − 固定段, 24k)` | 真正的"延迟预算" |
| 前端裁剪 | `ctx × 0.4` | 与服务端同源：`history_tokens()` 的结果（后端下发 or 同公式） | 消灭双口径 |
| 单条工具结果 | 24k **字符** | 10k **token**（≈20k 字符，按语言折算） | 统一到 token |
| 累计工具结果 | 120k 字符 | 16k token | 同上 |
| 压缩触发 | turn 前超过 0.8×ctx | **每 step 前 ≥ 0.85×预算** | DSH 式压力检查 |
| 摘要上限 | 6000 字符 | 2000 token | 摘要不该自己变成大头 |

> 兼容：`context_length=0`（未配置窗口）时退回旧常量 `MAX_MESSAGE_TOKENS=15k`，行为不变。

### 5.2 工具结果治理：三级 + 句柄

| 级别 | 触发条件 | 处理 | 成本 |
|---|---|---|---|
| **L0 原样** | ≤ 1.5k token | 不动 | 0 |
| **L1 无模型剪枝** | 1.5k–8k token | 保 head（~1.5k token）+ tail（~0.5k token）+ 结构摘要（行数/命中数/文件数/是否截断）+ 剪枝标记 + 续读句柄 | <1ms |
| **L2 模型摘要** | > 8k token，或同一工具在本轮累计超阈值 | L1 + 一次小摘要（可合并同批多条） | 秒级，且仅在必要时 |

**输出契约**（进历史的统一形状）：

```json
{
  "tool": "grep",
  "status": "ok",
  "summary": "在 12 个文件中命中 47 处（已截断）",
  "head": "…前 ~1.5k token…",
  "tail": "…后 ~0.5k token…",
  "pruned": { "dropped_chars": 41234, "reason": "budget" },
  "continue": { "tool": "grep", "args": { "pattern": "…", "offset": 47 }, "hint": "需要更多命中时续读" }
}
```

**关键点**：

- 模型看到的是"**结构化证据 + 可执行的续读指令**"，而不是"一大段被砍断的文本"；
- 全文留在进程内缓存（按 `call_id` 或内容哈希），续读时零 IO；
- 剪枝标记让模型**知道**自己看到的是片段（避免把截断当全文——这是现在"整条清零"最大的坑）。

### 5.3 历史治理：三档压缩 + 每 step 水位

```text
每 step 前（DSH 式 pressure check）：
  估算 prompt = system + tools + context + history(+本轮)
  ├─ < 0.85 × 预算 → 直发
  ├─ ≥ 0.85 × 预算 且 存在可剪枝项 → L1 无模型剪枝（旧工具结果 → 摘要+句柄）→ 重估
  ├─ 仍超 → L2 摘要压缩（前 2/3 摘要 + 后 1/3 滑窗，复用现有 Compressor）
  └─ 仍超 → 硬裁剪（按单元丢弃最旧，附"已省略 N 轮"标记）
```

- **摘要分层**：会话级摘要（checkpoint，已有）+ 段摘要（每 10 轮一条，新增）+ 本轮任务卡（`todo_write` 的当前状态，已有工具）；
- **可审计**：每次剪枝/压缩写一条 trace（`context/prune`、`context/compact`：省了多少 token、耗时、策略），前端"过程时间线"可见；
- **保留现有检查点机制**（`CompactionState`），不重造轮子。

### 5.4 工具 schema 治理

1. **描述瘦身**：长"强制规则"下沉到对应技能文档（渐进式披露），schema 描述控制在 ~200 字符内；
2. **按需暴露**：`active_tools` 已按技能窄化，进一步把"基础工具"固定为 12–15 个，其余按需挂载；
3. **稳定序列**：`names()` 字典序（已做）+ 每轮组装一次 `Arc<[ToolSchema]>`（P1-6 已排期）；
4. **预算门**：`tools_cap` 超限时按"最近使用 + 技能声明"排序截断，并在 trace 里标记。

### 5.5 系统提示与注入治理

- system 只留静态规约（已完成，见 `docs/Agent 内核架构.md` §2.2.1）；
- 检索上下文注入上限从 12k **字符**收敛到 `context_cap`（6k token，≈9k 字符），按"文档数 × 每文档片段数"分配（例如 3 文档 × 2 片段 × 1.5k）；
- 记忆块 ≤ 3 条、每条 ≤ 200 字符（已有，补硬上限）。

### 5.6 运行时与度量

- **水位线**：`0.60 提醒`（trace 黄）、`0.85 剪枝`、`0.95 摘要`、`1.0 硬裁剪`；
- **指标**（新增 trace 事件，复用现有 `trace:event` 通道）：

| 事件 | 字段 |
|---|---|
| `context/snapshot` | prompt_tokens、history_tokens、tools_tokens、context_tokens、cached_tokens |
| `context/prune` | tool、dropped_chars、kept_head/tail、耗时 |
| `context/compact` | before/after token、策略（summarize/window/hard）、耗时 |
| `context/budget` | 预算值、触发阈值、当前比例 |

- **面板**：前端已有"上下文占用率/缓存命中率"角标，扩展为"预算/实际/命中"三值 tooltip；
- **回归**：`retrieval_eval/`（Recall@10 不降）+ 20 题人工 A/B（正确率、引用数、工具成功率）。

---

## 6. 参数与实现映射

| # | 改动 | 位置 | 类型 |
|---|---|---|---|
| 1 | 新增 `ContextBudget`（单一预算入口） | `core/loop/` 或 `core/context/` 新文件 | 新模块 |
| 2 | `compression_budget_tokens` → `ContextBudget::history_tokens` | `commands/llm.rs:214` | 替换 |
| 3 | 前端预算与服务端同源 | `chat-history.js:123`、`main.html:17256` | 改口径 |
| 4 | `MAX_TOOL_RESULT_CHARS` → token 口径 + L1 剪枝 | `core/loop/session.rs:18,400` | 重写截断 |
| 5 | `MAX_HISTORY_TOOL_RESULT_CHARS` → 16k token | `core/loop/session.rs:24` | 调参 |
| 6 | `COMPACT_TRIGGER_CHARS` → 每 step 水位 | `core/loop/session.rs:31` + `loop.rs` | 新增钩子 |
| 7 | 剪枝/压缩 trace | `core/trace.rs` + 调用点 | 新增事件 |
| 8 | 工具结果结构（head/tail/continue） | `core/agent/tools/mod.rs` 输出层 | 新契约 |
| 9 | 工具描述瘦身 | `core/agent/loop_tools.rs`、`tools/mod.rs` | 文案 |
| 10 | `/compact` 手动入口 | `commands/llm.rs` + 前端命令 | 新增命令 |

---

## 7. 实施计划

| 批次 | 内容 | 预估 | 依赖 | 验收 |
|---|---|---|---|---|
| **C1** | `ContextBudget` + 预算替换（后端）+ 前端同源 | 1.5d | 无 | 1M 窗口下历史预算 = 24k；trace 可见 |
| **C2** | 工具结果 L1 剪枝（head/tail/摘要/句柄）+ 标记 | 2d | C1 | 单条结果 ≤ 8k token；模型能续读 |
| **C3** | 每 step 水位检查（先剪枝后摘要）+ 硬裁剪兜底 | 2d | C2 | 20 步任务中途不超预算 |
| **C4** | 工具 schema 瘦身 + 按需暴露 | 1.5d | 无 | tools ≤ 12k token |
| **C5** | trace/面板/回归脚本 | 1d | C1–C3 | 三场景指标进基线 |

**回滚**：`ContextBudget` 全部走配置（`latency_profile: fast \| balanced \| quality`），`quality` 档保留当前行为。

---

## 8. 风险与取舍

| 风险 | 影响 | 缓解 |
|---|---|---|
| 剪枝导致模型看不到关键上下文，答案变浅 | 高 | 头尾 + 结构摘要 + 续读句柄；`quality` 档保留全量；20 题 A/B + 检索回归 |
| 每 step 水位检查带来额外 tokenizer 开销 | 中 | 用"上一步真实 usage"做基线（DSH tokenMeter 思路），不再整段重编码；字符预筛 |
| 摘要在多步中途触发，打断节奏 | 中 | 优先无模型剪枝；摘要仅在剪枝后仍超时触发；摘要调用 20s 超时 |
| 前端/后端预算口径再次漂移 | 中 | 预算由后端下发（`rag:status` 或专用事件），前端只读；单测锁定公式 |
| 工具描述瘦身影响调用准确率 | 中 | 先 A/B 10 个高频工具；长规则保留在技能文档，`activate_skill` 后可见 |
| `quality` 档用户仍遇到慢 | 低 | 档位说明里写明代价；默认 `fast` |

---

## 9. 附录

### 9.1 关键常量现状（改造基线）

| 常量 | 现值 | 位置 |
|---|---|---|
| `MAX_TOOL_RESULT_CHARS` | 24 000 字符 | `core/loop/session.rs:18` |
| `MAX_HISTORY_TOOL_RESULT_CHARS` | 120 000 字符 | `core/loop/session.rs:24` |
| `COMPACT_TRIGGER_CHARS` | 120 000 字符 | `core/loop/session.rs:31` |
| `MAX_MESSAGE_TOKENS`（ctx=0 兜底） | 15 000 token | `core/agent/limits.rs:16` |
| `MAX_CONTEXT_CHARS`（RAG 注入） | 12 000 字符 | `core/agent/limits.rs:24` |
| `MAX_MESSAGE_CHARS`（单条问题） | 30 000 字符 | `core/agent/limits.rs:14` |
| `SUMMARY_MAX_CHARS` | 6 000 字符 | `core/agent/limits.rs:18` |
| `SUBAGENT_SUMMARY_CHARS` | 4 000 字符 | `core/agent/limits.rs:81` |
| grep / git_diff | 60 000 字符 | `limits.rs:66,68` |
| webfetch / MCP / 外部 HTTP | 50k / 60k / 100k 字符 | `limits.rs:97,121,93` |
| 压缩预算 | `ctx × 0.8` | `commands/llm.rs:214` |
| 前端裁剪 | `ctx × 0.4` | `chat-history.js:123` |
| 摘要/滑窗切分 | 前 2/3 摘要 + 后 1/3 | `core/context/mod.rs:318` |
| token 估算 | tokenizer 优先，回退 `chars/2` | `core/context/mod.rs:40-62` |

### 9.2 DSH 机制摘录（供实现参考）

- `deriveMessages()` 增量投影 + 缓存游标；"模型可见即已记录"；
- `tokenMeter`：以上一次成功 usage 为基线，信封不匹配才退回启发式；
- compaction seam 双触发：`agent/pre-step` 压力检查 + `agent/request-error`（仅 `CONTEXT_WINDOW_EXCEEDED`，且压缩推进才 retry）；
- 压缩前**无模型工具结果剪枝**（保 head/tail + 剪枝标记）；
- `surfaceOp.replace(start,end)` 事务 + `compaction/start|summary|end` 全落日志；
- subagent 独立上下文；ralph 每轮全新 child + `maxHandoffChars=16384`。

### 9.3 验收脚本（建议）

```powershell
# 1) 单元/回归
cd tauri/src-tauri; cargo test --lib

# 2) 三场景基准（纯对话 / 单工具 / 3 步工具），各 5 次取 P50
cargo run --bin benchmark -- --scenario chat,one-tool,three-step --repeat 5

# 3) 线上观测（一轮真实任务后）
Select-String -Path "$env:APPDATA\com.mdgo\logs\mdgo.log" -Pattern '\[loop\] usage|context/prune|context/compact'
```

判定：`prompt` P50 ≤ 30k；`hit` 随轮次上升；`context/prune` 有记录且 `dropped_chars` 合理；TTFT 相对基线下降 ≥50%。

### 9.4 与既有文档的关系

- 本文替换"按窗口比例设预算"的旧口径；`docs/blog/07-上下文工程-Token 预算与压缩.md` 中 §2.2（"模型窗口的 80%"）与 §2.4（预算常量实值）**在 C1 落地后需同步修订**；
- 前缀缓存（P0-2）与本方案互补：P0-2 解决"能不能复用"，本文解决"要不要发"。

---

## 10. 实施记录（2026-11-24，C1–C5 已落地）

> 与 §5 设计一致；下列参数为**当前默认值**，全部集中在 `ContextBudget` 一处。

### 10.1 落地清单

| 批次 | 内容 | 代码位置 |
|---|---|---|
| C1 | `ContextBudget` 单一预算入口（窗口 → prompt / 历史 / 工具结果 / schema 预算）+ 后端替换 + 前端 24k 硬上限 | `core/loop/budget.rs`（新增）、`commands/llm.rs`（`context_budget` / `history_budget_tokens`）、`css_js/modules/chat-history.js` |
| C2 | 工具结果 L1 剪枝：头 4/5 + 尾 1/5 + 剪枝标记 + 续读提示；累计超限的旧结果保留 200 字符头部摘要 | `core/loop/session.rs`（`prune_tool_result` / `pruned_placeholder` / `derive_history_with`） |
| C3 | 每 step 前压力检查：超 85% 水位 → `compact_to` 无模型压缩（按 step 边界遮最旧，**保留本轮 system_context**） | `core/loop/loop.rs::turn`、`Session::compact_to` |
| C4 | 工具 schema 预算：超限渐进截断描述（1200→800→500→320 字符），**不丢工具** | `core/loop/loop.rs::fit_tool_schemas` |
| C5 | 观测：`LoopEvent::ContextPruned` → `trace:event`（阶段 `context`）+ `[loop] context pressure` 日志 | `core/loop/loop.rs`、`commands/llm.rs` |

### 10.2 默认参数

| 参数 | 值 | 说明 |
|---|---|---|
| 窗口安全系数 | 0.6 | prompt 预算 = 窗口 × 0.6 |
| 输出预留 | `min(max_tokens, 8192)` | 不把整窗都当输入 |
| 历史硬上限 | 32k token | 1M 窗口下也是 32k（2026-11-24 精度优先，由 24k 上调） |
| system / tools / context | 8k / **20k** / 6k token | 固定段；小窗口按 `prompt/2` 收缩（tools 由 12k 上调，现有工具集约 26.5k 字符，避免描述被兜底截断） |
| 单条工具结果 | **10k token**（≈20k 字符，且 ≤ 历史 40%） | 超限走头尾剪枝（由 8k 上调） |
| 工具结果累计 | **24k token** | 超限的旧结果保留头部 200 字符（由 16k 上调） |
| 压力水位 | 历史预算 × 85% | 超水位触发无模型压缩 |
| 未配置窗口 | 历史 15k token（旧行为） | `DEFAULT_HISTORY_TOKENS` |

### 10.3 行为变化（升级须知）

1. 大窗口模型不再"塞满窗口"：长会话会更早触发剪枝/压缩（这正是本次目的）；
2. 工具结果超限时是"头 + 尾 + 标记"，不再整条清空；累计超限的旧结果保留 200 字符头部；
3. 工具 schema 超预算时**只截描述、不丢工具**；
4. `MAX_MESSAGE_TOKENS` 常量退役，权威口径为 `ContextBudget`；
5. 压缩时**保留本轮 `system_context`**（检索/记忆注入），只遮历史 step。

### 10.4 验证

- `cargo test --lib`：**593 passed / 0 failed**（新增：budget 5 项、工具结果剪枝 1 项、schema 预算 2 项；并修正了按"全量字符"计预算的旧断言）；
- `cargo check --lib`：0 warning；
- 日志：`[loop] context pressure … before/after/dropped/compacted`、`[loop] usage … prompt/cached/hit`；
- 前端时间线：出现 `context` 阶段的 `pruned` 事件。

### 10.5 未覆盖（后续）

- 工具描述"内容级瘦身"（把长规则下沉到技能文档）：本次只落了机制（截断），内容编辑需 A/B；
- 服务端会话（只发增量）仍为 P2；
- 手动 `/compact` 入口未做；
- `docs/blog/07-上下文工程-Token 预算与压缩.md` §2.2/§2.4 的旧口径说明待随文修订（该文为历史存档，改动前先与作者确认）。

### 10.6 调参记录（2026-11-24：精度优先，上调上限）

**触发原因**：现网日志 `[loop] 工具 schema 仍超预算：26574 > 24000 字符`。描述已截到 320 字符下限仍超，
说明体积主要来自**参数 JSON + 每工具固定开销（+48）+ 工具数量**，再截描述只会持续损失工具说明、且降不下去。

| 旋钮 | 旧 | 新 | 理由 |
|---|---|---|---|
| `tools_cap` | 12k token（24k 字符） | **20k token（40k 字符）** | 实测工具集 26.5k 字符，留 ~50% 余量后不再触发截断 |
| `hard_cap_tokens`（历史） | 24k token | **32k token** | 减少压缩/剪枝触发频率，保留更多上下文细节 |
| `tool_result_item_cap` | 8k token | **10k token** | 单条结果被剪得过碎的边界抬高 |
| `tool_result_total_cap` | 16k token | **24k token** | 多工具轮次不急于清理较早结果 |
| `SUMMARY_MAX_CHARS` | 6k 字符 | **8k 字符** | 压缩摘要少丢细节 |
| `SUBAGENT_SUMMARY_CHARS` | 4k 字符 | **6k 字符** | 子代理交接一次看更多（全文仍可分页读） |
| 前端历史硬上限 | 24k token | **32k token** | 与后端同源 |

**日志噪声修复**：schema 超预算从"每 step 打一次 warn"改为**每 turn 只打一次**（`fit_tool_schemas`
返回 overage，由 `turn()` 记录）；"已截断到 N 字符"仍是 info。顺带给 `tools` 预算留出余量后，
正常情况不再走截断分支。

**同类限制审计**（是否也存在"静默截断、降低精度"）：

| 限制 | 处理 | 说明 |
|---|---|---|
| `MAX_CONTEXT_CHARS` 12k（RAG 注入） | **保留** | 继续加只会引入噪声；它属于"证据"而非"历史" |
| `MAX_TOOL_RESULT_CHARS` 24k（落库前存储上限） | 保留 | derive 层现在是 20k 字符，仍在护栏内 |
| `grep 60k / git_diff 60k / webfetch 50k / MCP 60k / 外部 100k` | 保留 | 工具**输出**上限，最终都受 20k 字符 item 剪枝约束；调大只增加进程内存 |
| `MAX_MESSAGE_CHARS` 30k（单条提问） | 保留 | 超长提问罕见，且截断会明确提示 |
| `MAX_FILE_READ_CHARS` 8k/页 | 保留 | 分页续读是设计，不是损失 |
| `DOC_DEFAULT_BUDGET_TOKENS` 16k | 保留 | 文档子 Agent 单文件预算，与主链独立 |

**成本影响**：`tools_cap` 是**上限**而非目标值——实际工具集 ≈13k token，只比之前"截断后"多 1–2k token；
历史段只有接近 32k 时才会多付。典型每 step prompt 仍在 25–35k，不会回到改造前的 60–140k。
