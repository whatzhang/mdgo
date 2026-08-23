# mdgo 知识图谱：Trunk 构建流水线与 AI 智能管线 —— 技术评审文档

> 版本：基于当前代码（`tauri/src-tauri`）评审
> 读者：**开发人员**（后续开发/重构依据）与**产品经理**（功能边界与演进判断）
> 触发：产品反馈「已正确配置 LLM，但后台 Worker 未调用 LLM 分析，图谱仍以目录层级为主题」
> 文档性质：技术实现全解 + 问题根因诊断 + 缺陷清单（§4-§6 为 Code Review 结论）

---

## 1. 产品视角：这条流水线在做什么

mdgo 知识图谱的目标不是"把文件画成图"，而是把用户的文件、知识、概念、经验与 AI 推理
统一成可持续演进的本地知识世界（PRD 结尾定义）。

**当前已落地的能力边界**（按演进路线）：

| 阶段 | 能力 | 现状 |
|---|---|---|
| L0 | 文档关系图（目录树 + 文件 + 引用） | ✅ 默认产物（用户当前所见） |
| L1 | 内容层（章节/语义块 + 相似边） | ✅ 构建时自动 |
| L2 | 语义聚类（替代目录聚类） | ⚠️ 依赖本地 Embedding 模型就绪，**当前未生效**（§5.1） |
| L3 | AI 实体/关系抽取 | ⚠️ 手动入口可用；后台自动抽取**依赖队列 + LLM**，**当前未生效**（§5.2） |
| L4 | GraphRAG 问答 + 经验库 | ✅ 入口可用（依赖 LLM） |

> 一句话结论：**"智能解析"不是没做，而是被两道开关卡住了——本地 Embedding 模型未就绪（语义聚类不触发）与 Worker 的 LLM 调用链路存在断点（自动抽取不触发）。** 详见 §5。

---

## 2. 系统架构总览

```text
┌─ 前端（vanilla JS iframe，图谱视图）─────────────────────────────┐
│  侧栏 15 个 AI 动作 · 5 视图 · LOD 缩放 · 节点详情 · 问答/经验     │
└───────────────┬──────────────────────────────────────────────────┘
                │ Tauri invoke（graph_* 命令，commands/graph.rs）
┌───────────────▼──────────────────────────────────────────────────┐
│ 命令层：graph_status/overview/clusters/ai_extract/query/...       │
└───────────────┬──────────────────────────────────────────────────┘
┌───────────────▼──────────────────────────────────────────────────┐
│ GraphEngine 门面（core/graph/mod.rs）                              │
│   store 缓存 · 构建调度（build_all/build_file）· 队列 · 查询        │
├───────────────────────────────────────────────────────────────────┤
│ 存储层 storage.rs    │ 构建层 builder/chunk   │ AI 层 ai/worker    │
│ SQLite 图存储        │ Document Graph/内容层  │ 抽取/问答/后台任务  │
│ LanceDB 向量         │ 聚类 cluster           │ 规则抽取 extractor  │
└───────────────────────────────────────────────────────────────────┘
```

**两条主链路：**

```text
【Trunk 构建流水线】（用户索引/文件变化触发，同步）
  索引完成 ──► build_all ──► Document Graph ──► Chunk 内容层 ──► 规则实体抽取
      │                        （builder）        （chunk）        （extractor）
      └──► 聚类（cluster：目录 或 语义 embedding）
      └──► AI 队列入队（graph_ai_queue，按重要度）

【后台智能管线】（worker 消费队列，异步、周期）
  每 30s ──► 空闲守卫 ──► 取队 ≤3 ──► 规则抽取（全部）──► LLM 抽取（前 2 条，可选）
      └──► 收尾（done / 重试 / failed）＋ 指标（graph_metrics）
```

---

## 3. Trunk 构建流水线 —— 逐阶段技术实现

### 3.1 触发层（索引器联动，`core/indexer.rs`）

