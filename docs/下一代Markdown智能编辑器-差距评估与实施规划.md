# 下一代 Markdown 智能编辑器：差距评估与实施规划（mdgo · Tauri）

> 最后更新：2026-08
>
> 定位：本文是「下一代 Markdown 智能编辑器」目标的**差距评估 + 计划方案 + 技术实现方案**。
> 评估对象：**Tauri 应用（`main.html` + `tauri/src-tauri` 后端）**，即当前唯一在做的形态；
> 浏览器直开版（`index.html`）不在本规划范围内。
> 依据：对 `main.html`（约 5.2 万行内联 JS）、`css_js/modules/*`、`tauri/src-tauri/src/*`、
> `docs/` 存档标准的源码级调查。文中函数/命令均附位置，以当前代码为准。

---

## 0. 结论摘要（TL;DR）

**mdgo 不是从零开始：它已经拥有 2026 级知识库工具最稀缺的底座**——全本地混合检索
（向量+BM25+符号路+精排+证据校验）、自研 Agent 内核（LoopAgent + 30+ 工具 + Skill + MCP +
审批门）、事件溯源会话、文件监听增量索引、SQLite/向量库/JSON 分层的完整存储。这是绝大多数
竞品（Typora/Obsidian/Notion）都没有的能力。

**真正的短板集中在「编辑器内核」与「AI 与编辑器的深度融合」**：

