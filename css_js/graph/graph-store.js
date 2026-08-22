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
                lod: model.LOD.LOCAL,        // 当前 LOD 层级
                selectedNodeId: null,        // 选中节点
                focusNodeId: null,           // 聚焦节点（高亮邻域）
                expandedNodeIds: new Set(),  // 已展开二跳的节点
                typeFilter: null,            // 类型过滤（null=全部）
                loading: false,
                lastError: null,
                engineReady: false,          // Sigma 引擎是否就绪
                buildStatus: null,           // 后端图构建状态
            };
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
            this._emit({ type: 'data' });
        }

        /** 清空图数据（切目录/重置） */
        clearData() {
            this.data.clear();
            this.state.expandedNodeIds.clear();
            this.set({ selectedNodeId: null, focusNodeId: null });
            this._emit({ type: 'data' });
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

        /** 当前类型过滤下的可见节点集合（typeFilter=null 全部可见；数组=多选白名单） */
        visibleNodeIds() {
            const filter = this.state.typeFilter;
            if (!filter || filter.length === 0) return null; // null = 全部可见
            const ids = [];
            this.data.nodes.forEach((n) => { if (filter.includes(n.type)) ids.push(n.id); });
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
