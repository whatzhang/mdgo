# Canvas 白板功能增强设计

**日期:** 2026-06-25
**状态:** 设计中

## 1. 概述

本次增强为 Canvas 白板添加节点拖拽移动和连接线样式功能，保持与 Obsidian Canvas 数据格式兼容。

## 2. 功能范围

### 2.1 节点拖拽移动

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

### 2.2 节点类型（现有保持）

| 类型 | 说明 | 数据字段 |
|------|------|----------|
| text | 文本节点 | `type: "text"`, `text: "markdown内容"` |
| file | 文件节点 | `type: "file"`, `file: "路径"` |
| image | 图片节点 | `type: "file"`, `file: "图片路径"` |

### 2.3 连接线增强

**箭头样式:**
- 在 SVG `<defs>` 中定义 `marker` 箭头
- 默认为直线箭头
- 箭头颜色跟随边的颜色

**曲线连接:**
- 使用二次贝塞尔曲线 (`Q`)
- 通过 `curveFactor` 属性控制曲率（可选，默认 0.5）

**边的数据结构:**
```json
{
  "id": "edge-xxx",
  "fromNode": "nodeId1",
  "toNode": "nodeId2",
  "fromSide": "right",
  "toSide": "left",
  "color": "#000000",
  "width": 2
}
```

**边的编辑:**
- 点击边打开编辑面板
- 可编辑 `fromNode`、`toNode`、`fromSide`、`toSide`
- 边的删除按钮

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
      "toSide": "left",
      "color": "#000000",
      "width": 2
    }
  ]
}
```

## 4. 实现计划

### Phase 1: 节点拖拽
1. 修改 `renderCanvasNode` 添加 drag 行为
2. 添加拖拽时连线更新逻辑
3. 拖拽结束后调用保存函数

### Phase 2: 连接线增强
1. 在 `renderCanvasFile` 添加 SVG `<defs>` 箭头定义
2. 修改 `renderCanvasEdges` 应用箭头 marker
3. 支持边的颜色和宽度属性

### Phase 3: 边编辑
1. 点击边显示编辑面板
2. 实现边的属性编辑和删除

## 5. 关键函数修改

| 函数 | 修改内容 |
|------|----------|
| `renderCanvasNode` | 添加 d3.drag() 拖拽支持 |
| `renderCanvasEdges` | 添加箭头 marker，应用曲线样式 |
| `renderCanvasFile` | 添加 SVG defs 箭头定义 |
| `saveNodeToFile` | 接收完整 nodes 数组而非单个节点 |
| 新增: `updateNodePosition` | 更新节点坐标并保存 |
| 新增: `showEdgeEditPanel` | 显示边编辑面板 |

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
