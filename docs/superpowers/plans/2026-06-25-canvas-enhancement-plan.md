# Canvas 白板功能增强实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**目标:** 为 Canvas 白板添加节点拖拽移动、连接线箭头和边交互功能

**架构:** 使用 D3.js drag 行为实现节点拖拽，SVG marker 实现箭头，点击弹出浮层实现边交互

**技术栈:** D3.js (drag, zoom), SVG marker, 原生 JS DOM 操作

---

## 文件修改清单

| 文件 | 修改内容 |
|------|----------|
| `index.html` | 主要修改文件，约 17500-17600 行区域为 canvas 渲染代码 |

---

## Phase 1: 节点拖拽

### Task 1: 添加节点拖拽支持

**Files:**
- Modify: `index.html:17489-17505` (`renderCanvasNode` 函数)
- Modify: `index.html:17942-17953` (`saveNodeToFile` 函数)

- [ ] **Step 1: 读取现有 `renderCanvasNode` 函数代码**

确认当前函数结构，为添加 drag 行为做准备。

- [ ] **Step 2: 添加 `saveNodesToFile` 函数**

在 `saveNodeToFile` 函数后添加新函数，用于保存完整的 nodes 数组：

```javascript
async function saveNodesToFile(nodes) {
    if (!currentFileHandle) return;
    const text = await getFileContent(currentFileHandle);
    let canvasData = JSON.parse(text);
    canvasData.nodes = nodes;
    await saveExistFileContent(JSON.stringify(canvasData));
}
```

- [ ] **Step 3: 修改 `renderCanvasNode` 添加 drag 行为**

在 `renderCanvasNode` 函数中添加 d3.drag() 支持：

```javascript
async function renderCanvasNode(node, group) {
    const nodeGroup = group.append('g')
        .attr('class', 'canvas-node')
        .attr('transform', `translate(${node.x}, ${node.y})`)
        .style('cursor', 'grab');

    // 添加拖拽行为
    const drag = d3.drag()
        .on('start', function(event) {
            d3.select(this).style('cursor', 'grabbing').attr('opacity', 0.8);
        })
        .on('drag', function(event) {
            const dx = event.dx;
            const dy = event.dy;
            const currentX = parseFloat(d3.select(this).attr('transform').replace('translate(', '').split(',')[0]);
            const currentY = parseFloat(d3.select(this).attr('transform').split(',')[1].replace(')', ''));
            const newX = currentX + dx;
            const newY = currentY + dy;
            d3.select(this).attr('transform', `translate(${newX}, ${newY})`);
            node.x = newX;
            node.y = newY;
            // 更新相关连线
            updateEdgesForNode(node.id);
        })
        .on('end', function(event) {
            d3.select(this).style('cursor', 'grab').attr('opacity', 1);
            // 保存更新后的节点位置
            saveNodesToFile(canvasData.nodes);
        });

    nodeGroup.call(drag);

    nodeGroup.append('rect')
        .attr('class', 'canvas-node-rect')
        .attr('width', node.width)
        .attr('height', node.height);
    if (node.type === 'text') {
        await renderTextNode(node, nodeGroup);
    } else if (node.type === 'file') {
        renderFileNode(node, nodeGroup);
    }
}
```

- [ ] **Step 4: 添加 `updateEdgesForNode` 函数**

在 `renderCanvasNode` 前添加，用于在拖拽时更新相关连线：

```javascript
function updateEdgesForNode(nodeId) {
    const edgesGroup = d3.select('.canvas-edges');
    if (edgesGroup.empty()) return;
    const edgePaths = edgesGroup.selectAll('.canvas-edge-path');
    edgePaths.each(function(d) {
        const edge = d3.select(this).datum();
        if (edge.fromNode === nodeId || edge.toNode === nodeId) {
            // 重新计算边的路径
            const fromNode = canvasData.nodes.find(n => n.id === edge.fromNode);
            const toNode = canvasData.nodes.find(n => n.id === edge.toNode);
            if (fromNode && toNode) {
                const path = calculateEdgePath(fromNode, toNode, edge);
                d3.select(this).attr('d', path);
            }
        }
    });
}

function calculateEdgePath(fromNode, toNode, edge) {
    let startX = fromNode.x + fromNode.width / 2;
    let startY = fromNode.y + fromNode.height / 2;
    let endX = toNode.x + toNode.width / 2;
    let endY = toNode.y + toNode.height / 2;
    if (edge.fromSide === 'right') startX = fromNode.x + fromNode.width;
    if (edge.fromSide === 'left') startX = fromNode.x;
    if (edge.fromSide === 'top') startY = fromNode.y;
    if (edge.fromSide === 'bottom') startY = fromNode.y + fromNode.height;
    if (edge.toSide === 'right') endX = toNode.x + toNode.width;
    if (edge.toSide === 'left') endX = toNode.x;
    if (edge.toSide === 'top') endY = toNode.y;
    if (edge.toSide === 'bottom') endY = toNode.y + toNode.height;
    const dx = endX - startX;
    const dy = endY - startY;
    const controlPointX = startX + dx / 2;
    const controlPointY = startY + dy / 2;
    return `M ${startX} ${startY} Q ${controlPointX} ${controlPointY} ${endX} ${endY}`;
}
```

