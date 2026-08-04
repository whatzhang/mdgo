# mdgo Skill（本地知识库技能）功能 PRD

> 版本：v0.2（草案，含评审调整）
> 日期：2026-08-03
> 范围：在 mdgo 中集成"本地知识库相关 Skill 的增删查改与使用"能力

---

## 1. 背景与目标

### 1.1 背景

mdgo 已具备完整的本地知识库 + RAG 链路：

- 本地嵌入模型（BGE-Small-ZH v1.5，ONNX）→ LanceDB 向量库 + BM25 混合检索
- 基于 Rig 的 Agent 系统，内置 5 个只读工具：`kb_search`（语义检索）、`code_lookup`（符号检索）、`read_file`（读文件）、`list_files`（列目录）、`git_status`（Git 状态）
- 两种对话模式：普通对话（`kb_llm_query`）与 RAG 问答（`kb_rag_query`，查询扩展 → 多查询检索 → 聚合 → 生成）
- 聊天历史（SQLite）、AI 历史、配置持久化均已就绪

但当前这些能力是**硬编码**的：Agent 的 system prompt、工具集、检索参数均由代码写死。**Skill 功能补上这一层**：把"知识库使用流程"变成可注册、可管理、可复用、可观测的能力单元。

### 1.2 架构演进方向

**RAG 问答将迭代为统一的 Agent 执行体系**：

- 现 `kb_rag_query` 的全流程（意图匹配 → 检索 → 聚合 → 生成）将抽象为 **Skill 调度执行器（SkillDispatcher）**的默认编排；
- 检索、代码定位、读文件、Git 状态等现有能力沉淀为 **内置 Skill**；
- 新增用户 Skill 与内置 Skill 走同一套"注册 → 意图匹配 → 调度 → 执行 → 观测"流水线，**对外保持兼容入口**（`kb_rag_query`/`kb_llm_query` 内部统一转发到调度器）。

### 1.3 目标

- 建立**三层 Skill 体系**：系统内置 / 用户全局 / 用户项目，各自独立存储与生命周期
- 强制 **Schema 契约**注册：每个 Skill 必须携带完整元数据（id、优先级、触发规则、互斥列表、token 预算、入参 schema、输出格式、权限角色、超时、启用、版本、时间戳）
- **配置化管理 + 动态注册**：元数据存 YAML/JSON，注册表读写分离，热更新不重启
- **意图匹配分层**：关键词精准匹配 → 语义相似度打分 → 兜底模糊匹配，**拒绝纯模型自由选择**
- **执行层可靠性**：异步化、超时熔断、重试分级、降级兜底、状态机与链路追踪
- **并发与性能**：实例池、TTL 缓存、信号量流量管控、并行/串行编排
- **可观测性**：全链路日志、监控指标聚合接口、线上动态启停
- UI：左右分栏管理界面，样式与主界面（index.html）统一，全部 skill 前缀隔离

### 1.4 非目标（v1 不做）

- 不支持 Skill 内嵌 Python/Shell 脚本执行（仅"提示词 + 工具组合 + 参数"）
- 不做 Skill 市场/分享平台（仅支持本地导入/导出单个 SKILL.md）
- 不做多用户权限/团队协作（权限角色字段仅预留扩展）

---

## 2. 竞品调研：业界 Skill 功能怎么做的

### 2.1 商业软件

| 产品 | Skill 形态 | 触发方式 | 关键设计 |
|---|---|---|---|
| **OpenAI GPTs** | 指示（Instructions）+ 知识（文件）+ 能力（工具）+ 动作（API）四要素 | 用户主动选择 GPT；对话中 `@GPT名` 带入 | 无代码构建器；可分享/发布 |
| **ChatGPT Skills** | 可重复任务的"流程化指令" | 模型按描述自动匹配 | 高频任务沉淀为一致工作流 |
| **OpenAI Workspace Agents** | 智能体 = 提示词 + 工具 + MCP + 技能 + 文件 | 手动/排程/API 触发 | 可测试、可分享、可排程 |

### 2.2 开源软件

| 产品 | Skill 形态 | 触发方式 | 关键设计 |
|---|---|---|---|
| **Claude Code Skills**（事实标准） | `skills/<name>/SKILL.md`：YAML frontmatter + Markdown 正文 | ① 模型按 description 自动加载 ② 手动 `/skill-name` | **渐进式披露**；`allowed-tools` 工具白名单；`disable-model-invocation` 控制自动触发；作用域：个人/项目/插件 |
| **Open WebUI** | Functions（Python 函数工具）+ Pipelines（热插拔管道）+ Knowledge（知识集合） | 模型按描述判断调用；用户手动开关并挂载；聊天 `#知识库` 引用 | description 即触发说明；工具与知识库解耦 |
| **Dify Skills** | YAML 元数据 + Markdown 指令；Skill = Agent 专属能力单元，Plugin = 工作空间级连接器 | 关键词/触发词智能匹配；`priority`、`category`、`enabled_skills` 配置 | Skill 与 Plugin 分层；可视化创建；Agent 沙箱 |
| **Obsidian Copilot / AnythingLLM** | LLM + 向量索引 + 上下文拼装三层；无独立 Skill 概念 | 聊天侧栏手动选模式/指令 | 知识库与 AI 深度绑定 |

### 2.3 对 mdgo 的设计启示

