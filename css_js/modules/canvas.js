const _CANVAS_IMG_CONCURRENCY = 4;         // Canvas 图片节点加载并发限制（避免 20 张大图同时解码导致 CPU 飙升）
function editCanvasNodeContent(e, btn){
    // L35：_canvasObject 未初始化时静默跳过，避免 TypeError 中断（画布未加载/已销毁时点击节点按钮）
    if (_canvasObject && typeof _canvasObject.editNodeContent === 'function') {
        _canvasObject.editNodeContent(e, btn);
    }
}
function saveCanvasNodeContent(e, btn){
    // L35：同上，save 路径同样防护
    if (_canvasObject && typeof _canvasObject.saveNodeContent === 'function') {
        _canvasObject.saveNodeContent(e, btn);
    }
}
class CanvasLess {
    constructor() {
        this.selectedCanvasNodeId = null;
        this.pendingCanvasNodeType = null;
        this._canvasSaveDebounced = null;
        this._nodeMapCache = new Map();
        this._panelGeneration = 0;
        this.canvasData = { nodes: [], edges: [] };
        this._nodePanelState = {
            node: null,
            nodeType: 'text',
            filePath: '',
            fileContent: ''
        };
        this._canvasImgInFlight = 0;         // 当前正在加载的图片数量
        this._canvasImgQueue = [];           // 待加载的图片任务队列
        this._canvasObjectUrls = [];         // 本实例创建的 object URLs（重渲染时只撤销自己的，避免误撤销其他实例/父画布的图片）
        this.connectingFrom = null; // { nodeId, side } | null — 连线模式：已选中的起点连接点
        this._canvasEscHandler = null;
        this._canvasSaveDebounced = null;
        this.canvasState = null;
        this._canvasGroup = null;      // 当前渲染的 <g class="canvas-group">，供 bbox 居中计算使用
    }
    // 键盘 Esc 取消连线（命名函数引用，可正确移除避免泄漏）
    _ensureCanvasEscHandler() {
        if (this._canvasEscHandler) return; // 已注册，避免重复
        this._canvasEscHandler = this.handleCanvasShortcut;
        document.addEventListener('keydown', this._canvasEscHandler);
    }
    handleCanvasShortcut(e) {
        if (e.key === 'Escape' && this.connectingFrom) {
            this.cancelConnecting();
            showNotification('已取消连线', 'info');
            return;
        }
        if ((e.ctrlKey || e.metaKey) && e.code === 'KeyS') {
            e.preventDefault();
            e.stopImmediatePropagation();
            this.canvasSave();
        }
    }

    async renderCanvas(text, contentDiv, showSaveBtn = false, _canvasData = null, isChildCanvas = false) {
        await flushSaveQueue();
        if (!isChildCanvas) {
            showCanvasControls(showSaveBtn);
            await this.cleanupCanvasState();
            this._ensureCanvasEscHandler();
        }
        try {
            if (text) {
                this.canvasData = JSON.parse(text);
            } else {
                if (_canvasData) {
                    this.canvasData = _canvasData;
                } else {
                    throw new Error('数据不能为空！');
                }
            }
        } catch (e) {
            contentDiv.innerHTML = `<div style="color:#cf222e; padding:20px;">无效的 JSON Canvas 格式: ${escapeHtml(e.message)}</div>`;
            return;
        }
        const nodes = this.canvasData.nodes || [];
        const edges = this.canvasData.edges || [];
        if (this._canvasObjectUrls && this._canvasObjectUrls.length > 0) {
            this._canvasObjectUrls.forEach(url => { try { URL.revokeObjectURL(url); } catch (e) { /* 忽略 */ } });
            this._canvasObjectUrls = [];
        }
        this._canvasImgQueue = [];
        contentDiv.innerHTML = `
                <div class="canvas-container">
                    <svg class="canvas-svg" ${isChildCanvas ? 'style="background-color: var(--color-bg);"' : ''}></svg>
                </div>
                <span id="selectedNodeFilePath" style="display: ${isChildCanvas ? 'none' : 'block'}; position: absolute; bottom: 20px; left: 10px; font-size:12px;font-weight:500;color:var(--color-primary);z-index: 3004;"></span>`;
        const container = contentDiv.querySelector('.canvas-container');
        const containerWidth = container.clientWidth || 800;
        const containerHeight = container.clientHeight || 600;
        const svgEl = contentDiv.querySelector('.canvas-svg');
        svgEl.setAttribute('width', containerWidth);
        svgEl.setAttribute('height', containerHeight);
        const svg = d3.select(svgEl);
        const g = svg.append('g').attr('class', 'canvas-group');
        this._canvasGroup = g; // 保存组引用：居中计算以实际渲染内容 bbox 为准（而非仅依赖数据坐标）

        // 添加箭头 marker 定义
        const defs = svg.append('defs');
        defs.append('marker')
            .attr('id', 'arrowhead')
            .attr('markerWidth', '7')
            .attr('markerHeight', '5')
            .attr('refX', '6')
            .attr('refY', '2.5')
            .attr('orient', 'auto')
            .append('polygon')
            .attr('points', '0 0, 7 2.5, 0 5')
            .attr('fill', '#1f2328');
        // hover 时的蓝色箭头 marker：悬停连线时通过 CSS marker-end 切换，实现箭头同步变蓝
        defs.append('marker')
            .attr('id', 'arrowhead-hover')
            .attr('markerWidth', '7')
            .attr('markerHeight', '5')
            .attr('refX', '6')
            .attr('refY', '2.5')
            .attr('orient', 'auto')
            .append('polygon')
            .attr('points', '0 0, 7 2.5, 0 5')
            .attr('fill', '#3182f7');

        const zoom = this.initCanvasZoom(svg, g);
        if (!isChildCanvas) {
            // SVG 空白区域点击：关闭面板 + 自适应缩放
            let svgClickStart = null;
            svg.on('pointerdown', (event) => {
                svgClickStart = { x: event.clientX, y: event.clientY };
                // 每次 pointerdown 重新注册 document 级 pointerup，拖拽到 SVG 外释放时清理状态
                if (window._svgPointerUpCleanup) {
                    document.removeEventListener('pointerup', window._svgPointerUpCleanup);
                }
                window._svgPointerUpCleanup = () => { svgClickStart = null; };
                document.addEventListener('pointerup', window._svgPointerUpCleanup);
            });
            svg.on('pointerup', async (event) => {
                if (!svgClickStart) return;
                const dx = Math.abs(event.clientX - svgClickStart.x);
                const dy = Math.abs(event.clientY - svgClickStart.y);
                svgClickStart = null;
                // pointerup 已在 SVG 内触发，清理 document 级备份监听器
                if (window._svgPointerUpCleanup) {
                    document.removeEventListener('pointerup', window._svgPointerUpCleanup);
                    window._svgPointerUpCleanup = null;
                }
                if (dx < 4 && dy < 4) {
                    // 只有打开节点内容面板（右侧面板）时，点击空白才关闭面板并重置大小；
                    // 未打开任何节点时点击空白不做任何处理（保持当前缩放/平移状态）
                    if (document.getElementById('node-content-panel')) {
                        await this.closeNodeContentPanel();
                        this.fitCanvasToContent(container, svg, zoom);
                    }
                }
            });
        }
        const containerSelector = contentDiv.querySelector('.canvas-container');
        const contentMainContainer = contentMain.querySelector('.canvas-container');
        if (containerSelector === contentMainContainer) {
            // 清理旧的 canvasState（如果存在 d3 zoom 监听器）
            await this.cleanupCanvasState();
            this.canvasState = {
                svg,
                zoom,
                group: g,
                containerWidth,
                containerHeight
            };
        }
        this.renderCanvasEdges(edges, g, isChildCanvas);
        await this.renderCanvasNodes(nodes, g, isChildCanvas);
        const fitFn = () => this.fitCanvasToContent(container, svg, zoom);
        if (!isChildCanvas) {
            this.addCanvasControls(contentDiv, svg, zoom, fitFn);
        }
        setTimeout(fitFn, 100);
    }

    async _canvasImgProcessQueue() {
        while (this._canvasImgInFlight < _CANVAS_IMG_CONCURRENCY && this._canvasImgQueue.length > 0) {
            const task = this._canvasImgQueue.shift();
            this._canvasImgInFlight++;
            try { await task(); } catch (e) { console.error('canvas 图片加载失败:', e); }
            this._canvasImgInFlight = Math.max(0, this._canvasImgInFlight - 1);
            this._canvasImgProcessQueue();
        }
    }
    _canvasImgEnqueue(fn) {
        this._canvasImgQueue.push(fn);
        this._canvasImgProcessQueue();
    }

    async canvasSave() {
        const data = this.canvasData;
        const content = JSON.stringify({
            nodes: data.nodes,
            edges: data.edges
        });
        if (currentFileHandle) {
            const fileHandle = currentFileHandle;
            // 稳定持有防抖函数，复用同一 timer 实现真正的去抖；惰性创建，传参为最新 content/handle
            if (!this._canvasSaveDebounced) {
                this._canvasSaveDebounced = timeDebounce(async (fh, ct) => {
                    await enqueueFileAtomic(fh, async () => {
                        await _writeToFileHandle(fh, ct);
                        showNotification('✓ 画布保存成功', 'success');
                    });
                }, 500);
            }
            this._canvasSaveDebounced(fileHandle, content);
        } else {
            await showSaveModal({
                title: '保存canvas画布文件',
                getDefaultFileName: () => 'untitled.canvas',
                getContent: () => content,
                onSave: doSaveToFolder
            });
        }
    }
    removeNodeFromCanvas(nodeId) {
        // 移除节点 DOM
        d3.selectAll(`.canvas-node[data-node-id="${nodeId}"]`).remove();
        // 移除关联的连线 DOM（一次性筛选，避免 while(true) 低效遍历）
        // 注意：filter 回调必须用 function，d3 才会把 this 绑定到当前 <g> 元素
        d3.selectAll('.canvas-edge-group').filter(function () {
            const edge = d3.select(this).select('.canvas-edge-hit').datum();
            return edge && (edge.fromNode === nodeId || edge.toNode === nodeId);
        }).remove();
        // 关闭节点内容面板（如有未保存内容则自动保存）
        const contentPanel = document.getElementById('node-content-panel');
        if (contentPanel && this._nodePanelState.node && this._nodePanelState.node.id === nodeId) {
            this.closeNodeContentPanel();
        }
        // 更新节点计数（如果存在）
        const nodeCountEl = document.getElementById('node-count');
        if (nodeCountEl) {
            const n = parseInt(nodeCountEl.textContent) || 0;
            nodeCountEl.textContent = Math.max(0, n - 1);
        }
    }

