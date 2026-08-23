/**
 * ===== 图谱状态存储（css_js/graph/graph-store.js） =====
 * 【职责】客户端图缓存 + 局部状态（当前 LOD、选中节点、展开历史、类型过滤）。
 *        单一数据源（S）：renderer / interaction / panel 都只读写 store，
 *        通过订阅（subscribe）响应变化，模块间不互相直接引用 DOM。
 *
 * 【设计】Store 是「读模型」：持有 GraphData + 视图状态；不发起任何 IO（IO 在 graph-api）。
 *        OCP：状态字段通过泛型 set() 维护，新增状态不改结构。
 */
(function () {
    'use strict';

    class GraphStore {
        /**
         * @param {{ model: object }} deps 注入 graph-model（GraphData 类）
         */
        constructor({ model }) {
            this._GraphData = model.GraphData;
            /** 图数据（节点/边/邻接表） */
            this.data = new this._GraphData();
            /** 视图状态 */
            this.state = {
                dirPath: '',
                view: 'global',            // 当前视图：global/document/topics/domains/timeline
                lod: model.LOD.CLUSTERS,   // 当前 LOD 层级（L0 聚类起步，PRD §12）
                selectedNodeId: null,        // 选中节点
                focusNodeId: null,           // 聚焦节点（高亮邻域）
                clusterId: null,             // 当前展开的知识簇 id（全局视图）
                expandedNodeIds: new Set(),  // 已展开二跳的节点
                typeFilter: null,            // 类型过滤（null=全部）
                relationFilter: null,        // 关系过滤（null=全部）
                layoutPreset: null,          // 用户选择的布局模式（null=视图默认；force/hierarchy/radial/cluster）
                loading: false,
                lastError: null,
                engineReady: false,          // Sigma 引擎是否就绪
                buildStatus: null,           // 后端图构建状态
                graphVersion: 0,             // 图版本（缓存失效依据）
            };
            /** 知识簇缓存（L0 聚合单元） */
            this.clusters = [];
            /** 订阅者列表 */
            this._listeners = new Set();
            /** 依赖的 model（供外部取常量） */
            this.model = model;
        }

        /** 订阅状态变化（返回取消函数） */
        subscribe(fn) {
            this._listeners.add(fn);
            return () => this._listeners.delete(fn);
        }

        /** 通知所有订阅者（变化说明可选） */
        _emit(change) {
            this._listeners.forEach((fn) => {
                try { fn(this.state, change); } catch (e) { console.error('[graph-store] 订阅者异常:', e); }
            });
        }

        /** 通用状态更新：set({ key: value }) → 触发一次通知 */
        set(patch) {
            let changed = false;
            for (const [k, v] of Object.entries(patch)) {
                if (this.state[k] !== v) { this.state[k] = v; changed = true; }
            }
            if (changed) this._emit({ type: 'state', patch });
        }

        // ─── 图数据操作 ───

        /** 装载邻域/展开返回的数据（幂等合并） */
        loadData(payload = {}) {
            this.data.upsertNodes(payload.nodes);
            this.data.upsertEdges(payload.edges);
            // 补算 degree：graph_overview 返回的节点不带 degree（后端只对邻域查询填 degree），
            // 而 LOD 核心/重要过滤（visibleNodeIds）与节点尺寸（renderer._nodeSize）
            // 都依赖 degree —— 从邻接表推算，保证文档关系视图的层级与缩放过滤正确。
            this.data.nodes.forEach((n) => {
                if (typeof n.degree !== 'number') {
                    n.degree = this.data.neighbors(n.id).size;
                }
            });
            this._emit({ type: 'data' });
        }

        /** 清空图数据（切目录/重置） */
        clearData() {
            this.data.clear();
            this.state.expandedNodeIds.clear();
            this.set({ selectedNodeId: null, focusNodeId: null });
            this._emit({ type: 'data' });
        }

        // ─── 知识簇（L0 聚合单元） ───

        /** 装载簇列表（替换缓存） */
        setClusters(list = []) {
            this.clusters = list;
            this._emit({ type: 'clusters', clusters: list });
        }

        /** 按 id 取簇 */
        getCluster(clusterId) {
            return this.clusters.find((c) => c.id === clusterId) || null;
        }

        /** 设置当前展开的簇（并清空节点选中态） */
        setClusterId(clusterId) {
            this.set({ clusterId, selectedNodeId: null, focusNodeId: null });
        }

        /** 标记某节点已展开（二跳历史） */
        markExpanded(nodeId) {
            this.state.expandedNodeIds.add(nodeId);
            this._emit({ type: 'expanded', nodeId });
        }

        /** 是否已展开 */
        isExpanded(nodeId) {
            return this.state.expandedNodeIds.has(nodeId);
        }

        /** 按 id 取节点 */
        getNode(nodeId) {
            return this.data.nodes.get(nodeId) || null;
        }

        /**
         * 当前过滤 + LOD 下的可见节点 id 集合（PRD §8 分类过滤 + §12/§40 LOD）。
         * - typeFilter：分类白名单（CATEGORIES；null/空 = 全部可见）；
         * - lod：L0 仅簇 → L1 簇+核心 → L2 +重要 → L3 全量 → L4 细粒度（+chunk 内容层）。
         * 返回 null = 全部可见；返回 [] = 无可见节点（LOD 过滤到空）。
         */
        visibleNodeIds() {
            const model = this.model;
            const filter = this.state.typeFilter;
            const lod = this.state.lod;
            const hasFilter = Array.isArray(filter) && filter.length > 0 && !filter.includes('all');

            // 核心/重要节点阈值（degree 对数语义；与 renderer 尺寸一致）
            const isCore = (n) => (n.degree || 0) >= 8;
            const isImportant = (n) => (n.degree || 0) >= 3;

            let anyFiltered = false;
            const ids = [];
            this.data.nodes.forEach((n) => {
                // 簇节点恒显示（L0 聚合单元）
                if (n.type === 'cluster') { ids.push(n.id); return; }
                // chunk 内容层：仅 L4 细粒度显示（概览/文档视图不混入内容块）
                if (n.type === 'chunk') {
                    if (lod !== model.LOD.DETAIL) { anyFiltered = true; return; }
                }
                // section 结构层：L3 起显示
                if (n.type === 'section' && lod < model.LOD.FULL) { anyFiltered = true; return; }
                // 分类过滤（type 直接匹配 or 按扩展名分类）
                if (hasFilter) {
                    const cat = model.categoryOf(n);
                    if (!filter.includes(n.type) && !filter.includes(cat)) {
                        anyFiltered = true;
                        return;
                    }
                }
                // LOD 过滤（chunk/section 已在上方处理）
                switch (lod) {
                    case model.LOD.CLUSTERS:
                        anyFiltered = true;
                        return; // 仅簇
                    case model.LOD.CORE:
                        if (!isCore(n)) { anyFiltered = true; return; }
                        break;
                    case model.LOD.IMPORTANT:
                        if (!isCore(n) && !isImportant(n)) { anyFiltered = true; return; }
                        break;
                    default:
                        break; // FULL / DETAIL：全部
                }
                ids.push(n.id);
            });
            if (!anyFiltered && !hasFilter) return null;
            return ids;
        }

        /** 选中 + 聚焦（联动高亮邻域） */
        selectNode(nodeId) {
            const node = nodeId ? this.getNode(nodeId) : null;
            this.set({
                selectedNodeId: node ? nodeId : null,
                focusNodeId: node ? nodeId : null,
            });
            if (node) {
                this._emit({ type: 'select', nodeId, neighbors: Array.from(this.data.neighbors(nodeId)) });
            }
        }

        /** 清除选中/聚焦 */
        clearSelection() {
            this.set({ selectedNodeId: null, focusNodeId: null });
        }

        /** 重置全部（销毁用） */
        dispose() {
            this._listeners.clear();
            this.data.clear();
            this.state.expandedNodeIds.clear();
        }
    }

    // ─── 对外暴露 ───
    window.GraphStore = GraphStore;
})();
