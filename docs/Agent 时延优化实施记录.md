# Agent 时延优化实施记录（P0-2 / P0-3 / P0-6 / P0-7）

> 状态：**已实施**（2026-11-24）。本文记录四处与模型/检索质量解耦的时延修复：
> 连接复用、工具事件去重、重复调用结果复用，以及前缀缓存（稳定 prompt 前缀 + token/命中率统计）。
> 关联：`docs/Agent 内核架构.md`（§2.2 turn 流程、§2.2.1 前缀缓存布局、§4.2 调度器、§5.2 适配器已同步更新）。

---

## 1. 为什么先做这几条

这几条都属于"每轮固定浪费"，不改变检索策略与答案质量，因此**可独立验收、可单独回滚**：

| 项 | 现象（改造前） | 证据 |
|---|---|---|
| P0-3 | 每个 step 新建 `reqwest::Client`，连接池随对象丢弃 → 每步一次 DNS/TCP/TLS 握手 | `core/loop/openai.rs` 的 `http_client()` 每次调用都 `builder().build()`；`services/anthropic.rs` 同样 |
| P0-7 | 模型重复发起同一调用时，熔断只回一条错误，模型常再换措辞试一次 → 多付一个完整 step | 现网日志：`[loop_guard] 熔断重复工具调用: 'git_log' … 连续调用过 2 次`（一轮内 2–4 条）；单轮实测 39.5s / 67.6s / 74.9s |
| P0-6 | 调度器与工具实现各上报一遍工具事件，`BusToolEventSink` 忽略 `call_id` → 两张卡片、IPC 翻倍、质量计数 +2 | `core/loop/tool_calls.rs` 的 `run_one` 与各工具的 `ctx.sink.on_call/on_result` |
| P0-2 | system 里装着每轮都变的时间/检索上下文 → provider 前缀缓存从第 0 个 token 起失效；用量"每 step 覆盖"导致 token 总数与命中率失真 | `core/loop/loop.rs::assemble_request`（时间块进 system）、`commands/llm.rs`（RAG 上下文进 system）、`LoopEvent::Usage` 覆盖式赋值 |

---

## 2. P0-3 进程级共享 HTTP 客户端

**改动**

- 新增 `core/http.rs`：`shared()` 返回进程级单例 `reqwest::Client`
  - `connect_timeout = 15s`（连接阶段快速失败）
  - `pool_idle_timeout = 90s`（跨 step、跨轮复用连接）
  - `tcp_keepalive = 60s`
  - **不设总超时**：总时限由调用方逐请求 `RequestBuilder::timeout()` 设置
- `core/loop/openai.rs`：删除 `http_client(total_timeout)`，`complete()` / `stream()` 改用
  `crate::core::http::shared()`，分别挂 `.timeout(self.timeout)` / `.timeout(STREAM_TOTAL_TIMEOUT)`。
- `services/anthropic.rs`：`open_stream()` 改用共享 client，总超时仍为 `ANTHROPIC_REQUEST_TIMEOUT`。

**设计取舍**

不把超时烘进客户端。若按"超时档位"各建一个 client，又会把连接池拆成多份，
跨 step 复用的收益打折；`RequestBuilder::timeout` 的语义（覆盖连接+发送+读 body）
与原客户端级 `.timeout()` 一致，流式空闲看门狗（600s）也仍在适配器内独立生效。

**预期收益**：每 step 省一次 DNS/TCP/TLS（公网端点 50–200ms/次；局域网端点可忽略）。

---

## 3. P0-7 同参数重复调用复用上次结果

**改动**

- `core/loop/tool.rs` 新增 `canonical_call_key(name, arguments)`：JSON 对象键排序 + 扁平拼接；
  非法 JSON 退化为原字符串（宁可少命中，不误命中）。
- `core/loop/loop.rs::turn` 增加 turn 内 `replay_cache: HashMap<key, ToolResult>`：
  - 一批调用里命中缓存的**直接回填上次真实结果**，不执行、不进 Hook/审批；
  - 回填文案由 `replay_note()` 生成：首次提示"未重复执行"，第 2 次起升级为"请勿再次重复调用，
    更换参数或基于已有结果作答"；
  - 命中项仍通过 `ToolEventSink` 补一对 call/result（前端卡片成对，不会缺卡片）；
  - **写操作（`concurrency_safe = false`）一旦执行，读类缓存整体失效**（读结果可能已过期），
    只保留写自身的结果——同参数重复写直接复用，避免重复副作用；
  - 单条结果 >64KB 不入缓存，条目上限 32（超限清空），复用项回写"原始结果"避免说明文案嵌套。