    async canvasDeleteNode() {
        if (!this.selectedCanvasNodeId) {
            showNotification('请选择要删除的节点', 'warning');
            return;
        }
        // 如果正处于连线模式，取消连线状态避免残留
        if (this.connectingFrom) {
            this.cancelConnecting();
        }
        const nodeId = this.selectedCanvasNodeId;
        // 从  this.canvasData 中删除节点
        this.canvasData.nodes = this.canvasData.nodes.filter(n => n.id !== nodeId);
        // 同时删除与该节点相关的边
        this.canvasData.edges = this.canvasData.edges.filter(e => e.fromNode !== nodeId && e.toNode !== nodeId);
        this.rebuildNodeMap(); // 更新节点索引
        // 持久化（节点+边）
        if (currentFileHandle) {
            await this.saveNodesToFile(this.canvasData.nodes);
            await this.saveEdgesToFile(this.canvasData.edges);
        } else {
            // 新画布虽无文件句柄，但标记数据已变更供后续保存感知
            this.canvasData._dirty = true;
        }
        // 增量删除 DOM（不重新渲染整张画布）
        this.removeNodeFromCanvas(nodeId);
        // 重置选择状态
        this.selectedCanvasNodeId = null;
        this.updateDeleteNodeButtonState();
    }

    updateDeleteNodeButtonState() {
        const deleteBtn = document.getElementById('canvas-delete-btn');
        if (deleteBtn) {
            if (this.selectedCanvasNodeId) {
                deleteBtn.style.display = 'flex';
            } else {
                deleteBtn.style.display = 'none';
            }
        }
    }

    canvasAddNode(type) {
        if (type === 'text') {
            this.createCanvasNode('text', null);
        } else if (type === 'image' || type === 'file') {
            this.pendingCanvasNodeType = type;
            showFileSelectModal({
                type: type,
                onSelect: (fp, fn) => {
                    const fileData = { path: fp, name: fn };
                    this.createCanvasNode(type, fileData);
                    this.pendingCanvasNodeType = null;
                }
            });
        }
    }

    addNodeToCanvas(node) {
        const nodesGroup = d3.select('.canvas-nodes');
        if (nodesGroup.empty()) return;
        this.renderCanvasNode(node, nodesGroup);
        this.rebuildNodeMap();
    }

    createCanvasNode(type, fileData) {
        const newNode = {
            id: 'node_' + Date.now(),
            type: type,
            x: 30 + Math.random() * 200,
            y: 30 + Math.random() * 200,
            width: type === 'text' ? 200 : 300,
            height: type === 'text' ? 120 : 200
        };

        if (type === 'text') {
            newNode.text = '';
        } else if (type === 'image') {
            newNode.file = fileData.path || fileData.name;
            newNode.fileData = fileData;
        } else if (type === 'file') {
            newNode.file = fileData.path || fileData.name;
            newNode.fileData = fileData;
        }
        this.canvasData.nodes.push(newNode);
        this.rebuildNodeMap();
        // 增量添加 DOM（不触发全局重渲染）
        this.addNodeToCanvas(newNode);
        // 持久化到文件
        if (currentFileHandle) {
            this.saveNodesToFile(this.canvasData.nodes).catch(err => {
                console.error('保存节点失败:', err);
                showNotification('✗ 保存失败: ' + err.message, 'error');
            });
        }
    }
    setNodePanelState(patch = {}) {
        if ('node' in patch) this._nodePanelState.node = patch.node ? JSON.parse(JSON.stringify(patch.node)) : null;
        if ('nodeType' in patch) this._nodePanelState.nodeType = patch.nodeType || 'text';
        if ('filePath' in patch) this._nodePanelState.filePath = patch.filePath || '';
        if ('fileContent' in patch) this._nodePanelState.fileContent = patch.fileContent || '';
    }

    getNodePanelState() { return this._nodePanelState; }

    clearNodePanelState() {
        this._nodePanelState.node = null;
        this._nodePanelState.nodeType = 'text';
        this._nodePanelState.filePath = '';
        this._nodePanelState.fileContent = '';
    }

    isPanelGenerationStale(generation) { return generation !== this._panelGeneration; }

    calculateCanvasBounds(nodes) {
        let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
        nodes.forEach(node => {
            minX = Math.min(minX, node.x);
            minY = Math.min(minY, node.y);
            maxX = Math.max(maxX, node.x + node.width);
            maxY = Math.max(maxY, node.y + node.height);
        });
        // 空画布：以 (0, 0) 为中心，使用合理默认尺寸
        if (nodes.length === 0) {
            minX = -400; minY = -300;
            maxX = 400; maxY = 300;
        }
        const padding = 100;
        return {
            minX,
            minY,
            maxX,
            maxY,
            contentWidth: maxX - minX + padding * 2,
            contentHeight: maxY - minY + padding * 2,
            centerX: (minX + maxX) / 2,
            centerY: (minY + maxY) / 2
        };
    }

    // 获取画布内容的实际边界：优先用渲染后的 <g> 的 getBBox()（含文字溢出、图片等真实内容），
    // 失败/为空时回退到基于节点数据的边界。这是水平垂直居中的唯一数据源，不再依赖 CSS Flexbox。
    getCanvasContentBounds(group, nodes) {
        const pad = 60;
        try {
            const gNode = group && group.node ? group.node() : group;
            if (gNode) {
                const bbox = gNode.getBBox();
                if (bbox && bbox.width > 0 && bbox.height > 0
                    && Number.isFinite(bbox.x) && Number.isFinite(bbox.y)) {
                    return {
                        minX: bbox.x - pad,
                        minY: bbox.y - pad,
                        maxX: bbox.x + bbox.width + pad,
                        maxY: bbox.y + bbox.height + pad,
                        contentWidth: bbox.width + pad * 2,
                        contentHeight: bbox.height + pad * 2,
                        centerX: bbox.x + bbox.width / 2,
                        centerY: bbox.y + bbox.height / 2
                    };
                }
            }
        } catch (e) {
            // getBBox 在某些环境下可能抛错，回退到数据边界
        }
        return this.calculateCanvasBounds(nodes);
    }

    initCanvasZoom(svg, group) {
        const zoom = d3.zoom()
            .scaleExtent([0.1, 4])
            .on('zoom', (event) => {
                group.attr('transform', event.transform);
            });
        svg.call(zoom);
        return zoom;
    }

    createEdgeGroupElement(edge, edgesGroup, nodeMap, isChildCanvas = false) {
        const fromNode = nodeMap.get(edge.fromNode);
        const toNode = nodeMap.get(edge.toNode);
        if (!fromNode || !toNode) return null;

        const path = this.calculateEdgePath(fromNode, toNode, edge);
        const edgeGroup = edgesGroup.append('g').attr('class', 'canvas-edge-group');

        // 可见路径（带箭头和动画）
        edgeGroup.append('path')
            .attr('class', 'canvas-edge-path animated')
            .attr('d', path)
            .attr('stroke', '#1f2328')
            .attr('stroke-width', '0.975')
            .attr('marker-end', 'url(#arrowhead)')
            .style('pointer-events', 'none');

        // 磁吸热区路径（宽透明描边便于选中，负责接收鼠标事件）
        if (!isChildCanvas) {
            edgeGroup.append('path')
                .attr('class', 'canvas-edge-hit')
                .attr('d', path)
                .datum(edge)
                .on('dblclick', async (event) => {
                    event.stopPropagation();
                    await this.deleteEdge(edge.id);
                });
        } else {
            edgeGroup.append('path')
                .attr('class', 'canvas-edge-hit')
                .attr('d', path)
                .datum(edge);
        }
        return edgeGroup;
    }

    rebuildNodeMap() {
        this._nodeMapCache = new Map(this.canvasData.nodes.map(n => [n.id, n]));
    }

    renderCanvasEdges(edges, group, isChildCanvas = false) {
        const edgesGroup = group.append('g').attr('class', 'canvas-edges');
        this.rebuildNodeMap();
        const nodeMap = this._nodeMapCache;
        edges.forEach(edge => {
            this.createEdgeGroupElement(edge, edgesGroup, nodeMap, isChildCanvas);
        });
    }
    selectCanvasNode(id, nodeGroup, path) {
        d3.selectAll('.canvas-node-selected').classed('canvas-node-selected', false);
        nodeGroup.classed('canvas-node-selected', true);
        this.selectedCanvasNodeId = id;
        this.updateDeleteNodeButtonState();
        const dom = document.getElementById('selectedNodeFilePath');
        if (dom && path) dom.textContent = `📝 ${escapeHtml(path)}`;
    }
    async renderTextNode(node, nodeGroup) {
        // 点击/双击处理已迁移到 drag.end（通过累计偏移检测点击），
        // 避免 D3 drag 抑制原生 click 事件导致节点无法选中。
        const previewText = node.text?.trim() || '';
        let renderedText = '';
        if (node.isRoot) {
            if (hasMarkdown(previewText)) {
                renderedText = `<div class="markdown-body" style="color:var(--color-text-white);" id="md_nd_${node.id}">${await markedParse(parseObsidianToHTML(previewText))}</div>`
            } else {
                renderedText = `<div id="md_nd_${node.id}" class="center-div">${previewText}</div>`
            }
        } else {
            renderedText = previewText ? `<div class="markdown-body" id="md_nd_${node.id}">${await markedParse(parseObsidianToHTML(previewText))}</div>` : '<p style="color:var(--t2);text-align:center;font-size:13px;">点击编辑内容...</p>';
        }
        const foreignObject = nodeGroup.append('foreignObject')
            .attr('x', 10)
            .attr('y', 10)
            .attr('width', node.width - 20)
            .attr('height', node.height - 20)
            .attr('overflow', 'hidden');
        foreignObject.append('xhtml:div')
            .attr('class', 'canvas-node-text' + (node.isRoot ? ' center-div' : ''))
            .style('width', '100%')
            .style('height', '100%')
            .style('overflow', 'hidden')
            .style('color', node.isRoot ? 'white' : '#1f2328')
            .style('fontFamily', '-apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif')
            .style('fontSize', '13px')
            .style('lineHeight', '1.4')
            .html(renderedText);
        if (!node.isRoot && previewText) postProcessCodeAndDiagrams(document.getElementById(`md_nd_${node.id}`));
    }