| 触发 | 调用链 | 图联动 |
|---|---|---|
| 全量索引（前端「索引」按钮） | `kb_index`（commands/knowledge.rs:52）→ `indexer.index_all`（:353）→ 收尾 `sync_graph_all`（:525） | `GraphEngine::build_all`（mod.rs:87） |
| 单文件保存/新增（watcher） | `indexer.index_file`（:568 `sync_graph_file`） | `GraphEngine::build_file`（mod.rs:169） |
| 文件删除 | `sync_graph_remove`（:748） | `GraphEngine::remove_path`（生命周期级联清边） |

图引擎注入：`lib.rs:474` `indexer.set_graph_engine(graph_engine.clone())`。

### 3.2 Document Graph 构建（`core/graph/builder.rs`）

`GraphBuilder`（持 `&GraphStore`，同步、锁内）：

- **`build_all`**（:216）= `clear()`（清空全部图数据，含 AI 候选/收藏/队列）+ `build_incremental`
- **`build_incremental`**（:223）三步：
  1. `collect_tree` 扫描目录树 → **folder 节点** + `CONTAINS`（folder→folder 父目录边）
  2. **doc 节点**（meta 存扩展名）+ `CONTAINS`（folder→doc）
  3. **`REFERENCES`**（doc→doc）：仅 Markdown（`is_linkable`），解析 `[[wikilink]]`/`[text](path)`
     内链 → `resolve_link` 相对路径匹配 → 去自环；未建节点的目标跳过（增量场景）
- **`build_file`**（:331）：单文档节点 + 重写该文档全部 REFERENCES 出边（先删后写，幂等）

节点 id 规范：`doc:{path}` / `folder:{dir}`（`node_id_for`，`\` 归一为 `/`）。

### 3.3 Chunk 内容层（`core/graph/chunk.rs`，知识图谱底座 Layer 1）

`ChunkGraphBuilder`（`build_all` / `build_file` → `build_doc`）：

1. 清理旧内容节点（`delete_content_nodes_for_doc`，幂等）
2. **AST 分块**：Markdown → `ComrakMarkdownParser` 解析 → `SemanticChunkEngine::new(800, 100, 1.25, 50)`，
   按标题路径产出 `Chunk{text, path(标题链), chunk_type}`；非 Markdown → 整篇单段落兜底分块
3. 写 **section 节点**（标题路径）+ **chunk 节点** + `CONTAINS` 层级边
4. 代码文件（JS/TS/Python/Rust/Java/Go/C++）解析 **import 依赖 → `IMPORTS` 边**（`extract_code_imports`，
   文件名 stem 匹配解析目标）

id 规范：`chunk:{doc}#{idx}`（确定性）/ `section:{doc}#{标题链}`。内容存 `graph_nodes.content` 列（Schema V4）。

### 3.4 规则实体抽取（`core/graph/extractor.rs`，Level 1 免费）

- **`extract_all_docs`**：遍历 doc 节点，`rule_candidates` 提取**外部链接 host**（`https://redis.io` → `entity:redis`），
  `EntityMerger::upsert_entity` 消歧合并（别名 + 规范化名，`merger.rs`）
- 构建流水线在 `build_all`/`build_incremental` 内自动执行（20,000 文档上限）；失败仅告警
- **Level 3 LLM 抽取不在构建内**——由后台 Worker / 手动「抽取」按钮执行（§3.7/§4）

### 3.5 聚类（`core/graph/cluster.rs`）

| 模式 | 算法 | 触发 | 产物 |
|---|---|---|---|
| `directory`（默认） | `rebuild`：目录前缀分组 → 全局兜底簇 → 成员多数投票 → 确定性选心（度降序，id 升序 tiebreak）→ 簇间跨簇边聚合 | 每次 build_all/incremental 自动 | `graph_clusters` + `graph_cluster_members` |
| `embedding`（语义） | `rebuild_from_embeddings`：doc 概览向量（前 300 字符）→ 贪婪在线聚类（余弦 ≥ 0.60，簇心在线均值） | `GraphEngine::embed_clusters`（手动）/ build_all 自动尝试（条件见下） | 替换目录簇 |