- [ ] **Step 5: 添加 CSS 样式**

在 `<style>` 区域添加拖拽相关样式（如果不存在）：

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
```

- [ ] **Step 6: 测试节点拖拽**

1. 打开 canvas 文件
2. 点击并拖拽节点
3. 确认节点可以移动
4. 确认释放后内容正确保存
5. 确认拖拽时连线同步更新

---

## Phase 2: 连接线箭头

### Task 2: 添加 SVG 箭头 marker 定义

**Files:**
- Modify: `index.html:17552-17598` (`renderCanvasFile` 函数)

- [ ] **Step 1: 在 `renderCanvasFile` 函数中添加 SVG defs**

在 `contentDiv.innerHTML = ...` 后添加箭头定义：

```javascript
// 在 contentDiv.innerHTML 之后添加
const svgEl = contentDiv.querySelector('.canvas-svg');
// 添加箭头 marker 定义
const defs = svgEl.querySelector('defs') || (() => {
    const d = document.createElementNS('http://www.w3.org/2000/svg', 'defs');
    svgEl.insertBefore(d, svgEl.firstChild);
    return d;
})();

defs.innerHTML = `
    <marker id="arrowhead" markerWidth="10" markerHeight="7" refX="9" refY="3.5" orient="auto">
        <polygon points="0 0, 10 3.5, 0 7" fill="#1f2328"/>
    </marker>
`;
```

- [ ] **Step 2: 修改 `renderCanvasEdges` 应用箭头 marker**

修改 `renderCanvasEdges` 函数（约 17207 行），在 `path` 元素上添加 `marker-end` 属性：

```javascript
// 找到 edgesGroup.append('path') 部分，添加 marker-end
edgesGroup.append('path')
    .attr('class', 'canvas-edge-path')
    .attr('d', `M ${startX} ${startY} Q ${controlPointX} ${controlPointY} ${endX} ${endY}`)
    .attr('stroke', '#1f2328')
    .attr('marker-end', 'url(#arrowhead)');
```

- [ ] **Step 3: 测试箭头显示**

1. 确认连线末端显示箭头
2. 确认箭头颜色与连线一致

---

## Phase 3: 边交互

### Task 3: 点击边显示浮层选择框

**Files:**
- Modify: `index.html:17207-17238` (`renderCanvasEdges` 函数)
- Add: 全局函数 `showEdgePopup`, `updateEdge`, `deleteEdge`

- [ ] **Step 1: 修改 `renderCanvasEdges` 添加点击事件**

在 `path` 元素上添加点击事件监听：

```javascript
edgesGroup.append('path')
    .attr('class', 'canvas-edge-path')
    .attr('d', `M ${startX} ${startY} Q ${controlPointX} ${controlPointY} ${endX} ${endY}`)
    .attr('stroke', '#1f2328')
    .attr('marker-end', 'url(#arrowhead)')
    .attr('cursor', 'pointer')
    .on('click', function(event, d) {
        event.stopPropagation();
        showEdgePopup(event, d);
    })
    .datum(edge); // 将 edge 数据绑定到元素
