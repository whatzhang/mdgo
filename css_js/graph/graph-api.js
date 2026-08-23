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
        CLUSTERS: 'graph_clusters',
        CLUSTER: 'graph_cluster',
        CLUSTER_SUBGRAPH: 'graph_cluster_subgraph',
        REBUILD_CLUSTERS: 'graph_rebuild_clusters',
        VERSION: 'graph_version',
        PATH: 'graph_path',
        COMMON_NEIGHBORS: 'graph_common_neighbors',
        SUBGRAPH: 'graph_subgraph',
        AI_EXTRACT: 'graph_ai_extract',
        AI_ENQUEUE_ALL: 'graph_ai_enqueue_all',
        AI_SUMMARIZE: 'graph_ai_summarize_clusters',
        AI_CANDIDATES: 'graph_ai_candidates',
        AI_CONFIRM: 'graph_ai_confirm',
        AI_REJECT: 'graph_ai_reject',
        AI_GAPS: 'graph_ai_gaps',
        AI_CONFLICTS: 'graph_ai_conflicts',
        AI_DUPLICATES: 'graph_ai_duplicates',
        QUERY: 'graph_query',
        RECOMMEND: 'graph_recommend',
        FAVORITE: 'graph_favorite',
        FAVORITES: 'graph_favorites',
        EVOLUTION: 'graph_evolution',
        METRICS: 'graph_metrics',
        RECLUSTER: 'graph_recluster',
        MEMORY_PREFERENCES: 'graph_memory_preferences',
        BUILD_CHUNKS: 'graph_build_chunks',
        CHUNKS: 'graph_chunks',
        CHUNK_SIMILARITY: 'graph_chunk_similarity',
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
         * 单节点增量展开（后端固定 1 跳；graph_expand 无 depth 参数，勿传）。
         * @param {string} dirPath
         * @param {string} nodeId
         * @returns {Promise<{nodes:Array,edges:Array}|null>}
         */
        async expand(dirPath, nodeId) {
            const res = await this._call(GRAPH_COMMANDS.EXPAND, { dirPath, nodeId });
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
         * @returns {Promise<{by_type?:object,top_degree?:Array,last_built_at?:number,graph_version?:number,cluster_count?:number}|null>}
         */
        stats(dirPath) {
            return this._call(GRAPH_COMMANDS.STATS, { dirPath });
        }

        /**
         * 图版本（每次图变更 +1；前端缓存失效依据，PRD §73）。
         * @param {string} dirPath
         * @returns {Promise<number|null>}
         */
        version(dirPath) {
            return this._call(GRAPH_COMMANDS.VERSION, { dirPath });
        }

        /**
         * 全部知识簇（L0 聚合单元；含 links 簇间关系 + top_files）。
         * @param {string} dirPath
         * @param {number} limit
         * @returns {Promise<Array|null>} GraphCluster[]
         */
        clusters(dirPath, limit = 200) {
            return this._call(GRAPH_COMMANDS.CLUSTERS, { dirPath, limit });
        }

        /**
         * 单聚类详情。
         * @returns {Promise<object|null>} GraphCluster
         */
        cluster(dirPath, clusterId) {
            return this._call(GRAPH_COMMANDS.CLUSTER, { dirPath, clusterId });
        }

        /**
         * 聚类子图（成员 + 簇内边；Cluster 展开数据源）。
         * @returns {Promise<{nodes:Array,edges:Array}|null>}
         */
        clusterSubgraph(dirPath, clusterId, maxNodes = 500) {
            return this._call(GRAPH_COMMANDS.CLUSTER_SUBGRAPH, { dirPath, clusterId, maxNodes });
        }

        /**
         * 手动重算聚类（build 后已自动；供「重新聚类」按钮）。
         * @returns {Promise<number|null>} 簇数量
         */
        rebuildClusters(dirPath) {
            return this._call(GRAPH_COMMANDS.REBUILD_CLUSTERS, { dirPath });
        }

        /**
         * 两节点最短路径（PRD §24 find_path）。
         * @returns {Promise<{found:boolean,path_ids:Array,nodes:Array,edges:Array}|null>}
         */
        path(dirPath, source, target, maxDepth = 6) {
            return this._call(GRAPH_COMMANDS.PATH, { dirPath, source, target, maxDepth });
        }

        /**
         * 两节点共同邻居（PRD §24 find_common_neighbors）。
         * @returns {Promise<{nodes:Array,edges:Array}|null>}
         */
        commonNeighbors(dirPath, a, b) {
            return this._call(GRAPH_COMMANDS.COMMON_NEIGHBORS, { dirPath, a, b });
        }

        /**
         * 子图查询（BFS 深度扩展；PRD §24 get_subgraph）。
         * @returns {Promise<{nodes:Array,edges:Array}|null>}
         */
        subgraph(dirPath, nodeId, depth = 2, maxNodes = 200, maxEdges = 400) {
            return this._call(GRAPH_COMMANDS.SUBGRAPH, { dirPath, nodeId, depth, maxNodes, maxEdges });
        }

        // ─── AI 层（P1/P2） ───

        /** AI 实体关系抽取（LLM；limit 控制批量规模） */
        aiExtract(dirPath, nodeId = null, limit = 10) {
            return this._call(GRAPH_COMMANDS.AI_EXTRACT, { dirPath, nodeId, limit }, 120000);
        }

        /**
         * 全库 AI 重新入队（D4）：按最新重要度重排队列（done 不重复、failed 重试），
         * 触发后台 worker 处理。返回参与入队/更新的文档数。
         * @param {string} dirPath
         * @returns {Promise<number|null>}
         */
        aiEnqueueAll(dirPath) {
            return this._call(GRAPH_COMMANDS.AI_ENQUEUE_ALL, { dirPath });
        }

        /** AI 簇摘要（生成 description + tags） */
        aiSummarizeClusters(dirPath, limit = 20) {
            return this._call(GRAPH_COMMANDS.AI_SUMMARIZE, { dirPath, limit }, 300000);
        }

        /** AI 候选关系列表（PRD §27-28） */
        aiCandidates(dirPath, status = null, limit = 100) {
            return this._call(GRAPH_COMMANDS.AI_CANDIDATES, { dirPath, status, limit });
        }

        /** 确认候选关系（落正式边） */
        aiConfirm(dirPath, candidateId) {
            return this._call(GRAPH_COMMANDS.AI_CONFIRM, { dirPath, candidateId });
        }

        /** 拒绝候选关系 */
        aiReject(dirPath, candidateId) {
            return this._call(GRAPH_COMMANDS.AI_REJECT, { dirPath, candidateId });
        }

        /** 知识缺口检测（PRD §52） */
        aiGaps(dirPath, clusterId) {
            return this._call(GRAPH_COMMANDS.AI_GAPS, { dirPath, clusterId }, 60000);
        }

        /** 知识冲突检测（PRD §54） */
        aiConflicts(dirPath) {
            return this._call(GRAPH_COMMANDS.AI_CONFLICTS, { dirPath }, 120000);
        }

        /** 知识重复检测（PRD §53） */
        aiDuplicates(dirPath) {
            return this._call(GRAPH_COMMANDS.AI_DUPLICATES, { dirPath }, 30000);
        }

        /**
         * GraphRAG 图谱问答（PRD §22-23）：实体检测 → 图扩展 + 混合检索 → LLM 回答 + 证据。
         * @param {string} dirPath
         * @param {string} question
         * @param {number} topK 混合检索 top-k（默认 20）
         * @returns {Promise<{answer:string,entities:Array,evidence:Array,related:Array,used_llm:boolean}|null>}
         *          evidence[].chunk_id 可定位到语义块（L4 内容层）
         */
        query(dirPath, question, topK = 20) {
            return this._call(GRAPH_COMMANDS.QUERY, { dirPath, query: question, topK }, 120000);
        }

        /** 基于图的推荐（PRD §51） */
        recommend(dirPath, nodeId, limit = 8) {
            return this._call(GRAPH_COMMANDS.RECOMMEND, { dirPath, nodeId, limit }, 30000);
        }

        /** 收藏 / 取消收藏（PRD §50） */
        favorite(dirPath, nodeId, on) {
            return this._call(GRAPH_COMMANDS.FAVORITE, { dirPath, nodeId, on });
        }

        /** 收藏列表 */
        favorites(dirPath, limit = 100) {
            return this._call(GRAPH_COMMANDS.FAVORITES, { dirPath, limit });
        }

        /** 知识演化统计 + AI 洞察（PRD §30-31） */
        evolution(dirPath, withAi = true) {
            return this._call(GRAPH_COMMANDS.EVOLUTION, { dirPath, withAi }, 60000);
        }

        /** 图可观测性指标（PRD §74） */
        metrics(dirPath) {
            return this._call(GRAPH_COMMANDS.METRICS, { dirPath });
        }

        /** 重新聚类（directory | embedding；PRD §11.1） */
        recluster(dirPath, mode) {
            return this._call(GRAPH_COMMANDS.RECLUSTER, { dirPath, mode }, 300000);
        }

        /** 我的知识偏好列表（Memory Graph；PRD §60） */
        memoryPreferences(dirPath) {
            return this._call(GRAPH_COMMANDS.MEMORY_PREFERENCES, { dirPath });
        }

        /** 重建 chunk/section 内容层（幂等；build 后已自动，手动入口） */
        buildChunks(dirPath) {
            return this._call(GRAPH_COMMANDS.BUILD_CHUNKS, { dirPath }, 300000);
        }

        /** 某文档的内容节点子图（chunk/section + 层级边；L4 数据源） */
        chunks(dirPath, nodeId) {
            return this._call(GRAPH_COMMANDS.CHUNKS, { dirPath, nodeId }, 60000);
        }

        /** Chunk 相似边构建（SIMILAR_TO；需本地 embedding 模型） */
        chunkSimilarity(dirPath, topK = 3) {
            return this._call(GRAPH_COMMANDS.CHUNK_SIMILARITY, { dirPath, topK }, 300000);
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