1. **SKILL.md 双段式是行业共识**：YAML frontmatter（机器可读元数据）+ Markdown 正文（执行指令），mdgo 直接采用，可与 Claude Code 生态互通。
2. **触发必须可确定**：Open WebUI / Claude Code 均靠 description 让模型判断触发；但对本地小模型不可靠，mdgo 采用**分层意图匹配**（关键词 → 语义打分 → 兜底模糊），模型只负责"是否执行、如何执行"，不负责"选哪个 Skill"。
3. **工具白名单绑定技能**：Claude Code 的 `allowed-tools` 与 Dify 分层都表明——技能声明自己的工具，运行时只挂载声明的工具。
4. **能力单元可观测**：商业平台均提供调用次数、成功率等度量，mdgo 需内置全链路日志与指标。
5. **分层管理是标配**：GPTs 的"个人/工作区"、Claude Code 的"个人/项目/插件"、Dify 的"Skill/Plugin"——mdgo 采用**系统内置 / 用户全局 / 用户项目**三层。

---

## 3. 产品定义

### 3.1 三层 Skill 体系与存储位置

| 层级 | 来源 | 存储位置 | 生命周期 | 只读性 |
|---|---|---|---|---|
| **系统内置 Skill** | 随安装包分发，打包发版 | `{安装目录}/resources/skills/`（代码侧归档于 `tauri/src-tauri/resources/skills/`，经 tauri.conf.json bundle 打包） | 随版本升级更新 | 用户不可改/不可删（UI 只读展示） |
| **用户全局 Skill** | 用户自建，跨项目共享 | **始终使用应用数据目录**：Windows `%APPDATA%/com.mdgo/skills/`；macOS `~/Library/Application Support/com.mdgo/skills/`；Linux `$XDG_DATA_HOME/com.mdgo/skills/` | 用户创建/修改/删除 | 可写 |
| **用户项目 Skill** | 用户自建，随知识库目录 | `{打开目录}/.mdgo/skills/`（与项目现有 `.mdgo/` 目录标准一致，如 `.mdgo/setting.json`） | 用户创建/修改/删除；随目录走（可进 Git） | 可写 |

> **决策记录**：用户全局 Skill **不放在安装目录**（Windows 安装目录常在 Program Files，无写权限），一律使用应用数据目录，避免平台差异与权限问题；安装目录仅承载系统内置技能（只读，随包升级）。

**优先级顺序（同名冲突解决）**：系统内置 < 用户全局 < 用户项目（项目级覆盖全局、全局覆盖内置）；`(scope, id)` 构成注册表唯一键。

### 3.2 名词定义

| 术语 | 定义 |
|---|---|
| **Skill（技能）** | 面向本地知识库的、可复用的"任务处理流程"能力单元。由一个 `SKILL.md`（YAML 元数据 + Markdown 指令）定义。 |
| **注册表（SkillRegistry）** | 内存中按 `(scope, id)` 索引的全部已注册技能；读写分离，写路径先落盘再刷新。 |
| **内置工具** | mdgo Agent 已有 5 个只读工具：`kb_search`、`code_lookup`、`read_file`、`list_files`、`git_status`。 |
| **调度执行器（SkillDispatcher）** | 统一执行入口：意图匹配 → 选中 Skills → 编排（并行/串行）→ 各自经 Agent 执行 → 结果合并。 |
| **技能挂载** | 把技能绑定到某次对话会话，其指令注入 Agent 提示词、其声明的工具被挂载。 |

### 3.3 用户画像与场景

#### 用户画像

- **核心用户**：知识密集型工作者——研究者、开发者、写作/笔记重度用户。每天产生与消费大量 Markdown 文档、会议记录、实验数据与代码。
- **典型痛点**：
  - 笔记只进不出：资料越积越多，检索靠人肉回忆，笔记彼此孤立、从不互相印证
  - AI 是"工具"而非"助手"：每次对话无状态，不记得专业背景、研究偏好、写作风格，更不会学习思维习惯
  - 知识盲区不自知：不知道自己缺什么资料，也没人指出笔记间的矛盾、重复与逻辑漏洞
  - 低价值重复劳动：摘要、翻译、纠错、打标签、格式整理占据大量时间

#### 功能场景（对应"AI 核心功能"，逐步以内置 Skill 交付）

- **场景 A（知识总结）**：项目级"主题综述"技能：`kb_search` 大召回 + `read_file` 精读，输出带来源引用的综述。新会话挂载即用。
- **场景 B（代码问答）**：全局"代码定位"技能：强制优先 `code_lookup` + `read_file`，符号检索参数偏向。
- **场景 C（工作区体检）**：内置"仓库状态汇报"技能：自动 `git_status` + `list_files`，汇报工作区改动。
- **场景 D（写作规范）**：内置/全局"读书笔记"技能：纯指令不绑检索工具，注入"卡片式输出"规范。
- **场景 E（多格式解析与识别）**：**目前仅支持 markdown、ompl、freemind 与主流代码**；后续扩展 PDF、Office、网页等格式，作为 Skill 输入解析层的前置能力（可独立成"格式解析"内置 Skill 或基础服务）。
- **场景 F（内容摘要与提炼）**：一键生成长文档/会议记录的摘要、要点提炼；输出结构化要点与来源锚点。
- **场景 G（智能写作辅助）**：续写、改写、扩写、语气调整、拼写与语法纠错；基于当前笔记上下文 + 知识库检索增强。
- **场景 H（自动分类与标签）**：根据文档内容自动打标签、归类，甚至自动填入 YAML front matter 属性（与 SKILL.md Schema 天然契合）。
- **场景 I（相关笔记推荐）**：基于语义相似度，在侧边栏展示与当前笔记相关的其他笔记；复用嵌入模型与 LanceDB 检索。
- **场景 J（多语言翻译）**：划词翻译或全文翻译（内置"翻译"Skill）。
- **场景 K（语音转文字）**：**优先级最低**；项目当前已具备基础能力，作为补充输入通道接入，不阻塞主线。