**语义聚类默认化开关**（`build_all` 内，mod.rs）：
```rust
if is_model_ready() && guard.get_property("graph_auto_semantic_done")?.is_none() {
    // 本地 BGE 模型就绪 且 本知识库从未尝试过 → 尝试一次（成功后 cluster_mode=embedding）
}
```
`graph_cluster_mode` 属性记录当前模式；前端按此展示（默认目录聚类 = 用户当前所见）。

### 3.6 AI 队列入队（`core/graph/storage.rs` → `graph_ai_queue` 表）

- **表结构**：`(id, dir_path, rel_path, importance, status, attempts, created_at, updated_at)`，
  `UNIQUE(dir_path, rel_path)`，索引 `(status, importance DESC)`（Schema V4，CREATE IF NOT EXISTS）
- **入队时机**：
  - `build_all` / `build_incremental` 末尾 → `enqueue_after_build`：`ai_priority_docs` 全量按重要度入队（`reset_done=false`，已完成项不重复）
  - `build_file` → 单文档入队（`reset_done=true`，内容变更强制重抽）
- **重要度评分**（`ai_priority_docs`，0..1）：`0.5·度归一化 + 0.3·新鲜度(90天线性) + 0.2·文件名启发式`
  （README/设计/方案/架构/总结/指南/guide/design）
- **状态机**：`pending → processing → done / failed`（attempts 上限 3）；卡死 >90s 的 processing 项自动重置 pending
- 入队/取队/收尾均事务原子（BEGIN IMMEDIATE + COMMIT/ROLLBACK）

### 3.7 后台 Worker（`core/graph/worker.rs`，异步消费）

- **生命周期**：`lib.rs:522` setup 中 `spawn_ai_worker` 启动一次 → 延迟 20s → 每 30s 一轮
- **每轮**（`run_once`）：
  1. `active_dirs()` = 所有打开过 store 的知识库
  2. **空闲守卫**：全库无 pending/processing 项 → 直接返回（不构建 LLM 客户端）
  3. `llm_ready = graph_llm_configured(app)`（内存 `llm_config` 的 endpoint/model 非空）
  4. 配置时构建 LLM 适配器（`build_graph_llm`，**每轮重建一次**，配置热更新生效）
  5. 每目录 `process_dir`：取队 ≤3 → 每条「规则抽取（全部）+ LLM 抽取（batch 内前 2 条）」→ `finish_ai_item`
- **可观测性**：`worker_processed` / `worker_failed` / `llm_calls` / `llm_failures` 写入 `graph_metrics`

### 3.8 LLM 调用链（Worker → 模型服务）

```text
worker::process_dir
  └─► GraphAiService::extract_relations（ai.rs）           // 内容截断 4000 字符，最多 10 条
        └─► GraphLlm::json（trait）
              └─► ServicesGraphLlm::json（worker.rs）      // LLMClient 适配
                    └─► LLMClient::complete_json（services/llm.rs:676）
                          └─► complete_text（:723）→ completion_with_retry（指数退避，最大重试）
                                └─► OpenAiAdapter::complete（core/loop/openai.rs:148）
                                      └─► POST {base_url}/chat/completions（Bearer 认证，超时 300s）
```

- 候选产出走状态机：`confidence ≥ 0.9 → auto_confirmed`（自动落正式边）；其余进入「待确认」列表
- **失败路径**：LLM 返回 None → `llm_failures` +1 → `extract_relations` 返回 Ok(0) → **Worker 视为成功并标记 done**（缺陷，见 §6-1）

---

## 4. 手动 LLM 入口（对照参考）

| 入口 | 命令 | 说明 |
|---|---|---|
| 侧栏「抽取」 | `graph_ai_extract`（手动批量，limit=10，高价值文档优先） | 走同一 `build_graph_llm` + `extract_relations` |
| 侧栏「摘要」 | `graph_ai_summarize_clusters` | 簇 description + tags |
| 侧栏「问答」 | `graph_query`（GraphRAG：实体检测 → 图扩展 + 混合检索 → LLM 回答 + chunk 证据） | 依赖 LLM + 本地 Embedding（混合检索） |
| 侧栏「经验」 | `graph_experience_record/search` | 录入/检索经验（LLM 富化 P/S） |

