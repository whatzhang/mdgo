# Knowledge Canvas（知识画布）功能测试用例（P0–P2）

> 适用范围：`.canvas`（JSON Canvas）知识画布的 AI 生成 / 整理 / 读取工具链（Rust 侧
> `tauri/src-tauri/src/core/agent/tools/canvas.rs`）与前端 D3 渲染器、技能声明
> （`resources/skills/canvas/SKILL.md`）。
>
> **前端源码位置**：`main.html`（仓库根目录，Vite 入口）；`tauri/dist/main.html` 为构建产物
> （`tauri/` 下 `npm run build` 生成，gitignore）。修改前端后必须重新构建 dist 才在打包版生效。
>
> 用例编号规则：`TC-<阶段>-<序号>`。每例含优先级（P0=必须通过 / P1=重要 / P2=建议）、
> 前置条件、步骤、预期结果。**「当前状态」列标注该用例在本次 code review 时的实现情况**：
> ✅ 已实现可测 / ⚠️ 已实现但存在已知缺陷（见缺陷编号）/ ❌ 未实现。
>
> 缺陷索引（详见 code review 报告；**修复轮次已全部修复**）：
> - ~~**D1**~~ ✅ 已修复：canvas skill 已嵌入 `SYSTEM_SKILL_MD`（schema.rs）
> - ~~**D2**~~ ✅ 已修复：前端 renderCanvasNode 已支持 link/url/bookmark/code + 未知类型降级 text
> - ~~**D3**~~ 🗑️ 已随 P3 删除：query 动态节点 + watcher 联动（功能已移除）
> - ~~**D4**~~ ✅ 已修复：write 工具对 .canvas 校验 file 节点路径存在性，不存在降级 text（`degrade_missing_file_nodes`）
> - ~~**D5**~~ ✅ 已修复：sanitize_ids 重编号后过滤悬空边
> - ~~**D6**~~ ✅ 已修复：sanitize_ids 统一补全边 id（原 organize 专属逻辑已并入确定性管线）
> - ~~**D7**~~ ✅ 已修复：write 原子写 + 回读验证 + 目录自动创建（乐观锁场景随 canvas_organize 删除不再需要）
> - 补充修复：`CanvasNode` 坐标/尺寸/type 与边 id 缺省容错（模型输出缺失不再报 `missing field`）、write 自动创建父目录
>
> 修复验证：`cargo test --lib canvas::` 12 个单元测试全部通过。

> **架构 v5（Canvas = 知识文件格式 + 语义布局引擎）**：`read_canvas`/`canvas_generate`/`canvas_organize` 三个专用
> Function 已全部删除。模型用通用 `read`/`write`/`kb_search` 读写画布；`write` 工具检测 `.canvas` 扩展名后
> 自动执行确定性管线（JSON parse → schema/ID/edge/file 校验 → sanitize → **语义布局** → 原子写），
> 校验失败拒绝写入。Canvas Skill v5 只声明 `[read, write, kb_search]`。
>
> **布局分工（v5 核心）**：模型在 Canvas JSON 顶层声明 `layout` 意图
> （`{mode: hierarchy|flow|radial|grouped, root, direction, groups}`），系统 Layout Engine
> 计算所有节点坐标（**模型不输出 x/y**）——Skill 负责布局意图，引擎负责布局坐标。
> 四种模式：hierarchy（层级 TB）/ flow（流程 LR）/ radial（中心辐射）/ grouped（分组分区）。

---

## 0. 测试环境准备（所有用例的前置）

| 项 | 要求 |
|---|---|
| 应用 | 开发或打包版本，已重新编译（含 `tools/canvas.rs`、skill 注册） |
| 知识库 | 一个真实目录作为打开的知识库，内含 ≥3 个 Markdown 文件（如 `rag-notes/embedding.md`、`rag-notes/rerank.md`），且已完成索引（kb 面板索引成功） |
| LLM | 设置中已配置端点与模型（canvas 工具依赖 LLM 生成/整理） |
| 技能 | 技能列表中应出现「知识画布（canvas）」（若缺失 → 缺陷 **D1** 复现） |
| 观察手段 | 应用内 Canvas 页面打开 `.canvas`；文件管理器查看生成的 `.canvas` 原始 JSON |