    // ---- Canvas 扩展节点类型渲染（JSON Canvas 兼容）：link/url/bookmark / code ----
    renderSimpleBox(node, nodeGroup, html) {
        const foreignObject = nodeGroup.append('foreignObject')
            .attr('x', 10)
            .attr('y', 10)
            .attr('width', node.width - 20)
            .attr('height', node.height - 20)
            .attr('overflow', 'hidden');
        foreignObject.append('xhtml:div')
            .attr('class', 'canvas-node-text')
            .style('width', '100%')
            .style('height', '100%')
            .style('overflow', 'hidden')
            .style('fontSize', '12px')
            .style('lineHeight', '1.4')
            .html(html);
    }

    renderUrlNode(node, nodeGroup) {
        const url = escapeHtml(node.url || '');
        const txt = node.text ? escapeHtml(String(node.text)) : '';
        this.renderSimpleBox(node, nodeGroup,
            `<div style="font-weight:600;margin-bottom:4px;white-space:pre-wrap;">🔗 ${txt || url}</div>` +
            (url ? `<div style="color:var(--color-primary);word-break:break-all;font-size:11px;">${url}</div>` : ''));
    }

    renderCodeNode(node, nodeGroup) {
        const code = node.code != null ? String(node.code) : (node.text || '');
        this.renderSimpleBox(node, nodeGroup,
            `<pre style="white-space:pre-wrap;font-family:var(--font-mono);margin:0;background:var(--color-bg-subtle);padding:6px;border-radius:6px;max-height:100%;overflow:hidden;">${escapeHtml(code)}</pre>`);
    }

    async renderEmbeddedMarkdown(node, contentDiv) {
        try {
            const fileHandle = await getEmbedNodeFileHandleByPath(node.file);
            if (!fileHandle) {
                contentDiv.html(`<div style="display:flex;align-items:center;gap:6px;color:#cf222e;font-weight:500;">无法加载文件！ ${escapeHtml(node.file)}</div>`);
                return;
            }
            const previewText = await getFileContent(fileHandle);
            const renderedText = await markedParse(parseObsidianToHTML(previewText));
            contentDiv.html(`<div class="markdown-body" style="zoom: 0.6;background: transparent;" id="md_nd_${node.id}">${renderedText}</div>`);
        } catch (error) {
            console.error('加载 markdown 预览失败:', error);
            contentDiv.html(`<div style="display:flex;align-items:center;gap:6px;color:#cf222e;font-weight:500;">无法加载文件！ ${escapeHtml(node.file)}</div>`);
        }
    }

    async renderFileNode(node, nodeGroup) {
        // 点击/双击处理已迁移到 drag.end
        const foreignObject = nodeGroup.append('foreignObject')
            .attr('x', 10)
            .attr('y', 10)
            .attr('width', node.width - 20)
            .attr('height', node.height - 20)
            .attr('overflow', 'hidden');
        const contentDiv = foreignObject.append('xhtml:div')
            .style('width', '100%')
            .style('height', '100%')
            .html(`<div style="color:#8b9496;">⌛ 加载中...</div>`);
        if (isPicImage(getExt(node.file))) {
            await this.embedNodeImage(node.file, contentDiv);
        } else if (node.file.endsWith('.svg')) {
            await this.embedNodeSVG(node.file, null, contentDiv, null);
        } else if (node.file.endsWith('.mmd')) {
            await this.embedNodeMmd(node.file, null, contentDiv, null);
        } else if (node.file.endsWith('.md')) {
            await this.renderEmbeddedMarkdown(node, contentDiv).then(() => {
                postProcessCodeAndDiagrams(document.getElementById(`md_nd_${node.id}`));
            });
        } else if (node.file.endsWith('.canvas')) {
            await this.embedNodeCanvas(node.file, contentDiv);
        } else if (node.file.endsWith('.excalidraw')) {
            contentDiv.style.padding = '16px';
            await this.embedNodeExcalidraw(node.file, contentDiv);
        } else {
            contentDiv.attr('class', 'center-div canvas-node-text')
            contentDiv.style('color', 'var(--color-text)')
            contentDiv.style('background-color', 'var(--color-bg)')
            contentDiv.html(`<div style="color:var(--color-warning);display:inline-block; width:100%; text-align:center;" title="📄 ${escapeHtml(node.file)}">📄 ${escapeHtml(basename(node.file) || node.file)}</div>`);
        }
    }

    calculateEdgePath(fromNode, toNode, edge) {
        // 计算起点（所在边的中点）
        let startX = fromNode.x + fromNode.width / 2;
        let startY = fromNode.y + fromNode.height / 2;
        if (edge.fromSide === 'right') startX = fromNode.x + fromNode.width;
        if (edge.fromSide === 'left') startX = fromNode.x;
        if (edge.fromSide === 'top') startY = fromNode.y;
        if (edge.fromSide === 'bottom') startY = fromNode.y + fromNode.height;

        // 计算终点（所在边的中点）
        let endX = toNode.x + toNode.width / 2;
        let endY = toNode.y + toNode.height / 2;
        if (edge.toSide === 'right') endX = toNode.x + toNode.width;
        if (edge.toSide === 'left') endX = toNode.x;
        if (edge.toSide === 'top') endY = toNode.y;
        if (edge.toSide === 'bottom') endY = toNode.y + toNode.height;

        // 出/入方向向量（垂直于边的方向，指向节点外部）
        const dirMap = {
            right: [1, 0],
            left: [-1, 0],
            top: [0, -1],
            bottom: [0, 1],
        };
        const [fromDirX, fromDirY] = dirMap[edge.fromSide] || [1, 0];
        const [toDirX, toDirY] = dirMap[edge.toSide] || [-1, 0];

        const dx = endX - startX;
        const dy = endY - startY;
        const distance = Math.sqrt(dx * dx + dy * dy);
        if (distance === 0) return `M ${startX} ${startY} L ${endX} ${endY}`;

        // 控制点延伸距离：取总距离的 50%，上限 150px，保证近距离也不会太弯
        const cpDist = Math.min(distance * 0.5, 150);

        // 控制点沿出/入方向延伸，保证首尾段为水平/垂直直线，箭头方向正
        let cp1x = startX + fromDirX * cpDist;
        let cp1y = startY + fromDirY * cpDist;
        let cp2x = endX + toDirX * cpDist;
        let cp2y = endY + toDirY * cpDist;

        // 沿连线垂直方向偏移控制点，避免同一方向多条连线（如文档画布内圈外圈）重叠
        if (distance > 1) {
            const idStr = String(edge.fromNode) + '|' + String(edge.toNode);
            let hash = 0;
            for (let c = 0; c < idStr.length; c++) {
                hash = ((hash << 5) - hash) + idStr.charCodeAt(c);
                hash |= 0;
            }
            const dir = (hash % 3) - 1; // -1, 0, 1
            if (dir !== 0) {
                const perpOffset = 16 * dir;
                const nx = -dy / distance; // 垂直方向
                const ny = dx / distance;
                cp1x += nx * perpOffset;
                cp1y += ny * perpOffset;
                cp2x += nx * perpOffset;
                cp2y += ny * perpOffset;
            }
        }

        return `M ${startX} ${startY} C ${cp1x} ${cp1y}, ${cp2x} ${cp2y}, ${endX} ${endY}`;
    }

    updateEdgesForNode(nodeId) {
        const nodeMap = this._nodeMapCache;
        // 在当前 canvas-group 内查找所有 edge-group（包括拖拽时被移出 canvas-edges 的）
        const canvasGroup = document.querySelector('.canvas-group');
        const edgeGroups = canvasGroup
            ? canvasGroup.querySelectorAll('.canvas-edge-group')
            : document.querySelectorAll('.canvas-edge-group');
        edgeGroups.forEach(el => {
            const edge = d3.select(el).select('.canvas-edge-hit').datum();
            if (edge && (edge.fromNode === nodeId || edge.toNode === nodeId)) {
                const fromNode = nodeMap.get(edge.fromNode);
                const toNode = nodeMap.get(edge.toNode);
                if (fromNode && toNode) {
                    const path = this.calculateEdgePath(fromNode, toNode, edge);
                    d3.select(el).selectAll('path').attr('d', path);
                }
            }
        });
    }
    async saveEdgesToFile(edges) {
        if (!currentFileHandle) return;
        const fileHandle = currentFileHandle;
        try {
            await enqueueFileAtomic(fileHandle, async () => {
                const text = await getFileContent(fileHandle);
                let canvasDataLocal = JSON.parse(text);
                canvasDataLocal.edges = edges;
                const newContent = JSON.stringify(canvasDataLocal);
                await _writeToFileHandle(fileHandle, newContent);
                if (fileHandle === currentFileHandle) {
                    originalContent = newContent;
                }
            });
        } catch (err) {
            console.error('保存连线失败:', err);
            showNotification('✗ 保存连线失败: ' + err.message, 'error');
        }
    }

    async deleteEdge(edgeId) {
        // 从内存数据中移除
        this.canvasData.edges = this.canvasData.edges.filter(e => e.id !== edgeId);
        // 从 DOM 中增量移除（不触发全局刷新）
        this.removeEdgeFromCanvas(edgeId);
        // 持久化到文件
        if (currentFileHandle) {
            await this.saveEdgesToFile(this.canvasData.edges);
        }
        showNotification('连线已删除', 'success');
    }

    addEdgeToCanvas(edge) {
        const edgesGroup = d3.select('.canvas-edges');
        if (edgesGroup.empty()) return;
        this.createEdgeGroupElement(edge, edgesGroup, this._nodeMapCache);
    }

    removeEdgeFromCanvas(edgeId) {
        const edgesGroup = d3.select('.canvas-edges');
        if (edgesGroup.empty()) return;
        // 注意：each 回调必须用 function，d3 才会把 this 绑定到当前 <g> 元素
        edgesGroup.selectAll('.canvas-edge-group').each(function () {
            const hitPath = d3.select(this).select('.canvas-edge-hit');
            const edge = hitPath.datum();
            if (edge && edge.id === edgeId) {
                d3.select(this).remove();
            }
        });
    }

