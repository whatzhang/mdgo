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

    const PRESET_NAMES = ['force', 'radial', 'circle', 'cluster', 'hierarchy'];

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
            const name = PRESET_NAMES.includes(preset) ? preset : 'cluster';
            switch (name) {
                case 'force':
                    return this._applyForce(graphData);
                case 'radial':
                    return this._applyRadial(graphData);
                case 'circle':
                    return this._applyCircle(graphData);
                case 'cluster':
                    return this._applyCluster(graphData);
                case 'hierarchy':
                    return this._applyHierarchy(graphData);
                default:
                    return this._applyCluster(graphData);
            }
        }

        /** 力导向：委托 graphology-layout-forceatlas2（引擎/Graph 缺失时回退径向）。
         *  管线（四阶段，针对 800+ 节点「中心聚簇/重叠遮挡」问题的完整修复）：
         *    ① 螺旋种子（Vogel 模型）——整平面均匀散布起步，杜绝密环/同点起算；
         *    ② forceAtlas2——强斥力 + 弱引力 + 大图阻尼，让社区/层次自然成型；
         *    ③ 稳健归一化——按 5~95 分位跨度缩放到视口友好尺度，不受离群点支配；
         *    ④ 碰撞分离——在显示空间把仍互相覆盖的节点推开（防重叠核心）。
         */
        async _applyForce(graphData) {
            if (this._engine && typeof this._engine.forceAtlas2 === 'function' && this._graph) {
                try {
                    // forceAtlas2 需要真正的 graphology 实例：把 GraphData 同步进 graphology
                    const g = this._graph;
                    g.clear();
                    const nodeIds = Array.from(graphData.nodes.keys());
                    const n = nodeIds.length;
                    if (n === 0) return new Map();

                    // 度数（mass 语义：与 renderer 视觉尺寸同源，保证碰撞半径一致）
                    const degreeOf = new Map();
                    nodeIds.forEach((id) => {
                        const nd = graphData.nodes.get(id);
                        degreeOf.set(id, (typeof nd.degree === 'number' && nd.degree > 0)
                            ? nd.degree
                            : graphData.neighbors(id).size);
                    });

                    // ① 螺旋种子（关键修复）：旧实现把 800+ 节点压在半径 100~180 的
                    // 密环上起步（间距 <1 单位），斥力/引力在极近距离互相抵消，
                    // 数百次迭代仍无法散开 → 中心毛球。黄金角螺旋保证任意规模下
                    // 起点间距 ≈ 28*sqrt(n)，斥力从一开始就能正常推离节点。
                    nodeIds.forEach((id, i) => {
                        try {
                            const angle = i * 2.399963229728653; // 黄金角 ≈ 137.5°
                            const r = 28 * Math.sqrt(i + 1);
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

                    // ② forceAtlas2 参数（针对大图中心塌缩的修复）：
                    // - scalingRatio 100：强斥力 → 节点间距显著拉大
                    // - gravity 0.1：弱向心力，只把整体拉回原点附近，不产生中心挤压
                    // - slowDown 3：大图阻尼，抑制振荡（旧值 1 在 800+ 节点时易抖成团）
                    // - barnesHutOptimize：大图 O(n log n) 加速
                    // - 迭代上限 260：控制布局耗时（螺旋种子 + 碰撞分离兜底，无需超长迭代）
                    const iterations = Math.min(260, Math.max(160, Math.round(n * 0.35)));
                    const positions = this._engine.forceAtlas2(g, {
                        iterations,
                        settings: {
                            barnesHutOptimize: n > 300,
                            barnesHutTheta: 0.6,
                            strongGravityMode: false,
                            gravity: 0.1,
                            scalingRatio: 100,
                            slowDown: 3,
                            adjustSizes: false,
                            linLogMode: false,
                            edgeWeightInfluence: 0,
                            outboundAttractionDistribution: false,
                        },
                    });
                    const map = this._toMap(positions);

                    // ③ 稳健归一化：5~95 分位跨度 → 目标 900 单位并居中。
                    // 旧实现用全量 bbox，会被极少数离群节点（如 AppSync 孤岛）撑大
                    // 尺度，导致主团块反而被压缩得更密。
                    this._normalizeTo(map, 900);

                    // ④ 碰撞分离（显示空间）：把仍互相覆盖的节点推开到「半径和 + gap」。
                    // 旧实现归一化把力导向自然间距（~14 单位）压缩到 ~2.4 单位，
                    // 而节点渲染半径是绝对单位 → 无论布局多好都会重叠；分离阶段
                    // 必须在归一化之后、以渲染半径为准执行。
                    this._separate(map, degreeOf, 4, 40);

                    return map;
                } catch (e) {
                    console.warn('[graph-layout] forceAtlas2 失败，回退径向:', e);
                }
            }
            return this._applyRadial(graphData);
        }

        /** 节点显示半径（与 renderer._nodeSize 同公式：min(12, 2.5 + log2(deg+1)*2.5)） */
        _nodeRadius(id, degreeOf) {
            const deg = degreeOf.get(id) || 1;
            return Math.max(2, Math.min(12, 2.5 + Math.log2(deg + 1) * 2.5) / 2);
        }

        /** 稳健归一化：5~95 分位跨度 → target 单位，并平移到原点居中 */
        _normalizeTo(map, target) {
            if (!map || map.size === 0) return;
            const xs = [], ys = [];
            map.forEach((p) => { xs.push(p.x); ys.push(p.y); });
            xs.sort((a, b) => a - b);
            ys.sort((a, b) => a - b);
            const pct = (arr, k) => arr[Math.min(arr.length - 1, Math.floor(k * arr.length))];
            const raw = Math.max(
                Math.hypot(pct(xs, 0.95) - pct(xs, 0.05), pct(ys, 0.95) - pct(ys, 0.05)),
                1e-6
            );
            const scale = target / raw;
            let cx = 0, cy = 0;
            map.forEach((p) => { cx += p.x; cy += p.y; });
            cx /= map.size; cy /= map.size;
            map.forEach((p) => { p.x = (p.x - cx) * scale; p.y = (p.y - cy) * scale; });
        }

        /** 碰撞分离：多轮斥力松弛，把重叠节点推开到「半径和 + gap」（防遮挡核心）。
         *  每轮 O(n²)，按图规模限制总工作量（约 800 万次成对检测），
         *  保证 5000 节点级全览也不卡死。 */
        _separate(map, degreeOf, gap = 4, roundsCap = 40) {
            const ids = Array.from(map.keys());
            const n = ids.length;
            if (n < 2) return;
            const pairCount = (n * (n - 1)) / 2;
            // 目标总工作量 ≈ 800 万次检测 → 轮数 = 800e4 / pairCount（下限 2，上限 roundsCap）
            const rounds = Math.min(roundsCap, Math.max(2, Math.floor(8000000 / pairCount)));
            for (let r = 0; r < rounds; r++) {
                let moved = false;
                for (let i = 0; i < n; i++) {
                    const a = ids[i];
                    const pa = map.get(a);
                    const ra = this._nodeRadius(a, degreeOf);
                    for (let j = i + 1; j < n; j++) {
                        const b = ids[j];
                        const pb = map.get(b);
                        const dx = pa.x - pb.x;
                        const dy = pa.y - pb.y;
                        const minD = ra + this._nodeRadius(b, degreeOf) + gap;
                        const d2 = dx * dx + dy * dy;
                        if (d2 > 1e-9 && d2 < minD * minD) {
                            const d = Math.sqrt(d2);
                            const push = (minD - d) / 2;
                            const ux = dx / d, uy = dy / d;
                            pa.x += ux * push; pa.y += uy * push;
                            pb.x -= ux * push; pb.y -= uy * push;
                            moved = true;
                        }
                    }
                }
                if (!moved) break;
            }
        }

        /** 径向布局：按邻域深度分层（root = 选中/首个节点），每层圆周散布。
         *  层半径按「周长 ≥ 层节点数 × spacing」自适应：即使某层节点极多
         *  （如星型图 800 个节点全在 depth=1），也不会挤成一条密环。 */
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
            const spacing = 14;
            const maxRadius = 28 * Math.sqrt(n);
            perLayer.forEach((layer, d) => {
                const frac = d / (maxDepth + 1);
                const radius = d === 0
                    ? 0
                    : Math.max(frac * maxRadius, (layer.length * spacing) / (2 * Math.PI));
                layer.forEach((id, i) => {
                    // 黄金角散布，避免同层节点对齐成放射线
                    const angle = i * 2.399963229728653 + d * 0.7;
                    positions.set(id, { x: Math.cos(angle) * radius, y: Math.sin(angle) * radius });
                });
            });
            return positions;
        }

        /** 环形布局：全部节点均匀散布在圆周（周长容纳 spacing 间距） */
        _applyCircle(graphData) {
            const ids = Array.from(graphData.nodes.keys());
            const n = ids.length;
            const radius = Math.max(120, (n * 14) / (2 * Math.PI));
            const positions = new Map();
            ids.forEach((id, i) => {
                const angle = (i / Math.max(1, n)) * Math.PI * 2;
                positions.set(id, { x: Math.cos(angle) * radius, y: Math.sin(angle) * radius });
            });
            return positions;
        }

        /** 聚类布局（PRD §10.4/§11 默认推荐）——两级布局解决「节点全部挤在一起」：
         *  ① 簇（Cluster）之间按圆圈排布，保持足够间距（Cluster Repulsion）；
         *  ② 簇内成员环绕簇心散布（小环），Force 只作用于簇内部。
         *  分组依据（优先级）：节点 meta.clusterId → path 顶层目录 → 根目录兜底；
         *  type=cluster 的聚合超节点独占一组（L0 全览数据源）。
         */
        _applyCluster(graphData) {
            const ids = Array.from(graphData.nodes.keys());
            const n = ids.length;
            if (n === 0) return new Map();
            const positions = new Map();

            // 1) 分组：cluster 超节点自成一组；其余按 meta.clusterId / path 顶层目录
            const groups = new Map(); // clusterKey -> [nodeId...]
            const clusterSuper = new Set(); // 聚合超节点 id
            ids.forEach((id) => {
                const nd = graphData.nodes.get(id);
                if (nd.type === 'cluster') {
                    clusterSuper.add(id);
                    // 超节点加入自己的组（与同簇成员同 key），放置阶段据此定位到簇心
                    if (!groups.has(id)) groups.set(id, []);
                    groups.get(id).push(id);
                    return;
                }
                let key = null;
                try {
                    const meta = typeof nd.meta === 'string' ? JSON.parse(nd.meta) : (nd.meta || {});
                    if (meta && meta.clusterId) key = String(meta.clusterId);
                } catch (e) { /* meta 解析失败忽略 */ }
                if (!key && nd.path && String(nd.path).includes('/')) {
                    key = 'cluster:' + String(nd.path).split('/')[0];
                }
                if (!key) key = 'cluster:__root__';
                if (!groups.has(key)) groups.set(key, []);
                groups.get(key).push(id);
            });

            const keys = Array.from(groups.keys());
            const k = keys.length;

            // 2) 簇心坐标：圆圈排布（半径随簇数自适应）
            const centerMap = new Map();
            const groupRadius = 46 * Math.sqrt(k) + 60;
            keys.forEach((key, i) => {
                const angle = (i / Math.max(1, k)) * Math.PI * 2 + 0.618 * i;
                centerMap.set(key, { x: Math.cos(angle) * groupRadius, y: Math.sin(angle) * groupRadius });
            });

            // 3) 成员环绕簇心；超节点落在簇心（超节点与成员可能同组：先放超节点，再环绕成员）
            const golden = 2.399963229728653;
            keys.forEach((key) => {
                const members = groups.get(key);
                const center = centerMap.get(key);
                const memberIds = members.filter((id) => !clusterSuper.has(id));
                // 超节点落簇心
                members.forEach((id) => {
                    if (clusterSuper.has(id)) positions.set(id, { x: center.x, y: center.y });
                });
                // 成员环绕（同组超节点不占环位）
                const count = memberIds.length;
                if (count > 0) {
                    const ringRadius = Math.max(18, 13 * Math.sqrt(count) + 8);
                    memberIds.forEach((id, i) => {
                        const angle = i * golden;
                        positions.set(id, {
                            x: center.x + Math.cos(angle) * ringRadius,
                            y: center.y + Math.sin(angle) * ringRadius,
                        });
                    });
                }
            });

            // 4) 归一化到视口友好尺度（与 force 管线一致）
            this._normalizeTo(positions, 900);
            return positions;
        }

        /** 层级布局（PRD §10.2：目录/领域/知识体系）：BFS 深度 → 水平分层，层内纵向散布 */
        _applyHierarchy(graphData) {
            const ids = Array.from(graphData.nodes.keys());
            const n = ids.length;
            if (n === 0) return new Map();
            const positions = new Map();

            // BFS 深度（root = 首个节点；孤立节点 depth = 0）
            const adj = new Map();
            ids.forEach((id) => adj.set(id, new Set()));
            graphData.edges.forEach((e) => {
                if (adj.has(e.source) && adj.has(e.target)) {
                    adj.get(e.source).add(e.target);
                    adj.get(e.target).add(e.source);
                }
            });
            const rootId = ids[0];
            const depth = new Map([[rootId, 0]]);
            const queue = [rootId];
            while (queue.length) {
                const cur = queue.shift();
                for (const nb of adj.get(cur) || []) {
                    if (!depth.has(nb)) { depth.set(nb, depth.get(cur) + 1); queue.push(nb); }
                }
            }
            ids.forEach((id) => { if (!depth.has(id)) depth.set(id, 0); });

            const byLevel = new Map();
            ids.forEach((id) => {
                const d = depth.get(id);
                if (!byLevel.has(d)) byLevel.set(d, []);
                byLevel.get(d).push(id);
            });
            const maxDepth = Math.max(0, ...byLevel.keys());
            const xGap = 120;
            const levelCount = Math.max(1, maxDepth + 1);
            // 纵向总跨度：按每层平均节点数 × 间距（避免同层节点重叠）
            const totalY = Math.max(200, 20 * (n / levelCount));
            byLevel.forEach((level, d) => {
                const x = (d - maxDepth / 2) * xGap;
                level.forEach((id, i) => {
                    const y = (i / Math.max(1, level.length - 1) - 0.5) * totalY + ((i % 3) - 1) * 10;
                    positions.set(id, { x, y });
                });
            });
            this._normalizeTo(positions, 900);
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