---

## 1. P0 基建

### TC-P0-01 技能可见性与激活（功能）
- 优先级：P0 ｜ 当前状态：✅（D1 已修复，需重启应用后验证）
- 前置：环境准备完成
- 步骤：
  1. 打开「技能」面板，查看系统技能列表。
  2. 查找「知识画布（canvas）」条目。
  3. 若存在，尝试激活；激活后在技能详情中查看声明工具列表。
- 预期：
  1. 系统技能列表中存在 id=`canvas`、name=`知识画布（Canvas）`、scope=system 的技能。
  2. 激活成功。
  3. 声明工具为 `read, write, kb_search`（**无任何 Canvas 专用工具**）。
- 通过标准：3 步全部满足；若第 1 步列表缺失即为 **D1** 复现（阻断）。

### TC-P0-02 工具注册完整性（回归）
- 优先级：P0 ｜ 当前状态：✅（架构 v4：Canvas 非工具）
- 前置：技能已激活
- 步骤：
  1. 在对话中发起「生成画布」意图。
  2. 观察 Agent 工具调用面板中出现的工具名。
- 预期：模型使用通用 `kb_search`（检索资料）→ `write`（写 .canvas），**不出现** read_canvas/canvas_generate/canvas_organize（已删除，未注册）；调用轨迹（agent:tool_call / agent:tool_result）正常记录。
- 通过标准：生成流程全部走通用工具，无 UnknownToolCall。

### TC-P0-03 生成最小合法画布（端到端）
- 优先级：P0 ｜ 当前状态：✅（D2 已修复，link 节点可渲染）
- 前置：技能已激活；知识库有 1 个文件
- 步骤：
  1. 对话输入：「生成一个主题为『测试画布』的画布，保存为 canvas/e2e-p0.canvas」。
  2. 等待完成，记录返回信息。
  3. 用文件管理器打开 `canvas/e2e-p0.canvas` 检查 JSON。
  4. 在 Canvas 页面打开该文件。
- 预期：
  1. 返回消息包含路径与「N 节点 / M 连线」统计。
  2. JSON 顶层含 `layout`（布局意图）、`nodes`、`edges` 数组；节点含 `id/type/x/y/width/height`（坐标由系统布局引擎计算）；type 为 `text|file|link` 之一；无重复 id；id 为 `n1..nN` 格式。
  3. 边引用 `fromNode/toNode` 均指向存在的节点 id；`layout.root`/`groups` 引用重映射后仍有效。
  4. Canvas 页面渲染出节点与连线，节点可拖拽、连线带箭头；link 节点显示标题与 URL。
- 通过标准：1–4 全满足。

### TC-P0-04 布局有效性（功能）
- 优先级：P0 ｜ 当前状态：✅（语义布局引擎：Skill 负责意图，引擎负责坐标）
- 前置：TC-P0-03 已生成画布
- 步骤：打开画布 JSON 与 Canvas 页面，检查坐标分布与 layout 意图。
- 预期：
  1. 模型声明了 `layout` 意图（mode/root/direction 至少其一）；**未输出 x/y**（或输出被引擎覆盖）。
  2. hierarchy：根在顶部（y=0），子节点逐层下移（层间距 240+V_GAP=300）；同层节点水平错开 ≥H_GAP，无重叠；同父子节点相邻。
  3. flow+LR：主链自左向右（x 递增）；radial：root 居中、一级环绕半径 320、二级更远；grouped：组间留白 ≥160。
  4. 孤立节点（无入边无出边）也被放置，不被丢弃；节点尺寸随内容长度自适应（短标题小、长说明大）。