    async renderCanvasNode(node, group, isChildCanvas = false) {
        const self = this;
        const nodeGroup = group.append('g')
            .attr('class', 'canvas-node')
            .attr('data-node-id', node.id)
            .attr('transform', `translate(${node.x}, ${node.y})`)
            .style('cursor', 'grab')
            .classed('is-root', !!node.isRoot);
        let resizeMode = null;
        let initX, initY, initW, initH;
        // 在 DOM 上缓存节点尺寸（供委托的 mousemove 使用，避免闭包查找）
        nodeGroup.node().__nodeW = node.width;
        nodeGroup.node().__nodeH = node.height;
        nodeGroup.node().__hoveredEdge = null;
        // 拖拽：缩放/移动统一处理（event.dx/dy 是增量值，非累计）
        // 注意：必须使用 function 而非箭头函数，d3 才会把 this 绑定到被拖拽的 SVG <g> 元素；
        // 类方法/属性通过闭包捕获的 self（CanvasLess 实例）访问。
        const drag = d3.drag()
            .on('start', function (event) {
                const nodeEl = this;
                const nodesGroup = nodeEl.parentNode;
                const svgG = nodesGroup ? nodesGroup.parentNode : null;
                if (!svgG) return;
                // 标记该节点正在被拖拽（用于 drag.end 中判断是否需要恢复层级）
                nodeEl.__wasDragging = true;
                // 1. 将关联连线从 canvas-edges 移到 svgG（放在节点之前，节点会在连线之上）
                const edgesGroup = svgG.querySelector('.canvas-edges');
                if (edgesGroup) {
                    edgesGroup.querySelectorAll('.canvas-edge-group').forEach(el => {
                        const edge = d3.select(el).select('.canvas-edge-hit').datum();
                        if (edge && (edge.fromNode === node.id || edge.toNode === node.id)) {
                            svgG.insertBefore(el, nodesGroup.nextSibling);
                        }
                    });
                }
                // 2. 将被拖拽节点从 canvas-nodes 移到 svgG 最顶层
                svgG.appendChild(nodeEl);
                // 3. 给拖拽中的节点添加样式类
                d3.select(nodeEl).classed('canvas-node-dragging', true);
                resizeMode = this.__hoveredEdge;
                initX = node.x; initY = node.y;
                initW = node.width; initH = node.height;
                this.__accDx = 0;
                this.__accDy = 0;
                const cursorMap = {
                    e: 'ew-resize', w: 'ew-resize', s: 'ns-resize', n: 'ns-resize',
                    se: 'nwse-resize', nw: 'nwse-resize', sw: 'nesw-resize', ne: 'nesw-resize'
                };
                d3.select(this)
                    .style('cursor', resizeMode ? cursorMap[resizeMode] : 'grabbing')
                    .attr('opacity', 0.9);
            })
            .on('drag', function (event) {
                // 累加增量偏移（event.dx/dy 是每帧增量）
                this.__accDx += event.dx;
                this.__accDy += event.dy;
                const accDx = this.__accDx;
                const accDy = this.__accDy;
                const ng = d3.select(this);
                if (!resizeMode) {
                    // ---- 移动模式 ----
                    node.x = initX + accDx;
                    node.y = initY + accDy;
                    ng.attr('transform', `translate(${node.x}, ${node.y})`);
                    self.updateEdgesForNode(node.id);
                    return;
                }
                // ---- 缩放模式 ----
                let newW = initW, newH = initH, newX = initX, newY = initY;
                if (resizeMode.includes('e')) newW = initW + accDx;
                if (resizeMode.includes('w')) { newW = initW - accDx; newX = initX + accDx; }
                if (resizeMode.includes('s')) newH = initH + accDy;
                if (resizeMode.includes('n')) { newH = initH - accDy; newY = initY + accDy; }
                // 最小尺寸下限：不低于 40px，且不低于初始尺寸（允许用户缩回原始大小）
                const MIN_W = Math.min(100, initW);
                const MIN_H = Math.min(80, initH);
                if (newW < MIN_W) { newW = MIN_W; if (resizeMode.includes('w')) newX = initX + initW - MIN_W; }
                if (newH < MIN_H) { newH = MIN_H; if (resizeMode.includes('n')) newY = initY + initH - MIN_H; }
                node.x = newX; node.y = newY;
                node.width = newW; node.height = newH;
                // 更新 DOM 缓存尺寸（事件委托 mousemove 使用）
                this.__nodeW = newW;
                this.__nodeH = newH;
                ng.attr('transform', `translate(${newX}, ${newY})`);
                ng.select('.canvas-node-rect').attr('width', newW).attr('height', newH);
                const fo = ng.select('foreignObject');
                if (!fo.empty()) {
                    // text 节点 foreignObject 在 (10,10) 偏移处；file/image/md 节点同样在 (10,10) 偏移处
                    const padding = (node.type === 'text' || node.type === 'image' || node.type === 'file') ? 20 : 0;
                    fo.attr('width', Math.max(0, newW - padding))
                        .attr('height', Math.max(0, newH - padding));
                }
                ng.selectAll('.canvas-connector-dot').each(function () {
                    const dot = d3.select(this);
                    const side = dot.attr('data-side');
                    if (side === 'top') dot.attr('cx', newW / 2).attr('cy', -2);
                    if (side === 'bottom') dot.attr('cx', newW / 2).attr('cy', newH + 2);
                    if (side === 'left') dot.attr('cx', -2).attr('cy', newH / 2);
                    if (side === 'right') dot.attr('cx', newW + 2).attr('cy', newH / 2);
                });
                self.updateEdgesForNode(node.id);
            })
            .on('end', async function (event) {
                const nodeEl = this;
                d3.select(nodeEl)
                    .style('cursor', 'grab')
                    .attr('opacity', 1)
                    .classed('canvas-node-dragging', false);
                if (nodeEl.__wasDragging) {
                    nodeEl.__wasDragging = false;
                    const svgG = nodeEl.parentNode;
                    const edgesGroup = svgG.querySelector('.canvas-edges');
                    const nodesGroup = svgG.querySelector('.canvas-nodes');
                    // 1. 将关联连线移回 edgesGroup
                    if (edgesGroup) {
                        Array.from(svgG.children).forEach(el => {
                            if (el.matches && el.matches('.canvas-edge-group')) {
                                edgesGroup.appendChild(el);
                            }
                        });
                    }
                    // 2. 将节点移回 nodesGroup 末尾（保持在其他节点之上）
                    if (nodesGroup) {
                        nodesGroup.appendChild(nodeEl);
                    }
                    // 3. 确保整体层级正确：edgesGroup 在 nodesGroup 之前（下层）
                    if (edgesGroup && nodesGroup) {
                        const pos = nodesGroup.compareDocumentPosition(edgesGroup);
                        if (pos & Node.DOCUMENT_POSITION_FOLLOWING) {
                            svgG.insertBefore(edgesGroup, nodesGroup);
                        }
                    }
                }
                // 通过累计偏移判断是否点击（无拖拽），D3 drag 会抑制原生 click 事件
                const totalDx = Math.abs(this.__accDx || 0);
                const totalDy = Math.abs(this.__accDy || 0);
                if (!resizeMode && totalDx < 5 && totalDy < 5) {
                    const now = Date.now();
                    const dt = now - (this.__lastClickTime || 0);
                    this.__lastClickTime = now;
                    if (dt < 350) {
                        // 双击：显示内容面板 / 打开链接
                        if (node.type === 'text' || node.type === 'code') {
                            self.selectCanvasNode(node.id, d3.select(this), null);
                            await self.showCanvasNodeContent(node);
                            if (!node.text && !dirCanvasDisplayFlag) {
                                const btn = document.getElementById('content-panel-btn');
                                if (btn) btn.click();
                            }
                        } else if (node.type === 'image' || node.type === 'file') {
                            self.selectCanvasNode(node.id, d3.select(this), node.file);
                            await self.showNodeFileContent(node);
                        } else if (node.type === 'url') {
                            self.selectCanvasNode(node.id, d3.select(this), null);
                            if (node.url) {
                                if (typeof openUrlInBrowser === '') openUrlInBrowser(node.url);
                                else window.open(node.url, '_blank');
                            }
                        }
                    } else {
                        // 单击：选中节点
                        const path = (node.type === 'image' || node.type === 'file') ? node.file : null;
                        self.selectCanvasNode(node.id, d3.select(this), path);
                    }
                }
                if (currentFileHandle && (node.x !== initX || node.y !== initY || node.width !== initW || node.height !== initH)) {
                    await self.saveNodesToFile(self.canvasData.nodes);
                    self.rebuildNodeMap();
                }
                resizeMode = null;
            });
        if (!isChildCanvas) {
            nodeGroup.call(drag);
        }
        nodeGroup.append('rect')
            .attr('class', 'canvas-node-rect')
            .attr('rx', 16)
            .attr('ry', 16)
            .attr('width', node.width)
            .attr('height', node.height);
        if (node.type === 'text') {
            await this.renderTextNode(node, nodeGroup);
        } else if (node.type === 'image' || node.type === 'file') {
            this.renderFileNode(node, nodeGroup);
        } else if (node.type === 'url') {
            this.renderUrlNode(node, nodeGroup);
        } else if (node.type === 'code') {
            this.renderCodeNode(node, nodeGroup);
        } else {
            // 未知类型降级为文本节点（可靠渲染，不白屏）
            await this.renderTextNode(node, nodeGroup);
        }
        this.renderConnectorDots(nodeGroup, node);
    }

    async refreshNodeContentInDOM(node) {
        const nodeGroup = d3.select(`.canvas-node[data-node-id="${node.id}"]`);
        if (nodeGroup.empty()) return;
        nodeGroup.selectAll('foreignObject').remove();
        nodeGroup.selectAll('.canvas-connector-dot').remove();
        if (node.type === 'text') {
            const foreignObject = nodeGroup.append('foreignObject')
                .attr('x', 10)
                .attr('y', 10)
                .attr('width', node.width - 20)
                .attr('height', node.height - 20)
                .attr('overflow', 'hidden');
            const previewText = node.text || '';
            const renderedText = previewText.trim()
                ? await markedParse(parseObsidianToHTML(previewText))
                : '<p style="color:var(--t2);text-align:center;font-size:13px;">点击编辑内容...</p>';
            foreignObject.append('xhtml:div')
                .attr('class', 'canvas-node-text')
                .style('width', '100%')
                .style('height', '100%')
                .style('overflow', 'hidden')
                .style('color', node.isRoot ? 'white' : '#1f2328')
                .style('fontFamily', '-apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif')
                .style('fontSize', '13px')
                .style('lineHeight', '1.4')
                .html(renderedText);
        } else if (node.type === 'image' || node.type === 'file') {
            this.renderFileNode(node, nodeGroup);
        } else if (node.type === 'url') {
            this.renderUrlNode(node, nodeGroup);
        } else if (node.type === 'code') {
            this.renderCodeNode(node, nodeGroup);
        } else {
            // 未知类型降级为文本节点（可靠渲染，不白屏）
            await this.renderTextNode(node, nodeGroup);
        }
        this.renderConnectorDots(nodeGroup, node);
    }

