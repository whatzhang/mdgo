---
id: mermaid
scope: system
name: Mermaid 图表
description: 当用户需要生成 Mermaid 图表代码（流程图、时序图、类图、状态图、ER 图、甘特图、思维导图等 30 种类型）并在 Markdown 中嵌入图表时触发。能够自动选择合适的 Mermaid 图表类型，并生成可直接渲染的 Mermaid 代码。
priority: 50
tools: [read]
enabled: true
version: 2
created_at: 1754200000000
updated_at: 1754200000000
---

# Mermaid 图表生成

生成**语法正确**的 Mermaid 图表代码。图表以文本形式描述并自动布局，通过 `mermaid` 代码块嵌入 Markdown。

## 不适用场景（改用其他工具）
- 像素级布局、自定义定位、品牌图标 → **drawio**
- 手绘 / 草图 / 自由白板 → **excalidraw** / **tldraw**
- 严格规范的 UML → **plantuml**

## 强制规则
1. **先读参考再生成**：生成前用 read 读取所选类型的参考文件 `references/<file>`；不要凭记忆生成不常见类型。
2. **标签加引号**：当标签包含以下内容时必须加引号：
    - 中文字符
    - 非 ASCII 字符
    - 空格
    - 标点符号
    - 括号、冒号、斜杠等特殊字符
    - 编程语言关键字或 Mermaid 保留字符
3. **一个代码块只含一种类型**：不要在一个代码块中混用多种图表类型。
4. **不要用小写 `end` 作节点/标签/状态名**：会导致解析失败。改用 `End`、`END`、引号、`(end)` 或 `["end"]`。
5. **未经要求不加样式**：除非用户明确要求，不要添加主题、`init` 指令或 style 块，仅用默认主题。

## 工作流程
1. 根据请求选择匹配的图表类型
2. 用 read 读取该类型的 `references/<file>`
3. 生成图表并放在单个 `mermaid` 代码块中；代码块后如需补充，只可附简短说明，不要混入其他图表或冗长解释
4. 若用户反馈渲染失败，回到 references 复核语法并修正后重新输出

## 类型对照表
| ---- | ---- |
| Flowchart 流程图 | [flowchart.md](references/flowchart.md) |
| Sequence 时序图 | [sequenceDiagram.md](references/sequenceDiagram.md) |
| Class 类图 | [classDiagram.md](references/classDiagram.md) |
| State 状态图 | [stateDiagram.md](references/stateDiagram.md) |
| ER 实体关系图 | [entityRelationshipDiagram.md](references/entityRelationshipDiagram.md) |
| Gantt 甘特图 | [gantt.md](references/gantt.md) |
| Pie 饼图 | [pie.md](references/pie.md) |
| Mindmap 思维导图 | [mindmap.md](references/mindmap.md) |
| Timeline 时间线图 | [timeline.md](references/timeline.md) |
| Git graph Git提交图 | [gitgraph.md](references/gitgraph.md) |
| Quadrant 象限图 | [quadrantChart.md](references/quadrantChart.md) |
| Requirement 需求图 | [requirementDiagram.md](references/requirementDiagram.md) |
| C4 C4架构图 | [c4.md](references/c4.md) |
| Sankey 桑基图 | [sankey.md](references/sankey.md) |
| XY chart XY图表 | [xyChart.md](references/xyChart.md) |
| Block 框图 | [block.md](references/block.md) |
| Packet 数据包图 | [packet.md](references/packet.md) |
| Kanban 看板图 | [kanban.md](references/kanban.md) |
| Architecture 架构图 | [architecture.md](references/architecture.md) |
| Radar 雷达图 | [radar.md](references/radar.md) |
| Treemap 树形矩阵图 | [treemap.md](references/treemap.md) |
| User journey 用户旅程图 | [userJourney.md](references/userJourney.md) |
| ZenUML ZenUML时序图 | [zenuml.md](references/zenuml.md) |
| Wardley 沃德利图 | [wardley.md](references/wardley.md) |
| Venn 维恩图 | [venn.md](references/venn.md) |
| Tree view 树形视图 | [treeView.md](references/treeView.md) |
| Swimlanes 泳道图 | [swimlanes.md](references/swimlanes.md) |
| Railroad 铁路图（语法图） | [railroad.md](references/railroad.md) |
| Ishikawa 鱼骨图 | [ishikawa.md](references/ishikawa.md) |
| Event modeling 事件建模图 | [eventmodeling.md](references/eventmodeling.md) |
| Cynefin 辛温框架图 | [cynefin.md](references/cynefin.md) |