- 通过标准：1–4 满足；不同模式的空间语义可辨识、无重叠。

### TC-P0-05 write 对 .canvas 的确定性校验（异常/安全）
- 优先级：P0 ｜ 当前状态：✅（架构 v4：校验内置于 write）
- 前置：技能已激活
- 步骤（逐项发起，全部用 `write` 写 .canvas）：
  1. 内容非 JSON（如 `not json`）。
  2. 内容 JSON 但非 Canvas 结构（如 `{"foo":1}`）。
  3. `nodes` 为空数组。
  4. edge 引用不存在的节点（悬空边）。
  5. file 节点指向不存在的路径。
  6. 目标路径含 `../` 或绝对路径（如 `C:\x.canvas`、`../x.canvas`）。
  7. 目标路径指向 `.mdgo` 内部（如 `.mdgo/skills/canvas/SKILL.md`）。
- 预期：均返回明确错误信息（无效的 JSON Canvas 格式 / 画布无节点 / 路径越界 / 不允许写入 .mdgo 等），不 panic、不写文件、进程不崩；悬空边与编造 file 路径被**静默规整**（不报错，写入清理后的合法内容）。
- 通过标准：7 项全部返回可读错误且无副作用（4/5 项验证写入内容已被规整）。

### TC-P0-06 写入安全与原子性（安全）
- 优先级：P0 ｜ 当前状态：✅
- 前置：技能已激活
- 步骤：
  1. 让模型生成画布写入 `canvas/` 子目录（父目录不存在时应自动创建）。
  2. 覆盖生成到同一路径，观察返回信息。
  3. 生成到 `.mdgo` 内部路径。
- 预期：
  1. `canvas/` 自动创建，文件写入成功。
  2. 返回「覆盖写入」且回读验证通过（含 `[verified]` 字样）。
  3. 拒绝写入 `.mdgo` 内部。
- 通过标准：1–3 满足；临时文件 `.mdgo-tmp` 不残留。

### TC-P0-07 节点 id 唯一性与边引用完整性（回归）
- 优先级：P0 ｜ 当前状态：✅（对应 `sanitize_ids` 单元测试）
- 前置：TC-P0-03 产物
- 步骤：统计生成画布 JSON 的 id 与边引用。
- 预期：节点 id 唯一且全部为 `n{数字}`；每条边的 from/to 均在节点 id 集合中。
- 通过标准：脚本/手工校验全通过。

### TC-P0-08 前端未知节点类型降级（功能）
- 优先级：P0 ｜ 当前状态：✅（D2 已修复）
- 前置：手工构造一个含未知 type（如 `type:"diagram"`）节点的 `.canvas` 放入知识库
- 步骤：Canvas 页面打开该文件。
- 预期：未知类型节点降级为文本节点渲染（显示其 text 内容），页面不报错、其他节点正常；同时验证 link/code/query 类型节点分别显示链接卡、代码预览、检索卡片。
- 通过标准：未知类型有可见内容而非空白框。

---

## 2. P1 AI → Canvas（生成）

### TC-P1-01 纯主题生成（无检索）（功能）
- 优先级：P0 ｜ 当前状态：✅
- 前置：技能已激活；不传 query
- 步骤：对话：「生成主题为『JVM GC』的画布」。
- 预期：生成画布含 6~20 个节点；节点为 text 或 link 类型；连线表达层级/流程关系；文件写入知识库 `canvas/` 下；link 节点在 Canvas 页面显示标题 + URL。
- 通过标准：节点数 6–20、画布可打开、link 节点有可见内容。