    renderConnectorDots(nodeGroup, node) {
        const sides = [
            { side: 'top', cx: node.width / 2, cy: -2 },
            { side: 'bottom', cx: node.width / 2, cy: node.height + 2 },
            { side: 'left', cx: -2, cy: node.height / 2 },
            { side: 'right', cx: node.width + 2, cy: node.height / 2 },
        ];
        sides.forEach(({ side, cx, cy }) => {
            const dot = nodeGroup.append('circle')
                .attr('class', 'canvas-connector-dot')
                .attr('cx', cx)
                .attr('cy', cy)
                .attr('r', 5)
                .attr('data-node-id', node.id)
                .attr('data-side', side)
                .on('mousedown touchstart pointerdown', (event) => {
                    // 阻止按下事件冒泡到 nodeGroup，避免触发节点拖拽
                    event.stopPropagation();
                    // 阻止浏览器默认行为（文本选中等）
                    event.preventDefault();
                })
                .on('click', (event) => {
                    event.stopPropagation();
                    this.handleConnectorClick(node.id, side);
                });

            // 如果该连接点正处于激活状态（连线起点），添加高亮类
            if (this.connectingFrom && this.connectingFrom.nodeId === node.id && this.connectingFrom.side === side) {
                dot.classed('connecting-active', true);
            }
        });
    }

    async handleConnectorClick(nodeId, side) {
        if (!this.connectingFrom) {
            // 开始连线
            this.connectingFrom = { nodeId, side };
            this.setConnectingMode(true);
            showNotification('已选择起点，请点击目标节点的连接点完成连线（按 Esc 取消）', 'info');
        } else if (this.connectingFrom.nodeId === nodeId && this.connectingFrom.side === side) {
            // 点击同一个连接点——取消连线
            this.cancelConnecting();
        } else if (this.connectingFrom.nodeId === nodeId) {
            // 不允许自连接
            showNotification('不支持节点自连接，请选择其他节点', 'warning');
        } else {
            // 完成连线
            await this.createConnectorEdge(this.connectingFrom.nodeId, this.connectingFrom.side, nodeId, side);
            this.cancelConnecting();
        }
    }

    setConnectingMode(active) {
        const self = this;
        const svgEl = d3.select('.canvas-svg');
        if (active) {
            svgEl.classed('connecting-mode', true);
            // 重新高亮当前起点连接点
            // 注意：classed 回调必须用 function，d3 才会把 this 绑定到当前 <circle> 元素；
            // 类属性通过闭包捕获的 self 访问。
            d3.selectAll('.canvas-connector-dot').classed('connecting-active', function () {
                const el = d3.select(this);
                return self.connectingFrom
                    && el.attr('data-node-id') === self.connectingFrom.nodeId
                    && el.attr('data-side') === self.connectingFrom.side;
            });
        } else {
            svgEl.classed('connecting-mode', false);
            d3.selectAll('.canvas-connector-dot').classed('connecting-active', false);
        }
    }

    cancelConnecting() {
        this.connectingFrom = null;
        this.setConnectingMode(false);
    }

    async createConnectorEdge(fromNodeId, fromSide, toNodeId, toSide) {
        const newEdge = {
            id: 'edge_' + Date.now(),
            fromNode: fromNodeId,
            fromSide: fromSide,
            toNode: toNodeId,
            toSide: toSide,
            animated: true,
        };
        // 更新内存数据
        this.canvasData.edges.push(newEdge);
        // 增量添加到 DOM（不触发全局刷新）
        this.addEdgeToCanvas(newEdge);
        // 持久化到文件
        if (currentFileHandle) {
            await this.saveEdgesToFile(this.canvasData.edges);
        }
        showNotification('连线已创建', 'success');
    }
    // 节点光标委托处理器（挂载在父级 nodesGroup 上，避免每个节点独立注册 mousemove）
    _setupNodeCursorDelegation(nodesGroup) {
        const EDGE_ZONE = 14;
        // 注意：mousemove 回调必须用 function，d3 才会把 this 绑定到 nodesGroup DOM 元素；
        // 否则 while (target !== this) 永远不会匹配，导致光标委托完全失效。
        nodesGroup.on('mousemove', function (event) {
            // 找到鼠标所在的 .canvas-node 子元素
            let target = event.target;
            while (target && target !== this) {
                if (target.classList && target.classList.contains('canvas-node')) break;
                target = target.parentNode;
            }
            if (!target || target === this) {
                // 鼠标不在任何节点上，恢复所有节点光标
                d3.select(this).selectAll('.canvas-node').style('cursor', 'grab');
                return;
            }
            // 读取 DOM 缓存
            const el = target;
            const w = el.__nodeW || 200;
            const h = el.__nodeH || 120;
            // 使用 d3.pointer 获取相对坐标
            const [mx, my] = d3.pointer(event, el);
            const onRight = mx > w - EDGE_ZONE;
            const onLeft = mx < EDGE_ZONE;
            const onBottom = my > h - EDGE_ZONE;
            const onTop = my < EDGE_ZONE;

            let hovered = null;
            let cursor = 'grab';
            if (onRight && onBottom) { hovered = 'se'; cursor = 'nwse-resize'; }
            else if (onLeft && onBottom) { hovered = 'sw'; cursor = 'nesw-resize'; }
            else if (onRight && onTop) { hovered = 'ne'; cursor = 'nesw-resize'; }
            else if (onLeft && onTop) { hovered = 'nw'; cursor = 'nwse-resize'; }
            else if (onRight) { hovered = 'e'; cursor = 'ew-resize'; }
            else if (onLeft) { hovered = 'w'; cursor = 'ew-resize'; }
            else if (onBottom) { hovered = 's'; cursor = 'ns-resize'; }
            else if (onTop) { hovered = 'n'; cursor = 'ns-resize'; }

            el.__hoveredEdge = hovered;
            d3.select(el).style('cursor', cursor);
        });
    }

    async renderCanvasNodes(nodes, group, isChildCanvas = false) {
        const nodesGroup = group.append('g').attr('class', 'canvas-nodes');
        // 注册委托 mousemove（单例，全局一次）
        if (!isChildCanvas) {
            this._setupNodeCursorDelegation(nodesGroup);
        }
        for (const node of nodes) {
            await this.renderCanvasNode(node, nodesGroup, isChildCanvas);
        }
        this.rebuildNodeMap(); // 所有节点渲染完成后建立索引
    }

    // 自适应 + 水平垂直居中：以 <g> 实际渲染内容的 bbox 为基准计算 transform，
    // 并通过 d3 zoom 应用（zoom/pan 完全兼容，不会像 CSS Flexbox 那样与 transform 冲突）。
    fitCanvasToContent(container, svg, zoom) {
        const currentContainerWidth = container.clientWidth || 800;
        const currentContainerHeight = container.clientHeight || 600;
        const svgNode = svg.node();
        if (svgNode) {
            svgNode.setAttribute('width', currentContainerWidth);
            svgNode.setAttribute('height', currentContainerHeight);
        }
        const group = this._canvasGroup || svg.select('.canvas-group');
        const bounds = this.getCanvasContentBounds(group, this.canvasData.nodes);
        const scale = Math.min(currentContainerWidth / Math.max(bounds.contentWidth, 1), currentContainerHeight / Math.max(bounds.contentHeight, 1)) * 0.9;
        // 将容器中心移动到内容 bbox 中心：translate(容器中心) → scale → translate(-内容中心)
        const transform = d3.zoomIdentity
            .translate(currentContainerWidth / 2, currentContainerHeight / 2)
            .scale(scale)
            .translate(-bounds.centerX, -bounds.centerY);
        svg.transition().duration(500).call(zoom.transform, transform);
    }

    addCanvasControls(contentDiv, svg, zoom, fitFn) {
        const controls = document.createElement('div');
        controls.className = 'image-controls';
        controls.innerHTML = `
                <button class="image-btn" id="canvas-zoom-in" title="放大">
                    <svg viewBox="0 0 24 24" width="15" height="15" fill="currentColor">
                        <path d="M19 13h-6v6h-2v-6H5v-2h6V5h2v6h6v2z"/>
                    </svg>
                </button>
                <button class="image-btn" id="canvas-zoom-out" title="缩小">
                    <svg viewBox="0 0 24 24" width="15" height="15" fill="currentColor">
                        <path d="M19 13H5v-2h14v2z"/>
                    </svg>
                </button>
                <button class="image-btn reset" id="canvas-reset" title="重置">
                    <svg viewBox="0 0 24 24" width="15" height="15" fill="currentColor">
                        <path d="M12 5V1L7 6l5 5V7c3.31 0 6 2.69 6 6s-2.69 6-6 6-6-2.69-6-6H4c0 4.42 3.58 8 8 8s8-3.58 8-8-3.58-8-8-8z"/>
                    </svg>
                </button> `;
        contentDiv.appendChild(controls);
        controls.querySelector('#canvas-zoom-in').onclick = () => svg.transition().call(zoom.scaleBy, 1.3);
        controls.querySelector('#canvas-zoom-out').onclick = () => svg.transition().call(zoom.scaleBy, 0.7);
        controls.querySelector('#canvas-reset').onclick = fitFn;
    }

    createNodeContentPanel(height = '100%') {
        let contentPanel = document.getElementById('node-content-panel');
        if (!contentPanel) {
            contentPanel = document.createElement('div');
            contentPanel.id = 'node-content-panel';
            contentPanel.style.position = 'absolute';
            contentPanel.style.right = '0';
            contentPanel.style.top = '0';
            contentPanel.style.width = '60%';
            contentPanel.style.height = height;
            contentPanel.style.backgroundColor = 'var(--color-bg)';
            contentPanel.style.boxShadow = '-5px 0 12px rgba(0, 0, 0, 0.12)';
            contentPanel.style.transition = 'all 0.2s ease';
            contentPanel.style.overflowY = 'auto';
            contentPanel.style.zIndex = '200';
            contentMain.appendChild(contentPanel);
            const canvasContainer = contentMain.querySelector('.canvas-container');
            if (canvasContainer) {
                canvasContainer.style.width = '50%';
                canvasContainer.style.position = 'relative';
            }
            setControlsState({ editBtn: false });
        }
        return contentPanel;
    }