> 验证 LLM 是否真正可用，最直接的办法：点侧栏「抽取」，然后查看 `graph_metrics.llm_calls` 是否增长、
> 「待确认」是否出现 LLM 候选。若手动入口也不增长 → 问题在配置/适配层（§5.2 候选根因 ①③④）。

---

## 5. Code Review 诊断：为什么「配置了 LLM，图谱仍是目录层级」

### 5.1 诊断 A —— 图谱是目录层级 = 语义聚类未生效（与远程 LLM 无关）

**机制**：聚类模式由 `graph_cluster_mode` 属性决定（`directory` / `embedding`）。默认 `directory`。
语义聚类只在下述条件**全部成立**时自动尝试（`build_all` 内）：

```text
is_model_ready() == true          ← 本地 BGE-Small-ZH Embedding 模型已下载并在当前进程加载
  AND graph_auto_semantic_done 未设置
```

**`is_model_ready()` 的实现**（`core/db/utils.rs:336`）：

```rust
pub fn is_model_ready() -> bool {
    MODEL_DIR.get().is_some()   // 进程内 OnceLock，仅 ensure_model_ready() 成功后 set
}
```

**模型加载**：`lib.rs:393` 启动时后台线程 `ensure_model_ready()`（HuggingFace/ModelScope 下载，逐文件，
可回退镜像）。**若下载未完成 / 失败 / 首次运行** → `MODEL_DIR` 为空 → `is_model_ready()=false` →
`build_all` 跳过语义聚类 → **图谱保持目录聚类**（用户所见现象）。

**结论**：用户看到的"目录层级主题图谱"大概率是**本地 Embedding 模型未就绪**，而不是 LLM 配置问题。
验证：日志应有 `[startup] embedding 模型后台预下载失败/已就绪`；或 `graph_cluster_mode` 仍为 `directory`。

### 5.2 诊断 B —— Worker 未调用 LLM 的四个候选根因（需日志/指标确认）

按可能性排序：

**① 队列无待处理项（最常见）—— Worker 空闲守卫直接返回**
- 机制：`run_once` 先查 `queue_stats`，全库无 `pending/processing` 项 → 直接返回，**不构建 LLM 客户端**
- 场景：图谱是**旧构建**（队列全 done 或为空）；用户配置 LLM 后只是打开/刷新图谱——
  刷新走 `graph_related`（只读），**不触发入队** → Worker 无活可干 → "没调 LLM"
- 验证：日志无 `[graph-worker]` 处理记录；`graph_metrics.worker_processed` 无增长
- 触发修复：重新构建图谱（`kb_index`）或修改任一文件（`build_file` 重新入队）

**② 内存 LLM 配置为空 —— `graph_llm_configured()` 返回 false**
- 机制：Worker 读 `AppState.llm_config`（`RwLock<LlmConfig>`，启动时为 `LlmConfig::default()` 空值）。
  该内存值只由 `kb_update_llm_config` / `kb_save_setting` 写入；**重启后依赖前端回填**：
  `main.html loadSetting() → syncLlmConfigToBackend() → kb_update_llm_config`
- 风险：若前端 `loadSetting` 失败（文件缺失/解析失败）、`currentRootPath` 为空、或 `.catch` 静默吞错
  → 内存保持空 → `graph_llm_configured=false` → Worker 全程只做规则抽取，**LLM 一次都不调**
- 验证：日志应出现 `[graph] LLM 适配器构建失败，AI 操作降级`（若构建失败）；或手动「抽取」也不增长 llm_calls

**③ LLM 调用失败被静默吞掉 —— 无告警、无重试**
- 机制：`extract_relations`（ai.rs）中 `llm.json(...)` 返回 None → 仅 `llm_failures`+1 → `return Ok(0)`。
  Worker 收到 Ok(0) → `ok=true` → 项标记 **done**（不重试！）
- 场景：endpoint 拼写/模型名/认证错、协议不兼容、网络不通 → 每次调用失败但流水线"假装成功"
- 验证：`graph_metrics.llm_failures` 持续增长、`llm_calls` 不增长、「待确认」只有规则候选