#### 核心竞争力场景（差异化能力，逐期评估落地）

- **场景 L（主动知识维护与冲突检测）**：主动告知"这篇笔记的结论与三个月前的实验记录矛盾"；检测"有 3 条笔记讲同一概念，建议合并去重"。依赖定时/触发式后台 Skill（写入、导入、周期性触发）。
- **场景 M（长程记忆与人格化）**：打破无状态会话；记录专业背景、研究偏好、写作风格，持续学习思维习惯，像长期助手一样提供一致风格输出。依赖用户画像持久化 + 会话记忆（`chat_session_skills` 快照扩展）。
- **场景 N（知识缺口分析）**：研究某主题时指出"知识库中缺乏关于 X 的关键文献，需要补充"；基于知识图谱覆盖率与检索召回空白推断缺口。
- **场景 O（跨媒体因果推断）**：从多张图表、多条笔记中自动提炼趋势或异常，如"最近三个月的血糖记录在周二普遍偏高"；需结构化数据解析 + 聚合分析 Skill。
- **场景 P（行为意图预测）**：结合当前浏览的文档、打开的文件夹与日历，预测下一步可能需要的资料并预先准备（预取/预检索）。
- **场景 Q（自主知识图谱演进）**：从纯文本持续提取实体与关系，动态构建并更新知识图谱，并基于图谱做推理。
- **场景 R（主动语境捕获）**：感知"复制一段文字、截图、浏览器高亮一句话"等知识碎片，建议整理进知识库。
- **场景 S（对抗性思维激发）**：AI 扮演反方，挑战笔记中的论点，帮用户发现逻辑漏洞（"红队"Skill）。
- **场景 T（全自动综述/周报/项目分析生成）**：结合活动日志、日历、新增笔记，自动生成有洞察的周度知识总结。

> **落地策略**：以上场景中，A~K 属于确定性高、贴近现有 RAG/Agent 能力的**短期内置 Skill 候选**（M1~M3 分批交付）；L~T 属于差异化竞争力，依赖记忆、图谱、主动触发等进阶设施，**统一通过 Skill 体系注册，逐步由"手动触发"演进到"定时/事件触发"**，为 v2+ 预留扩展点，避免阻塞当前主链路。

### 3.4 核心交互流

```
[注册]  扫描安装目录/全局目录/.mdgo/skills → 解析 SKILL.md → 校验 Schema → 写入注册表（读写分离，热更新）
                  │
[匹配]  用户消息 → 关键词精准匹配 → 语义相似度打分 → 兜底模糊匹配（拒绝纯模型选择）
                  │
[调度]  SkillDispatcher：并行/串行编排 → 信号量限流 → 实例池取执行器
                  │
[执行]  Agent 生成（指令注入 + 工具白名单 + 检索参数覆盖）→ 超时熔断/重试/降级
                  │
[观测]  状态机（待调度/执行中/成功/失败/降级）→ 全链路日志 + 指标 → 前端轨迹卡片
```

---

## 4. 功能需求

### 4.1 Skill 注册规范（强制 Schema 契约）

每个 Skill 定义必须包含以下字段（SKILL.md YAML frontmatter 与注册表/数据库结构一致）：

| 字段 | 类型 | 必填 | 说明 | DB 列 |
|---|---|---|---|---|
| `id` | string | 是 | 唯一标识（小写/数字/连字符）；注册表键 = `(scope, id)` | `id` |
| `scope` | enum | 是 | `system` / `global` / `project` | `scope` |
| `name` | string | 是 | 展示名 | `name` |
| `description` | string | 是 | 技能做什么、何时触发（触发匹配依据之一） | `description` |
| `priority` | int | 是 | 优先级（0~100，越大越优先；同优先级按创建时间） | `priority` |
| `trigger_rules` | JSON | 是 | 触发规则：`{type, keywords[], similarity_threshold}`（详见 §4.5） | `trigger_rules` |
| `mutex` | string[] | 否 | 互斥列表：与本技能互斥的 skill id 集（调度时不共存） | `mutex` |
| `token_budget` | int | 否 | token 预算（指令注入+执行消耗上限，预留扩展） | `token_budget` |
| `input_schema` | JSON | 否 | 入参 schema（JSON Schema 子集：name/type/required/description/default） | `input_schema` |
| `output_format` | enum | 否 | `text` / `json` / `markdown`（预留，当前仅记录展示） | `output_format` |
| `roles` | string[] | 否 | 权限角色（预留扩展，v1 恒为 `["owner"]`） | `roles` |
| `timeout_ms` | int | 否 | 单次执行超时（默认 30000），超时熔断依据 | `timeout_ms` |
| `tools` | string[] | 是 | 声明的工具白名单（5 个内置工具枚举） | `tools` |
| `top_k` / `min_score` / `max_docs` / `max_chunks_per_doc` | number | 否 | 检索参数覆盖（对应现有 `IndexerConfig`） | 各列 |
| `enabled` | bool | 否 | 是否启用（默认 true），支持线上动态启停 | `enabled` |
| `version` | int | 否 | 版本号，每次保存自增 | `version` |
| `created_at` / `updated_at` | u64 | 是 | 创建/更新时间（毫秒时间戳） | 各列 |

> **扩展性说明**：`token_budget`、`output_format`、`roles`、`input_schema` 等字段 v1 仅做登记/校验/展示，不参与执行逻辑，为后续（脚本执行、权限体系、Token 计费）预留契约，避免破坏性变更。