### TC-P1-02 生成+检索绑定真实文件（功能）
- 优先级：P0 ｜ 当前状态：✅（D4 已修复：编造路径会被降级并提示）
- 前置：知识库存在 `rag-notes/embedding.md` 等文件且已索引
- 步骤：对话：「把『RAG 技术路线』做成画布，结合知识库资料」。
- 预期：画布中 file 节点绑定的相对路径**全部真实存在于知识库**（与 kb_search 命中一致）；file 节点在 Canvas 页面可点击打开来源文件；若有 LLM 编造路径会被自动降级为 text 并在返回消息中提示。
- 通过标准：抽查全部 file 节点路径存在；打开文件成功；返回消息含降级提示时文件内无对应 file 节点。

### TC-P1-03 检索失败降级（异常）
- 优先级：P1 ｜ 当前状态：✅
- 前置：把知识库索引清空或指向无内容目录
- 步骤：带 query 生成画布。
- 预期：工具不失败（kb_search 失败仅告警跳过），画布以纯 text/link 节点生成成功，返回消息正常。
- 通过标准：生成成功且无 file 节点编造。

### TC-P1-04 输出路径控制（功能/边界）
- 优先级：P1 ｜ 当前状态：✅（write 工具自动创建父目录）
- 步骤：
  1. 让模型用 `write` 写 `sub/dir/my.canvas`（深层目录）。
  2. 写知识库根目录 `my.canvas`。
- 预期：1 深层目录自动创建；2 根目录写入成功。
- 通过标准：2 项均符合。

### TC-P1-05 模型输出异常 JSON 容错（异常）
- 优先级：P1 ｜ 当前状态：✅（write 对 .canvas 的确定性校验兜底）
- 前置：让模型写出缺字段/缺坐标/边缺 id 的 Canvas JSON
- 步骤：发起生成。
- 预期：缺坐标/type/边 id → 自动规整（补全默认、布局、边 id）；非 JSON / 空 nodes / 结构错误 → write 拒绝并返回明确原因；不产生半写文件。
- 通过标准：错误信息可读，无脏文件；规整后文件合法。

### TC-P1-06 并发/重复生成（回归）
- 优先级：P2 ｜ 当前状态：✅（原子写保证无半写）
- 步骤：连续发起两次写入同一 .canvas 路径。
- 预期：两次均成功，最终文件为最后一次内容且 JSON 合法。
- 通过标准：文件合法、无损坏。

---

## 3. P2 RAG / 检索 → Canvas

### TC-P2-01 RAG 问答→画布（端到端）
- 优先级：P0 ｜ 当前状态：✅（架构 v4：kb_search + read + write）
- 前置：知识库含 RAG 相关文档（Chunk/Rerank/Embedding 等）且已索引
- 步骤：对话：「为什么我的 RAG 效果不好？把原因梳理成画布」。
- 预期：模型先 `kb_search` 检索真实资料 → 生成 Canvas JSON → `write` 写入 .canvas；画布节点覆盖检索、分块、重排等概念；关键结论节点绑定真实文件；连线表达因果/依赖；可点击文件节点打开原文。
- 通过标准：节点与知识库内容相关、file 节点全部真实、画布可打开、全程无 Canvas 专用工具。

### TC-P2-02 file 节点可打开来源（功能）
- 优先级：P0 ｜ 当前状态：✅（前端已有 file 节点打开能力）
- 步骤：在 P2-01 画布中单击/双击 file 节点。
- 预期：单击选中并高亮文件路径；双击打开文件内容面板；md 类文件内联渲染。
- 通过标准：打开成功、内容正确。

### TC-P2-03 检索结果数量与 top_k（边界）
- 优先级：P2 ｜ 当前状态：✅（kb-search 技能声明 top_k=8/min_score=0.4）
- 前置：知识库文件数 ≥ 20
- 步骤：生成画布并要求结合知识库。
- 预期：检索 top_k 由技能策略（8）与安全边界（1..=50）钳制；file 节点数量合理（≤ 命中数）。
- 通过标准：file 节点数 ≤ 实际命中数。

---

## 4. 整理 / 动态（架构 v4 后的形态）