    adjustCanvasViewport() {
        if (this.canvasState) {
            const { svg, zoom, containerWidth, containerHeight, group } = this.canvasState;
            const canvasContainer = contentMain.querySelector('.canvas-container');
            const currentContainerWidth = canvasContainer ? canvasContainer.clientWidth : containerWidth * 0.5;
            const currentContainerHeight = canvasContainer ? canvasContainer.clientHeight : containerHeight;
            const svgNode = svg.node();
            if (svgNode) {
                svgNode.setAttribute('width', currentContainerWidth);
                svgNode.setAttribute('height', currentContainerHeight);
            }
            // 以实时渲染 bbox 重新居中：删除节点/面板开合/侧边栏切换后不会使用过期边界
            const bounds = this.getCanvasContentBounds(group, this.canvasData.nodes);
            const scale = Math.min(currentContainerWidth / Math.max(bounds.contentWidth, 1), currentContainerHeight / Math.max(bounds.contentHeight, 1)) * 0.9;
            const transform = d3.zoomIdentity
                .translate(currentContainerWidth / 2, currentContainerHeight / 2)
                .scale(scale)
                .translate(-bounds.centerX, -bounds.centerY);
            svg.transition().duration(300).call(zoom.transform, transform);
        }
    }

    async showCanvasNodeContent(node) {
        const contentPanel = this.createNodeContentPanel('100%');
        // code 节点以 code 字段为主体内容（双击查看/编辑代码片段）
        const bodyText = node.type === 'code' ? (node.code || node.text || '') : (node.text || '');
        const htmlContent = await markedParse(parseObsidianToHTML(bodyText));
        contentPanel.innerHTML = ` <div class="canvas-edit-controls" id="canvas-edit-controls" ${dirCanvasDisplayFlag ? 'style="display:none;"' : ''}>
                    <button id="content-panel-btn" class="btn btn-primary btn-sm" data-node-id="${node.id}" onclick="editCanvasNodeContent(event, this)">
                        编辑内容
                    </button>
                </div>
                <div class="markdown-body" style="padding: 8px 8px;background: transparent;">${htmlContent}</div>`;
        setControlsState({ copyBtn: false });
        this.setNodePanelState({ node, nodeType: 'text', filePath: '', fileContent: '' });
        const gen = this._panelGeneration;
        postProcessMarkdown(contentPanel).then(result => {
            if (this.isPanelGenerationStale(gen)) return;
            this.adjustCanvasViewport();
        });
    }

    async showNodeFileContent(node) {
        if (isPicImage(getExt(node.file))) {
            this.openCanvasImageFile(node.file);
            return;
        }
        if (node.file.endsWith('.canvas') || node.file.endsWith('.excalidraw') || node.file.endsWith('.mmd')) {
            this.openCanvasLinkedFile(node.file);
            return;
        }
        if (node.file.endsWith('.md')) {
            const contentPanel = this.createNodeContentPanel('100%');
            try {
                const fileHandle = await getEmbedNodeFileHandleByPath(node.file);
                if (!fileHandle) {
                    throw new Error('文件不存在');
                }
                const text = await getFileContent(fileHandle);
                const htmlContent = await markedParse(parseObsidianToHTML(text));
                contentPanel.innerHTML = `
                        <div class="canvas-edit-controls" id="canvas-edit-controls" ${dirCanvasDisplayFlag ? 'style="display:none;"' : ''}>
                            <button class="btn btn-primary btn-sm" data-node-id="${node.id}" onclick="editCanvasNodeContent(event, this)">
                                编辑内容
                            </button>
                        </div>
                        <div class="markdown-body" style="padding: 8px 8px;background: transparent;">${htmlContent}</div>`;
                this.setNodePanelState({ node, nodeType: 'file', filePath: node.file, fileContent: text });
                const gen = this._panelGeneration;
                postProcessMarkdown(contentPanel).then(result => {
                    if (this.isPanelGenerationStale(gen)) return;
                    this.adjustCanvasViewport();
                });
            } catch (error) {
                contentPanel.innerHTML = `<div style="color:#cf222e; padding:20px;">加载文件失败: ${escapeHtml(error.message)}</div>`;
            }
            setControlsState({ copyBtn: false });
        }
    }

    async openCanvasImageFile(filePath) {
        try {
            const fileHandle = await getEmbedNodeFileHandleByPath(filePath);
            if (!fileHandle) {
                throw new Error('文件不存在');
            }
            const file = await getFile(fileHandle);
            const blobUrl = URL.createObjectURL(file);
            currentObjectUrls.push(blobUrl);
            showImageModal(blobUrl);
        } catch (error) {
            console.error('打开图片失败:', error);
            showNotification('✗ 打开图片失败: ' + error.message, 'error');
        }
    }
    async embedNodeExcalidraw(filePath, contentDiv) {
        try {
            const fileHandle = await getEmbedNodeFileHandleByPath(filePath);
            if (!fileHandle) {
                contentDiv.html(`<div style="display:flex;align-items:center;justify-content:center;width:100%;height:100%;color:#cf222e;font-size:12px;">无法加载子画布！${escapeHtml(filePath)}</div>`);
                return;
            }
            const fileText = await getFileContent(fileHandle);
            const el = contentDiv.node();
            await renderExcalidrawStaticPreview(fileText, el);
        } catch (error) {
            console.error('加载 子excalidraw 文件失败:', error);
            contentDiv.html(`<div style="display:flex;align-items:center;justify-content:center;width:100%;height:100%;color:#cf222e;font-size:12px;">加载文件失败！ ${escapeHtml(filePath)}</div>`);
        }
    }
    async embedNodeCanvas(filePath, contentDiv) {
        const div = document.createElement('div');
        try {
            const fileHandle = await getEmbedNodeFileHandleByPath(filePath);
            if (!fileHandle) {
                div.innerHTML = `<div style="display:flex;align-items:center;justify-content:center;width:100%;height:100%;color:#cf222e;font-size:12px;">无法加载子画布！${escapeHtml(filePath)}</div>`;
                return;
            }
            const fileText = await getFileContent(fileHandle);
            await new CanvasLess().renderCanvas(fileText, div, false, null, true);
        } catch (error) {
            console.error('加载 子canvas 文件失败:', error);
            div.innerHTML = `<div style="display:flex;align-items:center;justify-content:center;width:100%;height:100%;color:#cf222e;font-size:12px;">加载文件失败！ ${escapeHtml(filePath)}</div>`;
        } finally {
            if (contentDiv) {
                contentDiv.html(div.innerHTML);
                this.setupEmbeddedCanvas(contentDiv.node());
            }
        }
    }

    // 节点内嵌子画布的自适应：一次性把 <g> 的实际渲染 bbox 写入 svg 的 viewBox，
    // 并用 preserveAspectRatio="xMidYMid meet" 让浏览器原生负责水平垂直居中与等比缩放。
    // 节点被拖拽改大小、容器尺寸变化时，svg 宽度/高度由 CSS 100% 接管，浏览器逐帧自适应，零 JS、无瞬移。
    // 参数可为包含 .canvas-container 的外层 div，也可直接传 .canvas-container 本身。
    setupEmbeddedCanvas(containerEl) {
        if (!containerEl) return;
        const container = (containerEl.classList && containerEl.classList.contains('canvas-container'))
            ? containerEl
            : containerEl.querySelector('.canvas-container');
        const svgEl = containerEl.querySelector('.canvas-svg');
        const gEl = containerEl.querySelector('.canvas-group');
        if (!container || !svgEl || !gEl) return;
        // 内嵌子画布只读：不响应任何指针事件（悬停/选中/点击），事件穿透到父节点。
        // 配合 main.html 的子选择器样式（.canvas-node-selected > .canvas-node-rect），
        // 父节点选中时不会级联高亮子画布内的所有节点。
        container.classList.add('canvas-readonly');
        let bbox = null;
        try { bbox = gEl.getBBox(); } catch (e) { /* ignore */ }
        if (!bbox || bbox.width <= 0 || bbox.height <= 0
            || !Number.isFinite(bbox.x) || !Number.isFinite(bbox.y)) return;
        // 内容四周留少量边距，随后将内容边界写入 viewBox
        const pad = 12;
        svgEl.setAttribute('viewBox', `${bbox.x - pad} ${bbox.y - pad} ${bbox.width + pad * 2} ${bbox.height + pad * 2}`);
        svgEl.setAttribute('preserveAspectRatio', 'xMidYMid meet');
        // 清除固定宽高与 group 上残留的 d3 zoom transform，完全交给 viewBox + CSS 自适应
        svgEl.removeAttribute('width');
        svgEl.removeAttribute('height');
        gEl.removeAttribute('transform');
        // 递归处理嵌套子画布（画布节点内再嵌画布），每一层各自写入自己的 viewBox
        container.querySelectorAll('.canvas-container').forEach(nested => {
            this.setupEmbeddedCanvas(nested);
        });
    }
    async embedNodeImage(filePath, contentDiv) {
        contentDiv.attr('class', 'center-div')
        contentDiv.style('overflow', 'hidden')
        contentDiv.style('border-radius', '8px')
        contentDiv.style('padding', '8px')
        contentDiv.style('background', 'white')
        // 异步加载图片（受并发限制，最多同时加载 4 张，避免 CPU 飙升）
        this._canvasImgEnqueue(async () => {
            try {
                const fileHandle = await getEmbedNodeFileHandleByPath(filePath);
                if (!fileHandle) {
                    contentDiv.html(`<div style="display:flex;align-items:center;justify-content:center;width:100%;height:100%;color:#cf222e;font-size:12px;">无法加载图片！${escapeHtml(filePath)}</div>`);
                    return;
                }
                const file = await getFile(fileHandle);
                const blobUrl = URL.createObjectURL(file);
                this._canvasObjectUrls.push(blobUrl); // 实例级：重渲染时只撤销本实例的 URL，避免误伤父画布
                currentObjectUrls.push(blobUrl);      // 全局：视图销毁时统一清理
                contentDiv.html(`<img src="${blobUrl}" style="width:100%;height:100%;object-fit:contain;display:block;border-radius:4px;" />`);
            } catch (error) {
                console.error('加载图片预览失败:', error);
                contentDiv.html(`<div style="display:flex;align-items:center;justify-content:center;width:100%;height:100%;color:#cf222e;font-size:12px;">加载失败！${escapeHtml(filePath)}</div>`);
            }
        });
    }
    async embedNodeSVG(filePath, contentDiv) {
        contentDiv.attr('class', 'center-div')
        contentDiv.style('overflow', 'hidden')
        contentDiv.style('border-radius', '8px')
        contentDiv.style('padding', '8px')
        contentDiv.style('background', 'white')
        try {
            const fileHandle = await getEmbedNodeFileHandleByPath(filePath);
            if (!fileHandle) {
                contentDiv.html(`<div style="display:flex;align-items:center;justify-content:center;width:100%;height:100%;color:#cf222e;font-size:12px;">无法加载svg！${escapeHtml(filePath)}</div>`);
                return;
            }
            const svgContent = await getFileContent(fileHandle);
            contentDiv.html(svgContent);
        } catch (error) {
            console.error('加载svg失败:', error);
            contentDiv.html(`<div style="display:flex;align-items:center;justify-content:center;width:100%;height:100%;color:#cf222e;font-size:12px;">加载失败！${escapeHtml(filePath)}</div>`);
        }
    }
    async embedNodeMmd(filePath, text, contentDiv, dom) {
        const div = document.createElement('div');
        try {
            let mermaidCode = text || '';
            if (contentDiv) {
                const fileHandle = await getEmbedNodeFileHandleByPath(filePath);
                if (!fileHandle) {
                    div.innerHTML = `<div style="display:flex;align-items:center;justify-content:center;width:100%;height:100%;color:#cf222e;font-size:12px;">无法加载图表！${escapeHtml(filePath)}</div>`;
                    return;
                }
                mermaidCode = await getFileContent(fileHandle);
            }
            try {
                await mermaid.parse(mermaidCode);
                const { svg } = await mermaid.render("mermaid_mmd_node" + crypto.randomUUID(), mermaidCode);
                div.innerHTML = `<div style="display:flex; justify-content:center; align-items:center; width:100%; height:100%; box-sizing:border-box;">${svg}</div>`;
            } catch (err) {
                const eMermaid = extractErrorMessageMermaid(err);
                div.innerHTML = `<div style="color:#cf222e;">渲染出错: ${escMermaid(eMermaid)}</div>`;
            }
        } catch (error) {
            console.error('加载 mmd 文件失败:', error);
            div.innerHTML = `<div style="display:flex;align-items:center;justify-content:center;width:100%;height:100%;color:#cf222e;font-size:12px;">加载文件失败！ ${escapeHtml(filePath)}</div>`;
        } finally {
            if (contentDiv) {
                contentDiv.html(div.innerHTML);
                // 节点内嵌 mmd：写入 viewBox + preserveAspectRatio，浏览器原生按节点尺寸自适应缩放，
                // 节点改大小时图像自动缩放居中，不再被裁切或溢出
                this.setupEmbeddedMmd(contentDiv.node());
            }
            if (dom) {
                dom.appendChild(div);
            }
        }
    }

