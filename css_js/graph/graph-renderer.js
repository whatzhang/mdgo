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
        chunk: '#1a7f37',
        entity: '#d97706',
        experience: '#cf222e',
        memory: '#953800',
        cluster: '#57606a',
    };

    /** 关系类型 → 线型/颜色 */
    const RELATION_STYLES = {
        CONTAINS: { color: '#57606a', dashed: false },
        REFERENCES: { color: '#0969da', dashed: false },
        IMPORTS: { color: '#1a7f37', dashed: false },
        DERIVED_FROM: { color: '#d97706', dashed: true },
        SAME_TOPIC: { color: '#8250df', dashed: true },
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
                    // Sigma v3：renderer 由包自动选择（webgl/webgpu 回退 canvas），
                    // 无需显式 renderer 选项；camera 缩放范围在 settings 中配置
                    minCameraRatio: 0.02,
                    maxCameraRatio: 8,
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
                // 这里兜底一个确定性初始布局（圆周散布），避免 Sigma 抛
                // "could not find a valid position"；已布局过的节点复用旧坐标。
                const prev = prevPos.get(n.id);
                const angle = (i / Math.max(1, nodes.length)) * Math.PI * 2;
                const x = prev ? prev.x : (typeof n.x === 'number' ? n.x : Math.cos(angle) * 260);
                const y = prev ? prev.y : (typeof n.y === 'number' ? n.y : Math.sin(angle) * 260);
                try {
                    g.addNode(n.id, {
                        label: n.name,
                        size: this._nodeSize(n),
                        color: TYPE_COLORS[n.type] || '#57606a',
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
                const style = RELATION_STYLES[e.relation] || RELATION_STYLES.REFERENCES;
                try {
                    g.addEdge(e.source, e.target, {
                        color: style.color,
                        type: style.dashed ? 'dashed' : 'line',
                        size: 1,
                        relation: e.relation,
                    });
                } catch (err) { /* 平行边忽略 */ }
            });

            this._highlight = focusNodeId
                ? { focus: focusNodeId, neighbors: Array.from(graphData.neighbors(focusNodeId)) }
                : null;
            this._sigma.refresh();
            this._applyStyle();
        }

        /** 节点大小：degree 对数刻度（1→8，封顶 30） */
        _nodeSize(node) {
            const d = node.degree || 1;
            return Math.min(30, 4 + Math.log2(d + 1) * 4);
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

        /** LOD 切换：更新样式缓存并刷新（数据子集由 interaction/store 决定） */
        setLod(lod) {
            this.store.set({ lod });
            this._styleCache = null;
            this._applyStyle();
        }

        /** 应用布局结果：写入 Sigma 节点位置（layout.apply 产物） */
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
            this._sigma.refresh();
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
