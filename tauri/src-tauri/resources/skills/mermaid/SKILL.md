---
id: mermaid
scope: system
name: Mermaid 图表
description: Generate syntax-correct Mermaid diagram code (flowchart, sequence, class, ER, gantt, mindmap, etc.) from user requirements. Trigger when the user wants a diagram, flowchart, chart, or diagram-as-code embedded in Markdown.
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
| Type | File |
| ---- | ---- |
| Flowchart | [flowchart.md](references/flowchart.md) |
| Sequence | [sequenceDiagram.md](references/sequenceDiagram.md) |
| Class | [classDiagram.md](references/classDiagram.md) |
| State | [stateDiagram.md](references/stateDiagram.md) |
| ER | [entityRelationshipDiagram.md](references/entityRelationshipDiagram.md) |
| Gantt | [gantt.md](references/gantt.md) |
| Pie | [pie.md](references/pie.md) |
| Mindmap | [mindmap.md](references/mindmap.md) |
| Timeline | [timeline.md](references/timeline.md) |
| Git graph | [gitgraph.md](references/gitgraph.md) |
| Quadrant | [quadrantChart.md](references/quadrantChart.md) |
| Requirement | [requirementDiagram.md](references/requirementDiagram.md) |
| C4 | [c4.md](references/c4.md) |
| Sankey | [sankey.md](references/sankey.md) |
| XY chart | [xyChart.md](references/xyChart.md) |
| Block | [block.md](references/block.md) |
| Packet | [packet.md](references/packet.md) |
| Kanban | [kanban.md](references/kanban.md) |
| Architecture | [architecture.md](references/architecture.md) |
| Radar | [radar.md](references/radar.md) |
| Treemap | [treemap.md](references/treemap.md) |
| User journey | [userJourney.md](references/userJourney.md) |
| ZenUML | [zenuml.md](references/zenuml.md) |
| Wardley | [wardley.md](references/wardley.md) |
| Venn | [venn.md](references/venn.md) |
| Tree view | [treeView.md](references/treeView.md) |
| Swimlanes | [swimlanes.md](references/swimlanes.md) |
| Railroad | [railroad.md](references/railroad.md) |
| Ishikawa | [ishikawa.md](references/ishikawa.md) |
| Event modeling | [eventmodeling.md](references/eventmodeling.md) |
| Cynefin | [cynefin.md](references/cynefin.md) |
| Examples | [examples.md](references/examples.md) |

## Optional config (only if the user explicitly asks)
- Themes/colors: [config-theming.md](references/config-theming.md)
- Directives: [config-directives.md](references/config-directives.md)
- Layout: [config-layouts.md](references/config-layouts.md)
- Math/LaTeX: [config-math.md](references/config-math.md)
- Tidy tree: [config-tidy-tree.md](references/config-tidy-tree.md)
- Global config: [config-configuration.md](references/config-configuration.md)