**SKILL.md 示例：**

```markdown
---
id: kb-summary
scope: project
name: 知识库综述
description: 当用户要求对知识库、某个主题或一批文档进行总结、综述、概览时触发。
priority: 60
trigger_rules:
  type: hybrid
  keywords: ["总结", "综述", "概览", "归纳", "summary", "overview"]
  similarity_threshold: 0.55
mutex: ["kb-rewrite"]
token_budget: 4000
input_schema:
  - { name: "query", type: "string", required: true, description: "综述主题" }
  - { name: "max_docs", type: "number", required: false, description: "最大文档数", default: 6 }
output_format: markdown
roles: ["owner"]
timeout_ms: 60000
tools: [kb_search, read_file]
top_k: 12
min_score: 0.25
max_docs: 6
enabled: true
version: 1
created_at: 1754200000000
updated_at: 1754200000000
---

# 知识库综述

## 适用场景
- 用户需要对某主题在知识库中的内容做整体性总结

## 执行步骤
1. 先用 kb_search 从多个角度检索（可检索 2~3 轮，每次聚焦单一角度）
2. 对高相关文档用 read_file 精读关键章节
3. 按主题归纳，而非按文档罗列

## 输出规范
- 用 ## 分级标题组织；每个结论标注来源文档名；信息不足时明确说明
```

### 4.2 配置化管理与动态注册

- **元数据存 YAML/JSON，不硬编码**：所有 Skill（含系统内置）以 `SKILL.md` 文件为唯一事实来源；代码不内嵌任何 Skill 定义（仅内置工具的注册表常量除外）。
- **注册流程**：启动时与运行中由 `SkillStore` 扫描三个目录 → 解析 frontmatter → Schema 校验 → 写入内存注册表；失败条目记录错误并跳过，不影响整体启动。
- **读写分离**：注册表读路径（意图匹配、会话挂载、指标查询）走内存 `RwLock`；写路径（创建/更新/删除/启停）→ 校验 → 写文件（立即 flush）→ 更新内存 → 同步 DB 缓存 → 向前端广播 `skill:changed`。
- **热更新不重启（独立 Watcher 服务，单一职责原则）**：新增专属 `SkillWatcherService`（`services/skill_watcher.rs`），**只负责一件事**——监控三个 Skill 目录的文件变更（复用 `notify-rs`，800ms 防抖），变更后仅向注册表发送刷新事件；解析、Schema 校验、写库等职责保留在 `SkillStore`/`SkillRegistry`，**不混入 Watcher**。外部手工修改 SKILL.md 也能被感知并刷新注册表。
- **系统内置与用户覆盖**：同名 `id` 时按 §3.1 优先级覆盖；删除用户技能后自动回落到低优先级定义。

### 4.3 管理能力（增删查改 + 动态管控）

| 功能 | 说明 | 验收要点 |
|---|---|---|
| 技能列表 | 全部技能（含三层来源），卡片显示：名称/作用域/分类/启用状态/工具标签 | 支持按作用域过滤 + 搜索 |
| 新建技能 | 表单（全部 Schema 字段）+ Markdown 指令正文编辑 | frontmatter 校验通过才保存；`(scope,id)` 唯一 |
| 编辑技能 | 加载现有 SKILL.md 回填表单；保存后 version 自增 | 系统内置仅读（隐藏编辑/删除） |
| 删除技能 | 删除用户级 Skill 文件（系统内置不可删） | 二次确认；已挂载会话按快照运行 |
| 启用/停用 | 切换 `enabled`；**线上动态生效**，已注册实例不重启即应用 | 停用后不再参与匹配与挂载候选 |
| 导入/导出 | 仅限用户级 Skill（global/project）：导入单个 SKILL.md（name 冲突策略：覆盖/跳过）；导出为 SKILL.md。**系统内置技能不可导入/导出**（随安装包分发、受版本管理） | 非法 frontmatter 拒绝并定位错误字段；对系统内置技能执行导入/导出直接拒绝 |
| 语法校验 | 保存/导入时校验字段类型、`tools`/`scope` 枚举、`trigger_rules` 结构 | 错误信息定位到具体字段 |

### 4.4 意图匹配（分层，拒绝纯模型自由选择）

匹配**优先由确定性逻辑完成**（在模型之前，输出 `(skills, score)` 排序结果）；**仅当前置确定性逻辑未匹配到任何技能时**，才将候选列表交由模型兜底选择（L4）；模型仅在被选中技能的指令下执行，且**不可脱离候选范围自由选择**。

| 层级 | 算法 | 说明 |
|---|---|---|
| L1 关键词精准匹配 | 用户消息对 `trigger_rules.keywords` 做子串/分词命中（大小写不敏感） | 命中即入选，得分最高；支持中英文关键词 |
| L2 语义相似度打分 | 复用本地嵌入模型（`call_embedding_query`）对消息与各 Skill 的 `description`+`keywords` 向量化，余弦相似度打分 | 对 L1 未命中的候选做排序；`similarity_threshold` 过滤 |
| L3 兜底模糊匹配 | 编辑距离/Jaccard 词重叠对关键词做模糊匹配（允许错别字、变体） | 仅在前两层均无命中时触发，得分最低 |
| L4 模型兜底选择 | 前三层确定性逻辑均未匹配到任何 Skill 时，将候选列表（所有 `enabled` 技能的 `id`+`description`）交由模型做最终选择 | 仅在前三层全部无命中时触发，作为最后兜底；模型输出仍需通过启用/互斥/作用域校验，且不能脱离候选列表自造技能 |