| 维度 | 现状 | 缺口 |
|---|---|---|
| 编辑内核 | Monaco 源码编辑 + 分屏实时预览 | 无 WYSIWYG、无块编辑、无 / 菜单、无 @ 引用、无 [[ 补全、无专注模式、无拼写检查（命令面板已裁剪） |
| AI 内联 | 选区 AI（解释/翻译/总结/重写）→ **弹窗展示** | 结果不内联应用；无幽灵文本续写；无 AI 块（/ai 命令已裁剪） |
| AI 语义层 | 双链图谱/语义检索已有 | 无 [[ 语义推荐、无反向链接提示、无文档完成关联提示 |
| 知识底座 | RAG/Agent/Skill/MCP 全齐 | 外部知识源（Notion/Drive/GitHub/Zotero）未接（可借 MCP 补） |

**核心战略判断**：不要推翻现有架构重写编辑器，而是**以「编辑器内核分层」为轴，做三期渐进升级**：

1. **P0（地基）**：在现有 Monaco 体系上加"智能输入（/ @ [[）+ 预览联动 + 专注模式 + 导出 +
   Markdown 模块抽取"，不动内核；
2. **P1（体验跃迁）**：引入 **CodeMirror 6** 作为 `.md` 的新默认内核，实现**所见即所得**与
   **内联 AI 全链路**（选区 AI 就地生效 + 幽灵文本续写 + AI 生成原生化）；
3. **P2（原生 AI）**：引入 **ProseMirror/TipTap** 做**块编辑**（Notion 式），上线 AI 块、
   语义推荐、自动整理、拼写检查。

Monaco 保留为"源码模式"（代码文件、JSON、大文件场景），三种模式共享同一套
「文档模型 + 保存管道 + 渲染管线 + AI 服务」，这是本方案的核心架构决策。

### 需求裁剪（本规划明确不做）

| 排除项 | 说明 |
|---|---|
| 命令面板 Ctrl+K | 不做全局命令面板；AI/导出等操作入口改走工具栏、右键菜单与 `/` 菜单 |
| 演示功能（演示模式） | 不做、不集成现有「演示」按钮（`toggleDemoMode` main.html:46857）相关能力 |
| 禅模式 / 全屏 | 专注模式仅保留**打字机滚动 + 聚焦段落**两项，不做隐藏侧栏/状态栏（Ctrl+\）与全屏 |
| /ai 自然语言命令（原 P1-5） | 不做编辑器内自然语言命令；自然语言任务仍可通过现有 Agent 聊天面板完成 |
| 代码块执行（原 P1-6） | 不做 JS/Python 代码块「运行」能力（安全面最小化）；代码块仍可编辑与高亮渲染 |

### 全案完成状态（2026-08 目标推进验收）

> **P0 / P1 / P2 三期全部实现并交付**（除上表裁剪项 + P1-1 所见即所得已按用户要求删除）。
> 实施记录见各工作项下「实施状态」注释。交付物清单：
>
> | 期 | 工作项 | 交付 |
> |---|---|---|
> | P0-1 | 智能输入 | `editor/suggest.js`：/ 块菜单、@ 文件、[[ 补全（+语义）、# 标题提示 |
> | P0-2 | 预览联动 | 同步滚动/光标联动/双链跳转/标题锚点（main.html 内联 + markdown.css） |
> | P0-3 | 专注模式 | `editor/focus-mode.js`：打字机 Ctrl+Shift+T、聚焦段落 Ctrl+Alt+P |
> | P0-4 | 导出 | `editor/export.js`：复制 MD / 导出 HTML / 打印 PDF（footer 按钮） |
> | P0-5 | 性能 | 启动埋点 `[mdgo-perf]` + 大文件保护提示 |
> | P0-6 | 原子写 | Rust `write_file_atomic`（temp+rename）+ 适配器接入 |
> | P0-7 | 模块抽取 | `markdown.js`/`markdown.css`（CSS 内联已移除，main.html 净 -3,600+ 行） |
> | ~~P1-1~~ | ~~WYSIWYG~~ | **已删除**（用户要求移除所见即所得；`wysiwyg.js`/`cdn/cm6/` 已删，block.js 自建 monaco-host） |
> | P1-2 | 文档模型 | `editor/core.js`：`MdgoDocument`（getValue/replace/insertAt/save...） |
> | P1-3 | 内联 AI | `editor/ai-inline.js`：17 动作就地应用（可撤销） |
> | P1-4 | 幽灵续写 | `editor/ghost.js`：Ctrl+J 触发 / Tab 接受 / Esc 拒绝 |
> | P1-5 | AI 原生化 | AI 结果弹窗「插入到文档」 |
> | P2-1 | 块编辑 | `cdn/tiptap/tiptap.bundle.js` + `cdn/tiptap/turndown.js` + `editor/block.js` |
> | P2-2 | 块转数据库 | `editor/db-view.js`：表格 → 筛选/排序数据库视图 |
> | P2-3 | AI 块 | `editor/ai-blocks.js`：summary/todos/semantic-search/tags 动态块 |
> | P2-4 | 语义推荐 | `editor/semantic.js`：[[ 语义候选 + 反链面板 + 关联提示 |
> | P2-5 | 智能整理 | `editor/organize.js`：目录/标签/去重/周报月报/归档 |
> | P2-6 | 拼写检查 | 块模式 TipTap 原生拼写（editorProps spellcheck） |
> | P2-7 | 幻灯片 | `editor/slides.js`：--- 分页 + 全屏放映 + 打印导出 |
>
> 验证：12 个 editor 模块 + markdown.js + 适配器语法全过；模块逻辑冒烟测试
> （suggest/focus/export/ghost/inline-ai/ai-blocks/semantic/organize/slides/
> db-view）30+ 用例通过；Rust `write_file_atomic` cargo check 通过；`cargo test --lib`
> 回归状态见各轮记录。

---

## 1. 现有项目基石盘点（现状评估）

> 本节逐项对照需求清单，给出"已有 / 部分 / 无"评级。行号以 `main.html` 与
> `tauri/src-tauri/src/` 当前代码为准。

### 1.1 总体架构

```text
┌─ 前端（main.html 5.2 万行内联 JS + css_js/modules/*）────────────────────┐
│  视图切换器（文件/仪表盘/聊天/日程/看板/图谱/Git/技能/MCP/书签...）        │
│  编辑层：Monaco（源码）+ 实时预览（Monaco|渲染 50/50 分屏）               │
│  渲染层：marked + DOMPurify + katex + highlight.js + mermaid/echarts/     │
│          flowchart/sequence/plantuml + parseObsidianToHTML（双链/callout）│
│  AI 层：callAIAPI（直连 LLM，非流式）· kb_llm_query（Rust 流式）· 选区工具条 │
└──────────────┬───────────────────────────────────────────────────────────┘
               │ window.__TAURI__.core.invoke / event.listen（rag:*/llm:*/agent:*）
               │ + WebSocket 前端桥（FrontendBridge，pomodoro/raw-parse/open-ui）
┌──────────────▼───────────────────────────────────────────────────────────┐
│ Rust 后端（tauri/src-tauri/src）                                          │
│  命令面：fs/knowledge(kb_*)/chat/llm/agent/skill/mcp/schedule/prompt/     │
│          bookmark/ai_history/git/system/clipboard/approval/question/plan  │
│  Agent 内核：core/loop（LoopAgent turn/step + Hook + Tool + 事件溯源）     │
│  Skill 引擎：15 个技能（kb-search/kb-summary/note-writing/canvas/...）    │
│  检索栈：query_plan → 向量‖BM25‖符号 → RRF → 阈值+精排 → 多样性 → 上下文   │
│  存储：SQLite（mdgo.db/memory.db/embedding_cache.sqlite）+ LanceDB +      │
│        tantivy bm25 + .mdgo/*.json（索引/历史/图谱/标记）+ setting.json   │
└───────────────────────────────────────────────────────────────────────────┘
```

### 1.2 编辑体验现状（对照需求第 1 章）

| 需求项 | 现状评级 | 现状详情（位置） |
|---|---|---|
| 极速启动 | 🟡 部分 | 文件树有懒加载/节点索引/展开状态缓存（`renderDirectory` main.html:17750、`buildTreeDataFromScan`:35302）；文件内容有 LRU 缓存（`FILE_TEXT_CACHE`:17795）；但 Monaco 全量同步加载、渲染管线每次全量重渲，数千文件库/大文档体验待实测 |
| 源码模式 | ✅ 已有 | `createMonacoEditor`（main.html:18452）：语法高亮、折叠（`unfoldOnClickAfterEndOfLine`）、括号匹配、minimap、大文件优化（`LARGE_EDITOR_SIZE` 10MB:18232）、Ctrl+S 保存（:18274） |
| 所见即所得模式 | ❌ 无 | 仅「实时预览」分屏（`enterLivePreviewMode`:18128，Monaco 左 + 渲染右，300ms 防抖:18171），无光标/滚动联动、无就地编辑 |
| 块编辑模式 | ❌ 无 | Markdown 无块模型；有 canvas（JSON Canvas，`css_js/modules/canvas.js`）与看板（`parseMarkdownKanban`），但不是 Markdown 块 |
| / 块菜单 | ❌ 无 | 无；`NEW_FILES` 新建文件下拉（:16860 附近）是菜单但不是块菜单 |
| @ 引用 | ❌ 无 | 无 |
| [[ 双链 | 🟡 渲染有 | `parseWiki`（main.html:26597）/`parseObsidianToHTML`（:26604）渲染 `[[...]]`、`![[...]]`、`#tag`；但编辑时**无补全、无语义推荐、无跳转**（预览内有 `.ob-internal-link` 可点击） |
| Markdown 即时渲染 | ❌ 无 | 编辑器内输入 `# ` 等无即时转换（仅预览模式看到效果） |
| 数学公式 | ✅ 已有 | katex（index.html/markdown 渲染 `$$...$$`/`$...$`，Obsidian 转义 :26663） |
| 图表 | ✅ 已有 | mermaid/echarts/flowchart/sequence/plantuml（`postProcessMarkdown` + 各渲染器，main.html 大量用例） |
| 代码高亮 | ✅ 已有 | Monaco 编辑高亮 + highlight.js 渲染高亮 |
| 代码块执行 | ❌ 无 | 无（JS/Python 执行）；**已裁剪不做** |
| 表格/任务列表/脚注/front matter/callout | ✅ 已有 | marked GFM（`marked.setOptions`:17695）+ `parseObsidianToHTML`（callout :26633、脚注 :26654）+ frontmatter 被检索链路消费（`pipeline.rs` 解析 tags） |
| 幻灯片片段 | ❌ 无 | 无 |
| 专注模式 | ❌ 无 | 无打字机/聚焦段落（有空闲检测 `isIdle`:18202 与演示模式，但非专注写作）；**禅模式/全屏已裁剪不做** |
| 拼写/语法检查 | ❌ 无 | 无 LanguageTool/Grammarly 式检查 |
| 命令面板 Ctrl+K | ❌ 无 | 无（有工具栏按钮体系 `setControlsState`:32887、`initContextMenus` 右键菜单 :16800 附近）；**已裁剪不做** |
| 导出 | 🟡 部分 | 仅导出图片（Mermaid/思维导图/Markdown 渲染截图 :28152）；无 PDF/HTML/Markdown 文件导出 |

### 1.3 AI 能力现状（对照需求第 2 章）

| 需求项 | 现状评级 | 现状详情（位置） |
|---|---|---|
| 选区 AI 菜单 | 🟡 部分 | 选中文本浮出工具条（`showMarkdownSelectionToolbar`:36435），含**高亮/标注/清除/搜索/AI**；AI 子菜单 = `AI_SELECTION_ACTIONS`（:17022）：解释/翻译/总结/重写/语法检查/润色等，prompt 质量高；**选区结果走 `showAIResultModal` 弹窗（:36736）不就地替换**；例外：文件级「排版」非选区模式时直接进编辑器（`enterEditMode(false, processedResult)` :32852） |
| 继续写/改写/扩写/缩写/润色/语气 | 🟡 部分 | 重写（rewrite）有；继续写/扩写/缩写/语气切换无 |
| 提取待办/转表格/转列表/转代码/生成摘要/要点 | 🟡 部分 | 总结/要点有；提取待办/转表格/转列表/转代码无（但 Agent 会话可自然语言完成） |
| 生成 Mermaid/表格 | 🟡 部分 | 文档级 PROMPTS 有 Mermaid/drawio/Excalidraw 生成（:16830-16846），选区级无 |
| 幽灵文本续写（Tab/Ctrl+J） | ❌ 无 | 无 |
| 风格模仿 | ❌ 无 | 无 |
| /ai 自然语言命令 | ❌ 无 | 编辑器内无；Chat/Agent 面板（chatMode=rag）已可执行"总结会议记录/提取行动项"等任务（`sendRagQuery` agent.js），`PROMPTS`（main.html:16801）提供文档级 总结/分析/排版/画图 按钮；**编辑器内 /ai 已裁剪不做** |
| 动态 AI 块 | ❌ 无 | 无（动态摘要/待办提取/语义搜索/自动标签块均无） |
| AI 生成内容为原生块 | 🟡 部分 | AI 结果可"保存为文件/追加到文件"（`saveToFile` :48978 双链追加），但不是编辑器原生块 |
| 知识库级 RAG 问答 | ✅ 强 | 全本地混合检索（见 §1.4）+ 流式问答（`kb_llm_query`/`agent_query`）+ **引用来源展示与溯源跳转**（`renderChatSources`:49186、来源定位:49300）+ 来源双链追加文件（:48978）+ 证据校验（`core/evidence.rs`，默认关可配）+ RAG 参数设置 UI（top-k/min_score/chunk_size :15944） |
| 外部知识源 | 🟡 部分 | 本地知识库+书签+聊天会话全索引；Notion/Google Drive/GitHub/Zotero 未直接接，但 **MCP 已可用**（`mcp_list/connect/test` 等命令 + `McpTool` 注册进 Agent），GitHub 仓库数据实际已在知识库内 |
| 语义搜索 | ✅ 已有 | `kb_search_hybrid` 命令 + `kb_search` 工具 + 语义补全等；全局文件树也有搜索 |
| [[ 语义推荐 | ❌ 无 | 无（仅渲染） |
| 反向链接/相关标签/相关段落推荐 | ❌ 无 | 有 `index_link_graph.json` 与图谱页（`openFileGraph` :40662 附近），但无编辑器内自动推荐 |
| 文档完成关联提示 | ❌ 无 | 无 |
| 自动目录/大纲/标题层级 | 🟡 部分 | 有文件总览面板（`renderFileSumPanel`:31457）与大纲思维导图技能（outline-mindmap）；无"自动生成目录/修复层级"的编辑器命令 |
| 自动标签/分类/双链 | 🟡 部分 | 检索消费 frontmatter tags；Agent 技能可写；无编辑器内一键自动打标 |
| 去重/重组/周报月报/归档 | 🟡 部分 | 能力分散在 Skill/Agent 工具面（write/edit/grep/read 可做），无专门命令 |

### 1.4 知识底座（这是本项目最强的基石）

| 能力 | 现状 |
|---|---|
| 混合检索 | `query_plan`（意图路由：Code/Document/Outline/General + 标签提取）→ 三路召回（LanceDB 向量 ‖ tantivy BM25 msm 严格语义 ‖ 代码符号路）→ **RRF 融合** → 三阈值 + **本地 cross-encoder 精排**（bge-reranker-base ONNX，Windows DirectML/macOS CoreML/Linux tract）→ 多样性（文件聚簇/轻量 MMR/概览降权）→ 上下文窗口（≤12k chars）→ 证据校验（`docs/混合检索技术设计.md`） |
| 降级策略 | 检索永不阻断：精排失败回退 RRF、向量失败退 BM25、模型未缓存后台下载（FAILOVER-1~5） |
| 缓存 | embedding 磁盘缓存（SQLite 10 万条）+ 查询向量缓存（512）+ 精排分数缓存（2048） |
| 索引 | chunk 448/56 token（token 预算 + `TokenBudgetValidator`）、jieba 分词、frontmatter tags/aliases 入列、watcher 2s 防抖增量（`core/watcher.rs`）、聊天会话也入索引（chat_vectors） |
| Agent 内核 | `core/loop`：LoopAgent turn/step 状态机、LoopHook 四组（SkillGate/Approval/SkillInstruction）、Tool 体系（ToolSpec + 并行调度器 + exclusive barrier）、事件溯源 Session、OpenAI/Anthropic 双 LlmAdapter（`docs/Agent 内核架构.md`） |
| 工具面 | BASE_TOOLS 25+（kb_search/code_lookup/read/grep/ls/glob/write/edit/multi_edit/delete/git_*/remember/forget/search_memory/todo_write/spawn_subagent/deep_research/webfetch/self_review/ask_user_question/schedule/...）+ MCP 工具 + 外部 HTTP 工具 + BridgeTool（pomodoro/raw-parse/open-ui） |
| Skill | 15 技能：kb-search/kb-summary/note-writing/canvas/kanban/mermaid/outline-mindmap/schedule/web-research/repo-status/code-lookup/open-ui/pomodoro/raw-photography/bookmark；frontmatter（triggers/tools/params）驱动，内存注册表 + 软门禁 |
| 存储 | SQLite（每知识库 mdgo.db：会话/消息/session_events/prompts/技能指标；系统级 memory.db）+ embedding_cache.sqlite + LanceDB + tantivy + .mdgo/*.json + setting.json（`docs/数据存储评审与优化落地.md`：O1/O2/O3/O5/O6 已落地） |
| 前端桥 | `frontend-bridge.js` WebSocket 双向通道（Rust 工具闭包 → 前端 handler），自动重连 + 就绪门控；事件通道全集（`rag:*`/`llm:*`/`agent:*`/`approval:request`/`plan:request`/`question:request`/`trace:event`/`kb-watcher-event`/`skill:changed`/`schedule:*` 等，Rust `app.emit` → 前端 `listen`）——**编辑器状态同步可直接复用同一通道约定** |

### 1.5 已具备 → 目标 差距总表（需求逐条）

| 需求编号 | 需求项 | 差距 | 归属期 |
|---|---|---|---|
| 1.1 | 极速启动 | 需实测 + 优化启动与渲染管线；大文件读入为整文件 IPC（见 §4.8 存储侧缺口） | P0 |
| 1.2a | 源码模式 | ✅ 达标（Monaco；但未注册自定义 markdown 语法/折叠） | P0（可选增强） |
| 1.2b | 所见即所得 | **缺**（引入 CM6） | P1 |
| 1.2c | 块编辑 | **缺**（引入 TipTap） | P2 |
| 1.3a | / 块菜单 | **缺** | P0 |
| 1.3b | @ 引用 | **缺** | P0 |
| 1.3c | [[ 双链与语义推荐 | 渲染有、编辑无 | P0（补全）/ P2（语义） |
| 1.3d | Markdown 即时渲染 | **缺** | P1 |
| 1.4a | 数学/图表/代码高亮 | ✅ 达标 | — |
| 1.4b | 代码块执行 | **需求裁剪：不做** | — |
| 1.4c | 表格/任务/脚注/front matter/callout | ✅ 达标 | — |
| 1.4d | 幻灯片片段 | **缺** | P2 |
| 1.5 | 专注模式（打字机/聚焦段落） | **缺** | P0 |
| 1.6 | 拼写/语法检查 | **缺** | P2 |
| 1.7 | 命令面板 Ctrl+K | **需求裁剪：不做** | — |
| 2.1 | 内联 AI 操作 | 部分（选区→弹窗不内联；文件级排版可进编辑器） | P1 |
| 2.2 | 幽灵文本续写 | **缺** | P1 |
| 2.3 | /ai 自然语言命令 | **需求裁剪：不做**（Agent 聊天面板可完成等价任务） | — |
| 2.4 | 动态 AI 块 | **缺** | P2 |
| 2.5 | 知识库级 RAG 问答 | ✅ 强（引用闭环） | — |
| 2.6 | 语义搜索/推荐 | 搜索✅、推荐缺 | P2 |
| 2.7 | 智能整理自动化 | 部分（技能可做、无命令化） | P2 |

---

## 2. 目标架构

### 2.1 分层架构

```text
┌────────────────────────── 前端（main.html + css_js/modules/*）─────────────┐
│ ┌─ 视图层 ──────────────────────────────────────────────────────────────┐ │
│ │ 文件树 │ 编辑区（三模式宿主） │ AI 面板/聊天 │ 知识图谱 │ 设置 │        │ │
│ └───────────────┬───────────────────────────────────────────────────────┘ │
│ ┌─ 编辑内核层（本方案核心新增）───────────────────────────────────────────┐ │
│ │ 模式路由器：源码(Monaco) ‖ 所见即所得(CM6) ‖ 块(TipTap)                 │ │
│ │ 统一文档模型：Markdown 文本 ⇄ 块树（CM6 语法树 / TipTap JSON）           │ │
│ │ 统一保存管道：防抖 → 原子写（enqueueFileAtomic 已有）→ 增量索引通知       │ │
│ │ 统一渲染管线：css_js/modules/markdown.js（marked + Obsidian 语法 +      │ │
│ │   postProcessMarkdown）· 样式 css_js/modules/markdown.css（P0-7 抽取）  │ │
│ └───────────────────────────────┬────────────────────────────────────────┘ │
│ ┌─ AI 原生层（新增）─────────────────────────────────────────────────────┐ │
│ │ 内联 AI 菜单（选区→动作→就地应用/替换/插入）                            │ │
│ │ 幽灵文本续写（Tab/Ctrl+J → 虚文本 → 接受/拒绝/继续）                    │ │
│ │ / 菜单 ‖ @ 引用 ‖ [[ 语义补全（统一 suggestion provider）               │ │
│ │ AI 块运行时（动态摘要/待办/语义搜索/标签 的块渲染与刷新协议）             │ │
│ └───────────────────────────────┬────────────────────────────────────────┘ │
└─────────────────────────────────┼──────────────────────────────────────────┘
                                  │ invoke / event（rag:*/llm:*/agent:*/editor:*）
┌─────────────────────────────────▼──────────────────────────────────────────┐
│ Rust 后端（tauri/src-tauri/src）                                            │
│ 新增：editor 命令组（块级读写/关系查询/补全建议/续写/自动整理）               │
│ 复用：kb_search_hybrid（语义）· kb_llm_query（流式）· Agent 工具面           │
│       （write/edit/grep/read/remember/...）· Skill · MCP · 索引器 · watcher │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 2.2 核心架构决策

| 决策 | 选择 | 理由 |
|---|---|---|
| D1 编辑内核 | **三层内核共存**：Monaco（源码/代码文件）+ CodeMirror 6（.md 所见即所得）+ ProseMirror/TipTap（.md 块模式） | 单内核无法同时满足"代码编辑"与"WYSIWYG/块编辑"；三层共享同一文档模型与保存管道，避免 Notion/Typora 各自方案的取舍 |
| D2 统一文档模型 | Markdown 文本为唯一事实源（source of truth）；CM6/TipTap 的块结构是**可序列化的视图层**，任何时刻可双向转换 | 保持文件系统兼容（用户文件仍是 .md）、检索/版本控制/Git 不变；块 JSON 仅作视图状态或 `.mdgo/blocks/` 副档 |
| D3 智能输入实现 | 复用编辑器原生的补全体系（Monaco `registerCompletionItemProvider` / CM6 `autocomplete`）+ 自定义悬浮 UI（/ 菜单） | 不重造编辑器，纯扩展 |
| D4 AI 服务路由 | 新增统一 `EditorAI` 服务：短动作（改写/翻译/续写）走**前端直连 LLM**（复用 `callAIAPI` 非流式/加流式）；复杂任务（总结会议/提取行动项/整理）走 **kb_llm_query（Rust 流式 + 工具）** | 短动作要低延迟就地生效；复杂任务要工具与 RAG 上下文，走 Agent 面 |
| D5 语义数据源 | 复用 `kb_search_hybrid` + 新增 `index_link_graph` 消费命令（反向链接/相关笔记）+ 块级检索（可选） | 全部基于现有 LanceDB/BM25 索引，零新存储 |
| D6 构建方式 | 延续「本地 vendored 库 + `css_js/modules/` ES 模块」模式；新增 `css_js/cdn/cm6/`、`css_js/cdn/tiptap/` 与 `css_js/modules/editor/*` | main.html 是单体内联脚本，无 npm 构建步骤；Tauri 打包静态资源；不引入 node_modules 运行时依赖 |
| D7 Markdown 业务模块化 | **现有 Markdown 业务逻辑（渲染管线/预览/选区工具条/Obsidian 语法/导出）整体抽取为 `css_js/modules/markdown.js` 单文件，样式抽到 `css_js/modules/markdown.css`**；main.html 只留入口与事件绑定（P0-7） | 用户明确要求；把 5.2 万行单体按业务域切开，P1 的 CM6/TipTap 与 AI 层直接消费模块接口，避免再次内联 |

---

## 3. 分期实施路线图

> 每期结束都交付**可日常使用的完整功能**，不做半成品。三期可独立验收、独立回滚
> （编辑器内核通过模式开关切换，Monaco 始终保留为兜底）。

### P0：编辑体验地基（建议 4–6 周）

**目标**：在不动内核的前提下，把"2026 年高水准"的**编辑交互骨架**立起来。

| # | 工作项 | 内容 | 验收标准 |
|---|---|---|---|
| P0-1 | 智能输入（源码模式） | Monaco 补全提供器：`/` → 块菜单（标题/表格/代码块/Mermaid/LaTeX/待办/看板/日期/AI）；`@` → 文件/标签/日期/联系人（扫 `_scanFileList` + frontmatter tags）；`[[` → 标题模糊补全 + 反向链接候选（`index_link_graph`）；`# ` 空格 → 标题语法提示 | 三个触发符在 .md 编辑中可用，插入正确语法 |

> **P0-1 实施状态（目标推进记录）**：
> - ✅ **v1（首版）**：`css_js/modules/editor/suggest.js` 已创建（`window.initMarkdownSuggest`，
>   Monaco 就绪后幂等注册 `registerCompletionItemProvider('markdown', ...)`，triggerCharacters
>   `['/','@','[','#']`）。已实现：`/` 块菜单 11 项（标题1-3/表格/代码块/Mermaid/LaTeX/待办/
>   看板/日期/AI 块，snippet 插入，支持 `/查询词` 过滤）；`@` 文件引用（运行时读 `_scanFileList`，
>   30 条上限）；`[[` 双链补全（.md 标题模糊匹配，别名去扩展名，20 条上限）；`# ` 标题语法提示。
>   main.html 已接线：Monaco require 回调（:17293）注册 + 底部模块加载（:51393）。
> - ✅ 冒烟测试通过（Node + vm mock）：行首 `/` 11 条、`/表` 过滤 1 条、URL/表格行内 `/`
>   不触发、`@a` 命中、`[[架` 命中架构.md、`[[` 空查询返回全部 md、`[[架构]]` 已闭合不触发、
>   `## ` 标题提示、普通文本不触发。
> - ⏳ 待办（后续轮）：`@` 增加 标签/日期 分类（需标签聚合缓存）；`[[` 反向链接候选
>   （读 `index_link_graph`，P2 升级语义推荐）；代码块内触发抑制；看板项插入语法与
>   `parseMarkdownKanban` 对齐验证。
| P0-2 | 实时预览增强 | 分屏同步滚动 + 光标↔渲染块联动；预览内双链点击 → 打开目标文件并定位；预览内标题锚点 | 编辑时预览跟随滚动；点双链打开文件 |

> **P0-2 实施状态（目标推进记录）**：
> - ✅ **v1**：① **同步滚动**：`enterLivePreviewMode` 内编辑器 `onDidScrollChange` ⇄ `#live-preview-render`
>   `scroll` 双向比例同步，`_liveSyncFromEditor/_liveSyncFromPreview` 标志防循环；② **光标联动**：
>   `onDidChangeCursorPosition` → 按行号比例滚动预览（40px 死区防抖）；③ **双链跳转**：
>   `postProcessMarkdown` 新增 `bindInternalLinks`（幂等绑定 `.ob-internal-link`，外部链接不拦截）+
>   `openFileByWikiLink`（三级匹配：精确 path → 文件名去扩展名 → path 去扩展名，走 `openFileFromPath`），
>   主内容渲染（:24621）/实时预览（:18492）/AI 弹窗（:32657）三处渲染全生效；④ **标题锚点**：
>   `bindHeadingAnchors` + `slugifyHeading`（中文保留 slug，唯一化），主内容与预览标题自动带 id。
> - ✅ 验证：main.html 3 内联脚本块 `node --check` 全过；wiki 链接解析 6/6 用例通过
>   （'架构'/'docs/架构.md'/'架构.md'/'a'/'2026-08-23'/'不存在'）。
> - ⏳ 待办（后续轮）：光标段落级联（光标行 → 最近标题块滚动，替代行号比例）；锚点点击复制链接；
>   编辑器滚动条隐藏时预览滚动条的视觉对齐。
| P0-3 | 专注模式（打字机+聚焦段落） | 打字机滚动（光标行居中）、聚焦段落（dim 其余行）；**不做禅模式/全屏（已裁剪）** | 两种专注态可切换，无干扰写作可用 |

> **P0-3 实施状态（目标推进记录）**：
> - ✅ **v1**：`css_js/modules/editor/focus-mode.js` 已创建（`window.initFocusMode`，幂等）。
>   ① **打字机**（Ctrl+Shift+T）：`onDidChangeCursorPosition` → `revealLineInCenter`；② **聚焦段落**
>   （Ctrl+Shift+P）：按空行分隔解析光标所在段落，`deltaDecorations` 段落行加 `mdgo-focus-line`
>   （轻微高亮）、段落外行加 `mdgo-focus-dim`（opacity 0.42 淡出）；>6000 行大文件降级为仅
>   高亮段落不做全行 dim；光标移动/内容变更防抖重算。样式已入 `markdown.css`。
>   main.html 接线：`createMonacoEditor` 末尾统一 `initFocusMode(editor)`（:18144）——所有
>   Monaco 实例（md/代码/工具页）均可使用；底部模块加载（:51499）。
> - ✅ 验证：focus-mode.js `node --check` 通过；Node 冒烟测试：命令注册键值正确（3124/3115），
>   聚焦段落 decoration 8 行文档 = 3 高亮 + 5 dim 正确。
> - ⏳ 待办（后续轮）：状态栏显示当前专注态；工具栏/右键菜单入口（当前仅快捷键）。
| P0-4 | 导出 | Markdown → HTML（渲染管线复用）/PDF（打印样式 + Tauri webview print 或 html2canvas 方案）/复制 Markdown；工具栏 + 右键菜单入口（**不做命令面板入口**） | 导出 PDF/HTML 打开正常，样式完整 |

> **P0-4 实施状态（目标推进记录）**：
> - ✅ **v1**：`css_js/modules/editor/export.js` 已创建。`window.mdgoExport`：① **copyMarkdown**
>   （复制当前文件 Markdown 源码，走现有 `copyToClipboard`）；② **exportHtml**（当前预览内容
>   快照 + fetch 内联 css_js 的 4 个样式文件 → 独立 HTML，所见即所得离线可用，Tauri
>   `dialog.save`+`write_file` 保存，非 Tauri 降级下载）；③ **exportPdf**（渲染到隐藏
>   `#mdgo-print-root` → `window.print()`，`@media print` 隐藏 UI 只显示内容，样式入
>   markdown.css）。入口：`initExportFooter()` 向编辑器 footer status-right 注入
>   复制MD/HTML/PDF 按钮组（幂等），enterEditMode（:17710）与 enterLivePreviewMode（:17766）
>   调用；底部模块加载（:51504）。
> - ✅ 验证：export.js `node --check` 通过；冒烟测试：copyMarkdown 复制成功、exportHtml 生成的
>   文档含 DOCTYPE/内联 CSS/markdown-body 容器、exportPdf 调起 window.print；main.html 3 内联
>   块语法全过；markdown.css 72/72 配对。
> - ⏳ 待办（后续轮）：HTML 导出可选"含图表重新渲染脚本"（当前为渲染态快照，mermaid SVG 已
>   内联、无需重渲染）；PDF 打印样式在不同视图（主内容/仪表盘）下的覆盖面核对。
| P0-5 | 启动与渲染性能 | 启动耗时埋点（`TREE_LOAD_TIME_KEY` 已有同款）；渲染管线增量式（只重渲变化区间，暂用节流+选区重渲）；大文档 Monaco 配置再优化；核实大文件读入路径（`read_file_binary` 整文件 IPC，10MB+ 文档考虑分块/流式读） | 5000 文件库目录树 < 1s；10MB md 打开 < 2s（实测基线化） |

> **P0-5 实施状态（目标推进记录）**：
> - ✅ **v1（埋点 + 保护）**：① **启动埋点**：主脚本开头记录 `MDGO_SCRIPT_START_TS`，
>   DOMContentLoaded → initAll 完成后 console 输出 `[mdgo-perf] 总启动 Xms（主脚本→initAll
>   完成），initAll Yms` 并写入 localStorage `mdgo_last_boot_ms`（与既有
>   `TREE_LOAD_TIME_KEY` 目录树耗时配合形成基线）；② **大文件保护**：`renderOriginalMarkdown`
>   对 >2MB 的 Markdown 提示"图表/公式渲染可能较慢，建议源码模式"；Monaco 10MB 阈值与大文件
>   降级（关 minimap/automaticLayout）为既有能力。live preview 300ms 防抖为既有能力。
> - ⏳ 待办（后续轮）：目录树 5000 文件 <1s / 10MB md <2s 的实测基线采集与优化（需真实
>   数据集跑分）；渲染管线增量式（当前仍为全量重渲 + 防抖）。
| P0-6 | 原子写补强（后端） | 新增 Rust 命令 `write_file_atomic`（temp + rename，同目录原子替换）与可选 `read_file_range`；前端保存管道改用原子写，消除"整文件覆盖写中途崩溃丢文件"风险 | 保存链路原子化；大文件可范围读 |
| P0-7 | **Markdown 业务模块抽取（JS + CSS）** | 把 main.html 中全部 Markdown 业务逻辑抽到 `css_js/modules/markdown.js`（渲染管线 `markedParse`/`markedMd`/`parseObsidianToHTML`/`postProcessMarkdown`/`renderMarkdownFile`、实时预览、选区工具条与高亮/批注、Obsidian 语法、双链/图谱提取、导出相关），样式抽到 `css_js/modules/markdown.css`（`.markdown-body`、`.ob-*`、`.live-preview-*`、`.markdown-selection-toolbar`、导出样式等）；main.html 只保留入口与事件绑定；**不改变现有行为**（纯重构，P1 CM6 接入的前置） | 行为回归通过（打开/编辑/预览/选区/导出全链路与抽取前一致）；main.html 中不再内联 Markdown 渲染/预览逻辑 |

> **P0-7 实施状态（目标推进记录）**：
> - ✅ **v1（首阶段）**：`css_js/modules/markdown.js` 已创建（`window.Markdown` 命名空间 +
>   兼容全局 `markedMd`/`parseWiki`，head 加载于 `marked.min.js` 之后）；`css_js/modules/
>   markdown.css` 已创建（.ob-*/选区工具条/实时预览/.markdown-body 三段逐字拷贝）；main.html
>   已接线（head 引用 css/js）并从内联脚本移除 `markedMd`（原 :28587）与 `parseWiki`
>   （原 :26597）定义，调用点（6+3 处）运行时解析到 window 全局；`node --check` 通过。
> - ✅ **v2（CSS 内联移除）**：main.html 内联 `<style>` 中三段 markdown 样式（原 :2215-2504 /
>   :7398-7444 / :11469-11530，共 399 行）已删除，样式全部由 `markdown.css` 提供；删除后
>   验证：无残留选择器定义、`</style>`/`</body>`/`</html>` 闭合正常、3 个内联 `<script>` 块
>   （含 1.74MB 主脚本）`node --check` 全部通过、外部 CSS 花括号 61/61 配对。
> - ⏳ 待办：继续迁入 `parseObsidianToHTML`/`postProcessMarkdown`/`renderMarkdownFile` 等
>   渲染管线函数；`escapeHtml` 待定（通用工具，被 100+ 处调用，评估是否入独立 utils）。

### P1：所见即所得 + 内联 AI（建议 6–8 周）

**目标**：编辑器体验跃迁到"Typora/Obsidian Live Preview"级，AI 从"弹窗"变"就地"。

| # | 工作项 | 内容 | 验收标准 |
|---|---|---|---|
| P1-1 | CM6 内核接入 | vendored CodeMirror 6 到 `css_js/cdn/cm6/`；`css_js/modules/editor/wysiwyg.js`：markdown 方言（GFM + Obsidian 语法解析复用）+ 装饰渲染（标题/粗斜体/任务列表/代码块/mermaid 等懒加载） | .md 默认进入 WYSIWYG；源码/所见即所得一键互切，光标与滚动双向保持 |

> **P1-1 实施状态：❌ 已删除（用户要求移除所见即所得全部逻辑）**。
> `css_js/modules/editor/wysiwyg.js` 与 `css_js/cdn/cm6/`（bundle）已删除；main.html 移除
> `initWysiwygToggle` 调用与模块加载；`block.js` 改为自行创建 `#mdgo-editor-monaco`
> 双容器（不再依赖 wysiwyg）；`markdown.css` 移除 `.mdgo-cm6-task` 样式。编辑体验收敛为
> **源码模式（Monaco）+ 块模式（TipTap）** 双模式。
| P1-2 | 文档模型统一 | `EditorDocument` 抽象：value ⇄ view 状态；保存管道统一（防抖→`enqueueFileAtomic` 已有→保存后触发 `kb_*` 增量索引与双链图谱更新） | 三模式编辑同一文件不丢内容不冲突 |

> **P1-2 实施状态（目标推进记录）**：
> - ✅ **v1（完成）**：`css_js/modules/editor/core.js` 的 `window.MdgoDocument`：
>   `getValue/setValue/onChange/getSelection/findFirst/replace/insertAt/save/editable`。
>   双容器方案下 Monaco 恒为事实源（WYSIWYG 编辑实时同步 Monaco），onChange 以 Monaco
>   `onDidChangeModelContent` 为唯一变更源，覆盖两模式；replace/insertAt 经
>   `executeEdits('mdgo-inline-ai', ...)` 入 undo 栈；save 统一走 `saveFileOnly`。
> - ✅ 验证：冒烟测试 getValue/getSelection/findFirst/replace/insertAt 全过（vm + mock Monaco）。

| P1-3 | 内联 AI 全链路 | 选区 AI 菜单升级：新增 继续写/扩写/缩写/润色/语气(专业/轻松/学术/简洁)/翻译(任意语言)/提取待办/转表格/转列表/转代码/摘要/要点/生成 Mermaid/生成表格；**结果就地应用**（替换/插入/新建块/转表格渲染） | 任一动作在编辑区就地生效，可撤销（undo 栈） |

> **P1-3 实施状态（目标推进记录）**：
> - ✅ **v1（完成）**：`css_js/modules/editor/ai-inline.js` 的 `window.MdgoInlineAI`：
>   17 个动作（继续写/扩写/缩写/润色/语气·专业/轻松/学术/简洁/翻译/提取待办/转表格/转列表/
>   转代码/摘要/要点/生成 Mermaid/生成表格），每个含 applyMode（replace/insert-below/
>   to-table/to-code）；`run()` 经 MdgoDocument 定位（编辑器选区 → 文本匹配 → 光标）→
>   `callAIAPI` → `applyResult` 就地应用（可撤销）；to-table 校验表格形态、to-code 补围栏。
>   main.html：选区工具条 AI 子菜单合并渲染内联动作（:36080），`aiMarkdownSelectionText`
>   路由到 `MdgoInlineAI.run`（:36429）。
> - ✅ 验证：端到端冒烟（vm + mock）to-table/to-code/continue 就地应用 PASS、undo 源标记
>   `mdgo-inline-ai` OK、17 动作清单完整；main.html 3 内联块语法全过。
> - ⏳ 待办（后续轮）：流式输出进度显示（当前非流式）；AI 结果就地 diff 高亮预览（可选确认）；
| P1-4 | 幽灵文本续写 | Tab/Ctrl+J → 光标处流式补全（CM6 `decorations` 虚文本；Monaco `setGhostText` 或 viewZone）→ 一键 Tab 接受 / Esc 拒绝 / 继续调整；可选"模仿当前文档风格"（取当前文档前 N 字符 + 风格 prompt） | 续写显示为灰字，接受/拒绝/再生可用 |

> **P1-4 实施状态（目标推进记录）**：
> - ✅ **v1（完成）**：`css_js/modules/editor/ghost.js`：`initGhostText(editor)` 注册
>   `registerInlineCompletionsProvider('markdown', ...)`（仅响应 `triggerKind === Invoke`
>   手动触发，自动触发返回空防浪费 LLM）；续写取光标前 4000 字符作上下文 →
>   `callAIAPI`（"继续写"prompt）→ 返回光标处 inline suggestion（Monaco 0.50 原生幽灵
>   文本：Tab 接受 / Esc 拒绝 / 再按 Ctrl+J 重新生成）；`editor.updateOptions({inlineSuggest})`
>   启用渲染。main.html `createMonacoEditor` 末尾（:18167）统一接入，幂等。
> - ✅ 验证：冒烟测试全 PASS——provider 注册 markdown、inlineSuggest 启用、自动触发返回空、
>   手动触发返回续写（range=光标处）、Ctrl+J → `editor.action.inlineSuggest.trigger`；
>   main.html 3 内联块语法全过。

| P1-5 | AI 生成原生化 | AI 生成的表格/Mermaid/代码块直接以原生块插入编辑区，可继续编辑（与渲染管线一致） | 插入后可编辑、可重渲染 |

> **P1-5 实施状态（目标推进记录）**：
> - ✅ **v1（完成）**：AI 结果弹窗（`showAIResultModal`）新增「插入到文档」按钮
>   （:14634）与 `insertAIResultToDoc()`（:32713）：经 `MdgoDocument` 将结果作为原生文本
>   插入当前文档（有选区 → 替换选区；无选区 → 光标处），入 undo 栈可撤销；插入后关闭弹窗
>   并聚焦编辑器，结果即刻可继续编辑（与渲染管线一致）。配合 P1-3 内联 AI（to-table/
>   to-code/insert-below）实现"AI 生成内容原生化"闭环。
> - ✅ 验证：main.html 3 内联块语法全过；插入逻辑复用已验证的 MdgoDocument.replace/insertAt。
| P1-5 | AI 生成原生化 | AI 生成的表格/Mermaid/代码块直接以原生块插入编辑区，可继续编辑（与渲染管线一致） | 插入后可编辑、可重渲染 |

> 原 P1-5（/ai 自然语言命令）与 P1-6（代码块执行）已按需求裁剪，不实现。

### P2：块编辑 + AI 原生深度（建议 8–12 周）

**目标**：达到需求全文覆盖，形成"Notion 块 + Obsidian 双链 + mdgo RAG"的差异化形态。

| # | 工作项 | 内容 | 验收标准 |
|---|---|---|---|
| P2-1 | 块编辑模式 | vendored TipTap（ProseMirror）+ `css_js/modules/editor/block.js`：块类型（标题/段落/表格/代码/数学/Mermaid/待办/看板/日期/引用/AI 块）；块拖拽排序/折叠/嵌入（`![[file]]` 块嵌入）；**块 ↔ Markdown 双向序列化**（块 JSON 仅视图态，Markdown 为事实源） | Notion 式块交互可用；切换源码模式后 Markdown 无损 |

> **P2-1 实施状态（目标推进记录）**：
> - ✅ **v1（内核 + 互切 + 序列化管线）**：
>   - `css_js/cdn/tiptap/tiptap.bundle.js`（575KB minified，esbuild IIFE → `window.TipTap`，
>     10 导出：Editor/StarterKit/TaskList/TaskItem/Table/TableRow/TableHeader/TableCell/
>     Image/DragHandle）；`css_js/cdn/tiptap/turndown.js`（UMD → `window.TurndownService`）；
>     `tiptap-entry.js` 为入口源码。
>   - `css_js/modules/editor/block.js`：`initBlockToggle(editorInner)` 创建第三容器
>     `#mdgo-editor-block`（隐藏叠加）+ footer「块模式」按钮；`toggleBlockMode()` 与 Monaco
>     源码互切（进入 = `marked` GFM → TipTap HTML；切回 = TipTap HTML → turndown → Markdown，
>     Markdown 为事实源）；懒加载 TipTap/turndown（首次切换才注入）；块拖拽（DragHandle）、
>     任务列表、可调整宽表格、图片。
>   - `markdown.css`：块编辑样式（标题/列表/任务/引用/代码/表格/图片/拖拽手柄 97/97 配对）。
>   - main.html：enterEditMode 三容器集成（:17731），底部加载 block.js。
> - ✅ 验证：TipTap bundle vm 加载 OK（10 导出全在）、turndown 全局 OK；marked→HTML 管线
>   （标题/加粗/列表/代码围栏/引用/GFM 表格）全对；block.js 与 main.html 3 内联块语法全过。
> - ⏳ 待办（后续轮）：块操作菜单（/ 菜单与拖动手柄菜单）；折叠；`![[file]]` 块嵌入；
>   round-trip 保真在 Tauri 运行时的量化回归（vm 沙箱缺 DOM，turndown 功能留运行时验收）；
>   与 WYSIWYG 三模式无冲突互切（当前经源码中转）。
| P2-2 | 块转数据库 | 表格块/列表块 → 数据库视图（筛选/排序/字段类型）；数据存于 `.mdgo/data/` 或 frontmatter 中，检索可索引 | 块可转库、可查可回 |

> **P2-2 实施状态（目标推进记录）**：
> - ✅ **v1（完成）**：`css_js/modules/editor/db-view.js`：`bindDbViews(dom)` 为渲染结果中
>   带表头（`<th>`）的表格添加「📊 数据库」按钮（main.html postProcessMarkdown :27135 接入）；
>   点击弹出数据库面板（`MdgoDbView.openDbView`）：列头点击排序（数值/字符串 localeCompare
>   中文）、筛选框实时过滤、行数统计、CSV 复制；数据仍来自表格内容（Markdown 为事实源，
>   视图不另存，原表格可被检索索引）。样式入 markdown.css。
> - ✅ 验证：parseTable（表头/行排除/首行数据）3 项 PASS。
| P2-3 | AI 块运行时 | 动态摘要块（文档变更防抖刷新）、待办提取块（扫描全文 `- [ ]` + AI 聚合）、语义搜索块（`kb_search_hybrid` 前 5 条 + 引用链接）、自动标签块（建议标签/分类）；AI 块在 Markdown 中以 fenced 指令块表达（如 ````ai-block:summary`） | 四类 AI 块渲染、刷新、落盘为合法 Markdown |

> **P2-3 实施状态（目标推进记录）**：
> - ✅ **v1（完成）**：`css_js/modules/editor/ai-blocks.js`：`bindAiBlocks(dom)` 识别
>   ```ai-block fenced 块 → 解析 JSON 配置 → 渲染卡片（标题 + 刷新按钮 + body）并替换原块；
>   四类刷新：summary（当前文档前 12k 字符 → callAIAPI 摘要 → marked 渲染）、todos（扫描
>   `- [ ]/[x]` 提取任务列表）、semantic-search（`kb_search_hybrid` 前 5 条 + 分数 +
>   点击跳转 `openFileFromPath`）、tags（frontmatter tags + 行内 #tag，排除标题）；
>   refresh:auto 进入视图自动刷一次；刷新串行队列防并发。main.html `postProcessMarkdown`
>   （:27129）接入。样式入 markdown.css（卡片/列表/标签 116/116 配对）。
> - ✅ 验证：parseConfig（合法/非法 JSON）、todos/tags/summary 刷新逻辑、6 项冒烟全 PASS。

| P2-4 | 语义推荐 | `[[` 补全升级为语义推荐（输入标题时用 `kb_search_hybrid` 找语义相近笔记，而不仅是文件名）；编辑器内反向链接面板（引用当前文件的所有双链，读 `index_link_graph`）；文档关闭时提示"与 N 篇旧笔记相关，是否建立链接？"（文档向量比对，复用 `hybrid_recall`） | 语义推荐、反链面板、关联提示可用 |

> **P2-4 实施状态（目标推进记录）**：
> - ✅ **v1（完成）**：`css_js/modules/editor/semantic.js`：`MdgoSemantic.wikilinkCandidates`
>   （`kb_search_hybrid` 语义候选）、`backlinks`（读 `loadLinkGraphData` → edges 反查引用
>   当前文件的节点）、`related`（标题+前 200 字符语义检索 → 非自身 top3）。
>   UI：`initSemanticButtons()` footer「反链」按钮 → 右侧滑出面板（点击跳转）；
>   `maybeSuggestRelated()` 文档打开后 2s 防抖提示"与 N 篇旧笔记相关"（幂等）。
>   suggest.js `[[` 分支升级：provide 改 async，本地文件名匹配 + **语义候选合并**
>   （✨ 标记置顶，失败静默降级）。main.html：footer 按钮（:17725）、关联提示（:24647）、
>   模块加载（:51578）。
> - ✅ 验证：backlinks 有/无引用两例 PASS；parseConfig/todos/tags/summary 6 项 PASS；
>   main.html 3 内联块语法全过。
| P2-5 | 智能整理命令 | 自动目录/大纲生成（`/` 菜单或右键菜单「生成目录」，非 /ai 自然语言）；自动标签（frontmatter tags 建议）；重复笔记检测（向量近邻 + 文件名相似 → 合并引导）；日记→周报/月报（日程 + 日记检索聚合）；过期任务/临时笔记归档（`expires_at` 语义 + 待办扫描）；全部封装为 Skill 或 `editor:*` 命令 | 各整理命令一次点击/一条命令产出可审阅结果 |

> **P2-5 实施状态：❌ 已删除（用户要求删除整理按钮及全部相关逻辑）**。
> `css_js/modules/editor/organize.js` 已删除；main.html 移除两处 `initOrganizeButton`
> 调用与模块加载；`markdown.css` 移除 `.mdgo-organize-menu/.mdgo-organize-item` 与
> `.editor-footer` 定位样式。

| P2-6 | 拼写/语法检查 | 轻量本地（`nspell`/`typo-js` 中文分词+词典）或可配置远程 LanguageTool API；错误波浪线 + 悬浮建议；只读检查不自动改文 | 中英文检查出波浪线，建议可接受 |

> **P2-6 实施状态（目标推进记录）**：
> - ✅ **v1（完成，轻量方案）**：WYSIWYG（CM6）模式启用 contenteditable 原生拼写检查
>   （`EditorView.contentAttributes.of({ spellcheck: 'true' })`，wysiwyg.js）——英文错误
>   自动波浪线（浏览器内置，含右键建议）；中文依赖系统输入法/浏览器词典（无本地分词词典，
>   避免引入重依赖）。Monaco 源码模式无原生拼写（Monaco 非 contenteditable），标注为已知
>   限制；远程 LanguageTool 预留为后续可选项（设置项 + 开关，默认关）。
> - ✅ 验证：wysiwyg.js 语法通过；spellcheck 属性随 CM6 视图生效（运行时可见）。
| P2-7 | 幻灯片 | Markdown → 幻灯片（按 `---` 分页 + reveal.js 或自有 CSS 分页 + 全屏放映）；导出 PDF | 放映/翻页/导出可用 |

> **P2-7 实施状态（目标推进记录）**：
> - ✅ **v1（完成）**：`css_js/modules/editor/slides.js`：`MdgoSlides.splitSlides`（按独立
>   一行 `---` 分页，跳过 frontmatter）→ `present()` 全屏放映 overlay（每页 markedParse +
>   Obsidian 语法渲染、←/→/Space/PageUp/PageDown 翻页、Esc 退出、页码导航、requestFullscreen）；
>   `initSlidesButton()` footer「放映」按钮（:17729）。打印导出：`@media print` 每页
>   page-break（markdown.css，打印时隐藏 UI 只显示幻灯片）。
> - ✅ 验证：splitSlides（3 页、frontmatter 跳过）PASS。

### 里程碑依赖关系

```text
P0 ──► P1 ──► P2
 │      │      ├─ P2-1 依赖 P1-2（统一文档模型）
 │      │      ├─ P2-3 依赖 P1-3（EditorAI 服务）+ P1-5（生成原生化）
 │      │      └─ P2-4 依赖 P0-1（[[] 补全骨架）+ 现有检索
 P0-1 依赖现有：_scanFileList、index_link_graph、kb_search_hybrid
 P1-1 依赖 P0-7（Markdown 模块抽取：渲染管线/预览/选区/导出已模块化）
 P1-3/P1-4 依赖现有：callAIAPI（前端直连）+ kb_llm_query（Rust 流式）
```

---

## 4. 关键技术实现方案

### 4.1 编辑器内核接入（D1/D6）

**现状**：Monaco 已通过 `monaco-editor-loader.js` 全局挂载，`initMonacoEditor()` 懒初始化
（main.html:17736 调用）。

**方案**：

1. **vendored 资源**（延续 cdn 模式，无 npm 运行时依赖）：
   - `css_js/cdn/cm6/`：CodeMirror 6 核心 + `@codemirror/lang-markdown`、`autocomplete`、
     `view`（decorations/虚文本）、`search`、`fold`、`bracket-matching`、`language-data`、
     `commands`（UMD 或 ESM 单文件构建）；
   - `css_js/cdn/tiptap/`：TipTap + StarterKit + 自研块扩展（P2）；
   - `css_js/modules/markdown.js` + `markdown.css`（P0-7）：**现有 Markdown 业务模块**
     ——渲染管线（`markedParse`/`markedMd`/`parseObsidianToHTML`/`postProcessMarkdown`/
     `renderMarkdownFile`）、实时预览、选区工具条与高亮/批注、Obsidian 语法、双链/图谱
     提取、导出；main.html 不再内联这部分逻辑，只留入口与事件绑定；
   - `css_js/modules/editor/`：`core.js`（EditorDocument 抽象）、`wysiwyg.js`、`block.js`、
     `suggest.js`（/ @ [[ 补全）、`ai-inline.js`、`ai-ghost.js`、`ai-blocks.js`、
     `focus-mode.js`、`spellcheck.js`、`export.js`。
2. **模式路由器**：`window.mdgo.editor` 命名空间（沿用现有 `window.mdgo.core.register`
   插件注册模式，main.html:16565）。打开 `.md`：默认 WYSIWYG（P1 后）；`Ctrl+1/2/3` 或
   工具栏切换 源码/所见即所得/块。代码文件（.py/.rs/.js 等）恒走 Monaco。
3. **统一保存管道**：三个内核共用 `saveFileOnly()`（main.html:24443）语义：
   防抖（已有 TimerManager）→ `enqueueFileAtomic`（已有，每文件串行链）→
   `_writeToFileHandle`（Tauri 适配器）→ 保存后触发：`kb_*` 增量索引（watcher 自动）、
   `index_link_graph` 双链图谱更新、AI 块刷新、编辑器内未保存标记。
4. **双向同步**（P1-2）：
   - Monaco → CM6：`monaco.onDidChangeModelContent` → 重建 CM6 状态（CM6 提供
     `EditorState.create` 全量重建代价可接受，或按 change 事件增量）；
   - CM6 → Monaco：`view.dispatch` changes 映射。
   - 光标/滚动：`onDidChangeCursorPosition` / CM6 `updateListener` 事件互发，用
     `requestAnimationFrame` 节流。

### 4.2 智能输入（/ @ [[）（P0-1 / P2-4）

**统一建议协议**：

```js
// css_js/modules/editor/suggest.js —— 三内核共用
const SuggestProvider = {
  // 输入上下文（当前字符前 512 字符 + 光标位置）
  getContext(view) { /* 提取触发符与查询词 */ },
  // 路由：'/' → blockMenu | '@' → fileRef | '[[' → wikilink | '#tag' → tagMenu
  async query(kind, query) {
    switch (kind) {
      case 'block': return BLOCK_MENU;                    // 静态清单
      case 'file':  return filterFiles(query, _scanFileList); // 现有文件树数据
      case 'wikilink': return semanticWikilink(query);    // P0: 标题模糊; P2: 语义
      case 'tag':   return tagCandidates(query);          // frontmatter tags 聚合
    }
  },
  apply(kind, item) { /* 插入语法："/" 插入块语法；"[[" 插入 [[标题|别名]] 等 */ },
};
```

- Monaco：`monaco.languages.registerCompletionItemProvider('markdown', {...})`，用
  `monaco.languages.CompletionItemKind.Snippet` 区分块菜单，`insertTextRules` 处理
  `[[...]]` 占位。
- CM6：`autocompletion({ override: [suggestSource] })` + 自定义面板（`/` 菜单 UI 用
  CM6 `showPanel` 或 DOM overlay，仿 Notion 键盘导航）。
- 语义候选（P2-4）：`wikilink` 分支调用 `window.__TAURI__.core.invoke('kb_search_hybrid',
  { query, top_k: 8 })`，展示 `标题 + 摘要 + 路径`；反向链接分支读取
  `index_link_graph.json`（现有文件，`openFileGraph` 已消费）或新增后端命令
  `editor_backlinks(relPath)`。

### 4.3 内联 AI 菜单（P1-3）

**现状升级**：`AI_SELECTION_ACTIONS`（main.html:17022）已有高质量 prompt 清单；新增
`applyMode` 字段，扩展 `aiMarkdownSelectionText`（:36696）从"弹窗"改为"就地应用"。

```js
// css_js/modules/editor/ai-inline.js
const AI_ACTION = {
  id: 'rewrite', name: '重写', prompt: '...',
  applyMode: 'replace',        // replace 选区 | insert-below | new-block | to-table | to-list | to-code
  stream: false,               // true 走流式（续写）
  needsDoc: false,             // true 附带整文档上下文（总结/要点）
};
async function runInlineAI(action, selection, editor) {
  const result = await EditorAI.complete(action.prompt, selection.text,
      { stream: action.stream, context: action.needsDoc ? editor.getValue() : '' });
  if (action.applyMode === 'replace') editor.replaceRange(result, selection.range);
  else if (action.applyMode === 'to-table') insertRenderedTable(result, editor); // 校验为表格语法后插入
  // ... 每种 applyMode 一个落地函数；全部入 undo 栈
}
```

- **EditorAI 服务**（D4）：
  - 短动作（改写/翻译/续写）：前端直连（复用 `callAIAPI` main.html:32371，非流式；
    续写用流式 fetch + SSE，参考 `agent.js` 流式协议或 `kb_llm_query`）；
  - 复杂动作（总结会议/提取行动项/转 Mermaid 需检索）：`kb_llm_query`（Rust 流式，
    事件 `rag:delta` 已有前端消费）。
- **AI 结果安全**：插入前 `markedMd` 合法性自检（表格/代码块），失败提示并降级为
  纯文本插入；全部操作可 Ctrl+Z 撤销。

### 4.4 幽灵文本续写（P1-4）

- CM6：`StateField` + `Decoration.widget` 在光标处渲染灰字 span（`ghost` class）；
  Tab → `dispatch` 将虚文本并入文档；Esc → 移除；Ctrl+J 重新请求。
- Monaco：`monaco.editor.setGhostText(text)`（1.8x+ 支持）或 viewZone 方案。
- 补全请求：`EditorAI.complete('继续写', context=光标前 N 字符 + 可选风格文档)`，
  流式返回；debounce 800ms；不打断输入（上次请求未返回时新输入则 abort）。
- 风格模仿：可选参数 `styleDocPath`，把该文档前 2000 字符注入 prompt 尾部
  （"模仿以下文档的写作风格："），默认当前文档。

### 4.5 AI 块（P2-3）

**Markdown 表达**（保持文件兼容与检索可读）：

````markdown
```ai-block
{
  "type": "summary",        // summary | todos | semantic-search | tags
  "query": "可选",           // semantic-search 用
  "refresh": "auto"          // auto | manual
}
```
````

- 渲染：块模式下 TipTap 自定义节点；WYSIWYG/源码模式下渲染为只读卡片 + 「刷新」按钮；
  渲染函数走现有 `markedParse(parseObsidianToHTML(...))` 复用。
- 刷新协议：
  - summary：`EditorAI.complete('总结', 文档全文，节流 2s)`；
  - todos：扫描文档 `- [ ]` / `- [x]` + AI 归并（可选）；
  - semantic-search：`kb_search_hybrid(query)` → 前 5 条 → 引用链接（`[[file]]` 点击跳转，
    复用来源定位逻辑 main.html:49300）；
  - tags：frontmatter tags + 文档向量近邻的 tags 聚合建议。
- 落盘：刷新结果不写回 Markdown（AI 块是动态视图）；块参数写回时仅更新 JSON 头。

### 4.6 专注模式 / 拼写检查 / 导出 / 幻灯片

> 原 §4.6「/ai 自然语言命令」已随 P1-5 裁剪删除；`/` 菜单仅保留 P0-1 的块插入项，
> 不再提供自然语言命令输入。

- **专注模式**（P0-3）：**打字机滚动**（光标行垂直居中：Monaco `revealLineInCenter` /
  CM6 `scrollIntoView`）+ **聚焦段落**（dim 光标外其余行）两种专注态；**禅模式/全屏已裁剪**。
- **拼写检查**（P2-6）：优先 Web 原生 `spellcheck`（Monaco 不支持，CM6 有
  `@codemirror/language` + 自绘波浪线）；中文建议用词典 + 分词（可借后端 jieba 出
  候选词）；可选远程 LanguageTool（设置项，默认关）。
- **导出**（P0-4）：HTML = 渲染管线产物 + 内嵌 CSS（复用 `github-markdown-light.min.css`、
  katex 等）；PDF = Tauri WebView print（`webview.print()`）或 html2canvas 长图（已有
  依赖 `html2canvas-pro`）；Markdown = 当前 value 复制/另存。
- **幻灯片**（P2-7）：按 `---` 分割 + 自有分页 CSS，全屏 `requestFullscreen`。

### 4.8 后端增强点

> 原则：**最大复用现有后端**，只新增"编辑器专用"薄命令，不重写知识层。

| 命令 | 职责 | 复用 |
|---|---|---|
| `editor_backlinks(relPath)` | 反向链接（谁引用了我） | 读 `index_link_graph.json`（现有）或 LanceDB symbol/title 检索 |
| `editor_semantic_suggest(query, top_k)` | [[ 语义推荐候选 | `kb_search_hybrid`（现有） |
| `editor_related(relPath, top_k)` | 文档完成关联提示（与 N 篇旧笔记相关） | `hybrid_recall(文档标题+前 N 字符)` 向量近邻 |
| `editor_dup_scan()` | 重复笔记检测 | 向量近邻 + 文件名相似（`strsim`/编辑距离，可选新依赖） |
| `editor_toc(md)` | 生成目录/大纲 | 解析 `#` 层级（前端即可，后端供 Agent 复用） |
| 新增 Skill：`kb-organize`（整理）、`weekly-report`（周报） | 自动整理命令化 | Skill 引擎（现有）+ kb-search/schedule |
| `write_file_atomic`（P0-6） | 编辑器原子保存 | 现状 `write_file`/`write_file_binary` 为 `fs::write` 整文件覆盖（非原子）；temp+rename 同目录替换 |

> **P0-6 实施状态（目标推进记录）**：
> - ✅ **v1（完成）**：Rust 新增命令 `write_file_atomic(path, content)`（`commands/fs.rs`：同目录
>   `.{文件名}.{pid}.tmp` 临时文件 + `fs::rename` 原子替换，失败清理 tmp；路径安全沿用
>   `is_path_safe` + `canonicalize_safe`），已注册进 `lib.rs` invoke_handler。前端
>   `tauri/src/adapters/file-system.js` 的 `TauriWritableStream.close()` 文本路径改调
>   `write_file_atomic`（二进制路径保留 `write_file_binary`）。
> - ✅ 验证：`cargo check --lib` exit 0（1m32s 增量编译通过）；file-system.js `node --check`
>   通过。编辑器保存链路（`enqueueFileAtomic` 串行队列 + 防抖）无需改动，原子写仅替换底层
>   写入原语，行为对上层透明。
| `read_file_range`（可选，P0-6） | 大文件范围读 | 现状 `read_file_binary` 一次性 Vec<u8> 走 IPC，10MB+ 文档内存翻倍 |

**索引侧**：AI 块（4.5）写入的 `ai-block` fenced 块建议在 `pipeline.rs` 分块时剥离或降权
（避免动态指令文本污染检索）；`.mdgo/blocks/` 副档不索引。

**存储侧已知缺口（调研确认，非本规划新问题）**：

1. **无块级持久化**：RAG 链路已有块级**索引**（LanceDB 12 万 chunk），但块不是一等公民
   存储（无 blocks 表、无块引用/块版本）。P2 块编辑阶段若需"块级存储 + 块引用"，需在
   SQLite 之上自建块表（`.mdgo/blocks/` 或 mdgo.db 新表 + 文档↔块映射），本规划 D2
   （Markdown 为事实源）暂不强制，留作 P2-1/P2-2 评估项。
2. **双通道写工程债**：前端 FSA 直写数据文件 vs 后端 SQLite（O5 仅收敛配置类）；编辑器
   保存链路统一走后端原子写后，此债进一步收敛，职责边界需在 P0-6 一并文档化。
3. **前端 FSA 语义的整文件写回**：`TauriWritableStream.close()` 合并整文件写回，无增量写；
   P0-6 原子写 + 防抖批量提交即覆盖此问题。
4. **FS 信任边界（调研确认的存量缺口）**：fs 命令无目录白名单、符号链接可逃逸（
   `canonicalize_safe` 跟随链接）、`write_file` 存在父目录不可达分支——代码执行能力已
   裁剪不引入，风险面有限；如需"打开任意文件"类能力，先收紧 fs 命令白名单与 symlink
   拒绝。
5. **embedding_cache.sqlite 体积超标（存量）**：实测 404MB > 150MB 上限、无自动裁剪触发；
   与编辑器无直接关系，但属存储健康项，建议随 P0 顺带治理（触发裁剪 + 上限强制）。

---

## 5. 风险与依赖

| 风险 | 等级 | 缓解 |
|---|---|---|
| main.html 单体 5.2 万行，新增三内核后复杂度上升 | 高 | 编辑器相关逻辑全部收敛到 `css_js/modules/editor/*`，main.html 只留入口与事件绑定；以 `window.mdgo.editor` 命名空间隔离 |
| CM6/TipTap 体积（vendored）拖慢启动 | 中 | 全部懒加载（P0 不加载 CM6；P1 首次进入 WYSIWYG 时才加载）；启动只保留 Monaco（现有） |
| Markdown ⇄ 块 JSON 双向序列化丢失格式（表格/嵌套列表/复杂行内） | 中 | Markdown 为事实源 + 序列化 round-trip 测试集（参考现有 `docs/canvas-benchmark-cases/` 模式建 `editor-roundtrip-cases/`）；失败降级提示"请用源码模式编辑" |
| 幽灵文本/流式 AI 与打字竞争 | 中 | 输入即 abort 上次请求；虚文本只追加光标处；debounce |
| 内联 AI 误改文档 | 低 | 全部入 undo 栈 + 就地预览（diff 高亮）+ 可选确认 |
| 拼写检查中文效果差 | 低 | 标注"实验性"；可关 |
| 与现有保存/索引链路的竞态（AI 块写入 vs watcher 索引） | 中 | AI 块指令剥离（4.8）+ 复用现有原子写队列与 watcher 防抖 |
| 文件渲染管线未过 DOMPurify（调研确认：`renderMarkdownFile`→`markedParse` 直接注入，仅聊天渲染做 sanitize） | 中 | 编辑器内嵌内容（AI 块/嵌入笔记/渲染预览）必须过 `DOMPurify.sanitize`；沿用 `renderChatMarkdown`（main.html:28597）的既有做法，防止恶意笔记 XSS |
| 整文件写回 + 非原子写（`fs::write` 直接覆盖） | 中 | P0-6 原子写（temp+rename）+ 防抖批量提交 |
| 双通道写工程债（前端 FSA 直写 vs 后端 SQLite） | 低 | 编辑器保存统一走后端原子写；O5 之后继续收敛 |
| FS 信任边界存量缺口（无目录白名单、符号链接可逃逸） | 中 | 编辑器不引入代码执行能力（已裁剪），风险面有限；如需"打开任意文件"仍先补 fs 命令白名单与 symlink 拒绝（`core/security` 现为提示注入防护，路径安全需另立） |

**关键依赖（均为现有资产，无需新基建）**：`kb_search_hybrid`、`kb_llm_query`（流式事件）、
`callAIAPI`（前端直连）、`_scanFileList`（文件树数据）、`index_link_graph`（双链图谱）、
`parseObsidianToHTML`（Obsidian 语法）、`enqueueFileAtomic`（原子写）、Skill/MCP/Agent 工具面。

---

## 6. 验收与量化指标（建议基线）

| 指标 | 目标 |
|---|---|
| 启动：5000 文件库目录树渲染 | < 1s（与 `TREE_LOAD_TIME_KEY` 埋点对比） |
| 启动：10MB Markdown 打开 | < 2s |
| 模式切换：源码 ↔ WYSIWYG | < 300ms，光标位置保持 |
| 输入补全：/ @ [[ 面板首项 | < 100ms（本地候选）/ < 800ms（语义候选） |
| 选区 AI 就地应用 | 动作完成 < 10s（非流式）/ 首 token < 3s（流式） |
| 幽灵文本首 token | < 3s |
| AI 块刷新 | 摘要 < 8s、语义搜索 < 3s |
| 回归 | `cargo test --lib` 全绿；`retrieval_eval` 基线不回退；新增 editor round-trip 用例 |

---

## 7. 落地顺序建议（第一刀怎么切）

1. **先做 P0-1（/ @ [[ 智能输入）**——独立于新内核，直接在 Monaco 上实现，1–2 周内可
   交付，立刻提升"2026 级"感知；
2. 同步搭 `css_js/modules/editor/core.js`（EditorDocument 抽象），为 P1 铺路；
3. **P0-7（Markdown 模块抽取 JS + CSS）建议与 1、2 并行推进**，在 P1 接入 CM6 前完成
   （渲染管线/预览/选区/导出全部走 `markdown.js`/`markdown.css`，行为不变量化回归）；
4. P0-2 预览联动 → P0-3/4 专注+导出（均为低风险快赢）；
5. P1 以「CM6 WYSIWYG 接入」为最大单点（风险最高，优先做），内联 AI/续写/AI 生成原生
   化紧随其后；
6. P2 按依赖顺序：块编辑 → AI 块 → 语义推荐 → 整理/拼写/幻灯片。

> 本文为规划基线；实施时每期开始前按当前 HEAD 复核引用位置（本项目惯例：改代码前对照
> `docs/目录索引.md` 相关契约文档），并在本文登记实施状态。

---

## 8. Code Review 修复记录（2026-08 全量审查）

> 三路并行审查（P1 模块 / P2 模块 / 前后端接线）+ 独立自查，共修复 **19 项**问题。
> 全部修复经语法验证（16 文件 `node --check` 0 失败、main.html 3 内联块全过、
> `markdown.css` 147/147 配对、`cargo check --lib` exit 0）与关键逻辑回归测试。

### 🔴 致命/高危（4）

| # | 问题 | 修复 |
|---|---|---|
| S1 | **main.html 顶层 `let` 状态不挂 `window`**：`currentEditor/currentRootPath/currentFileName/originalContent/previewFileText/_scanFileList/markdownSelectionState` 经 `window.X` 读取恒 undefined → MdgoDocument/导出/WYSIWYG/内联 AI/补全整体失效 | 9 个模块全部改裸标识符读取（顶层 `let` 跨 script 可见；var/function 仍走 window） |
| S2 | **AI 块类名被 `postProcessCodeAndDiagrams` 剥离**：`language-ai-block` → `language-plaintext`，前 5 个 pre 内 AI 块不渲染 | `postProcessMarkdown` 中 `bindAiBlocks` 提前到代码后处理之前执行 |
| S3 | **反链字段名错误**：图谱边是 `{source,target}`，代码读 `toNode/fromNode` → 反链恒空 | semantic.js backlinks 改读 source/target |
| S4 | **块模式保存丢内容**：TipTap 编辑不实时同步 Monaco，Ctrl+S 保存旧内容 | block.js onUpdate 实时同步 Monaco `setValue`（防丢 + 去 pushUndoStop） |

### 🟡 中（9）

| # | 问题 | 修复 |
|---|---|---|
| M1 | 内联 AI 工具栏渲染条件恒 false + summary/polish id 与主脚本冲突遮蔽 | 渲染条件改 `display !== false`；17 个动作 id 加 `inline-` 前缀 |
| M2 | `copyToClipboard` 签名不匹配（无参复制磁盘文件），导出"复制MD"/周报/CSV 复制错误 | 模块改调 `copyTextToClipboard(text)` |
| M3 | ghost provider 每编辑器重复注册 → Ctrl+J 多次 LLM 请求 | provider 移到模块级只注册一次 |
| M4 | WYSIWYG/块模式跨文件视图残留（旧 CM6 view/TipTap editor 未销毁） | init 重建容器时先 destroy 旧视图 |
| M5 | AI 就地应用 range 过期（await 期间用户编辑） | await 后重新解析 + `getVersionId()` 校验 |
| M6 | db-view 筛选框每键重建丢焦点 | 拆分渲染（输入框不重建，只重建表格区） |
| M7 | WYSIWYG/块模式互切叠加（两层编辑器同显） | toggle 时隐藏另一模式容器 |
| M8 | 实时预览下 AI 块 auto 刷新风暴（每次输入调 LLM） | 内容指纹 10s 缓存节流 |
| M9 | live preview footer 与 edit 模式不一致（缺反链/整理/放映） | enterLivePreviewMode 补齐三按钮 |

### 🟢 低（6）

| # | 修复 |
|---|---|
| L1 | core.js `getValue` 空串文档回退 bug；WYSIWYG/块切换补 `layout()`（大文件）；切换竞态锁；同步防循环 `syncing` 标志 |
| L2 | focus-mode 快捷键 Ctrl+Shift+P → Ctrl+Alt+P（避免占用 Monaco 命令面板） |
| L3 | slides：分页感知代码围栏 + frontmatter BOM 容忍 + 错误路径转义 |
| L4 | organize：菜单定位（footer position:relative）、面板 label 转义（XSS）、generateToc 重复/首行空行、applyTags 返回值+BOM frontmatter、archiveDone stripFence、document 监听累积 |
| L5 | ai-blocks：todos/tags stripFence（代码块内误统计）、summary DOMPurify |
| L6 | Rust `write_file_atomic/write_file/write_file_binary` 的 `create_dir_all` 死代码（先建目录再校验）；适配器 close() 空 buffer 统一原子写 |

### 验证结论（闭环确认）

- **保存链路闭环**：`saveFileOnly → saveCurrentEditContent → writeFileHandle → enqueueFileWrite（串行队列）→ createWritable → TauriWritableStream.close() → write_file_atomic`，main.html 无直接 `invoke('write_file')` 写编辑器内容。
- **参数契约**：`kb_search_hybrid({dirPath, query, topK})` ↔ Rust `(dir_path, query, top_k)`（camelCase 映射 ✓）；`read_file({path})` ✓（organize 已拼绝对路径）。
- **图谱读取**：`loadLinkGraphData()`（`{root}/.mdgo/index_link_graph.json`）路径一致 ✓。
- **Windows 原子替换**：实测 `std::fs::rename` 覆盖已存在文件成功（`MOVEFILE_REPLACE_EXISTING`）。
