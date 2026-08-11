# mdgo 数据存储评审：结构 / 选型 / 业务匹配度 / 主流 Agent 对比

> 版本：2026-08 · 评审范围：tauri/src-tauri（Rust 后端）+ index.html（前端）全部数据存储
> 目标：验证「合适业务用合适存储」原则，找出性能与一致性可优化点，对比主流 Agent 存储方案。
> 依据：全量源码调查（`docs/data_storage_analysis.md` 前序调查）+ 本次专项核实。

---

## 0. 结论摘要

mdgo 的存储选型**整体合理**（SQLite 承载结构化持久、LanceDB+BM25 分层检索、内存一切有界），与主流 Agent（Claude Code / Cline / Pi / Reasonix）的存储实践同构。发现 **7 个可优化点**，其中 3 个与性能直接相关（记忆检索无向量/无 FTS、会话写入逐条、双通道配置碎片），其余为一致性/工程化增强。

---

## 1. 现状与业务匹配度逐项评审

| 业务 | 现状存储 | 匹配度 | 评审意见 |
|---|---|---|---|
| 对话/会话/消息 | SQLite `{dir}/.mdgo/mdgo.db`（WAL+IMMEDIATE，session 索引） | ✅ 合适 | 与 Claude Code/Cline 同构（会话用 SQLite）。分支/压缩检查点作为表字段演进（parent_id/branch_point/compaction_state）合理。**可优化**：流式消息逐条 INSERT，长会话高频写 |
| 文档检索 | LanceDB `vectors` + tantivy `bm25` + RRF 融合 + rerank | ✅ 合适 | 分层检索符合检索范式。**可优化**：增量写入是否批处理（合并小写）需实测 |
| 历史对话检索 | LanceDB `chat_vectors`（独立表） | ✅ 合适 | 与文档向量分表隔离，重建互不影响 |
| 长期记忆 | SQLite `memory.db` `memory_items`（关键词打分） | 🟡 够用但可增强 | revision 审计链已对齐 Reasonix；但：**无向量召回**（长尾语义召回弱）、**expires_at 定义了但未启用**（无过期/衰减）、**无 FTS5 倒排**（关键词检索全表扫描 + 内存打分，规模大时变慢） |
| 技能 | SQLite（mdgo.db 技能表 + 指标）+ `resources/skills/*/SKILL.md` + 内存注册表 | ✅ 合适 | 指标落库（skill_exec_metrics 带 request_id）与文件化规约分离正确 |
| 提示词模板 | SQLite `prompts.db`（全局） | ✅ 合适 | 与 Agent 规约（打包资产）职责分离 |
| 配置 | JSON `setting.json` + 内存 `LlmConfig`/`ConfigStore` + YAML `approval.yaml`/`agent_tools.yaml` + plugin-store `app_settings.json` | 🟡 功能正确但碎片化 | 4 类配置 4 种加载路径（前端 FSA 直写 / 后端命令 / YAML 缺失降级 / plugin-store），无统一配置加载层；`app_settings.json` 物理路径未在源码显式构造（依赖 plugin-store 约定） |
| 外部工具/审批策略 | YAML（全局，缺失降级不阻断） | ✅ 合适 | 策略/扩展类配置用 YAML（可读可 diff），与运行配置分离正确 |
| Agent 运行时状态 | 纯内存有界（ToolCallBus 64 / TraceBus 64 / LruResultStore 16 / llm_client_cache 8 / 工具结果缓存 256+mtime / 审批规划挂起表 60s 超时） | ✅ 合适 | 全部有容量治理；挂起表无显式条数上限但 60s 超时 fail-closed 兜底（可接受） |
| 可观测 | 日志文件 + SQLite 指标 + 内存 TraceBus | ✅ 合适 | tracing 双输出 + 指标落库，够用（未达 OTel 级，非缺陷） |
| 模型资产 | 文件系统 `%APPDATA%/mdgo/`（按平台 ONNX） | ✅ 合适 | 模型下载缓存，标准做法 |
| 前端状态 | localStorage + IndexedDB + File System Access 直写 `.mdgo/` | 🟡 功能正确 | 前端 FSA 直写数据文件与后端 SQLite 构成**双通道写**（setting.json 前端写、会话消息后端写）——职责边界需文档化，避免同一文件双侧写竞争 |

---

## 2. 主流 Agent 存储方案对比

| 维度 | mdgo | Claude Code | Cline | Pi（编码 Agent） | Reasonix |
|---|---|---|---|---|---|
| 会话存储 | SQLite（每知识库 `mdgo.db`，分支/压缩检查点作列） | SQLite（`~/.claude/projects/`，消息/会话） | SQLite（WAL，会话+任务） | **JSONL 追加式**会话文件 + `/tree` 分支 + compaction entry（`retainedTail` 自包含检查点） | SQLite + 会话存档可检索 |
| 记忆 | SQLite 结构化记忆（revision 审计链，关键词检索） | 无显式记忆层（靠 CLAUDE.md + 会话历史） | 无独立记忆（会话历史为主） | 无独立记忆（会话/分支摘要替代） | **结构化记忆**（memory/remember/forget + revision + 自动写入授权 + 可选向量） |
| 文档 RAG | LanceDB 向量 + tantivy BM25 + RRF + rerank（本地化） | 无内置（外部 MCP/知识库） | 可选向量库（自定义 RAG） | 无内置（`@file` 引用 + 上下文文件） | 无内置（agent 工具面 + 记忆检索） |
| 配置 | JSON+内存双通道 + YAML 策略 | `settings.json` + CLAUDE.md | `settings/` 多文件 | `settings.json` + `auth.json` + context 文件分层 | `reasonix.toml` + REASONIX.md/AGENTS.md 分层 |
| 上下文压缩 | 摘要+滑窗（SQLite compaction_state 检查点） | 自动压缩（会话内） | 自动压缩 | **compaction**（token 精确、keepRecent、split turn、retainedTail） | 按需加载 + token 经济 |
| 工具/扩展配置 | YAML 外部工具（HTTP 适配器）+ 硬编码内置 | MCP（JSON 配置） | MCP | 扩展（TS 代码）+ skills（渐进披露） | MCP + Skills + 插件 |
| 可观测 | 日志 + SQLite 指标 + 内存 TraceBus | 日志 | 日志 | `pi-debug.log` + 完整事件流 | trace + 步骤记录 |