**调度规则**：

- 按 `priority` 降序取 Top-N（默认 3）进入执行；`enabled=false`、已删除、互斥冲突者剔除
- `mutex` 互斥：选中集合内存在互斥关系时，保留优先级高者（记录被剔除项到日志/链路）
- 会话显式挂载的 Skill 无条件入选（用户意图 > 自动匹配）
- 所有层级命中与否、得分、被剔除原因，全部写入链路日志（供可观测性分析）

### 4.5 执行层可靠性设计

| 能力 | 设计 |
|---|---|
| **异步化** | 全部 Skill 执行走 `tokio` 协程池（`spawn_blocking` 隔离嵌入/检索等阻塞任务），调度线程不阻塞；Tauri 命令仅做入队与事件订阅 |
| **超时熔断** | 每个 Skill 独立 `timeout_ms`，执行超时自动熔断该 Skill（取消其 Agent 流式任务，复用 `CancellationToken` 机制），不阻塞整条链路；连续熔断 N 次（默认 3）进入 30s 冷却，冷却期间直接返回"降级" |
| **重试分级** | 区分可重试（网络波动/上游 5xx/流式 InvalidContentType 降级场景）与不可重试（参数校验失败、Schema 不符、幂等性被破坏）；可重试采用指数退避（300ms → 900ms → 2700ms，上限 3 次）；不可重试直接返回标准化错误 |
| **降级兜底** | 任一 Skill 失败返回**标准化错误结构** `{code, message, retryable, skill_id, execution_ms}`，由编排层决定：有替代 Skill 则降级调用，无则如实告知用户；RAG 主链路失败沿用现有"非流式降级重试" |
| **状态机** | Skill 执行状态：`pending（待调度）→ running（执行中）→ success（成功）| failed（失败）| degraded（降级）`；每次状态迁移写入链路 trace（`request_id` + `execution_id`），前端工具轨迹卡片展示 |

### 4.6 并发与性能优化

| 能力 | 设计 |
|---|---|
| **实例池** | 无状态 Skill（纯指令/检索参数型）单例复用；有状态 Skill（预留）池化管理，控制并发数 |
| **TTL 缓存** | ① 元数据缓存（扫描结果，TTL 30s）② 路由结果缓存（`消息hash → skills`，TTL 60s）③ 常用 Skill 执行结果缓存（仅纯指令类可缓存，TTL 300s，**显著降低 LLM 调用开销**）；缓存 key 含 `(scope,id,version)`，版本变化即失效 |
| **流量管控** | 全局信号量限制并发 LLM 请求（默认 2，可配置），防止本地/远端模型 API 被打满；超限请求排队而非丢弃 |
| **并行编排** | 无依赖的多 Skill 并行执行（`futures::buffer_unordered`，复用现有并行检索模式）；有依赖则串行链式调用（后者入参引用前者输出）；v1 仅实现并行，串行依赖编排为扩展接口预留 |

### 4.7 可观测性与运维

| 能力 | 设计 |
|---|---|
| **全链路日志** | 记录每个 Skill 的：触发层级与得分、入参（脱敏）、出参摘要、耗时、token 消耗、异常栈、状态机迁移；`request_id` 贯穿匹配→调度→执行 |
| **监控指标** | 指标项：调度命中率、Skill 执行成功率、平均耗时 P50/P95、token 消耗、错误码分布、熔断/降级次数；指标在内存环形缓冲聚合 |
| **聚合接口** | 提供 `skill_metrics(dir_path, skill_id?, since?)` Tauri 命令返回聚合数据，**为后续其他业务（成本分析、技能推荐、运营报表）做前置准备** |
| **动态管控** | `skill_set_enabled` 支持线上启用/禁用某个 Skill，即时生效并广播 `skill:changed` |

### 4.8 使用能力（对话集成）

| 功能 | 说明 |
|---|---|
| 会话挂载 | 聊天输入区"技能"选择器（chips，≤3 个）。**仅 RAG（Agent）模式可挂载**；普通对话（`chat-mode-normal`）纯粹是对话/聊天，**不调用任何工具与 Skill** |
| 指令注入 | 被挂载/命中的技能正文合并注入 Agent preamble；`description` 一并注入供模型遵循流程（仅 RAG 链路） |
| 工具挂载 | 仅挂载"所选技能声明工具的并集"；RAG 主链路默认保留 `kb_search` |
| 检索参数覆盖 | 技能声明的 `top_k/min_score/max_docs/max_chunks_per_doc` 覆盖会话级 RAG 设置 |
| 运行轨迹 | 复用 `agent:tool_call` / `agent:tool_result`，工具卡片标注"由哪个技能驱动"及执行状态 |
| 会话记忆 | 新表 `chat_session_skills` 保存挂载技能（id+scope+version 快照），恢复会话自动恢复（仅 RAG 会话记录） |
| 手动指定 | RAG 模式输入 `/技能名` 直接触发指定技能（无视自动匹配） |

---

## 5. 数据模型

### 5.1 SOLID：单一职责的 Schema 代码文件

> **约定**：新增 `core/db/schema.rs`，作为**全项目唯一**承载"建表 DDL + 初始化/种子数据"的代码文件（单一职责原则）。存量 `services/chat.rs`、`services/ai_history.rs` 中的建表语句在 M1 迁移至此文件（可选），**此后所有新表一律在此定义**；各 Store 只负责读写逻辑，不再内嵌 DDL。

`schema.rs` 提供：

