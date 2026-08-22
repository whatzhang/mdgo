/**
 * ===== 图谱数据访问层（css_js/graph/graph-api.js） =====
 * 【职责】封装 graph_* Tauri 命令为 Promise API —— 唯一数据访问点（单一职责 S）。
 *        后端 Phase 1（SQLite Graph Engine）落地后按相同签名实现，前端零改动。
 *
 * 【依赖注入】constructor({ invoke })：
 *   - Tauri 模式：invoke = window.parent.__TAURI__.core.invoke（主页面同源复用）
 *   - 非 Tauri 模式：invoke = null → 自动进入 mock 模式（演示数据，供浏览器预览）
 *
 * 【失败降级】所有方法 catch 后返回 null/空结构，绝不抛未捕获异常 —— 检索/图操作永不阻断 UI。
 */
(function () {
    'use strict';

    const GRAPH_COMMANDS = {
        STATUS: 'graph_status',
        RELATED: 'graph_related',
        EXPAND: 'graph_expand',
        SEARCH: 'graph_search',
        OVERVIEW: 'graph_overview',
        STATS: 'graph_stats',
        EXTRACT_ENTITIES: 'graph_extract_entities',
        EXPERIENCE_RECORD: 'graph_experience_record',
        EXPERIENCE_SEARCH: 'graph_experience_search',
        EXPERIENCE_EVENTS: 'graph_experience_events',
    };

    /** mock 演示数据（非 Tauri 模式 / 引擎缺失时可用） */
    function mockOverview() {
        const nodes = [
            { id: 'doc:readme', type: 'doc', name: 'README.md', degree: 3 },
            { id: 'doc:rag', type: 'doc', name: 'docs/rag.md', degree: 2 },
            { id: 'chunk:rag-1', type: 'chunk', name: 'RAG 检索管线', degree: 2 },
            { id: 'chunk:rag-2', type: 'chunk', name: '向量召回', degree: 1 },
            { id: 'chunk:rag-3', type: 'chunk', name: 'RRF 融合', degree: 1 },
            { id: 'entity:redis', type: 'entity', name: 'Redis', degree: 2 },
        ];
        const edges = [
            { source: 'doc:readme', target: 'doc:rag', relation: 'REFERENCES' },
            { source: 'doc:rag', target: 'chunk:rag-1', relation: 'CONTAINS' },
            { source: 'chunk:rag-1', target: 'chunk:rag-2', relation: 'SAME_TOPIC' },
            { source: 'chunk:rag-1', target: 'chunk:rag-3', relation: 'SAME_TOPIC' },
            { source: 'chunk:rag-1', target: 'entity:redis', relation: 'REFERENCES' },
            { source: 'entity:redis', target: 'chunk:rag-2', relation: 'DERIVED_FROM' },
        ];
        return { nodes, edges, truncated: false };
    }

    class GraphApiClient {
        /**
         * @param {{ invoke?: Function }} deps
         */
        constructor({ invoke } = {}) {
            /** @type {Function|null} */
            this._invoke = typeof invoke === 'function' ? invoke : null;
            this.isMock = !this._invoke;
            if (this.isMock) {
                console.warn('[graph-api] 未检测到 Tauri invoke，进入演示数据模式（mock）');
            }
        }

        /** 是否可用（Tauri 桥存在） */
        get available() { return !this.isMock; }

        /**
         * 执行 graph_* 命令（统一入口：超时/失败降级）。
         * @param {string} cmd 命令名
         * @param {object} payload 入参
         * @param {number} timeoutMs 超时（默认 5s）
         * @returns {Promise<any|null>}
         */
        async _call(cmd, payload, timeoutMs = 5000) {
            if (!this._invoke) return null;
            try {
                return await this._invoke(cmd, payload);
            } catch (err) {
                console.warn(`[graph-api] ${cmd} 失败:`, err);
                return null;
            }
        }

        /**
         * 图构建状态。
         * @param {string} dirPath
         * @returns {Promise<{schema_version?:number,node_count?:number,edge_count?:number,building?:boolean,progress_pct?:number}|null>}
         */
        status(dirPath) {
            return this._call(GRAPH_COMMANDS.STATUS, { dirPath });
        }

        /**
         * 邻域查询（L1/L2 数据源）：BFS + 扇出截断。
         * @param {string} dirPath
         * @param {object} opts { nodeId, depth, maxNodes, maxEdges, relations?, weightMin? }
         * @returns {Promise<{nodes:Array,edges:Array,truncated:boolean}|null>}
         */
        async related(dirPath, opts = {}) {
            // 无 nodeId 时邻域查询无意义 → 回退 L0 概览（全图，供初始渲染）
            if (!opts.nodeId) {
                return this.overview(dirPath, opts.maxNodes);
            }
            const res = await this._call(GRAPH_COMMANDS.RELATED, {
                dirPath,
                nodeId: opts.nodeId,
                depth: opts.depth ?? 2,
                maxNodes: opts.maxNodes ?? 200,
                maxEdges: opts.maxEdges ?? 400,
                ...(opts.relations ? { relations: opts.relations } : {}),
                ...(opts.weightMin != null ? { weightMin: opts.weightMin } : {}),
            });
            if (res) return res;
            return this.isMock ? mockOverview() : null;
        }

        /**
         * 单节点增量展开（点击展开二跳）。
         * @param {string} dirPath
         * @param {string} nodeId
         * @param {number} depth
         * @returns {Promise<{nodes:Array,edges:Array}|null>}
         */
        async expand(dirPath, nodeId, depth = 1) {
            const res = await this._call(GRAPH_COMMANDS.EXPAND, { dirPath, nodeId, depth });
            if (res) return res;
            return this.isMock ? { nodes: [], edges: [] } : null;
        }

        /**
         * 节点搜索。
         * @param {string} dirPath
         * @param {string} keyword
         * @param {number} limit
         * @returns {Promise<Array|null>}
         */
        search(dirPath, keyword, limit = 20) {
            return this._call(GRAPH_COMMANDS.SEARCH, { dirPath, keyword, limit });
        }

        /**
         * 聚合概览图（L0 数据源）。
         * @param {string} dirPath
         * @param {number} maxNodes
         * @returns {Promise<{nodes:Array,edges:Array}|null>}
         */
        async overview(dirPath, maxNodes = 5000) {
            const res = await this._call(GRAPH_COMMANDS.OVERVIEW, { dirPath, maxNodes });
            if (res) return res;
            return this.isMock ? mockOverview() : null;
        }

        /**
         * 图统计。
         * @param {string} dirPath
         * @returns {Promise<{by_type?:object,top_degree?:Array,last_built_at?:number}|null>}
         */
        stats(dirPath) {
            return this._call(GRAPH_COMMANDS.STATS, { dirPath });
        }

        /**
         * 全库实体抽取（Level 1 规则；同步执行，可能耗时）。
         * @param {string} dirPath
         * @returns {Promise<number|null>} 抽取的实体候选数
         */
        extractEntities(dirPath) {
            return this._call(GRAPH_COMMANDS.EXTRACT_ENTITIES, { dirPath }, 30000);
        }

        /**
         * 记录一条经验事件（Experience Brain）。
         * @param {string} dirPath
         * @param {object} event { id, source, title, body, file_path?, created_at }
         */
        experienceRecord(dirPath, event) {
            return this._call(GRAPH_COMMANDS.EXPERIENCE_RECORD, { dirPath, event });
        }

        /**
         * 「类似问题」检索（Experience Brain）。
         * @param {string} dirPath
         * @param {string} problem
         * @param {number} limit
         * @returns {Promise<Array|null>} ExperienceHit[]
         */
        experienceSearch(dirPath, problem, limit = 10) {
            return this._call(GRAPH_COMMANDS.EXPERIENCE_SEARCH, { dirPath, problem, limit });
        }

        /**
         * 全部经验事件列表。
         * @param {string} dirPath
         * @returns {Promise<Array|null>} ExperienceEvent[]
         */
        experienceEvents(dirPath) {
            return this._call(GRAPH_COMMANDS.EXPERIENCE_EVENTS, { dirPath });
        }
    }

    // ─── 对外暴露 ───
    window.GraphApiClient = GraphApiClient;
})();