**④ `LlmConfig.protocol=anthropic` 恒不生效（代码缺陷）**
- 机制：`llm_client_for_cfg`（lib.rs:293）与 `LLMClient::new`（services/llm.rs:267）**均不接收 protocol**，
  永远构建 `OpenAiAdapter`（OpenAI Chat Completions 格式）
- 场景：用户按 Anthropic 协议配置 → 请求以 OpenAI 格式发往 Anthropic 端点 → 4xx → 重试耗尽 → None → 同③
- 验证：setting.json 中 `localLlmProtocol="anthropic"` 且 endpoint 非 OpenAI 兼容

### 5.3 诊断结论汇总

| # | 现象 | 根因 | 严重度 |
|---|---|---|---|
| A | 图谱目录层级 | 本地 Embedding 模型未就绪（MODEL_DIR 空）→ 语义聚类跳过 | 中（产品预期差） |
| B-① | Worker 不调 LLM | 队列无 pending（旧构建/未重新入队），空闲守卫返回 | 高（最常见） |
| B-② | Worker 不调 LLM | 内存 llm_config 为空（前端回填失败） | 高 |
| B-③ | LLM 调了但静默失败 | extract_relations 吞错 + Worker 误标 done | 中（掩盖问题） |
| B-④ | LLM 必失败 | protocol 未接入 LLMClient（恒 OpenAI 适配器） | 中（Anthropic 用户） |

---

## 6. 缺陷清单（Code Review 发现，按严重度）—— 修复状态见右列

| ID | 严重度 | 位置 | 问题 | 修复状态 |
|---|---|---|---|---|
| D1 | 高 | worker.rs `process_dir` + ai.rs `extract_relations` | **LLM 失败静默**：失败返回 Ok(0) 被 Worker 当成功，项标记 done 不重试；`llm_failures` 是唯一痕迹 | ✅ 已修复：`GraphLlm::is_null()` 区分未配置/失败；失败返回 Err → Worker `ok=false` 有界重试 + 告警日志；未配置不计 `llm_failures` |
| D2 | 高 | lib.rs `llm_client_for_cfg` / services/llm.rs `LLMClient::new` | **protocol 未接入**：anthropic 配置恒走 OpenAI 适配器，必失败 | ✅ 已修复：`LLMClient::new` 按 protocol 经 LlmAdapter seam 选择（复用 `core::loop::AnthropicAdapter`）；指纹缓存含 protocol；测试覆盖三协议构建 |
| D3 | 中 | main.html `syncLlmConfigToBackend` | 启动回填 LLM 配置到后端内存，失败被 `.catch` 静默，Worker 侧无感知 | ✅ 已修复（后端主导）：`kb_load_setting` 内存为空时从 setting.json 回填 `LlmConfig`，前端只需 loadSetting 一次即生效 |
| D4 | 中 | worker.rs `run_once` | **空闲守卫 + 队列幂等**导致"配置 LLM 后不重建就不抽取"的产品盲区 | ✅ 已修复：新增 `graph_ai_enqueue_all` 命令 + 侧栏「重新分析」按钮（全库重排队，done 不重复、failed 重试） |
| D5 | 低 | worker.rs `run_once` | **成功路径无日志**：不构建 LLM 客户端/处理几条/抽到几条均无日志，排查困难 | ✅ 已修复：每轮摘要日志 `[graph-worker] 本轮完成: active_dirs/processed/llm_ready` |
| D6 | 低 | mod.rs `build_all` | 语义聚类默认化**失败无显式用户提示**（仅 log），`graph_cluster_mode` 静默回退 directory | ✅ 已修复（重试策略）：仅成功置位 `graph_auto_semantic_done`——失败后模型就绪的下次构建自动重试；失败日志含"下次构建自动重试"说明 |

> 另：前端侧栏按钮"点击无反应"问题已做防御性修复——AI 分析动作与布局模式改 **document 级事件委托**（容器被重建不失效）、`runAiAction` 目录缺失时**显式提示**（不再静默 return）、`init()` 单点异常不中断整页绑定。

---

## 7. 修复/改进路线（开发）—— 状态见右列