**与既有 Loop Guard 的关系**

`SkillGateHook` 的重复调用熔断保留作为兜底；正常情况下重复调用在 loop 层已被复用，
不会再走到"（被拦截）"分支。子代理路径没有该 Hook，现在也自动受益（复用发生在内核层）。

**取舍**

- 复用绕过审批门：命中的是同参数写操作，而它此前已被批准过（缓存里只有成功结果）。
- 状态可能在两步之间被外部修改（用户手动编辑文件）：缓存随"任一批含写调用"清空，
  且只在同一 turn 内有效，风险窗口很小。

---

## 4. P0-6 工具事件按 call_id 去重

**改动**

- `core/agent/tools/mod.rs::record_tool_call` 改为接收 `call_id`（模型给出的 `ToolCall.id`），
  不再自造 `call_{uuid}`；`ToolCallBus::record_call` 本来就以该 ID 落库，配对逻辑不变。
- `core/agent/loop_tools.rs::BusToolEventSink` 增加两个 `Mutex<HashSet<String>>`：
  - `on_call`：同一 `call_id` 只记录第一次；
  - `on_result`：同一 `call_id` 只记录第一次（并跳过质量计数，避免 `tool_successes/failures` 翻倍）。
  - 兜底：`call_id` 为空（个别网关不下发 tool_call id）时**不去重**——宁可重复，也不吞掉不同调用。

**效果**

- 每次工具调用只产生一对 call/result 事件：IPC 载荷减半、前端不再出现重复卡片、
  会话历史里不再重复记录同一次工具调用、质量指标恢复正确。
- 结果事件继承真实 `call_id`，前端可按 ID 精确配对（不再只靠工具名 + 顺序）。

---

## 5. P0-2 稳定前缀（前缀缓存）+ token/命中率统计（2026-11-24）

**问题**：system 里塞了每轮都变的内容（时间 / 检索上下文 / 记忆 / 技能目录 / 过程摘要），
而 system 是 prompt 第 0 个 token —— provider 的前缀缓存从第一个字节起失配，长会话每轮
都从零预填充；同时"每 step 覆盖"的用量统计让 token 总数与命中率都失真。

**改动**

1. **system 静态化**：`LoopConfig.system_prompt` 只保留 `rag_agent.md` 规约 +
   Hook 的稳定 preamble。以下内容不再进 system（命令层 `commands/llm.rs` 负责拆分）：
   过程摘要、检索上下文、技能目录、技能状态纠正、时间块、预算提醒。
2. **本轮上下文（turn context）**：`LoopAgent::set_turn_context()` 注入、`build_turn_context()`
   在 turn 开始时落成一条 `SessionEvent::UserMessage { source: "system_context" }`，
   位置固定在本轮用户问题**之前**，整轮只插一次 → 多 step 之间字节完全一致。
   时间块也在这里（仍是相对时间的唯一权威依据，语义位置不变）。
3. **预算提醒**改为**尾部提示**（`assemble_request` 返回 `tail_notice`，追加在 history 之后）：
   最后几轮逐 step 变化只影响尾部增量，不再冲掉前缀。
4. **工具顺序确定化**：`HashMapToolRegistry::names()` 排序返回（`tools` 数组在 messages 之前
   参与前缀，`HashMap` 迭代序随实例随机会让两轮 tools 段不同）。
5. **用量统计修正**：
   - `TokenUsage::accumulate()` + `cache_hit_ratio()`（`core/loop/types.rs`）；
   - 命令层 `LoopEvent::Usage` 由"每 step 覆盖"改为**全轮累加**（`UsageInfo`），
     `RagDone.prompt_tokens/completion_tokens/cached_input_tokens` 现在是整轮合计；
   - 新增 `RagDone.context_tokens`：**最后一个 step** 的输入 token（真实上下文规模），
     前端"上下文占用率"用它，避免多 step 累计值虚高（`css_js/modules/agent.js`）。
6. **网关兼容的缓存字段解析**（`core/loop/openai.rs::parse_usage`）：
   `prompt_tokens_details.cached_tokens`（OpenAI）/ `prompt_cache_hit_tokens`（DeepSeek）/
   `cached_tokens` / `cache_read_input_tokens` / `cache_creation_input_tokens` 多形状兜底；
   网关未上报 `total_tokens` 时用 `prompt + completion` 兜底（cached 已含在 prompt 内，不重复加）。