### TC-P4-01 模型整理画布（read + write）（功能）
- 优先级：P0 ｜ 当前状态：✅（架构 v4：模型自行 read→理解→write 覆盖）
- 前置：存在一个节点较多（≥10）、有孤立/空节点的画布
- 步骤：对话：「整理 canvas/jvm-gc.canvas」。
- 预期：模型 `read` 读取画布 → 分析并删除无意义节点、合并重复概念、补充关系 → `write` 覆盖原文件；写入内容经系统校验 + 自动布局后落盘。
- 通过标准：重读画布内容已更新、JSON 合法、Canvas 页面可正常打开。

### TC-P4-02 整理不破坏数据（回归/异常）
- 优先级：P1 ｜ 当前状态：✅
- 步骤：
  1. 对空画布（nodes=[]）让模型整理（write 会拒绝空 nodes）。
  2. 对含环（A→B→A）的画布整理。
- 预期：1 write 返回「画布无节点」错误，不落盘；2 正常完成、无死循环、布局不重叠。
- 通过标准：2 项均正常，画布文件仍为合法 JSON。

### TC-P4-03 整理后渲染一致性（功能）
- 优先级：P1 ｜ 当前状态：✅（write 管线过滤悬空边）
- 步骤：整理完成后立即在 Canvas 页面打开。
- 预期：无悬空连线（指向不存在节点的边被 write 管线过滤）、布局无重叠。
- 通过标准：无悬空边。

### TC-P3-04 导出为 Markdown 大纲 🗑️ 已删除
- ~~P0 ｜ `canvas_export` 工具已移除，不再测试~~（如需大纲沉淀，模型可用 `read` 读画布后自行生成 Markdown，用 `write` 落盘）

### TC-P3-05 导出后落盘沉淀 🗑️ 已删除
- ~~P1 ｜ `canvas_export` 已移除~~（模型 `read` 画布 → 生成大纲文本 → `write` .md）

### TC-P3-06 空画布导出 🗑️ 已删除
- ~~P2 ｜ `canvas_export` 已移除~~

### TC-P3-07 动态 query 节点 🗑️ 已删除
- ~~P1 ｜ query 节点类型与动态检索已移除~~（`CanvasNode.query` 字段、`renderQueryNode`/`fillCanvasQueryNodes`/watcher 联动均已删除）

### TC-P3-08 画布与索引联动 🗑️ 已删除
- ~~P2 ｜ watcher 联动已移除~~

---

## 5. 技能 / 安全 / 回归

### TC-S-01 技能触发词命中（功能）
- 优先级：P1 ｜ 当前状态：✅（D1 已修复）
- 步骤：分别输入「把 X 整理成画布」「生成画布」「画布里有孤立节点吗」。
- 预期：LLM 激活 canvas 技能并按其指令用 read/write/kb_search 完成任务；未激活时给出引导而非报错。
- 通过标准：3 类意图均被正确路由，全程无 Canvas 专用工具。

### TC-S-02 工具门禁（安全）
- 优先级：P1 ｜ 当前状态：✅（架构 v4：Canvas 专用工具已不存在）
- 前置：canvas 技能**未**激活
- 步骤：让模型直接写 .canvas（write 工具本身是 BASE_TOOLS 可用）。
- 预期：write 对 .canvas 内容做确定性校验（不依赖技能激活）；canvas 技能仅提供指令引导，不承担格式正确性。
- 通过标准：不崩溃、可恢复；未激活技能时写非法 .canvas 仍被拒绝。

### TC-S-03 覆盖写前确认（安全/流程）
- 优先级：P2 ｜ 当前状态：⚠️（依赖模型自觉，无硬校验）
- 步骤：对已存在的画布再次生成/整理（覆盖同路径）。
- 预期：模型在覆盖前说明将覆盖并征询用户，或用户明确同意后才写。
- 通过标准：行为符合 SKILL.md 边界描述（此项为引导性，允许人工判断）。