| 阶段 | 项 | 状态 |
|---|---|---|
| 短期止血 | D1 LLM 失败语义分离 + Worker 有界重试 + 告警 | ✅ 已修复 |
| 短期止血 | D2 protocol 接入（复用 AnthropicAdapter） | ✅ 已修复 |
| 短期止血 | D3 后端启动自加载 LLM 配置（kb_load_setting 回填） | ✅ 已修复 |
| 短期止血 | D4 「重新分析」入口（命令 + 侧栏按钮） | ✅ 已修复 |
| 中期闭环 | D5 Worker 每轮摘要日志 | ✅ 已修复 |
| 中期闭环 | D6 语义聚类失败自动重试 | ✅ 已修复 |
| 中期闭环 | 前端 cluster_mode + 模型状态展示（下载进度/失败重试） | ⏳ 待做（依赖主界面 UI） |
| 远期 | 经验事件采集源自动化（Git 提交 / Agent 操作） | ⏳ 规划中（`docs/experience-graph-collection-plan.md`） |

---

## 8. 测试与验证基线（回归依据）

| 项 | 命令/方法 | 预期 |
|---|---|---|
| 后端全量 | `cargo test --lib` | 310 通过 / 0 失败 |
| 图谱模块 | `cargo test --lib graph` | 38 通过 / 0 失败 |
| 编译 | `cargo check` | 0 error / 0 warning |
| 前端语法 | `node --check css_js\graph\*.js` | 全部通过 |
| dist 同步 | 哈希比对 `css_js/graph/` ↔ `tauri/dist/css_js/graph/` | 一致 |
| LLM 链路验证 | 侧栏「抽取」→ `graph_metrics` | `llm_calls` 增长、「待确认」出现 LLM 候选 |
| 语义聚类验证 | 日志 `[startup] embedding 模型已就绪` → 重建图谱 | `graph_cluster_mode=embedding`，前端展示语义簇 |
| Worker 验证 | 构建后等 1-2 轮（30-60s） | 日志 `[graph-worker]`；`worker_processed` 增长 |

详细测试用例：`docs/graph-intelligence-test-cases.md`（测试人员手册）。

---

## 9. 附录：关键文件与配置索引

| 关注点 | 文件 |
|---|---|
| 构建主链路 | `tauri/src-tauri/src/core/graph/mod.rs`（build_all/build_file/enqueue） |
| Document Graph | `tauri/src-tauri/src/core/graph/builder.rs` |
| 内容层 | `tauri/src-tauri/src/core/graph/chunk.rs` |
| 聚类 | `tauri/src-tauri/src/core/graph/cluster.rs` |
| 规则抽取 | `tauri/src-tauri/src/core/graph/extractor.rs` / `merger.rs` |
| AI 服务 | `tauri/src-tauri/src/core/graph/ai.rs`（extract_relations/graph_rag/经验富化） |
| 后台 Worker | `tauri/src-tauri/src/core/graph/worker.rs`（轮询/节流/LLM 适配） |
| 队列存储 | `tauri/src-tauri/src/core/graph/storage.rs`（graph_ai_queue） |
| LLM 客户端 | `tauri/src-tauri/src/services/llm.rs` + `core/loop/openai.rs`（OpenAiAdapter） |
| 配置链路 | `commands/config.rs`（kb_update_llm_config/save/load）+ `main.html`（loadSetting/syncLlmConfigToBackend） |
| 模型加载 | `core/db/utils.rs`（is_model_ready/ensure_model_ready）+ `lib.rs:393`（启动预下载） |
| 命令层 | `commands/graph.rs`（graph_* 全部命令） |
| 前端 | `css_js/graph/`（graph-api/interaction/panel/store/renderer/views/model） |
| 经验采集规划 | `docs/experience-graph-collection-plan.md` |
| 测试手册 | `docs/graph-intelligence-test-cases.md` |

**配置落点**：`{知识库}/.mdgo/setting.json`（键 `localLlmEndpoint/localLlmModel/localLlmToken/localLlmProtocol/...`）；
内存权威值 `AppState.llm_config`（`RwLock<LlmConfig>`）。