- `fn init_all(conn: &Connection) -> Result<(), String>`：依次执行全部 `CREATE TABLE IF NOT EXISTS`（skills、chat_session_skills、存量表）+ 列迁移
- `fn seed_system_data(conn: &Connection) -> Result<(), String>`：写入**系统内置 Skill 的种子数据**（首次启动时从打包资源导入注册表与 DB）

### 5.2 表结构

```sql
-- 技能注册表（与 SKILL.md frontmatter 一一对应）
CREATE TABLE IF NOT EXISTS skills (
    id                 TEXT NOT NULL,
    scope              TEXT NOT NULL,             -- system / global / project
    name               TEXT NOT NULL DEFAULT '',
    description        TEXT NOT NULL DEFAULT '',
    priority           INTEGER NOT NULL DEFAULT 50,
    trigger_rules      TEXT NOT NULL DEFAULT '{}',-- JSON：type/keywords/similarity_threshold
    mutex              TEXT NOT NULL DEFAULT '[]',-- JSON 数组：互斥 skill id
    token_budget       INTEGER NOT NULL DEFAULT 0,-- 预留扩展
    input_schema       TEXT NOT NULL DEFAULT '[]',-- JSON 数组：入参 schema
    output_format      TEXT NOT NULL DEFAULT 'text',-- 预留扩展
    roles              TEXT NOT NULL DEFAULT '[]',-- JSON 数组：预留扩展
    timeout_ms         INTEGER NOT NULL DEFAULT 30000,
    tools              TEXT NOT NULL DEFAULT '[]',-- JSON 数组：工具白名单
    top_k              INTEGER,
    min_score          REAL,
    max_docs           INTEGER,
    max_chunks_per_doc INTEGER,
    enabled            INTEGER NOT NULL DEFAULT 1,
    version            INTEGER NOT NULL DEFAULT 1,
    file_path          TEXT NOT NULL,             -- 事实来源文件路径
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL,
    PRIMARY KEY (scope, id)
);

-- 会话挂载快照（含 version，恢复时校验版本漂移）
CREATE TABLE IF NOT EXISTS chat_session_skills (
    session_id TEXT NOT NULL,
    scope      TEXT NOT NULL,
    skill_id   TEXT NOT NULL,
    version    INTEGER NOT NULL,
    PRIMARY KEY (session_id, scope, skill_id),
    FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE
);

-- 指标聚合（环形缓冲落库，可选，为其他业务前置准备）
CREATE TABLE IF NOT EXISTS skill_exec_metrics (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id   TEXT NOT NULL,
    scope        TEXT NOT NULL,
    skill_id     TEXT NOT NULL,
    match_level  TEXT NOT NULL,      -- L1/L2/L3/attached/manual
    score        REAL,
    state        TEXT NOT NULL,      -- pending/running/success/failed/degraded
    duration_ms  INTEGER,
    tokens_in    INTEGER,
    tokens_out   INTEGER,
    error_code   TEXT,
    created_at   INTEGER NOT NULL
);
```

> 所有 `TEXT` JSON 列采用"以 SKILL.md 文件为事实来源、DB 为查询缓存"的策略；文件被外部修改后由热更新重灌缓存。`chat_session_skills` 快照保证"挂载的旧版本技能被删除/升级后，历史会话展示不受影响"。

### 5.3 运行时结构（后端）

```rust
// core/skill.rs（新增）：模型 + 解析 + 注册表 + 调度
pub struct Skill {
    pub id: String,
    pub scope: SkillScope,          // system/global/project
    pub name: String,
    pub description: String,
    pub priority: u32,
    pub trigger_rules: TriggerRules,// keywords + similarity_threshold
    pub mutex: Vec<String>,
    pub token_budget: u32,          // 预留
    pub input_schema: Vec<ParamDef>,// 预留
    pub output_format: String,      // 预留
    pub roles: Vec<String>,         // 预留
    pub timeout_ms: u64,
    pub tools: Vec<String>,         // 工具白名单
    pub top_k: Option<u32>,
    pub min_score: Option<f32>,
    pub max_docs: Option<usize>,
    pub max_chunks_per_doc: Option<usize>,
    pub enabled: bool,
    pub version: u32,
    pub body: String,               // Markdown 指令
    pub file_path: String,
    pub created_at: u64,
    pub updated_at: u64,
}

pub struct SkillStore { /* 扫描三目录、解析校验、读写文件、同步 DB 缓存 */ }
pub struct SkillRegistry { /* RwLock<HashMap<(SkillScope, String), Skill>> 读写分离 */ }
pub struct SkillDispatcher { /* 意图匹配 + 编排 + 限流 + 熔断 + 指标 */ }
pub struct SkillMetrics { /* 环形缓冲聚合 */ }
pub struct SkillWatcherService { /* 单一职责：仅监控三个 Skill 目录变更，800ms 防抖，变更后发送刷新事件 */ }
```

**AppState 增加：**

```rust
pub skill_registry: Arc<SkillRegistry>,
pub skill_dispatcher: Arc<SkillDispatcher>,
pub skill_metrics: Arc<SkillMetrics>,
pub skill_watcher: Arc<SkillWatcherService>,
```

---

## 6. 技术方案（实施建议）

### 6.1 后端（Rust）

**新增模块**：

- `core/skill.rs`：Skill 模型、SKILL.md 解析（`serde_yaml` frontmatter + 正文）、Schema 校验、SkillStore/Registry/Dispatcher/Metrics
- `services/skill_watcher.rs`：SkillWatcherService（单一职责：仅监控 Skill 目录变更与防抖，不包含解析/校验/写库逻辑）
- `core/db/schema.rs`：全项目建表 DDL 与种子数据（SOLID 单文件，见 §5.1）
- `commands/skill.rs`：Tauri 命令层