### TC-S-04 大画布性能（回归/性能）
- 优先级：P2 ｜ 当前状态：✅（待实测）
- 步骤：生成 50+ 节点画布并在 Canvas 页面打开、拖拽、缩放。
- 预期：打开 <2s、拖拽流畅（≥30fps）、无内存暴涨。
- 通过标准：主观流畅 + 无控制台错误。

### TC-S-05 取消/超时（异常）
- 优先级：P2 ｜ 当前状态：✅（原子写保证）
- 步骤：写入大 .canvas 过程中中断。
- 预期：无残留半写文件（原子写保证）、进程稳定。
- 通过标准：无脏文件、进程稳定。

### TC-S-06 工具调用轨迹（回归）
- 优先级：P2 ｜ 当前状态：✅
- 步骤：完成一次生成后查看工具调用面板/日志。
- 预期：`record_tool_call` / `record_tool_result` 记录含参数摘要、结果截断、成功/失败标记。
- 通过标准：轨迹完整、敏感内容截断（`truncate`）。

---

## 6. 单元测试（Rust 侧，`cargo test --lib canvas::`）

| 用例 | 目标函数 | 当前状态 |
|---|---|---|
| UT-01 默认层级布局（root 顶、同层同行、无重叠） | `layout_canvas` | ✅ 通过 |
| UT-02 id 重编号与边引用保持 | `sanitize_ids` | ✅ 通过 |
| UT-03 悬空边过滤 | `sanitize_ids` | ✅ 通过 |
| UT-04 file 节点存在性校验 | `degrade_missing_file_nodes` | ✅ 通过（临时目录） |
| UT-05 空/重复边 id 补全唯一 | `sanitize_ids` | ✅ 通过 |
| UT-06 完整管线（缺字段规整 + 悬空边清理 + 非法拒绝） | `validate_canvas_json` | ✅ 通过 |
| UT-07 hierarchy 坐标语义（root 顶部/同层同 y/水平错开） | `layout_canvas`（hierarchy） | ✅ 通过 |
| UT-08 flow+LR 主链自左向右 | `layout_canvas`（flow LR） | ✅ 通过 |
| UT-09 radial root 居中、一级环绕、二级更远 | `layout_canvas`（radial） | ✅ 通过 |
| UT-10 grouped 组间留白、组内垂直堆叠 | `layout_canvas`（grouped） | ✅ 通过 |
| UT-11 内容感知尺寸（长文本节点更宽） | `estimate_size` | ✅ 通过 |
| UT-12 sanitize 重映射 layout.root/groups 引用 | `sanitize_ids` | ✅ 通过 |
| UT-13 **布局确定性**（相同输入两次布局坐标一致） | `layout_canvas` | ✅ 通过（新增） |
| UT-14 **中文/ASCII 宽度区分**（CJK≈1em vs ASCII≈0.55em） | `estimate_size` | ✅ 通过（新增） |
| UT-15 **孤立节点识别 + 仍被放置** | `layout_quality_check` | ✅ 通过（新增） |
| UT-16 **节点重叠检测**（手工构造重叠被检出） | `check_node_overlaps` | ✅ 通过（新增） |
| UT-17 **flow main_path 主链对齐 + 分支挂载** | `layout_flow_main_path` | ✅ 通过（新增） |
| UT-18 **radial 一级 >8 自动降级 hierarchy** | `layout_canvas`（radial） | ✅ 通过（新增） |
| UT-19 **layout.version 缺省 = 1** | `CanvasLayout` 反序列化 | ✅ 通过（新增） |

> 验证记录：`cargo test --lib canvas::` → **19 passed; 0 failed**（218 filtered out）。
> （`apply_organize_plan`、`canvas_to_markdown` 相关测试已随 canvas_organize / canvas_export 删除）
> 全量：`cargo test --lib` → **237 passed; 0 failed**。

---

## 7. 回归清单（每次改动后执行）

