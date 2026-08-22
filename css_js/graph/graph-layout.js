/**
 * ===== 图谱布局策略（css_js/graph/graph-layout.js） =====
 * 【职责】布局算法抽象（L：依赖接口，不依赖具体实现）+ 内置布局实现。
 *        OCP：新增布局只需实现 GraphLayout 接口，app.js 注册，调用方零改动。
 *
 * 【依赖注入】constructor({ engine })：
 *   - engine = 布局引擎适配器（graphology 布局函数集合），由 app.js 注入
 *   - 骨架阶段：engine 可为 null → 布局仅记录目标位置（由 renderer 兜底初始布局）
 */
(function () {
    'use strict';

    const PRESET_NAMES = ['force', 'radial', 'circle'];

    /**
     * 布局引擎适配器接口（graphology-layout-forceatlas2 / 自研 radial/circle 均可实现）。
     * 骨架阶段提供默认实现：force 委托 graphology，radial/circle 纯 JS 计算。
     */
    class GraphLayout {
        /**
         * @param {{ engine?: object, Graph?: Function }} deps
         *   engine = { forceAtlas2? }；Graph = graphology 构造器（force 布局需要真实实例）
         */
        constructor({ engine, Graph } = {}) {
            this._engine = engine || null;
            this._Graph = Graph || null;
            /** 复用的 graphology 实例（按需重建，避免每次 new） */
            this._graph = this._Graph ? new this._Graph() : null;
        }

        /** 支持的布局预设 */
        static presets() { return PRESET_NAMES.slice(); }

        /** 是否支持某预设（缺引擎/Graph 时 force 不可用） */
        supports(name) {
            if (name === 'force') return !!(this._engine && this._Graph);
            return PRESET_NAMES.includes(name);
        }

        /**
         * 对图执行布局，返回节点位置映射 { nodeId: {x, y} }。
         * @param {object} graphData GraphData 实例（store.data）
         * @param {string} preset 布局名
         * @returns {Promise<Map<string, {x:number, y:number}>>}
         */
        async apply(graphData, preset) {
            const name = PRESET_NAMES.includes(preset) ? preset : 'radial';
            switch (name) {
                case 'force':
                    return this._applyForce(graphData);
                case 'radial':
                    return this._applyRadial(graphData);
                case 'circle':
                    return this._applyCircle(graphData);
                default:
                    return this._applyRadial(graphData);
            }
        }

        /** 力导向：委托 graphology-layout-forceatlas2（引擎/Graph 缺失时回退径向） */
        async _applyForce(graphData) {
            if (this._engine && typeof this._engine.forceAtlas2 === 'function' && this._graph) {
                try {
                    // forceAtlas2 需要真正的 graphology 实例：把 GraphData 同步进 graphology
                    const g = this._graph;
                    g.clear();
                    const nodeIds = Array.from(graphData.nodes.keys());
                    const n = nodeIds.length;
                    nodeIds.forEach((id, i) => {
                        try {
                            // 初始坐标用圆周散布 + 轻微扰动（关键修复）：forceAtlas2 从
                            // 全 (0,0) 同点起步时对称图无法打破平衡，迭代后仍全部重叠在
                            // 原点 → 视觉上只剩 1 个节点。分散起点让斥力/引力正常作用。
                            const angle = (i / Math.max(1, n)) * Math.PI * 2 + 0.01 * i;
                            const r = 100 + (i % 5) * 20;
                            g.addNode(id, {
                                label: graphData.nodes.get(id).name,
                                x: Math.cos(angle) * r,
                                y: Math.sin(angle) * r,
                                size: 1,
                            });
                        } catch (e) { /* 重复节点忽略 */ }
                    });
                    graphData.edges.forEach((e) => {
                        try {
                            if (g.hasNode(e.source) && g.hasNode(e.target)) {
                                g.addEdge(e.source, e.target);
                            }
                        } catch (err) { /* 平行边忽略 */ }
                    });
                    // 返回位置映射 { nodeId: {x, y} }（inplace 语义：forceAtlas2 直接写节点坐标，
                    // 返回值即映射；同时 graphology 节点已被就地更新）
                    const positions = this._engine.forceAtlas2(g, {
                        iterations: 80,
                        settings: {},
                    });
                    return this._toMap(positions);
                } catch (e) {
                    console.warn('[graph-layout] forceAtlas2 失败，回退径向:', e);
                }
            }
            return this._applyRadial(graphData);
        }

        /** 径向布局：按邻域深度分层（root = 选中/首个节点），每层圆周散布 */
        _applyRadial(graphData) {
            const ids = Array.from(graphData.nodes.keys());
            const n = ids.length;
            if (n === 0) return new Map();
            // root = 首个节点（layout 不感知 store.state，保持纯模型依赖）
            const rootId = ids[0];
            // 用邻接表做 BFS 分层
            const depthMap = new Map([[rootId, 0]]);
            const queue = [rootId];
            const seen = new Set([rootId]);
            while (queue.length) {
                const cur = queue.shift();
                const d = depthMap.get(cur);
                graphData.neighbors(cur).forEach((nb) => {
                    if (!seen.has(nb)) { seen.add(nb); depthMap.set(nb, d + 1); queue.push(nb); }
                });
            }
            const maxDepth = Math.max(0, ...depthMap.values());
            const positions = new Map();
            const perLayer = new Map();
            ids.forEach((id) => {
                const d = depthMap.get(id) ?? maxDepth + 1;
                if (!perLayer.has(d)) perLayer.set(d, []);
                perLayer.get(d).push(id);
            });
            perLayer.forEach((layer, d) => {
                const radius = d === 0 ? 0 : (d / (maxDepth + 1)) * 300;
                layer.forEach((id, i) => {
                    const angle = (i / layer.length) * Math.PI * 2 + d * 1.1;
                    positions.set(id, { x: Math.cos(angle) * radius, y: Math.sin(angle) * radius });
                });
            });
            return positions;
        }

        /** 环形布局：全部节点均匀散布在圆周 */
        _applyCircle(graphData) {
            const ids = Array.from(graphData.nodes.keys());
            const n = ids.length;
            const positions = new Map();
            ids.forEach((id, i) => {
                const angle = (i / Math.max(1, n)) * Math.PI * 2;
                positions.set(id, { x: Math.cos(angle) * 260, y: Math.sin(angle) * 260 });
            });
            return positions;
        }

        /** 位置映射归一为 Map<string, {x,y}>（兼容 graphology 的 object 返回） */
        _toMap(positions) {
            const map = new Map();
            if (positions instanceof Map) return positions;
            for (const [id, p] of Object.entries(positions || {})) {
                map.set(id, { x: p.x, y: p.y });
            }
            return map;
        }
    }

    // ─── 对外暴露 ───
    window.GraphLayout = GraphLayout;
})();
