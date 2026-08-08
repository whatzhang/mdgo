---
id: mermaid
scope: system
name: Mermaid 图表
description: 根据用户需求生成语法正确的 Mermaid 图表代码（如流程图，时序图，类图，状态图，实体关系图，甘特图，饼图，思维导图，时间线图，Git 提交图，象限图，需求图，C4 架构图，桑基图，XY 图表，框图，数据包图，看板图，架构图，雷达图，树形矩阵图，用户旅程图，ZenUML 时序图，沃德利图，维恩图，树形视图，泳道图，铁路图（语法图）, 鱼骨图，事件建模图，辛温框架图）。当用户需要在 Markdown 中嵌入图表、流程图或“代码即图表”（diagram-as-code）内容时触发此功能。
priority: 60
roles: ["owner"]
tools: [read]
enabled: true
version: 2
created_at: 1754200000000
updated_at: 1754200000000
---

# Mermaid Diagram Generator

Generate **syntax-correct** Mermaid code. Diagrams are text-as-code with automatic layout; embed in Markdown via `mermaid` code blocks.

## Routing (do NOT use for)
- Pixel-precise layout, custom positioning, branded icons → **drawio**
- Hand-drawn / sketchy / freeform whiteboard → **excalidraw** / **tldraw**
- Strict conventional UML → **plantuml**

## Mandatory rules
1. **Read first**: before generating, `read` the reference file of the chosen type (`references/<file>`). Never generate uncommon types from memory.
2. **Quote all labels**: wrap every label in double quotes, especially Chinese / non-ASCII / special characters. e.g. `A["提交订单"]`. Never put unquoted Chinese or punctuation in labels.
3. **One type per block**: never mix diagram types inside one code block.
4. **Never use lowercase `end`** as a node/label/state name — it breaks parsing. Use `End`, `END`, quotes, `(end)`, or `["end"]` instead.
5. **No styling unless asked**: do NOT add themes, `init` directives, or style blocks unless the user explicitly requests them. Default theme only.

## Workflow
1. Pick the diagram type matching the request.
2. `read` `references/<file>` for that type.
3. Generate the diagram and output it in a single `mermaid` code block. Nothing else after the block.

## Type → reference
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