# Experience Graph 事件采集源 —— 后续实施计划（Phase 4.3）

> 状态：**规划中（未实施）**。本文档是「Experience 事件采集源」后续阶段的完整操作手册，
> 供后续开发/AI 直接按步骤执行。当前已完成链路见 §1 基线。

---

## 0. 一句话目标

让 Experience Brain 真正"自己长经验"：**Git 提交（含 Agent 提交与手动提交）与 AI 操作完成后自动落库**，
事件经已有的 LLM 富化（`LlmExperienceExtractor`）自动抽取 Problem/Solution，沉淀为可检索的经验图。

---

## 1. 当前基线（已完成，勿重复实现）

| 链路 | 代码入口 | 说明 |
|---|---|---|
| 事件模型 | `tauri/src-tauri/src/core/graph/experience.rs` | `ExperienceEvent{id, source(GitCommit/AiOperation/ChatMessage), title, body, file_path, created_at}`；`EventSource` serde snake_case |
| 写图（规则） | `ExperienceBrain::record()` → `record_extracted()` | 同步、事务原子；P/S 节点 + SOLVED_BY / IMPLEMENTED_IN / VALIDATED_BY 边 |
| 写图（LLM 富化） | `ExperienceBrain::record_extracted()` + `ai::LlmExperienceExtractor` | 正文截断 2000 字符；LLM 失败自动规则降级（fail-open） |
| 引擎入口 | `GraphEngine::experience_record()`（同步）/ `experience_record_ai()`（异步，锁外 LLM 抽取） | `mod.rs` |
| 命令 | `graph_experience_record` / `graph_experience_search` / `graph_experience_events` | `commands/graph.rs`；前端 `graph-api.js` 三个方法已就绪 |
| 前端入口 | 图谱侧栏「AI 分析 → 经验」 | 描述问题 → 匹配历史 P/S → 点击打开关联文档 |
| 事件存储 | `graph_properties` 表 `exp:{id}` 键（append-only，id 主键幂等） | `ExperienceBrain::record_extracted` 内 `set_property` |

**缺的正是采集源**：目前没有任何代码主动调用 `experience_record`——事件只能靠外部手动调命令写入。

---

## 2. 总体方案与实施顺序

```text
任务 A：Git 提交采集（Agent 提交挂点 + 手动提交挂点）   ← 先做，改动最小
任务 B：Git 历史增量轮询（覆盖 IDE/命令行手动提交）     ← 次做，需要后台任务
任务 C：Agent 操作自动落库（会话结束汇总）             ← 价值最高，改动中等
任务 D：对话历史采集（ChatMessage，可选）               ← 最后，低优先级
```

通用约束（全部任务遵守）：
- **幂等**：事件 id 必须稳定可重放（git 用 commit hash，agent 用 request_id），重复写入靠
  `exp:{id}` 键幂等（`set_property` 覆盖写，天然幂等）；agent 会话结束后端到端只落一次。
- **成本控制**（PRD §75-76）：LLM 富化只在 `graph_experience_record`（已配置 LLM）自动触发；
  采集源**不直接调 LLM**，只负责构造事件并落库，富化交给命令层既有逻辑。
- **噪声过滤**：纯 chore/合并提交（无 problem/solution）→ `record_extracted` 空 P/S 分支已处理
  （仅存事件不建图），无需采集端额外过滤；但可选的轻量过滤见 §3.4。
- **不阻塞**：采集是 best-effort 异步（`tokio::spawn`），失败只 log，绝不影响 git_commit / agent 主流程。

---

## 3. 任务 A：Git 提交采集（Agent 提交 + 手动提交挂点）

### 3.1 现状

- Agent 提交路径：`core/agent/loop_tools.rs` `GitCommitTool::execute`（约 2271-2287 行）
  调 `super::tools::git_commit(&self.cfg.dir_path, &message)`，成功/失败都经
  `ctx.sink.on_result(...)` 上报。
- 手动/前端提交路径：`commands/git.rs::git_commit`（约 268 行起，IPC 命令 `git_commit`，
  已注册于 `lib.rs` invoke_handler）。

### 3.2 步骤 A1：封装"提交 → 经验事件"工具函数（两处共用）