**对比结论**：
1. **会话**：mdgo 的 SQLite 方案与 Claude Code/Cline 一致（随机读、分支、统计友好），优于 Pi 的纯 JSONL（流式写轻但随机读/统计弱）。Pi 的 `retainedTail` 自包含检查点值得借鉴（mdgo 检查点已是同构思路）。
2. **记忆**：mdgo 已对齐 Reasonix 的 revision 审计链；差距在**检索增强**（Reasonix 记忆可接向量、mdgo 仅关键词）与**生命周期**（expires_at 未启用）。
3. **RAG**：mdgo 的本地向量+BM25+rerank 是主流 Agent 中少见的完整本地检索栈（多数依赖外部），是优势项。
4. **配置**：主流 Agent 均有"分层 context/配置"概念（AGENTS.md 层级、settings 多文件）；mdgo 配置入口碎片化是主要工程化差距。

---

## 3. 可优化点清单（按优先级）

### P1 — 性能/正确性相关（建议优先）

| # | 优化点 | 现状 | 问题 | 建议 | 预期收益 |
|---|---|---|---|---|---|
| O1 | **记忆检索接向量** | `memory_items` 关键词打分（`MemoryStore::search`，LIKE/内存打分） | 语义长尾召回弱；数据量大时全表扫描 | 复用 embedding 管线为记忆建向量（独立 `memory_vectors` LanceDB 表或复用 chat_vectors 管线），检索 = 向量 top-k ∪ 关键词，RRF 融合（复用 `core/search/rrf.rs`） | 记忆召回质量显著提升；架构复用现有 embedding/rerank |
| O2 | **启用记忆过期/衰减** | `expires_at` 字段已定义未使用 | 过期记忆永不回收，污染注入 | `search/list` 过滤 `expires_at < now`；提供 `remember` 的 `expires_at` 参数与清理路径 | 记忆保鲜，注入上下文减噪 |
| O3 | **记忆关键词倒排** | 无 FTS5 | 规模增长后检索 O(n) | 对 title/body/keywords 建 **SQLite FTS5 虚拟表**（rusqlite bundled 支持 FTS5），search 走 MATCH | 大记忆量下检索从 O(n) → 对数级 |
| O4 | **会话写入批量化** | 流式消息逐条 `save_message` INSERT | 长会话/高频工具调用下 SQLite 写放大（每轮多次写：消息+轨迹+统计+压缩检查点） | 消息与工具轨迹合并为一次事务批量写（或延迟落库 + 批量 flush）；压缩检查点写入降频（如每 N 轮一次） | 写次数减少 60%+，长会话吞吐提升 |

### P2 — 一致性/工程化

| # | 优化点 | 现状 | 问题 | 建议 | 预期收益 |
|---|---|---|---|---|---|
| O5 | **配置统一加载层** | setting.json（前端 FSA）+ app_settings.json（plugin-store）+ approval.yaml/agent_tools.yaml（后端 YAML）+ LlmConfig（内存） | 4 类配置 4 条路径；`app_settings.json` 路径未显式化；同文件双通道写风险 | 引入 `ConfigStore` 统一入口（读优先内存缓存 + 写回原载体），职责边界文档化；显式化 plugin-store 路径 | 配置一致性与可维护性；消除双写竞争 |
| O6 | **索引增量批处理** | BM25/Lance 增量写入（watcher 驱动） | 高频文件变更时小写入多 | 索引更新合并为批（如 200ms debounce / 批量 flush），复用 indexer 现有批处理点 | 高频变更下索引写放大下降 |
| O7 | **挂起表显式上限** | approval_pending/plan_pending 无条数上限（60s 超时兜底） | 极端并发可短暂堆积 | 与 ToolCallBus 一致：容量上限 + 满则清理最旧 | 内存边界显式化，防异常路径堆积 |

### 暂不建议动（保持现状）

- 文档检索 LanceDB+BM25+rerank 本地栈：已是优势，勿改。
- 会话 SQLite（非 JSONL）：与主流一致且分支/检索友好。
- 内存有界结构：容量治理已齐备。
- 日志/指标：够用。

---

## 4. 落地建议

- **✅ 已完成（2026-08，commit `561c605`）**：
  - **O2 记忆过期**：`expires_at` 启用（list/search 过滤），remember 工具支持 `expires_in_days`。
  - **O3 记忆 FTS5**：`memory_fts` 虚拟表 + bm25 排序检索（写后全量重建，FTS 不可用降级关键词打分）；顺带修复历史残留触发器导致的 SQLITE_ERROR。
- **O1（记忆向量）**：收益最高且复用现有 `core/embedding.rs` + `core/search/rrf.rs`，建议作为下一个落地项（独立一轮：异步 embedding + 全局记忆向量库 + 融合检索 + 模型可用性降级）。
- **O4（会话写入批量化）**：评审已注明"先量测真实写频率再决定"；改动涉及前端保存链路与后端事务，需量测数据支撑后谨慎实施。
- **O5** 属工程重构，可与前端设置面板统一改造合并。

> 备注：本评审为源码静态核实；O4 的"写放大"严重度建议在真实长会话场景用日志/指标验证后确认优先级。