7. **可观测**：每个 step 两条日志
   - `[loop] ttft turn/step/ms`（建连 + 预填充 + 首 token）；
   - `[loop] usage turn/step/prompt/cached/hit/completion/total`（命中率 = cached / prompt）。
   这两条是验证"前缀缓存是否生效"的唯一可靠信号（对照 `cached_input_tokens`）。

**边界（已知限制，非缺陷）**

- 单轮内（多 step）：前缀命中到 history 末尾。
- 跨轮：命中到「上一轮上下文之前」——前端每轮用自身消息重建会话，本轮的上下文消息不会回流；
  要拿到完整跨轮命中需要"服务端会话 + 只发增量"（P2 项，见 §8）。
- 技能在轮内被激活时 `active_tools` 会变（tools 段在 messages 之前）→ 该 step 起前缀失效，
  属语义必需，不优化。

---

## 6. 验收

**自动**

```powershell
cd tauri/src-tauri
cargo check --lib          # 通过（仅既有 git.rs dead_code 告警，与本次改动无关）
cargo test --lib           # 585 passed; 0 failed
```

新增单测（均已通过）：

- `core/loop/tool.rs::canonical_call_key_ignores_object_key_order`（键序不影响复用键）
- `core/loop/tool.rs::canonical_call_key_falls_back_for_invalid_json`
- `core/loop/loop.rs::duplicate_tool_call_replays_previous_result_without_reexecuting`
  （同参数二次调用：工具只执行 1 次、两次都有结果回填、第二次带"未重复执行"、不是错误）
- `core/http.rs::shared_client_builds_request_with_per_request_timeout`（共享 client 冒烟）
- `core/loop/loop.rs::system_and_turn_context_stay_stable_across_steps`
  （system 与上下文消息在两个 step 之间逐字节一致）
- `core/loop/loop.rs::turn_context_is_consumed_once`（上下文不泄漏到下一轮，时间块每轮仍注入）
- `core/loop/types.rs::usage_accumulates_across_steps_without_double_counting`（累加与命中率）
- `core/loop/openai.rs::parse_usage_reads_cache_fields_across_gateways`（各网关字段兜底）

**手工（建议按序做一次）**

1. 问「查看一下知识库 git 最近 10 条提交记录」：
   - 日志应出现 `[loop] 同参数工具调用复用上次结果（未重复执行）`；
   - 不应再出现 `[loop_guard] 熔断重复工具调用`；
   - 该轮 step 数应比改造前少 1–2。
2. 前端工具轨迹：一次调用只出现一张卡片（改造前是成对重复）。
3. 连接复用：同一轮多 step 期间，`netstat -ano | findstr <端点IP>` 的 ESTABLISHED 不应随 step 增长。
4. **前缀缓存**：同一会话连问 3 轮，看 `[loop] usage … hit=xx%`：
   - 第 1 轮（冷）hit 应为 0% 或很低；
   - 第 2、3 轮 hit 应明显上升（网关支持自动缓存时通常 ≥50%）；
   - 同时 `[loop] ttft` 应随命中率上升而下降。
5. **token 总数**：多 step 一轮结束后，`RagDone.prompt_tokens` 应≈各 step 之和
   （日志里 `[loop] usage` 的 prompt 逐条相加），而前端上下文占用率用 `context_tokens`
   显示的是单步规模，不应随 step 数变大。

---

## 7. 回滚

四处互不依赖，可分别回滚：

| 项 | 回滚点 |
|---|---|
| P0-3 | `core/loop/openai.rs`、`services/anthropic.rs` 恢复 `http_client()`；`core/http.rs` 可保留（无引用即不参与编译路径） |
| P0-7 | `core/loop/loop.rs` 删除 `replay_cache` 分支，恢复直接调 `execute_tool_calls` |
| P0-6 | `BusToolEventSink` 去掉两个 `HashSet`；`record_tool_call` 恢复自造 ID |
| P0-2 | `commands/llm.rs` 把 turn context 拼回 system；`loop.rs::assemble_request` 恢复时间块/预算提醒进 system；`RagDone` 去掉 `context_tokens`（前端 `|| promptTokens` 已兼容） |

---

## 8. 未做（下一批）

- P0-1 上下文分级预算、P0-4 首字前链路瘦身、P0-5 落库异步化。
- P1：工具结果摘要化、调度器滚动池/exclusive 语义/审批合批、检索缓存与 ONNX session 池、
  流式 IPC 合批与前端增量渲染。
- P2：服务端会话（只发增量）——这是拿到**完整跨轮**前缀命中的前提。
