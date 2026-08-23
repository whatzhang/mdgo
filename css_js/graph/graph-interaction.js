/**
 * ===== 图谱交互控制器（css_js/graph/graph-interaction.js） =====
 * 【职责】用户交互编排：点选节点 → 详情卡 + 高亮；双击/展开按钮 → 二跳展开；
 *        相机缩放 → LOD 联动；搜索 → 聚焦。交互层只发指令（store/api），不直接操作 DOM 细节。
 *
 * 【依赖注入】{ store, api, renderer, panel, layout, model }
 *   - 与渲染分离（S）：interaction 不感知 Sigma 内部，只调用 renderer 公开方法。
 */
(function () {
    'use strict';

    /** 相机缩放阈值 → LOD 层级（PRD §40：<0.5 聚类 → 0.5~1 簇+核心 → 1~2 重要 → 2~4 全量 → >4 细粒度 chunk） */
    const ZOOM_LOD_THRESHOLDS = { L0: 0.5, L1: 1.0, L2: 2.0, L3: 4.0 };

    class GraphInteraction {
        /**
         * @param {{ store: object, api: object, renderer: object, panel: object, layout: object, model: object }} deps
         */
        constructor({ store, api, renderer, panel, layout, model }) {
            this.store = store;
            this.api = api;
            this.renderer = renderer;
            this.panel = panel;
            this.layout = layout;
            this.model = model;
            this._unsubs = [];
        }

        /** 挂载全部交互监听（renderer 就绪后调用） */
        mount() {
            // 节点点击 → 选中 + 详情
            this._unsubs.push(
                this.renderer.onNodeClick((nodeId) => this.onNodeClick(nodeId))
            );
            // 相机缩放 → LOD 联动
            this._unsubs.push(
                this.renderer.onCamera(() => this._syncLodFromZoom())
            );
        }

        /** 节点点击处理：簇节点 → 展开簇子图；普通节点 → 选中 + 详情 */
        async onNodeClick(nodeId) {
            const node = this.store.getNode(nodeId);
            if (!node) return;
            // 簇超节点：展开成员子图（PRD §14.2/§39 Cluster 展开）
            if (node.type === 'cluster') {
                await this.expandCluster(nodeId);
                return;
            }
            this.store.selectNode(nodeId);
            this.renderer.setHighlight(nodeId, Array.from(this.store.data.neighbors(nodeId)));
            this.panel.showNodeDetail(node);
            // 未展开过 → 自动展开一跳（渐进加载核心）
            if (!this.store.isExpanded(nodeId)) {
                await this.expandNode(nodeId);
            }
        }

        /** 展开知识簇：加载簇成员 + 簇内边，聚类布局重排（PRD §11/§39） */
        async expandCluster(clusterId) {
            const dirPath = this.store.state.dirPath;
            if (!dirPath) return;
            const cluster = this.store.getCluster(clusterId);
            this.store.set({ loading: true });
            try {
                const res = await this.api.clusterSubgraph(dirPath, clusterId, 800);
                if (res && res.nodes && res.nodes.length) {
                    // 成员节点标记 clusterId（聚类布局分组依据）
                    res.nodes.forEach((n) => {
                        n.meta = { ...(typeof n.meta === 'object' ? n.meta : {}), clusterId };
                    });
                    this.store.loadData(res);
                    this.store.setClusterId(clusterId);
                    // 展开后提升 LOD 到 FULL：CLUSTERS 层级会过滤掉全部成员（可见性闭环）
                    this.store.set({ lod: this.model.LOD.FULL });
                    this.panel.updateLodBadge(this.model.LOD.FULL);
                    this.renderer.setData(this.store.data, {
                        focusNodeId: clusterId,
                        lod: this.store.state.lod,
                        visibleIds: () => this.store.visibleNodeIds(),
                    });
                    // 聚类布局：簇心 + 成员环绕
                    this.applyLayout('cluster').catch((e) => {
                        console.warn('[graph-interaction] 聚类布局失败:', e);
                    });
                    // 聚焦簇心（成员可见 + 相机到位）
                    this.renderer.focusNode(clusterId);
                    if (cluster) this.panel.showNodeDetail(cluster);
                } else {
                    this.panel.notify('该知识簇暂无节点', 'warn');
                }
            } catch (e) {
                this.store.set({ lastError: String(e) });
                this.panel.notify('簇展开失败: ' + String(e), 'warn');
            } finally {
                this.store.set({ loading: false });
            }
        }

        /** 展开节点邻域（二跳渐进加载 + 文档内容层 chunk/section） */
        async expandNode(nodeId) {
            const dirPath = this.store.state.dirPath;
            if (!dirPath || this.store.isExpanded(nodeId)) return;
            this.store.set({ loading: true });
            try {
                // 邻域（1 跳）+ 文档内容层（chunk/section，L4 细粒度数据源）
                const [res, contentRes] = await Promise.all([
                    this.api.expand(dirPath, nodeId),
                    this.api.chunks(dirPath, nodeId).catch(() => null),
                ]);
                if (res && (res.nodes?.length || res.edges?.length)) {
                    this.store.loadData(res);
                    if (contentRes && (contentRes.nodes?.length || contentRes.edges?.length)) {
                        this.store.loadData(contentRes);
                    }
                    this.store.markExpanded(nodeId);
                    // 重绘（含新节点，应用类型过滤）
                    this.renderer.setData(this.store.data, {
                        focusNodeId: this.store.state.focusNodeId,
                        lod: this.store.state.lod,
                        visibleIds: () => this.store.visibleNodeIds(),
                    });
                    this.panel.refreshNodeDetail(nodeId);
                }
            } catch (e) {
                this.store.set({ lastError: String(e) });
            } finally {
                this.store.set({ loading: false });
            }
        }

        /** 过滤变化（分类/关系）→ 仅重渲染当前数据（不发请求） */
        applyFilters() {
            this.renderer.setData(this.store.data, {
                focusNodeId: this.store.state.focusNodeId,
                lod: this.store.state.lod,
                visibleIds: () => this.store.visibleNodeIds(),
            });
        }

        /** 手动刷新：重载当前 LOD 数据（类型过滤变化 / 按钮触发） */
        async refresh() {
            const dirPath = this.store.state.dirPath;
            if (!dirPath) return;
            this.store.set({ loading: true });
            try {
                // 焦点节点可能已失效（图重建/数据变化）：先校验存在性，否则回退首节点/全图
                let seedId = this.store.state.focusNodeId;
                if (seedId && !this.store.getNode(seedId)) seedId = null;
                if (!seedId) seedId = this.store.data.nodes.keys().next().value;
                const res = await this.api.related(dirPath, {
                    nodeId: seedId,
                    depth: 2,
                });
                if (res) {
                    this.store.clearData();
                    this.store.loadData(res);
                    // 焦点节点在 reload 后若仍存在则保留高亮，否则清除
                    const focus = this.store.getNode(this.store.state.focusNodeId)
                        ? this.store.state.focusNodeId
                        : null;
                    if (!focus) this.store.set({ focusNodeId: null });
                    this.renderer.setData(this.store.data, {
                        focusNodeId: focus,
                        lod: this.store.state.lod,
                        visibleIds: () => this.store.visibleNodeIds(),
                    });
                    this.panel.renderTypeFilters();
                }
            } catch (e) {
                this.store.set({ lastError: String(e) });
            } finally {
                this.store.set({ loading: false });
            }
        }

        /** 搜索 → 聚焦首个命中节点 */
        async search(keyword) {
            if (!keyword.trim()) return 0;
            const dirPath = this.store.state.dirPath;
            const hits = await this.api.search(dirPath, keyword.trim(), 20);
            if (!hits || hits.length === 0) {
                this.panel.notify('未找到匹配节点', 'warn');
                return 0;
            }
            const node = hits[0];
            // 确保节点已装载：未装载则通过邻域查询拉取
            if (!this.store.getNode(node.id)) {
                const rel = await this.api.related(dirPath, { nodeId: node.id, depth: 1 });
                if (rel) this.store.loadData(rel);
            }
            this.store.selectNode(node.id);
            this.renderer.setHighlight(node.id, Array.from(this.store.data.neighbors(node.id)));
            this.renderer.focusNode(node.id);
            this.panel.showNodeDetail(this.store.getNode(node.id));
            return hits.length;
        }

        /**
         * 定位并聚焦任意节点（GraphRAG 证据跳转 / 推荐等外部入口）。
         * chunk/section 目标自动提升 LOD 到细粒度（否则内容层节点被 LOD 过滤不可见）。
         * @param {string} nodeId
         * @returns {Promise<boolean>} 是否定位成功
         */
        async locateNode(nodeId) {
            const dirPath = this.store.state.dirPath;
            if (!dirPath || !nodeId) return false;
            let node = this.store.getNode(nodeId);
            if (!node) {
                // 未装载：邻域查询拉取（BFS 含目标节点自身）
                try {
                    const rel = await this.api.related(dirPath, { nodeId, depth: 1 });
                    if (!rel || !rel.nodes || !rel.nodes.length) {
                        this.panel.notify('该节点不在当前图谱中（可能已重建）', 'warn');
                        return false;
                    }
                    this.store.loadData(rel);
                    node = this.store.getNode(nodeId);
                } catch (e) {
                    this.panel.notify('定位失败: ' + String(e), 'warn');
                    return false;
                }
            }
            if (!node) {
                this.panel.notify('该节点不在当前图谱中（可能已重建）', 'warn');
                return false;
            }
            // chunk/section 属 L4 内容层：提升 LOD 到 DETAIL 保证可见
            if (node.type === 'chunk' || node.type === 'section') {
                this.store.set({ lod: this.model.LOD.DETAIL });
                this.panel.updateLodBadge(this.model.LOD.DETAIL);
                this.renderer.setData(this.store.data, {
                    focusNodeId: nodeId,
                    lod: this.store.state.lod,
                    visibleIds: () => this.store.visibleNodeIds(),
                });
            }
            this.store.selectNode(nodeId);
            this.renderer.setHighlight(nodeId, Array.from(this.store.data.neighbors(nodeId)));
            this.renderer.focusNode(nodeId);
            this.panel.showNodeDetail(node);
            return true;
        }

        /** 切换布局预设（记住用户选择，画布视图加载时沿用；PRD §10） */
        async applyLayout(preset) {
            if (!this.layout.supports(preset)) {
                this.panel.notify(`布局 ${preset} 不可用（引擎未安装）`, 'warn');
                return;
            }
            this.store.set({ layoutPreset: preset });
            const positions = await this.layout.apply(this.store.data, preset);
            this.renderer.applyPositions(positions);
        }

        /** 相机缩放 → LOD 联动（PRD §40 阈值）。
         *  文档关系视图（无簇节点）不参与 LOD 过滤：降级到 CLUSTERS/CORE 会把整图清空。 */
        _syncLodFromZoom() {
            const camera = this.renderer.getCameraRatio && this.renderer.getCameraRatio();
            if (camera == null) return;
            let target;
            if (camera < ZOOM_LOD_THRESHOLDS.L0) target = this.model.LOD.CLUSTERS;
            else if (camera < ZOOM_LOD_THRESHOLDS.L1) target = this.model.LOD.CORE;
            else if (camera < ZOOM_LOD_THRESHOLDS.L2) target = this.model.LOD.IMPORTANT;
            else if (camera < ZOOM_LOD_THRESHOLDS.L3) target = this.model.LOD.FULL;
            else target = this.model.LOD.DETAIL;
            // 无簇节点的视图（文档关系）：不按 LOD 过滤（否则 degree<3 的叶子节点全部隐藏）
            let hasClusterNode = false;
            this.store.data.nodes.forEach((n) => { if (n.type === 'cluster') hasClusterNode = true; });
            if (!hasClusterNode) target = this.model.LOD.FULL;
            if (target !== this.store.state.lod) {
                this.renderer.setLod(target);
                this.panel.updateLodBadge(target);
            }
        }

        /** 工具栏：适应窗口 / 缩放 / 聚焦 / 重置（PRD §44） */
        fitView() { this.renderer.fitView(); }
        zoomIn() { this.renderer.zoomBy(1.3); }
        zoomOut() { this.renderer.zoomBy(1 / 1.3); }
        resetView() { this.renderer.resetView(); }

        /** 清空选中 */
        clearSelection() {
            this.store.clearSelection();
            this.renderer.clearHighlight();
            this.panel.hideNodeDetail();
        }

        /** 销毁：解绑全部监听 */
        dispose() {
            this._unsubs.forEach((u) => { try { u(); } catch (e) { /* ignore */ } });
            this._unsubs = [];
        }
    }

    // ─── 对外暴露 ───
    window.GraphInteraction = GraphInteraction;
})();