    // 节点内嵌 mermaid(mmd) 的自适应：把 svg 的 viewBox 收紧到图表实际边界，
    // 元素尺寸交给 CSS 100%，配合 preserveAspectRatio="xMidYMid meet"，
    // 节点缩放时由浏览器逐帧原生缩放居中，零 JS、无漂移、不被裁切。
    setupEmbeddedMmd(containerEl) {
        if (!containerEl) return;
        const svg = containerEl.querySelector('svg');
        if (!svg) return;
        let bbox = null;
        try { bbox = svg.getBBox(); } catch (e) { /* ignore */ }
        if (!bbox || bbox.width <= 0 || bbox.height <= 0
            || !Number.isFinite(bbox.x) || !Number.isFinite(bbox.y)) return;
        const pad = 8;
        svg.setAttribute('viewBox', `${bbox.x - pad} ${bbox.y - pad} ${bbox.width + pad * 2} ${bbox.height + pad * 2}`);
        svg.setAttribute('preserveAspectRatio', 'xMidYMid meet');
        // 清除 mermaid 固定的宽高/最大宽度，交给 CSS 100% 自适应
        svg.removeAttribute('width');
        svg.removeAttribute('height');
        svg.style.maxWidth = 'none';
        svg.style.width = '100%';
        svg.style.height = '100%';
        svg.style.display = 'block';
    }

    // 模态框内静态 SVG（mmd / excalidraw 静态预览）的交互：初始自适应居中 + 滚轮以鼠标为中心缩放 + 拖拽平移。
    // 复用 main.html 的 createPanZoomController（全局函数）。
    setupSvgModalZoom(container) {
        if (!container || typeof createPanZoomController !== 'function') return;
        const svg = container.querySelector('svg');
        if (!svg) return;
        // 消除嵌套 flex 包装层：embedNodeMmd 的结构是 container > div > div(flex 居中) > svg。
        // 若保留 flex 居中，svg 的布局盒会被先居中（高盒还会溢出），再叠加控制器的
        // translate+scale transform 会错位（元素跑到右上角）。
        // 从 svg 向上遍历，把中间所有 div 改为 block 并铺满容器，使 svg 布局原点 = 容器原点。
        let el = svg.parentNode;
        while (el && el !== container) {
            if (el.tagName === 'DIV') {
                el.style.display = 'block';
                el.style.width = '100%';
                el.style.height = '100%';
            }
            el = el.parentNode;
        }
        svg.style.display = 'block';
        requestAnimationFrame(() => {
            let sw = 0, sh = 0, vbx = 0, vby = 0;
            try {
                const bbox = svg.getBBox();
                if (bbox && bbox.width > 0 && bbox.height > 0 && Number.isFinite(bbox.x) && Number.isFinite(bbox.y)) {
                    vbx = bbox.x; vby = bbox.y;
                    sw = bbox.width; sh = bbox.height;
                }
            } catch (e) { /* 忽略 */ }
            if (!sw || !sh) {
                const vb = svg.viewBox && svg.viewBox.baseVal;
                if (vb && vb.width > 0 && vb.height > 0) {
                    vbx = vb.x; vby = vb.y;
                    sw = vb.width; sh = vb.height;
                }
            }
            if (!sw || !sh) { sw = 800; sh = 600; }
            // 关键：mermaid 的 viewBox 默认含四周留白（x-r y-r W+2r H+2r），且 svg 是 width="100%"+max-width。
            // 若只设元素尺寸，内容会在元素内 letterbox 偏移——中心偏差 + 滚轮缩放锚点漂移。
            // 这里把 viewBox 收紧到内容实际边界、元素尺寸设为内容尺寸，使内容精确铺满元素。
            svg.setAttribute('viewBox', `${vbx} ${vby} ${sw} ${sh}`);
            svg.setAttribute('width', sw);
            svg.setAttribute('height', sh);
            svg.style.maxWidth = 'none';
            svg.style.width = sw + 'px';
            svg.style.height = sh + 'px';
            const cw = container.clientWidth || 800;
            const ch = container.clientHeight || 600;
            const pad = 40;
            const scale = Math.min((cw - pad * 2) / sw, (ch - pad * 2) / sh);
            const zoom = Math.max(5, Math.min(400, Math.round(scale * 100)));
            const ctrl = createPanZoomController({
                target: svg,
                container: container,
                initialZoom: zoom,
                minZoom: 5,
                maxZoom: 400,
                panOnlyZoomed: false
            });
            // 初始水平垂直居中（fit 后可能 < 100%，panOnlyZoomed=false 保证平移保留）
            const finalScale = zoom / 100;
            ctrl.setPan((cw - sw * finalScale) / 2, (ch - sh * finalScale) / 2);
            container.__svgZoomCtrl = ctrl;
        });
    }

    async openCanvasLinkedFile(filePath) {
        try {
            const fileHandle = await getEmbedNodeFileHandleByPath(filePath);
            if (!fileHandle) {
                throw new Error('文件不存在');
            }
            const fileText = await getFileContent(fileHandle);
            const modal = document.createElement('div');
            modal.style.cssText = `
                        position: fixed;
                        top: 0;
                        left: 0;
                        width: 100%;
                        height: 100%;
                        background: rgba(0,0,0,0.2);
                        display: flex;
                        justify-content: center;
                        align-items: center;
                        z-index: 10000;
                        cursor: zoom-out;`;
            const container = document.createElement('div');
            container.style.cssText = `
                        width: 75%;
                        height: 85%;
                        background: var(--color-bg);
                        border-radius: 15px;
                        overflow: hidden;`;
            modal.appendChild(container);
            document.body.appendChild(modal);
            if (filePath.endsWith('.excalidraw')) {
                container.style.padding = '16px';
                await renderExcalidrawStaticPreview(fileText, container);
                this.setupSvgModalZoom(container);
            } else if (filePath.endsWith('.canvas')) {
                await new CanvasLess().renderCanvas(fileText, container, false, null, true);
            } else if (filePath.endsWith('.mmd')) {
                await this.embedNodeMmd(filePath, fileText, null, container);
                this.setupSvgModalZoom(container);
            }  
            let modalClosed = false;
            const closeModal = () => {
                if (modalClosed) return;
                modalClosed = true;
                // 销毁模态框的 pan-zoom 控制器（解除 document 级监听，避免泄漏）
                if (container.__svgZoomCtrl) {
                    try { container.__svgZoomCtrl.destroy(); } catch (e) { /* 忽略 */ }
                    container.__svgZoomCtrl = null;
                }
                modal.remove();
                document.removeEventListener('keydown', escHandler);
            };
            const escHandler = (e) => {
                if (e.key === 'Escape') closeModal();
            };
            document.addEventListener('keydown', escHandler);
            modal.onclick = (e) => {
                if (e.target === modal) closeModal();
            };
        } catch (error) {
            console.error('打开文件失败:', error);
            showNotification('✗ 打开文件失败: ' + error.message, 'error');
        }
    }

    async closeNodeContentPanel() {
        this._panelGeneration++;
        const contentPanel = document.getElementById('node-content-panel');
        if (contentPanel) {
            if (currentNodeEditor && currentNodeEditor.getValue) {
                const { node: nodeData, nodeType, filePath, fileContent } = this.getNodePanelState();
                const isFileNode = nodeType === 'file' && filePath;
                const newText = currentNodeEditor.getValue();
                const originalContent = isFileNode ? fileContent : (nodeData.text || '');
                if (newText !== originalContent) {
                    if (isFileNode) {
                        await this.saveFileNodeContent(filePath, newText);
                    } else {
                        nodeData.text = newText;
                        await this.saveNodeText(nodeData.id, newText);
                    }
                    for (const node of this.canvasData.nodes) {
                        if (node.id === nodeData.id) {
                            node.text = newText;
                            break;
                        }
                    }
                    // 增量更新 DOM，避免全局重渲染闪跳
                    const updatedNode = this.canvasData.nodes.find(n => n.id === nodeData.id);
                    if (updatedNode) {
                        await this.refreshNodeContentInDOM(updatedNode);
                    }
                }
                destroyNodeEditor();
            }
            this.clearNodePanelState();
            contentPanel.remove();
            const canvasContainer = contentMain.querySelector('.canvas-container');
            if (canvasContainer) {
                canvasContainer.style.width = '100%';
            }
            this.adjustCanvasViewport(); // 面板关闭后重新适配画布视口
        }
        setControlsState({ editBtn: true, copyBtn: true, demoBtn: true, commonToolControls: true });
    }