```

- [ ] **Step 2: 添加 `showEdgePopup` 函数**

在 `renderCanvasEdges` 函数附近添加：

```javascript
function showEdgePopup(event, edge) {
    // 移除已存在的弹窗
    const existing = document.getElementById('edge-popup');
    if (existing) existing.remove();

    const popup = document.createElement('div');
    popup.id = 'edge-popup';
    popup.className = 'edge-popup';
    popup.innerHTML = `
        <div class="edge-popup-title">编辑连接</div>
        <div class="edge-popup-section">
            <label>起点节点:</label>
            <select id="edge-from-node">
                ${canvasData.nodes.map(n => `<option value="${n.id}" ${n.id === edge.fromNode ? 'selected' : ''}>${n.type === 'text' ? (n.text || '').substring(0, 20) : n.file}</option>`).join('')}
            </select>
        </div>
        <div class="edge-popup-section">
            <label>起点连接点:</label>
            <select id="edge-from-side">
                <option value="left" ${edge.fromSide === 'left' ? 'selected' : ''}>左</option>
                <option value="right" ${edge.fromSide === 'right' ? 'selected' : ''}>右</option>
                <option value="top" ${edge.fromSide === 'top' ? 'selected' : ''}>上</option>
                <option value="bottom" ${edge.fromSide === 'bottom' ? 'selected' : ''}>下</option>
            </select>
        </div>
        <div class="edge-popup-section">
            <label>终点节点:</label>
            <select id="edge-to-node">
                ${canvasData.nodes.map(n => `<option value="${n.id}" ${n.id === edge.toNode ? 'selected' : ''}>${n.type === 'text' ? (n.text || '').substring(0, 20) : n.file}</option>`).join('')}
            </select>
        </div>
        <div class="edge-popup-section">
            <label>终点连接点:</label>
            <select id="edge-to-side">
                <option value="left" ${edge.toSide === 'left' ? 'selected' : ''}>左</option>
                <option value="right" ${edge.toSide === 'right' ? 'selected' : ''}>右</option>
                <option value="top" ${edge.toSide === 'top' ? 'selected' : ''}>上</option>
                <option value="bottom" ${edge.toSide === 'bottom' ? 'selected' : ''}>下</option>
            </select>
        </div>
        <div class="edge-popup-buttons">
            <button id="edge-delete-btn" class="btn btn-danger btn-sm">删除</button>
            <button id="edge-save-btn" class="btn btn-primary btn-sm">保存</button>
        </div>
    `;

    // 定位弹窗
    popup.style.position = 'fixed';
    popup.style.left = event.pageX + 'px';
    popup.style.top = event.pageY + 'px';
    popup.style.zIndex = '10000';
    popup.style.background = 'white';
    popup.style.border = '1px solid #ddd';
    popup.style.borderRadius = '4px';
    popup.style.padding = '10px';
    popup.style.boxShadow = '0 2px 10px rgba(0,0,0,0.1)';

    document.body.appendChild(popup);

    // 保存按钮
    document.getElementById('edge-save-btn').onclick = () => {
        edge.fromNode = document.getElementById('edge-from-node').value;
        edge.fromSide = document.getElementById('edge-from-side').value;
        edge.toNode = document.getElementById('edge-to-node').value;
        edge.toSide = document.getElementById('edge-to-side').value;
        saveEdgesToFile(canvasData.edges);
        popup.remove();
        reloadCanvasDataWithoutPanel();
    };

    // 删除按钮
    document.getElementById('edge-delete-btn').onclick = () => {
        deleteEdge(edge.id);
        popup.remove();
    };

    // 点击外部关闭
    setTimeout(() => {
        document.addEventListener('click', function closePopup(e) {
            if (!popup.contains(e.target)) {
                popup.remove();
                document.removeEventListener('click', closePopup);
            }
        });
    }, 10);
}
```

- [ ] **Step 3: 添加 `saveEdgesToFile` 函数**

```javascript
async function saveEdgesToFile(edges) {
    if (!currentFileHandle) return;
    const text = await getFileContent(currentFileHandle);
    let canvasData = JSON.parse(text);
    canvasData.edges = edges;
    await saveExistFileContent(JSON.stringify(canvasData));
}
```

- [ ] **Step 4: 添加 `deleteEdge` 函数**

```javascript
async function deleteEdge(edgeId) {
    if (!currentFileHandle) return;
    const text = await getFileContent(currentFileHandle);
    let canvasData = JSON.parse(text);
    canvasData.edges = canvasData.edges.filter(e => e.id !== edgeId);
    await saveExistFileContent(JSON.stringify(canvasData));
    reloadCanvasDataWithoutPanel();
}
```

- [ ] **Step 5: 添加 CSS 样式**

```css
.canvas-edge-path {
    fill: none;
    stroke-width: 2;
    cursor: pointer;
}
.canvas-edge-path:hover {
    stroke-width: 3;
}
.edge-popup {
    min-width: 180px;
}
.edge-popup-title {
    font-weight: 600;
    margin-bottom: 8px;
    border-bottom: 1px solid #eee;
    padding-bottom: 4px;
}
.edge-popup-section {
    margin-bottom: 6px;
}
.edge-popup-section label {
    display: block;
    font-size: 11px;
    color: #666;
    margin-bottom: 2px;
}
.edge-popup-section select {
    width: 100%;
    padding: 4px;
    border: 1px solid #ddd;
    border-radius: 3px;
}
.edge-popup-buttons {
    display: flex;
    gap: 6px;
    justify-content: flex-end;
    margin-top: 10px;
    padding-top: 8px;
    border-top: 1px solid #eee;
}
```

- [ ] **Step 6: 测试边交互**

1. 点击连线，弹出选择框
2. 修改起点/终点节点，确认连线更新
3. 修改连接点，确认连线更新
4. 点击删除，确认连线删除
5. 点击外部关闭弹窗

---

## 实现顺序

1. **Task 1** - 节点拖拽（核心功能）
2. **Task 2** - 连接线箭头（视觉增强）
3. **Task 3** - 边交互（编辑功能）

---

## 验证清单

- [ ] 节点可拖拽移动
- [ ] 拖拽时连线同步更新
- [ ] 拖拽释放后位置保存
- [ ] 连线显示箭头
- [ ] 点击连线弹出编辑框
- [ ] 修改节点/连接点后连线更新
- [ ] 可删除连线