**Tauri 命令**（注册进 `lib.rs` 的 `invoke_handler`）：

```
skill_list(dir_path, scope?)                → Vec<SkillMeta>      // 列表（支持作用域过滤）
skill_get(dir_path, scope, id)              → Skill               // 详情（含正文）
skill_create(dir_path, scope, skill_input)  → Skill
skill_update(dir_path, scope, id, input)    → Skill
skill_delete(dir_path, scope, id)           → ()
skill_set_enabled(dir_path, scope, id, enabled) → ()             // 线上动态启停
skill_import(dir_path, scope, content)      → Skill
skill_export(dir_path, scope, id)           → String             // SKILL.md 全文
skill_validate(dir_path, content)           → {ok, errors[]}     // 编辑器实时校验
skill_match(dir_path, query)                → [(Skill, level, score)]  // 分层意图匹配（调试用）
skill_metrics(dir_path, skill_id?, since?)  → MetricSummary      // 聚合接口
```

**Agent 集成改造**（`core/agent/mod.rs` + `commands/llm.rs`，为 RAG → Agent 演进预留）：

1. 新增统一入口 `skill_dispatch_query`（内部流水线：意图匹配 → 编排 → 逐 Skill 经 Rig Agent 执行 → 结果合并）；`kb_rag_query` / `kb_llm_query` 保留为兼容包装，内部转发到调度器（RAG 视为"内置 RAG Skill 的默认组合"）
2. `SkillDispatcher::dispatch(request_id, dir_path, query, messages, attached_skills, cancel)` 为未来扩展唯一入口，新增能力 = 新增 Skill 注册，不改调度核心
3. Agent 构建：preamble 追加命中的技能正文（总量受 `MAX_CONTEXT_CHARS` 约束，按优先级截断）；动态工具仅挂载白名单并集；检索参数按技能覆盖
4. 超时熔断复用 `TaskRegistry` 的 `CancellationToken`；非流式降级复用现有 `complete_fallback`
5. 工具轨迹事件增加 `skill_id` / `exec_state` 字段，前端展示技能来源与状态机

### 6.2 前端（index.html + adapters）

**适配器**：新增 `tauri/src/adapters/skill.js`（封装 skill_* 命令 + `skill:changed` 事件订阅），在 `adapters/index.js` 并行加载。

**入口**：新增主视图容器 `skill-container`（与 `chat-container`、`knowledge-container` 平级，加入 `MAIN_VIEW_CONTAINERS` 切换集合）。

**布局（左 40% / 右 60%）**：

```
skill-container
├── skill-sidebar（flex 0 0 40%，flex-direction: column）
│   ├── skill-search-box              ← 搜索框（样式/交互与 #file-tree-search-box 一致）
│   │   ├── skill-search-input        ← id=skill-search-input，placeholder="🔍按下回车搜索技能..."
│   │   ├── skill-search-clear        ← 清除按钮
│   │   └── skill-scope-select        ← 作用域筛选（复用 #level-select 样式：全部/系统/全局/项目）
│   ├── skill-list                    ← 技能列表（滚动区）
│   └── skill-footer
│       └── skill-create-btn          ← "新增 Skill"（主色按钮）
└── skill-main（flex 1，滚动区）
    ├── skill-empty-state             ← 空态（选择/新建引导）
    ├── skill-detail-view             ← 详情视图（参考 skill.html 右侧）
    └── skill-edit-view               ← 编辑视图
```

**左侧布局（行布局）**：

1. **第一行 = 搜索框**：完全复用主界面文件搜索框（`#file-tree-search-box` / `#file-search`）的**样式**（1px `var(--color-primary)` 边框、`var(--radius-md)` 圆角、`:focus-within` 高亮描边）与**交互逻辑**（`handleSearchKeydown` 语义：回车过滤、Esc 清空、空值退格清空、`clear-search` 清除按钮；搜索逻辑为名称/描述/触发关键词子串匹配，大小写不敏感，防抖渲染）；右侧追加作用域下拉（复用 `#level-select` 外观）
2. **第二层 = 技能列表**：行 = 图标 + 名称 + 副行（作用域标签 + 触发方式/优先级）+ 启用状态圆点；行交互参考 skill.html 的 `.skill-list-item`（hover/active 态），但颜色/圆角/字号取 index.html 现有变量；空结果提示"没有匹配的技能"
3. **最下层（footer）= "新增 Skill"按钮**：通栏主色按钮（复用现有 `.btn` / 主色变量），点击进入编辑视图（scope 默认当前目录 = project）

**右侧布局（参考 skill.html）**：

- 空态：居中图标 + "选择一个技能查看详情" + 创建按钮
- 详情视图（`skill-detail-view`）：
  - 头部：图标 + 名称 + 启用/禁用标签 + 操作按钮（测试/编辑/删除；系统内置只读时仅展示，无编辑/删除）
  - Meta 行：作用域、优先级、触发方式与关键词、token 预算、超时时间、版本、更新时间
  - "技能描述"块（左侧竖条强调）
  - "入参 Schema"表格（参数名/类型/必填/描述/默认值，参考 skill.html `.params-table`）
  - "互斥"与"工具白名单"标签区
  - "快速测试"面板（输入 JSON → 调用 `skill_match` + 执行，展示状态机结果；参考 skill.html `.test-panel`）