    editNodeContent(event, button) {
        event.stopPropagation();
        const contentPanel = button.closest('#node-content-panel');
        if (!contentPanel) return;
        // 进入编辑模式，隐藏右上角演示按钮
        setControlsState({ demoBtn: false, commonToolControls: false });
        const { node: nodeData, nodeType, filePath, fileContent } = this.getNodePanelState();
        const isFileNode = nodeType === 'file' && filePath;
        const editValue = isFileNode ? fileContent : (nodeData.text || '');
        const headerLabel = isFileNode ? `📝 编辑文件: ${basename(filePath)}` : '📝 编辑节点内容';
        contentPanel.innerHTML = `
                    <div class="canvas-edit-controls">
                        <button class="btn btn-success btn-sm" onclick="saveCanvasNodeContent(event, this)">显示</button>
                    </div>
                    ${createEditorContainer(headerLabel, 'node-editor-inner', 'height: 100%;')}`;
        // 状态对象独立于 DOM，innerHTML 替换不影响 _nodePanelState，无需重写
        const gen = this._panelGeneration;
        setTimeout(() => {
            if (this.isPanelGenerationStale(gen)) return;
            destroyNodeEditor();
            const editorInner = document.getElementById('node-editor-inner');
            if (editorInner) {
                initMonacoEditor().then(() => {
                    currentNodeEditor = createMonacoEditor(editorInner, {
                        value: editValue,
                        language: 'markdown'
                    });
                    updateStatus(currentNodeEditor, contentPanel, '', true);
                });
            }
        }, 100);
    }

    async saveNodeContent(event, button) {
        event.stopPropagation();
        const contentPanel = button.closest('#node-content-panel');
        if (!contentPanel) return;
        const { node: nodeData, nodeType, filePath, fileContent } = this.getNodePanelState();
        const isFileNode = nodeType === 'file' && filePath;
        if (currentNodeEditor && currentNodeEditor.getValue) {
            const newText = currentNodeEditor.getValue();
            const originalContent = isFileNode ? fileContent : (nodeData.text || '');
            const renderPreviewPanel = async (text) => {
                const htmlContent = await markedParse(parseObsidianToHTML(text));
                const fileLabel = isFileNode ? `<span style="font-size:13px;font-weight:500;color:var(--color-primary-light);">📝 ${escapeHtml(filePath)}</span>` : '';
                contentPanel.innerHTML = `<div class="canvas-edit-controls" id="canvas-edit-controls">
                                ${fileLabel}
                                <button class="btn btn-primary btn-sm" data-node-id="${nodeData.id}" onclick="editCanvasNodeContent(event, this)">
                                    编辑内容
                                </button> 
                            </div>
                            <div class="markdown-body" style="padding: 8px 8px;background: transparent;">${htmlContent}</div>`;
                this.setNodePanelState({ fileContent: text });
                destroyNodeEditor();
                setControlsState({ demoBtn: true, commonToolControls: true });
                postProcessMarkdown(contentPanel);
            };
            if (newText === originalContent) {
                await renderPreviewPanel(originalContent);
            } else {
                if (isFileNode) {
                    await this.saveFileNodeContent(filePath, newText).then(async () => {
                        await renderPreviewPanel(newText);
                        await this.reloadCanvasData();
                    }).catch(err => {
                        console.error('保存文件失败:', err);
                        showNotification('✗ 保存失败: ' + err.message, 'error');
                    });
                } else {
                    nodeData.text = newText;
                    for (const node of this.canvasData.nodes) {
                        if (node.id === nodeData.id) {
                            node.text = newText;
                            break;
                        }
                    }
                    await this.saveNodeText(nodeData.id, newText).then(async () => {
                        await renderPreviewPanel(newText);
                        await this.reloadCanvasData();
                    }).catch(err => {
                        console.error('保存节点失败:', err);
                        showNotification('✗ 保存失败: ' + err.message, 'error');
                    });
                }
            }
        }
        setControlsState({ copyBtn: false });
    }

    async saveFileNodeContent(filePath, content) {
        const fileHandle = await getEmbedNodeFileHandleByPath(filePath);
        if (!fileHandle) throw new Error('文件不存在: ' + filePath);
        await writeFileHandle(fileHandle, content);
        showNotification('✓ 文件保存成功', 'success');
    }

    async saveNodeText(nodeId, newText) {
        if (!currentFileHandle) return;
        const fileHandle = currentFileHandle;
        try {
            await enqueueFileAtomic(fileHandle, async () => {
                const text = await getFileContent(fileHandle);
                let canvasDataLocal = JSON.parse(text);
                canvasDataLocal.nodes.forEach(node => {
                    if (node.id === nodeId) {
                        node.text = newText;
                    }
                });
                const newContent = JSON.stringify(canvasDataLocal);
                await _writeToFileHandle(fileHandle, newContent);
                if (fileHandle === currentFileHandle) {
                    originalContent = newContent;
                }
            });
            showNotification('✓ 节点保存成功', 'success');
        } catch (err) {
            console.error('保存节点文本失败:', err);
            showNotification('✗ 保存节点失败: ' + err.message, 'error');
        }
    }

    async saveNodesToFile(nodes) {
        if (!currentFileHandle) return;
        const fileHandle = currentFileHandle;
        try {
            await enqueueFileAtomic(fileHandle, async () => {
                const text = await getFileContent(fileHandle);
                let canvasDataLocal = JSON.parse(text);
                canvasDataLocal.nodes = nodes;
                const newContent = JSON.stringify(canvasDataLocal);
                await _writeToFileHandle(fileHandle, newContent);
                if (fileHandle === currentFileHandle) {
                    originalContent = newContent;
                }
            });
            showNotification('✓ 节点保存成功', 'success');
        } catch (err) {
            console.error('保存节点失败:', err);
            showNotification('✗ 保存节点失败: ' + err.message, 'error');
        }
    }

    async reloadCanvasData() {
        try {
            // 保存右侧面板的状态
            const contentPanel = document.getElementById('node-content-panel');
            const panelData = contentPanel ? {
                nodeData: this.getNodePanelState().node,
                scrollTop: contentPanel.scrollTop,
                gen: this._panelGeneration
            } : null;

            if (currentFileHandle) {
                // 重新读取文件
                const text = await getFileContent(currentFileHandle);
                await this.renderCanvas(text, contentMain);
            } else {
                // 对于新canvas，直接使用内存中的数据重新渲染
                await this.renderCanvas('', contentMain, true, this.canvasData);
            }
            // 恢复右侧面板
            if (panelData) {
                setTimeout(() => {
                    if (this.isPanelGenerationStale(panelData.gen)) return;
                    const node = panelData.nodeData;
                    if (node.type === 'text') {
                        this.showCanvasNodeContent(node);
                    } else if (node.type === 'file') {
                        this.showNodeFileContent(node);
                    }
                    const newPanel = document.getElementById('node-content-panel');
                    if (newPanel) {
                        newPanel.scrollTop = panelData.scrollTop;
                    }
                }, 100);
            }
        } catch (error) {
            console.error('重新加载画布数据失败:', error);
        }
    }

    //重新加载画布数据(不恢复右侧面板)
    async reloadCanvasDataWithoutPanel() {
        try {
            if (currentFileHandle) {
                const text = await getFileContent(currentFileHandle);
                await this.renderCanvas(text, contentMain);
            } else {
                // 对于新canvas，直接使用内存中的数据重新渲染
                await this.renderCanvas('', contentMain, true, this.canvasData);
            }
        } catch (error) {
            console.error('重新加载画布数据失败:', error);
        }
    }

    // 清理 Canvas 相关状态（关闭画布或切换文件时调用）
    async cleanupCanvasState(cleanDom = false) {
        // 1. 清理 d3 SVG 状态
        if (this.canvasState) {
            try {
                const { svg, zoom } = this.canvasState;
                if (svg && zoom) {
                    svg.on('.zoom', null);
                    svg.on('pointerdown', null);
                    svg.on('pointerup', null);
                }
            } catch (e) {
                console.warn('清理 Canvas SVG 状态失败:', e);
            }
            this.canvasState = null;
        }
        // 清理 document 级 pointerup 备份监听器
        if (window._svgPointerUpCleanup) {
            document.removeEventListener('pointerup', window._svgPointerUpCleanup);
            window._svgPointerUpCleanup = null;
        }
        // 2. 清理 DOM（canvas 容器）
        if (cleanDom) {
            const container = contentMain.querySelector('.canvas-container');
            if (container) {
                container.innerHTML = '';
            }
            // 3. 清理节点编辑面板
            const contentPanel = document.getElementById('node-content-panel');
            if (contentPanel) {
                contentPanel.remove();
            }
            // 4. 清理 canvas 相关的全局状态
            this.canvasData = { nodes: [], edges: [] };
            this._nodeMapCache = new Map();
            if (currentFileHandle) {
                cleanupFileSaveQueue(currentFileHandle);
            }
            this._canvasImgQueue = [];
            this._canvasImgInFlight = 0;
            this._canvasObjectUrls = [];
            this.selectedCanvasNodeId = null;
            this.pendingCanvasNodeType = null;
            this.cancelConnecting(); // 确保退出连线模式
            // 4.5. 移除 Esc 键盘监听器
            if (this._canvasEscHandler) {
                document.removeEventListener('keydown', this._canvasEscHandler);
                this._canvasEscHandler = null;
            }
            // 5. 清理模态框（如果存在）
            const modal = document.getElementById('file-select-modal');
            if (modal) modal.remove();
            // 5.5. 清理 Object URL（释放图片 blob 内存）
            if (currentObjectUrls && currentObjectUrls.length > 0) {
                currentObjectUrls.forEach(url => {
                    try { URL.revokeObjectURL(url); } catch (e) { /* 忽略 */ }
                });
                currentObjectUrls.length = 0;
            }
            // 6. 隐藏底部工具栏
            const bottomToolbar = document.getElementById('canvas-bottom-toolbar');
            if (bottomToolbar) bottomToolbar.style.display = 'none';
            contentMain.innerHTML = '';
        }
    }
    resizeCanvas() {
        if (this.canvasData) {
            const canvasContainer = contentMain.querySelector('.canvas-container');
            if (canvasContainer) {
                canvasContainer.style.width = sidebarVisible ? '' : '100%';
                this.adjustCanvasViewport();
            }
        }
    }
}