1. TC-P0-03（端到端最小画布）—— 防 D1/D2 回退
2. TC-P0-04（布局有效性）—— 防布局引擎回归（UT-07~11 对应）
3. TC-P0-05（write 对 .canvas 的确定性校验）—— 防格式回归
4. TC-P0-06（写入安全/原子性）
5. TC-P1-02（file 节点真实性）—— 防 D4 回退
6. TC-P4-01（模型整理画布）—— 防 read+write 链路回归
7. `cargo test`（Rust 单元测试）全绿
8. 技能面板确认「知识画布」存在且可激活（D1 修复验证）
9. 技能详情确认工具列表为 `read, write, kb_search`（架构 v5 验证，不含任何 Canvas 专用工具）

---

## 8. 修复与回归结论（架构 v5 + 鲁棒性增强后）

| 项 | 方案 | 验证 |
|---|---|---|
| **架构** | Canvas = 知识文件格式（非 AI 工具）；删除 read_canvas/canvas_generate/canvas_organize | 注册表/白名单/SKILL.md 零残留 |
| **布局分工** | Skill 声明 `layout` 意图（mode/root/direction/main_path/groups），Layout Engine 计算全部坐标（模型不输出 x/y） | UT-07~10/17 通过 + TC-P0-04 |
| **P0 确定性** | 布局只依赖 nodes/edges 顺序 + node id，相同输入 → 相同坐标 | UT-13 通过 |
| **P0 碰撞检测** | 布局后 `layout_quality_check`（重叠/交叉/孤立/长连线），引擎数学保证无重叠 | UT-15/16 通过 |
| **P0 Edge=真相** | edges 是知识关系，layout.groups 仅空间分区（Skill 明确禁止第二套关系） | Skill v5 + 代码注释 |
| **P0 中文/代码尺寸** | CJK≈1em / ASCII≈0.55em / 数字≈0.6em；code 按最长行+行数单独估算 | UT-14 通过 |
| **P1 main_path** | flow 主链沿 direction 排列，分支挂最近主链祖先；未声明按最长路径推断 | UT-17 通过 |
| **P1 radial 限制** | 一级节点 >8 自动降级 hierarchy（不再无限增大半径） | UT-18 通过 |
| **P1 质量检测** | edge 交叉/孤立/超长连线检测记录（阶段一：发现问题，不做复杂自动修复） | UT-15/16 通过 |
| **确定性** | `write` 对 .canvas 自动执行 validate_canvas_json 管线（parse→校验→sanitize→语义布局→原子写） | UT-06 通过 + TC-P0-05 |
| **边界** | LLM 负责语义（read/write/kb_search），Rust 负责格式与坐标 | TC-S-02 通过 |
| D1 skill 未嵌入 | `schema.rs` `SYSTEM_SKILL_MD` 增加 canvas 条目 | 重启后技能列表出现「知识画布」 |
| D2 前端类型分支缺失 | `renderNodeContentByType` 统一分发 + 未知降级 text | TC-P0-03/08 渲染正常 |
| D4 file 无存在性校验 | `degrade_missing_file_nodes`（不存在降级 text，write 管线内执行） | UT-04 通过 |
| D5 悬空边未清理 | `sanitize_ids` 过滤悬空边（write 管线内执行） | UT-03 通过 |

> 明确不做（避免过度设计）：force-directed / ELK / Dagre / hybrid 混合模式 / group 局部 direction——当前
> hierarchy/flow/radial/grouped + 内容尺寸 + 确定性 + 碰撞检测已覆盖绝大多数知识画布场景。

> 回归顺序：技能列表出现 canvas（D1）→ TC-P0-03 端到端（模型声明 layout + read/write 生成）→
> TC-P0-04 布局语义 → 单元测试全绿 → TC-P0-05 校验行为 → TC-P4-01 整理链路。
> 前端改动需重新构建/刷新 Web 资源后生效。