在 `core/graph/experience.rs` 或 `commands/graph.rs` 新增（建议放 experience.rs，模块内聚）：

```rust
/// 由一次 git 提交构造经验事件（source=GitCommit）。
/// - id = `git:{hash}`（幂等：重复采集同一提交只覆盖写同一键）
/// - title = commit subject（首行）；body = commit body 全文
/// - file_path = 变更文件之一（git show --name-only 首行；失败则 None）
/// - created_at = 提交时间戳（秒→毫秒；失败用 now）
pub fn event_from_git_commit(dir_path: &str, hash: &str) -> ExperienceEvent
```

实现要点：
- 取 subject：`run_git_utf8(&["log", "-1", "--format=%s", hash], dir)`；
- 取 body：`--format=%b`；取时间：`--format=%ct`（unix 秒 ×1000）；
- 取文件名：`git show --name-only --format= hash` 首行（截断规范化 `\` → `/`）；
- **不在这里做 LLM 富化**（交给命令层）。

### 3.3 步骤 A2：Agent 提交挂点（loop_tools.rs）

`GitCommitTool::execute` 的 `Ok(text)` 分支，在 `on_result(...)` 之后追加：

```rust
Ok(text) => {
    ctx.sink.on_result(ctx.call_id, "git_commit", true, &text, Some(&text));
    // Phase 4.3：提交成功 → 异步落经验事件（best-effort，失败仅 log）
    let dir = self.cfg.dir_path.clone();
    let graph = /* 注入的 Arc<GraphEngine>，见下 */;
    tokio::spawn(async move {
        // 取 HEAD hash（git_commit 已确认 HEAD subject 与 message 一致）
        if let Ok(hash) = git_head_hash(&dir).await {
            let event = event_from_git_commit(&dir, &hash);
            if let Err(e) = graph.experience_record_ai(&dir, &event, None).await {
                log::warn!("[graph] git 提交经验落库失败: {}", e);
            }
        }
    });
    Ok(Value::String(text))
}
```

依赖注入：
- `GraphEngine` 需传入 `GitCommitTool`。当前 `GitCommitTool::new(cfg)` 只有
  `KbSearchConfig`；在 `build_loop_tool_registry`（loop_tools.rs 约 766 行
  `reg.register(Arc::new(GitCommitTool::new(cfg.clone())))`）处改为同时传入
  `Arc<GraphEngine>`（`lib.rs` setup 中 `app.state::<AppState>().graph_engine.clone()` 可得）。
- 无 LLM 时调 `experience_record_ai(..., None)` → 规则降级，与现状一致。

### 3.4 步骤 A3：手动/前端提交挂点（commands/git.rs）

`commands/git.rs::git_commit` 成功后（返回前）追加同样逻辑：
- 该函数同步返回 `Result<Vec<u8>, String>`，落库用 `tauri::async_runtime::spawn` 包
  `spawn_blocking`（git 命令本身同步）；或者接受事件写入的少量延迟直接同步写。
- 需要 `AppState` 或注入 `Arc<GraphEngine>`：`commands/git.rs` 当前是纯函数式实现，
  建议给 `git_commit` 增加 `engine: Option<Arc<GraphEngine>>` 参数（命令层从
  `app.state::<AppState>()` 传入；工具层传 None 不采集，避免重复——A2 已覆盖 Agent 路径）。

> 注意：A2 与 A3 若同时启用会对同一提交各落一次 → id 相同（`git:{hash}`）→
> `exp:{id}` 覆盖写幂等，无重复图节点。可放心双挂。

### 3.5 验收标准（A）

1. Agent 执行 `git_commit` 成功后，`graph_properties` 出现 `exp:git:<hash>`；
2. `graph_experience_events` 返回该事件（source=git_commit）；`graph_experience_search`
   能按关键词命中（规则打分 ≥ 0）；
3. 手动 `git_commit` IPC 同样落库；
4. 同一提交重复提交/重复采集不产生重复 P/S 节点（幂等）；
5. `cargo test --lib graph` 全绿；新增测试：`event_from_git_commit` 字段解析（可用
   `tempfile` 目录 + `git init` 造真实提交，或 mock run_git 输出）。

---

## 4. 任务 B：Git 历史增量轮询（覆盖 IDE / 命令行手动提交）

### 4.1 背景

任务 A 只覆盖"经应用提交"的 commit（Agent 工具 + 前端命令）。用户在 IDE / 终端手动
commit 不会触发。任务 B 用后台轮询补全：定期扫描各知识库目录的 `git log` 增量。

### 4.2 设计

- **存储水位**：`graph_properties` 键 `exp_git_watermark:{dir}` = 已采集的最新 commit hash。
- **轮询器**：仿照既有后台循环（`worker.rs spawn_ai_worker` 模式：`tokio::time::interval`
  + 每轮 `run_once`），新增 `spawn_git_exp_worker(app)`，在 `lib.rs` setup 与
  `spawn_ai_worker` 并列启动。周期建议 60s（git log 轻量）。
- **增量逻辑**（每目录）：
  1. `git rev-parse HEAD` → 当前 HEAD hash；与水位相同 → 跳过；
  2. `git log --format=%H <watermark>..HEAD`（首轮无水位 → `git log -n 20` 回溯最近 20 条）；
  3. 逆序（旧→新）逐条 `event_from_git_commit` + `experience_record_ai(..., None)`；
  4. 全部成功后水位 = HEAD hash。
- **降级**：目录非 git 仓库（`rev-parse` 失败）→ 跳过且不记录水位；
  单条失败 → log 并继续（不中断整批）。

### 4.3 涉及文件

- 新增 `core/graph/git_capture.rs`（或并入 `core/graph/worker.rs`）：
  `pub fn spawn_git_exp_worker(app: AppHandle)` + `run_once`；
- `mod.rs`：`GraphEngine` 增 `git_watermark(dir) -> Option<String>` /
  `set_git_watermark(dir, hash)`（转 `get_property`/`set_property`，键 `exp_git_watermark:{dir}`）；
- `lib.rs`：setup 中调用 `crate::core::graph::git_capture::spawn_git_exp_worker(app.handle().clone())`。

### 4.4 验收标准（B）

1. 非应用提交（模拟 `git commit` 在命令行完成）在 ≤2 个轮询周期内出现在经验事件里；
2. 重启应用不重放已采集提交（水位持久化）；首次运行回溯最近 20 条；
3. 非 git 目录不报错、不写水位；`cargo test --lib graph` 全绿（新增水位读写 + 增量选择测试）。

---

## 5. 任务 C：Agent 操作自动落库（ai_operation）

### 5.1 背景与价值

用户每次让 Agent 干活（改 bug、写功能、做调研）都是一次"问题→解决"经验。任务 C 在
**Agent 会话结束时**把本次操作汇总为 `ExperienceEvent{source: AiOperation}` 自动落库，
LLM 富化自动抽 P/S——这是"经验图自己长经验"的核心闭环。

### 5.2 采集内容

- `id`：`agent:{request_id}`（幂等；request_id 已是会话唯一键）；
- `title`：用户本次请求的前 60 字符（截断）；
- `body`：结构化汇总——工具调用序列（`edit/write/multi_edit/delete/git_commit` 及成功
  摘要各一行）+ 最终答复前 500 字符；
- `file_path`：本次实际改动文件之一（edit/write 目标路径；多个取首个或 None）；
- `created_at`：会话结束时间戳。

### 5.3 挂点（三选一，推荐 ①）

1. **① Agent 循环结束处（推荐）**：`commands/llm.rs` 的 agent 执行入口（约 2120-2136 行：
   `build_loop_tool_registry` + `agent.set_sink(...)` 所在函数）——在循环返回最终结果处
   （拿到 request_id / 工具调用记录 / 最终答复后）`tokio::spawn` 落库。
   需要把 `Arc<GraphEngine>` 传入该函数（命令层从 `AppState` 取）。
2. **② 工具结果聚合处**：`core/agent/tools/mod.rs` `record_tool_result_structured`（约 261 行）
   已有每工具调用记录——但不含"会话结束"边界，需额外维护会话聚合，改动更大。
3. **③ BusToolEventSink**（loop_tools.rs 约 2694 行）：同理需会话边界，不推荐首选。

> 采集端只组装事件文本；**不调 LLM**。富化由 `graph_experience_record` 命令层自动完成
> （已配置 LLM 时）。若想绕过命令层直接写，可调 `experience_record_ai(dir, &event, Some(&extractor))`，
> 但注意 `LlmExperienceExtractor` 需要 `Arc<dyn GraphLlm>`——命令层已封装，尽量复用命令。

### 5.4 噪声与去重

- 只读会话（未调用任何写工具、最终答复非"完成改动"语义）→ 可不落库（可选开关，
  默认落库，靠空 P/S 分支自然过滤）；
- 同一 request_id 重放（重试/续跑）→ `exp:agent:{request_id}` 覆盖写幂等。

### 5.5 验收标准（C）

1. 一次完整 Agent 会话（含 1+ 次 edit/write）结束后，经验事件出现（source=ai_operation），
   `graph_experience_search` 可命中该会话主题；
2. LLM 已配置时事件 P/S 为 LLM 抽取结果（非规则 raw 标题）；
3. 只读会话不产生 P/S 节点（空 P/S 分支）；
4. `cargo test --lib graph` 全绿；新增测试：body 汇总格式（工具行 + 答复截断）。

---

## 6. 任务 D：对话历史采集（ChatMessage，可选）

- `EventSource::ChatMessage` 已定义未使用。挂点：聊天命令完成处，`id = chat:{conversation_id}:{seq}`，
  title = 用户提问截断，body = 用户提问 + AI 答复，file_path = None。
- 低优先级：聊天流已有 `ai_history_stores` 持久化，经验图采集会重复存储——仅当产品上
  "从对话沉淀经验"有明确诉求时再实施。**默认不做**，本文档仅记录方案备查。

---

## 7. 建议实施顺序与依赖

```text
A1 → A2 → A3（任务 A，1 个 PR 内完成）
  ↓