- 编辑视图（`skill-edit-view`）：表单覆盖全部 Schema 字段（名称/id/作用域/优先级/触发规则/互斥/token预算/入参 schema/输出格式/超时/工具勾选/检索参数/启用）+ Markdown 指令正文编辑（复用现有编辑器能力）；保存前调 `skill_validate` 实时校验

**样式与命名约束（强制）**：

- 尽量复用 index.html 现有样式（`--color-*` 变量、`.btn`、`.modal-overlay`、标签/表格/表单类）；非必须**不新增组件级 CSS**；skill.html 仅作布局与信息架构参考，不照搬其独立 CSS
- **全部 style ID 与 JS 常量/方法名加 `skill` 前缀**：如 `skill-container`、`skill-search-input`、`skillCurrentId`、`skillRenderList()`、`skillRenderDetail()`、`skillRenderEdit()`、`skillSave()`、`skillDelete()`、`skillRunTest()`、`skillFilterByQuery()`、`skillScope` 等，与现有业务隔离，避免冲突
- 事件：新增 `skill:changed` 订阅（注册表变更 → 前端自动刷新列表/挂载 chips）

**对话挂载 UI**：聊天输入区上方新增技能 chips 选择器（复用现有 `chat-config-bar` 风格），显示已挂载技能（可移除），点击弹出候选列表（仅 `enabled` 且作用域可达者）。

---

## 7. 非功能需求与约束

### 7.1 安全

- **只读边界**：v1 不引入脚本执行；工具仅限 5 个内置只读工具，沿用 `safe_resolve` 防路径穿越
- **Schema 校验**：严格校验字段类型与枚举（`tools`/`scope`/`output_format`），白名单策略拒绝未知字段，防止提示词注入绕过工具声明
- **导入安全**：导入的 SKILL.md 视为不可信输入，校验后落盘；路径扁平化，禁止 `../` 逃逸
- **角色预留**：`roles` 字段登记不生效，v2 再接入权限模型

### 7.2 性能

- 注册表读写分离：读不锁写；扫描仅在启动/变更时
- 上下文预算：技能正文注入总量受 `MAX_CONTEXT_CHARS`（12000 字符）约束
- 挂载数量 ≤3；信号量限制并发 LLM 请求；缓存降低重复解析与 LLM 调用

### 7.3 兼容与迁移

- 三层目录任一不存在按空处理，不影响现有功能
- 旧版 `mdgo.db` 无新表时自动建表（`CREATE TABLE IF NOT EXISTS` + 列迁移模式）；建表统一收敛到 `schema.rs`
- SKILL.md 兼容 Claude Code 子集，后续可做"导入 Claude Code 技能目录"

### 7.4 后续演进（v2 候选）

- 有状态 Skill 池化与串行依赖编排（§4.6 扩展接口落地）
- 角色权限体系落地（`roles` 生效）、Token 计费（`token_budget` 生效）
- 技能模板市场（内置模板：综述/代码定位/周报/读书笔记）、技能测试台完善
- Skill 引用外部文件（`reference.md`）与脚本（隔离沙箱，需充分评估安全边界）
- 指标看板页（复用 `skill_metrics` 聚合接口）

---

## 8. 里程碑

| 阶段 | 内容 | 交付物 |
|---|---|---|
| M1 基础管理 | `schema.rs`（建表+种子）、SkillStore/Registry、skill_* 管理命令、技能管理 UI（列表/搜索/新建/编辑/删除/启停/校验）、系统内置打包 | 可管理的三层技能版本 |
| M2 匹配与调度 | 分层意图匹配（L1/L2/L3）、SkillDispatcher、超时熔断/重试/降级/状态机、会话挂载与指令/工具注入、`chat_session_skills` 快照 | 技能可驱动对话 |
| M3 观测与完善 | 全链路日志、`skill_metrics` 聚合接口、动态启停广播、导入/导出、`/技能名` 手动触发、轨迹标注、Watcher 热更新联动 | 完整闭环 |
| M4（v2） | 实例池/缓存/信号量细化、串行依赖编排、模板市场、角色权限 | 企业级增强 |

---

## 9. 验收清单（摘要）

- [ ] 三层目录（系统/全局/项目）扫描正确，同名 id 按优先级覆盖，重启后注册表完整
- [ ] 新建技能后列表可见、可编辑、可停用、可删除；系统内置只读；重启后状态保持
- [ ] 非法 frontmatter 无法保存/导入，错误信息定位到字段；未知字段被拒绝
- [ ] 意图匹配：命中 L1 关键词、L2 语义、L3 兜底均可在调试接口返回 `(level, score)`；未命中不执行任何 Skill
- [ ] 互斥列表生效：互斥技能不同时入选，被剔除项写入日志
- [ ] 超时熔断生效：超时 Skill 被取消，链路不阻塞；连续熔断进入冷却并返回降级
- [ ] 重试分级生效：网络类错误指数退避重试，参数类错误直接失败；失败返回标准化错误结构
- [ ] 状态机：每次执行产生 pending→running→success/failed/degraded 迁移，链路可追踪（request_id 关联）
- [ ] `skill_metrics` 返回命中率/成功率/耗时/token/错误码分布聚合数据
- [ ] 线上 `skill_set_enabled` 即时生效，前端收到 `skill:changed` 自动刷新
- [ ] RAG 会话挂载技能后，回答遵循指令、工具白名单生效（未声明工具不被调用）、检索参数被覆盖
- [ ] 挂载技能的会话重开后挂载状态恢复（版本快照）
- [ ] UI：左 40%/右 60% 布局；搜索框样式与交互同主界面文件搜索框；所有 ID/JS 名称带 skill 前缀
