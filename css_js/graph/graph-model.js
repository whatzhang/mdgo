/**
 * ===== 图谱数据模型（css_js/graph/graph-model.js） =====
 * 【职责】纯数据结构：GraphNode / GraphEdge / GraphData / 工厂与校验。
 *        本模块不做任何 IO、不依赖任何全局 —— 单一职责（S）、纯模型。
 *
 * 【后端 graph_* 契约对齐】（Phase 1 后端实现后保持一致）
 *   GraphNode = { id, type, name, path?, meta?, degree? }
 *   GraphEdge = { source, target, relation, weight?, confidence? }
 *   LOD 层级：0=概览聚合 / 1=局部邻域 / 2=焦点展开
 */
(function () {
    'use strict';

    /** 节点类型（对应六类图演进：doc/chunk 先行，entity/experience/memory 预留） */
    const NODE_TYPES = Object.freeze([
        'doc', 'folder', 'chunk', 'entity', 'experience', 'memory', 'cluster',
    ]);

    /** 关系类型（Document Graph 阶段） */
    const RELATIONS = Object.freeze([
        'CONTAINS', 'REFERENCES', 'IMPORTS', 'DERIVED_FROM', 'SAME_TOPIC',
    ]);

    /** LOD 层级常量 */
    const LOD = Object.freeze({ OVERVIEW: 0, LOCAL: 1, FOCUS: 2 });

    /** 默认邻域查询上限（与后端契约一致） */
    const QUERY_LIMITS = Object.freeze({
        MAX_NODES: 200,
        MAX_EDGES: 400,
        DEFAULT_DEPTH: 2,
        WEIGHT_MIN: 0.3,
        SEARCH_LIMIT: 20,
        OVERVIEW_NODES: 5000,
    });

    /**
     * GraphNode 工厂：统一补默认值 + 校验。
     * @param {object} input
     * @returns {{id:string,type:string,name:string,path?:string,meta?:object,degree?:number}}
     */
    function createNode(input) {
        if (!input || typeof input.id !== 'string' || !input.id) {
            throw new Error('[graph-model] 节点缺少 id');
        }
        const type = input.type || 'chunk';
        if (!NODE_TYPES.includes(type)) {
            throw new Error(`[graph-model] 未知节点类型: ${type}`);
        }
        return {
            id: input.id,
            type,
            name: input.name || input.id,
            ...(input.path ? { path: input.path } : {}),
            ...(input.meta ? { meta: input.meta } : {}),
            ...(typeof input.degree === 'number' ? { degree: input.degree } : {}),
        };
    }

    /**
     * GraphEdge 工厂。
     * @param {object} input
     * @returns {{source:string,target:string,relation:string,weight?:number,confidence?:number}}
     */
    function createEdge(input) {
        if (!input || !input.source || !input.target) {
            throw new Error('[graph-model] 边缺少 source/target');
        }
        const relation = input.relation || 'REFERENCES';
        if (!RELATIONS.includes(relation)) {
            // 允许未知关系（后续六类图扩展），仅告警不阻断
            console.warn(`[graph-model] 未注册关系类型: ${relation}`);
        }
        return {
            source: input.source,
            target: input.target,
            relation,
            ...(typeof input.weight === 'number' ? { weight: input.weight } : {}),
            ...(typeof input.confidence === 'number' ? { confidence: input.confidence } : {}),
        };
    }

    /**
     * GraphData 聚合：nodes/edges 数组 + 按 id 索引 + 邻接表。
     * 邻接表供 interaction/renderer 快速取邻居（避免每次遍历全边）。
     */
    class GraphData {
        constructor() {
            /** @type {Map<string, ReturnType<typeof createNode>>} */
            this.nodes = new Map();
            /** @type {Map<string, ReturnType<typeof createEdge>>} */
            this.edges = new Map();
            /** @type {Map<string, Set<string>>} 节点 id → 邻居 id 集合 */
            this.adjacency = new Map();
        }

        /** 清空全部 */
        clear() {
            this.nodes.clear();
            this.edges.clear();
            this.adjacency.clear();
        }

        /** 添加节点（幂等：同 id 覆盖） */
        upsertNode(input) {
            const node = createNode(input);
            this.nodes.set(node.id, node);
            return node;
        }

        /** 批量添加节点 */
        upsertNodes(list = []) {
            list.forEach((n) => this.upsertNode(n));
        }

        /** 添加边 + 更新邻接表（幂等：同 source/target/relation 覆盖） */
        upsertEdge(input) {
            const edge = createEdge(input);
            const key = `${edge.source}|${edge.target}|${edge.relation}`;
            this.edges.set(key, edge);
            if (!this.adjacency.has(edge.source)) this.adjacency.set(edge.source, new Set());
            if (!this.adjacency.has(edge.target)) this.adjacency.set(edge.target, new Set());
            this.adjacency.get(edge.source).add(edge.target);
            this.adjacency.get(edge.target).add(edge.source);
            return edge;
        }

        /** 批量添加边 */
        upsertEdges(list = []) {
            list.forEach((e) => this.upsertEdge(e));
        }

        /** 取某节点直接邻居 id 集合 */
        neighbors(nodeId) {
            return this.adjacency.get(nodeId) || new Set();
        }

        /** 节点数 / 边数 */
        get nodeCount() { return this.nodes.size; }
        get edgeCount() { return this.edges.size; }

        /** 序列化（供持久化/调试） */
        toJSON() {
            return {
                nodes: Array.from(this.nodes.values()),
                edges: Array.from(this.edges.values()),
            };
        }

        /** 从后端返回的 { nodes, edges } 装载 */
        load(payload = {}) {
            this.clear();
            this.upsertNodes(payload.nodes);
            this.upsertEdges(payload.edges);
        }
    }

    // ─── 对外暴露（模块惯例：window 单例） ───
    window.GraphModel = {
        NODE_TYPES,
        RELATIONS,
        LOD,
        QUERY_LIMITS,
        createNode,
        createEdge,
        GraphData,
    };
})();