B（依赖 A 的 event_from_git_commit；worker 模式参考 core/graph/worker.rs）
  ↓
C（依赖 experience_record_ai；挂点 ①）
  ↓
D（可选，暂缓）
```

- A、B、C 互不阻塞，可并行开发（都只依赖 §1 基线）；
- 每步完成都跑：`cargo check`（0 警告）、`cargo test --lib`（全绿）、
  `node --check css_js/graph/*.js`（如动前端）、dist 同步校验（如动前端）。

---

## 8. 风险与注意事项

1. **git 命令代价**：任务 B 每 60s 每目录跑 2-3 条 git 命令，目录多时聚合执行；
   水位查询先 `rev-parse HEAD` 短路，无新提交零 git log 开销。
2. **LLM 成本**：采集源绝不直接调 LLM；富化只在用户显式走
   `graph_experience_record`（已配 LLM）时发生——若希望"采集即富化"，需在 worker 内
   构建 `LlmExperienceExtractor`（参考 `commands/graph.rs` 的
   `build_graph_llm` + `graph_llm_configured`），并加每周期上限（仿 `worker.rs` 节流）。
3. **多知识库隔离**：事件带 `dir_path` 经 `exp:{id}` 键存储于各知识库自己的
   `mdgo.db`，跨库天然隔离，无需额外处理。
4. **`is_mdgo_internal` 防护**：git 相关采集不触碰 `.mdgo` 目录（tools/mod.rs 已有防护，
   采集只读 commit 元数据，不受影响）。
5. **不阻塞主流程**：所有采集 `tokio::spawn` + best-effort；`GraphStore` 锁由
   `experience_record_ai` 内部管理（锁外 LLM、锁内写图），无死锁风险。

---

## 9. 关联文档

- `docs/下一代 AI 知识图谱 PRD.md` §55（未来 Experience Graph）/ §56（Agent 使用图谱）
- `tauri/src-tauri/src/core/graph/experience.rs`（事件模型 + 写图 + 检索，基线）
- `tauri/src-tauri/src/core/graph/ai.rs`（`LlmExperienceExtractor`，LLM 富化）
- `tauri/src-tauri/src/core/graph/worker.rs`（后台 worker 模式参考）
- `tauri/src-tauri/src/commands/graph.rs`（`graph_experience_record` 命令层富化入口）
- `tauri/src-tauri/src/core/agent/loop_tools.rs`（`GitCommitTool`，任务 A 挂点）
- `tauri/src-tauri/src/commands/git.rs`（`git_commit`，任务 A 手动挂点）
