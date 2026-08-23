/**
 * ===== 图谱渲染控制器（css_js/graph/graph-renderer.js） =====
 * 【职责】Sigma.js 实例管理：图数据装载、视口、LOD 切换、样式映射。
 *        DIP：依赖 Sigma 抽象（sigmaFactory 注入），不直接 import Sigma ——
 *        引擎缺失/未安装时渲染器降级为「DOM 占位提示」，不阻塞其余模块。
 *
 * 【LOD 架构】仅渲染当前层级子集，绝不装载全图：
 *   L0 概览（聚合） / L1 局部（邻域） / L2 焦点（1 跳全量 + 2 跳截断）
 *   由 Sigma cameraUpdated 事件 → setLod 驱动（interaction 监听相机变化后调用）。
 */
(function () {
    'use strict';

    /** 节点类型 → 颜色（对齐主页面视觉：--color-primary 蓝系） */
    const TYPE_COLORS = {
        doc: '#0969da',
        folder: '#6e40c9',
        chunk: '#3aa8b8',
        section: '#6b7b8f',
        entity: '#d97706',
        experience: '#cf222e',
        memory: '#953800',
        cluster: '#57606a',
    };

    /** 关系类型 → 线型/颜色（PRD §15：引用实线 / 依赖箭头 / 包含层级线 / 相关虚线 / 派生双向点线） */
    const RELATION_STYLES = {
        CONTAINS: { color: '#9aa1a9', type: 'line', dashed: false },
        REFERENCES: { color: '#6b9be8', type: 'line', dashed: false },
        IMPORTS: { color: '#4caf78', type: 'arrow', dashed: false },
        DERIVED_FROM: { color: '#d9a04b', type: 'dotted', dashed: true },
        SAME_TOPIC: { color: '#a08ad8', type: 'dashed', dashed: true },
        SOLVED_BY: { color: '#4c8bf5', type: 'arrow', dashed: false },
        IMPLEMENTED_IN: { color: '#4caf78', type: 'arrow', dashed: false },
        VALIDATED_BY: { color: '#d9a04b', type: 'dashed', dashed: true },
        REPLACED_BY: { color: '#e07a6a', type: 'dashed', dashed: true },
        DEPRECATED: { color: '#9aa1a9', type: 'dotted', dashed: true },
        PREFERS: { color: '#e8749a', type: 'line', dashed: false },
        AVOIDS: { color: '#9aa1a9', type: 'dotted', dashed: true },
        USES: { color: '#4caf78', type: 'arrow', dashed: false },
        BELONGS_TO: { color: '#8a6fd8', type: 'line', dashed: false },
        SIMILAR_TO: { color: '#3aa8b8', type: 'dashed', dashed: true },
        DEPENDS_ON: { color: '#c26f4a', type: 'arrow', dashed: false },
    };

    /** 节点分类 → 颜色（PRD §8.1；与 model.CATEGORY_COLORS 对齐，兜底用） */
    const CATEGORY_FALLBACK_COLORS = {
        folder: '#4c8bf5', doc: '#e8b339', code: '#e08a3c', config: '#3dab6f',
        image: '#e8749a', concept: '#8a6fd8', script: '#3aa8b8', entity: '#6b7b8f',
        project: '#c26f4a', other: '#9aa1a9', cluster: '#6b7b8f',
    };

    class GraphRenderer {
        /**
         * @param {{ store: object, sigmaFactory?: Function, container: HTMLElement }} deps
         *   sigmaFactory：() => ({ Sigma, Graph }) —— app.js 注入（懒加载引擎）
         */
        constructor({ store, container, sigmaFactory }) {
            this.store = store;
            this.container = container;
            this._sigmaFactory = sigmaFactory || null;
            /** @type {object|null} Sigma 实例 */
            this._sigma = null;
            /** @type {object|null} graphology Graph 实例 */
            this._graph = null;
            /** 引擎是否可用 */
            this.engineReady = false;
            /** 渲染中节点 id 集合（LOD 子集） */
            this._renderedIds = new Set();
            /** 高亮（选中节点 + 其邻居） */
            this._highlight = null;
            /** LOD 样式缓存（camera 变化时重建） */
            this._styleCache = null;
        }

        /** 引擎是否就绪 */
        get ready() { return this.engineReady; }

        /**
         * 装载 Sigma 引擎（app.js 注入的 sigmaFactory 已动态 import 完成）。
         * 引擎不可用时设置占位提示并返回 false。
         */
        mount() {
            if (this.engineReady) return true;
            if (!this._sigmaFactory) {
                this._showPlaceholder('图谱引擎未安装：请将 Sigma.js v3 产物放入 css_js/cdn/sigma/');
                return false;
            }
            try {
                const { Sigma, Graph } = this._sigmaFactory();
                if (!Sigma || !Graph) throw new Error('Sigma 工厂返回空');
                this._graph = new Graph();
                this._sigma = new Sigma(this._graph, this.container, {
                    // Sigma v3：renderer 由包自动选择（webgl 优先，回退 canvas），
                    // 无需显式 renderer 选项；camera 缩放范围在 settings 中配置
                    minCameraRatio: 0.01,
                    maxCameraRatio: 8,
                    // 画布暂时无尺寸（如 HTML 面板视图覆盖期间）时容忍 refresh/布局调用，
                    // 恢复尺寸后自动正常渲染（避免 "Container has no width" 抛错）
                    allowInvalidContainer: true,
                    // 标签防遮挡（LOD）：labelRenderedSizeThreshold 之下隐藏标签，
                    // labelDensity / labelGridCellSize 控制同屏标签密度 ——
                    // 800+ 节点全览时只显示 hub 标签，放大后标签逐级浮现。
                    labelRenderedSizeThreshold: 10,
                    labelDensity: 2,
                    labelGridCellSize: 150,
                    renderLabels: true,
                });
                this.engineReady = true;
                this.hidePlaceholder();
                return true;
            } catch (e) {
                console.error('[graph-renderer] Sigma 装载失败:', e);
                this._showPlaceholder('图谱引擎加载失败: ' + e.message);
                return false;
            }
        }

        /**
         * 装载图数据（LOD 子集）并渲染。
         * @param {object} graphData store.data（GraphData）
         * @param {object} opts { focusNodeId?, lod? }
         */
        setData(graphData, opts = {}) {
            if (!this.engineReady || !this._sigma) return;
            const { focusNodeId = null, lod } = opts;
            // 类型过滤（R10 修复）：visibleIds 函数返回 null=全部可见 / Set / 数组；
            // 边两端都必须可见才渲染（避免悬空边）
            const visibleIds = (typeof opts.visibleIds === 'function')
                ? opts.visibleIds()
                : null;
            const isVisible = (id) => {
                if (!visibleIds) return true;
                if (visibleIds.has) return visibleIds.has(id);
                return visibleIds.includes(id);
            };
            // 记录旧位置：重建图时保留已布局坐标，避免每次刷新/展开节点跳回兜底布局
            const prevPos = new Map();
            if (this._graph) {
                this._graph.forEachNode((id, attrs) => {
                    if (typeof attrs.x === 'number' && typeof attrs.y === 'number') {
                        prevPos.set(id, { x: attrs.x, y: attrs.y });
                    }
                });
            }
            // 重建 graphology 图（Sigma 不支持增量删除，LOD 切换直接重建最简）
            const g = this._graph;
            g.clear();
            this._renderedIds.clear();

            const nodes = Array.from(graphData.nodes.values()).filter((n) => isVisible(n.id));
            nodes.forEach((n, i) => {
                // Sigma 要求每个节点必须有数字 x/y；接口数据通常不带坐标，
                // 这里兜底一个确定性初始布局（黄金角螺旋，尺度与 graph-layout
                // 种子一致），避免 Sigma 抛 "could not find a valid position"，
                // 也避免 800+ 节点挤在同心小环上；已布局过的节点复用旧坐标。
                // 增量展开的新节点优先锚定到已布局邻居附近（而非远离主图的螺旋）。
                const prev = prevPos.get(n.id);
                let x, y;
                if (prev) { x = prev.x; y = prev.y; }
                else if (typeof n.x === 'number' && typeof n.y === 'number') { x = n.x; y = n.y; }
                else {
                    const anchor = this._anchorNearNeighbor(n.id, prevPos, graphData);
                    const angle = i * 2.399963229728653;
                    const fallbackR = 28 * Math.sqrt(i + 1);
                    x = anchor ? anchor.x : Math.cos(angle) * fallbackR;
                    y = anchor ? anchor.y : Math.sin(angle) * fallbackR;
                }
                try {
                    // 节点视觉：簇超节点 = 大尺寸半透明圆 + 「名称 N 节点」标签；
                    // 普通节点 = 分类色圆点（degree 对数尺寸）
                    const isCluster = n.type === 'cluster';
                    const label = isCluster
                        ? `${n.name} · ${n.meta && n.meta.nodeCount != null ? n.meta.nodeCount : ''} 节点`.trim()
                        : n.name;
                    const color = isCluster
                        ? (CATEGORY_FALLBACK_COLORS.cluster)
                        : (TYPE_COLORS[n.type]
                            || (this.store && this.store.model && this.store.model.CATEGORY_COLORS
                                ? this.store.model.CATEGORY_COLORS[this.store.model.categoryOf(n)]
                                : null)
                            || '#9aa1a9');
                    g.addNode(n.id, {
                        label,
                        size: isCluster ? this._clusterSize(n) : this._nodeSize(n),
                        color,
                        x,
                        y,
                        ...(n.path ? { path: n.path } : {}),
                    });
                    this._renderedIds.add(n.id);
                } catch (e) { /* 重复节点忽略 */ }
            });
            graphData.edges.forEach((e) => {
                if (!g.hasNode(e.source) || !g.hasNode(e.target)) return;
                if (!isVisible(e.source) || !isVisible(e.target)) return;
                // 关系过滤（PRD §9：点击只显示该类型关系）
                if (this.store.state.relationFilter && e.relation !== this.store.state.relationFilter) return;
                const style = RELATION_STYLES[e.relation] || RELATION_STYLES.REFERENCES;
                try {
                    g.addEdge(e.source, e.target, {
                        color: style.color,
                        type: style.type || (style.dashed ? 'dashed' : 'line'),
                        size: 1,
                        relation: e.relation,
                    });
                } catch (err) { /* 平行边忽略 */ }
            });

            this._highlight = focusNodeId
                // neighbors 必须是 Set（_applyStyle 用 .has() 判断；graphData.neighbors 本身就是 Set）
                ? { focus: focusNodeId, neighbors: graphData.neighbors(focusNodeId) }
                : null;
            this._sigma.refresh();
            this._applyStyle();
        }

        /** 节点大小：degree 对数刻度（5→12，封顶 12）。
         *  与 graph-layout 的碰撞半径同公式 —— 大图下节点半径必须与布局尺度
         *  相称，旧值（封顶 30）在 900 单位画布里相互覆盖。 */
        _nodeSize(node) {
            const d = node.degree || 1;
            return Math.min(12, 2.5 + Math.log2(d + 1) * 2.5);
        }

        /** 簇超节点大小：按成员数对数刻度（28→60，PRD §13 Cluster Node 大圆点） */
        _clusterSize(node) {
            const count = (node.meta && node.meta.nodeCount) || node.degree || 10;
            return Math.min(60, 28 + Math.log2(count + 1) * 6);
        }

        /** 增量展开的新节点锚定：取第一个已布局邻居的位置 + 轻微偏移（避免落点远离主图） */
        _anchorNearNeighbor(nodeId, prevPos, graphData) {
            const neighbors = graphData.neighbors(nodeId);
            for (const nb of neighbors) {
                const p = prevPos.get(nb);
                if (p) {
                    return { x: p.x + 12, y: p.y + 8 };
                }
            }
            return null;
        }

        /** 高亮设置：聚焦节点 + 邻域（dim 其余） */
        setHighlight(focusNodeId, neighborIds = []) {
            if (!this.engineReady) return;
            this._highlight = focusNodeId ? { focus: focusNodeId, neighbors: new Set(neighborIds) } : null;
            this._applyStyle();
        }

        /** 清除高亮 */
        clearHighlight() {
            this._highlight = null;
            this._applyStyle();
        }

        /** 聚焦节点（相机动画移动到节点） */
        focusNode(nodeId) {
            if (!this.engineReady || !this._sigma) return;
            try {
                const data = this._sigma.getNodeDisplayData(nodeId);
                if (!data) return;
                const camera = this._sigma.getCamera();
                // Sigma v3：animate 到 {x, y, ratio}（坐标 + 缩放比例）
                camera.animate(
                    { x: -data.x, y: -data.y, ratio: Math.max(camera.getBoundedRatio(0.2), camera.getState().ratio * 1.2) },
                    { duration: 300 },
                );
            } catch (e) { console.warn('[graph-renderer] focusNode 失败:', e); }
        }

        /** LOD 切换：按新层级重新装载数据子集（visibleNodeIds 过滤）+ 刷新样式 */
        setLod(lod) {
            this.store.set({ lod });
            this._styleCache = null;
            if (this.store.data && this.store.data.nodeCount > 0) {
                this.setData(this.store.data, {
                    focusNodeId: this.store.state.focusNodeId,
                    lod: this.store.state.lod,
                    visibleIds: () => this.store.visibleNodeIds(),
                });
            } else {
                this._applyStyle();
            }
        }

        /** 应用布局结果：写入 Sigma 节点位置（layout.apply 产物），并重置视口适配。
         *  画布无尺寸（HTML 面板视图覆盖期间）时仍写入坐标，但跳过 refresh/相机
         *  （Sigma 对 0 尺寸容器抛 "Container has no width"，恢复尺寸后自动渲染）。 */
        applyPositions(positions) {
            if (!this.engineReady || !this._sigma || !positions) return;
            positions.forEach((p, id) => {
                try {
                    if (this._graph.hasNode(id)) {
                        this._graph.setNodeAttribute(id, 'x', p.x);
                        this._graph.setNodeAttribute(id, 'y', p.y);
                    }
                } catch (e) { /* ignore */ }
            });
            const hasSize = (this.container.clientWidth || 0) > 0 && (this.container.clientHeight || 0) > 0;
            if (!hasSize) return;
            this._sigma.refresh();
            // 布局后重置视口：Sigma 默认 autoRescale 会在 refresh 时重算图范围，
            // 但相机中心可能停留在旧位置；animatedReset 让视口平滑适配新布局
            // （Obsidian 打开图谱自动缩放至全图可见）
            try {
                this._sigma.getCamera().animatedReset({ duration: 400 });
            } catch (e) { /* 相机重置失败不阻断 */ }
        }

        /** 当前相机缩放比（LOD 联动用） */
        getCameraRatio() {
            if (!this.engineReady || !this._sigma) return null;
            try {
                return this._sigma.getCamera().ratio;
            } catch (e) {
                return null;
            }
        }

        // ─── 视口控制（工具栏：定位 / 缩放 / 适应窗口 / 聚焦 / 重置；PRD §44） ───

        /** 适应窗口（fit view，动画） */
        fitView() {
            if (!this.engineReady || !this._sigma) return;
            try {
                this._sigma.getCamera().animatedReset({ duration: 400 });
            } catch (e) { /* 相机重置失败不阻断 */ }
        }

        /** 缩放（factor > 1 放大；锚定画布中心） */
        zoomBy(factor) {
            if (!this.engineReady || !this._sigma) return;
            try {
                const camera = this._sigma.getCamera();
                const ratio = camera.getBoundedRatio(camera.getState().ratio * factor);
                camera.animatedSet({ ratio }, { duration: 120 });
            } catch (e) { /* ignore */ }
        }

        /** 重置视角（回到初始适配视图） */
        resetView() {
            this.fitView();
        }

        /** 订阅 Sigma 相机事件（LOD 联动由 interaction 层处理）。
         *  Sigma v3 相机更新对外 emit 'updated'（非 'cameraUpdated'） */
        onCamera(callback) {
            if (!this._sigma) return () => {};
            const handler = () => callback();
            this._sigma.on('updated', handler);
            return () => this._sigma.off('updated', handler);
        }

        /** 订阅节点点击事件（Sigma v3 统一 emit 'click'，载荷 {node}） */
        onNodeClick(callback) {
            if (!this._sigma) return () => {};
            const handler = ({ node }) => callback(node);
            this._sigma.on('click', handler);
            return () => this._sigma.off('click', handler);
        }

        /** 应用高亮/淡化样式（Sigma 自定义 nodeReducer） */
        _applyStyle() {
            if (!this._sigma) return;
            if (!this._highlight) {
                this._sigma.setSetting('nodeReducer', null);
                this._sigma.setSetting('edgeReducer', null);
                return;
            }
            const { focus, neighbors } = this._highlight;
            this._sigma.setSetting('nodeReducer', (node, data) => {
                if (node === focus) return { ...data, highlighted: true, zIndex: 2 };
                if (neighbors.has(node)) return { ...data, zIndex: 1 };
                return { ...data, color: '#d0d7de', size: data.size * 0.5, zIndex: 0 };
            });
            this._sigma.setSetting('edgeReducer', (edge, data) => {
                const ext = this._sigma.getGraph().extremities(edge);
                if (ext.source === focus || ext.target === focus) return data;
                if (neighbors.has(ext.source) && neighbors.has(ext.target)) return data;
                return { ...data, color: '#d0d7de', size: 0.3 };
            });
            this._sigma.refresh();
        }

        /** 占位提示 */
        _showPlaceholder(msg) {
            let el = this.container.querySelector('.kg-canvas-placeholder');
            if (!el) {
                el = document.createElement('div');
                el.className = 'kg-canvas-placeholder';
                this.container.appendChild(el);
            }
            el.innerHTML = `<div class="kg-placeholder-text">${msg}</div>`;
            el.style.display = 'flex';
        }

        hidePlaceholder() {
            const el = this.container.querySelector('.kg-canvas-placeholder');
            if (el) el.style.display = 'none';
        }

        /** 销毁：反向释放 Sigma 实例与图形 */
        destroy() {
            if (this._sigma && typeof this._sigma.kill === 'function') {
                try { this._sigma.kill(); } catch (e) { /* ignore */ }
            }
            this._sigma = null;
            this._graph = null;
            this.engineReady = false;
            this._renderedIds.clear();
            this._highlight = null;
            this._styleCache = null;
        }
    }

    // ─── 对外暴露 ───
    window.GraphRenderer = GraphRenderer;
})();
