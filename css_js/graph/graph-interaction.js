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

    /** 相机缩放阈值 → LOD 切换（L0 概览 / L1 局部 / L2 焦点） */
    const ZOOM_LOD_THRESHOLDS = { L0: 0.08, L1: 0.3 };

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

        /** 节点点击处理 */
        async onNodeClick(nodeId) {
            const node = this.store.getNode(nodeId);
            if (!node) return;
            this.store.selectNode(nodeId);
            this.renderer.setHighlight(nodeId, Array.from(this.store.data.neighbors(nodeId)));
            this.panel.showNodeDetail(node);
            // 未展开过 → 自动展开一跳（渐进加载核心）
            if (!this.store.isExpanded(nodeId)) {
                await this.expandNode(nodeId);
            }
        }

        /** 展开节点邻域（二跳渐进加载） */
        async expandNode(nodeId) {
            const dirPath = this.store.state.dirPath;
            if (!dirPath || this.store.isExpanded(nodeId)) return;
            this.store.set({ loading: true });
            try {
                const res = await this.api.expand(dirPath, nodeId, 1);
                if (res && (res.nodes?.length || res.edges?.length)) {
                    this.store.loadData(res);
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

        /** 切换布局预设 */
        async applyLayout(preset) {
            if (!this.layout.supports(preset)) {
                this.panel.notify(`布局 ${preset} 不可用（引擎未安装）`, 'warn');
                return;
            }
            const positions = await this.layout.apply(this.store.data, preset);
            this.renderer.applyPositions(positions);
        }

        /** 相机缩放 → LOD 联动 */
        _syncLodFromZoom() {
            const camera = this.renderer.getCameraRatio && this.renderer.getCameraRatio();
            if (camera == null) return;
            let target;
            if (camera < ZOOM_LOD_THRESHOLDS.L0) target = this.model.LOD.OVERVIEW;
            else if (camera < ZOOM_LOD_THRESHOLDS.L1) target = this.model.LOD.LOCAL;
            else target = this.model.LOD.FOCUS;
            if (target !== this.store.state.lod) {
                this.renderer.setLod(target);
                this.panel.updateLodBadge(target);
            }
        }

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
