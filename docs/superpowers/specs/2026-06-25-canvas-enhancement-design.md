# Canvas 白板功能增强设计

**日期:** 2026-06-25
**状态:** 设计中

## 1. 概述

本次增强为 Canvas 白板添加节点拖拽移动和连接线样式功能，保持与 Obsidian Canvas 数据格式兼容。

## 2. 功能范围

### 2.1 工具栏入口

**功能描述:**

- 在顶部工具栏添加「白板」按钮（类似 dashboard 按钮）
- 点击按钮进入 Canvas 主界面
- Canvas 主界面右上角显示保存按钮

**实现方式:**

- 在工具栏按钮区域添加 canvas 按钮
- 添加 `toggleFile('canvas')` 处理
- Canvas 模式下显示专属的 top-right-controls

### 2.2 保存功能

**功能描述:**

- 点击保存按钮弹出文件目录树选择面板
- 保存逻辑与 Mermaid 保持一致

**实现方式:**

- 使用与 Mermaid 相同的 `renderDirectoryTree` 函数
- 提供目录树选择保存位置
- 保存后更新 `currentFileHandle`

### 2.3 节点拖拽移动

**实现位置:** `renderCanvasNode` 函数

**实现方式:**

- 使用 D3.js `d3.drag()` 行为
- 给每个 `.canvas-node` 添加拖拽事件监听
- 拖拽过程中实时更新节点位置
- 拖拽结束后自动保存节点数据到文件

**交互细节:**

- 鼠标按下节点开始拖拽
- 拖拽时鼠标变为 `grabbing` 样式
- 拖拽过程中相关连线同步跟随
- 释放鼠标后保存更新

### 2.3 节点类型（现有保持）

| 类型  | 说明     | 数据字段                               |
| ----- | -------- | -------------------------------------- |
| text  | 文本节点 | `type: "text"`, `text: "markdown内容"` |
| file  | 文件节点 | `type: "file"`, `file: "路径"`         |
| image | 图片节点 | `type: "file"`, `file: "图片路径"`     |

### 2.4 边交互

**曲线连接:**

- 使用二次贝塞尔曲线 (`Q`)
- 通过 `curveFactor` 属性控制曲率（默认 0.5）

**点击边:**

- 点击边弹出下拉选择框
- 选择目标节点：切换 `fromNode` 或 `toNode`
- 选择连接点：切换 `fromSide` / `toSide`（left/right/top/bottom）

**删除边:**

- 在选择框中提供「删除」按钮
- 点击后直接删除边并保存

**数据结构更新:**

```json
{
  "id": "edge-xxx",
  "fromNode": "nodeId1",
  "toNode": "nodeId2",
  "fromSide": "right",
  "toSide": "left"
}
```

**交互流程:**

1. 点击边 → 显示浮层选择框
2. 选择「切换目标」→ 显示节点列表选择
3. 选择「删除」→ 删除边并保存

## 3. 数据结构

### 3.1 Canvas 数据格式（兼容 Obsidian）

```json
{
  "nodes": [
    {
      "id": "node-xxx",
      "x": 100,
      "y": 100,
      "width": 200,
      "height": 150,
      "type": "text",
      "text": "节点内容",
      "file": null
    }
  ],
  "edges": [
    {
      "id": "edge-xxx",
      "fromNode": "node-xxx",
      "toNode": "node-yyy",
      "fromSide": "right",
      "toSide": "left"
    }
  ]
}
```

## 4. 实现计划

### Phase 0: 工具栏入口与保存功能

1. 在工具栏添加 canvas 按钮
2. 添加 `toggleFile('canvas')` 处理
3. Canvas 模式下显示 top-right-controls 保存按钮
4. 实现目录树保存弹窗（复用 mermaid 逻辑）

### Phase 1: 节点拖拽

1. 修改 `renderCanvasNode` 添加 drag 行为
2. 添加拖拽时连线更新逻辑
3. 拖拽结束后调用保存函数

### Phase 2: 连接线箭头

1. 在 `renderCanvasFile` 添加 SVG `<defs>` 箭头定义
2. 修改 `renderCanvasEdges` 应用箭头 marker

### Phase 3: 边交互

1. 点击边显示浮层选择框
2. 支持切换目标节点和连接点
3. 支持删除边

## 5. 关键函数修改

| 函数                      | 修改内容                         |
| ------------------------- | -------------------------------- |
| 工具栏按钮                | 添加 canvas 按钮                 |
| `toggleFile`              | 添加 'canvas' case               |
| `openCanvas`              | 新增：打开 canvas 界面           |
| `renderCanvasNode`        | 添加 d3.drag() 拖拽支持          |
| `renderCanvasEdges`       | 添加箭头 marker，应用曲线样式    |
| `renderCanvasFile`        | 添加 SVG defs 箭头定义           |
| `saveCanvasToFile`        | 新增：保存 canvas 到文件         |
| `loadCanvasDirectoryTree` | 新增：加载目录树（复用 mermaid） |
| `showCanvasNodeContent`   | 显示节点内容面板                 |
| 新增: `showEdgePopup`     | 显示边编辑浮层                   |
| 新增: `deleteEdge`        | 删除边并保存                     |

## 6. 样式

```css
.canvas-node {
  cursor: grab;
}
.canvas-node:active {
  cursor: grabbing;
}
.canvas-node.dragging {
  opacity: 0.8;
}
.canvas-edge-path {
  fill: none;
  stroke-width: 2;
  cursor: pointer;
}
.canvas-edge-path:hover {
  stroke-width: 3;
}
```

## 7. 限制范围

- 不实现多选
- 不实现键盘快捷键
- 不实现右键菜单
- 不实现分组 (Group)
- 仅支持文本、文件、图片三种节点类型
