/**
 * ===== 图谱面板 UI（css_js/graph/graph-panel.js） =====
 * 【职责】侧栏/详情卡/顶栏 UI 渲染与事件 —— 只做展示与用户输入收集（S），
 *        业务指令转发给 store/interaction，不直接调后端。
 *
 * 【依赖注入】{ store, interaction, model }（DOM 元素按 id 自取，模块内持有引用）
 */
(function () {
    'use strict';

    const TYPE_LABELS = {
        doc: '文档', folder: '目录', chunk: '语义块', entity: '实体',
        experience: '经验', memory: '记忆', cluster: '聚合簇',
    };

    class GraphPanel {
        /**
         * @param {{ store: object, interaction: object|null, api: object, model: object }} deps
         */
        constructor({ store, interaction, api, model }) {
            this.store = store;
            this.interaction = interaction;
            this.api = api;
            this.model = model;
            /** DOM 引用（懒取） */
            this.el = {
                graphType: document.getElementById('kg-graph-type'),
                layout: document.getElementById('kg-layout'),
                lodBadge: document.getElementById('kg-lod-badge'),
                search: document.getElementById('kg-search-input'),
                refresh: document.getElementById('kg-refresh-btn'),
                statsBtn: document.getElementById('kg-stats-btn'),
                buildStatus: document.getElementById('kg-build-status'),
                progressBar: document.getElementById('kg-progress-bar'),
                typeFilters: document.getElementById('kg-type-filters'),
                stats: document.getElementById('kg-stats'),
                detailCard: document.getElementById('kg-detail-card'),
                detailTitle: document.getElementById('kg-detail-title'),
                detailBody: document.getElementById('kg-detail-body'),
                detailActions: document.getElementById('kg-detail-actions'),
                detailClose: document.getElementById('kg-detail-close'),
                statNodes: document.getElementById('kg-stat-nodes'),
                statEdges: document.getElementById('kg-stat-edges'),
                statLod: document.getElementById('kg-stat-lod'),
                statEngine: document.getElementById('kg-stat-engine'),
            };
            this._unsubs = [];
        }

        /** 初始化：绑定控件事件 + 订阅 store */
        init() {
            const el = this.el;
            if (el.graphType) {
                el.graphType.addEventListener('change', () => {
                    // 图谱类型切换（Document/Chunk/Entity）：骨架阶段触发刷新，类型语义由后端图数据决定
                    this.interaction.refresh();
                });
            }
            if (el.layout) {
                el.layout.addEventListener('change', () => {
                    this.interaction.applyLayout(el.layout.value);
                });
            }
            if (el.search) {
                el.search.addEventListener('keydown', (e) => {
                    if (e.key === 'Enter') this.interaction.search(el.search.value);
                });
            }
            if (el.detailClose) {
                el.detailClose.addEventListener('click', () => this.interaction.clearSelection());
            }
            if (el.refresh) {
                el.refresh.addEventListener('click', () => {
                    if (this.store.state.dirPath) this.interaction.refresh();
                });
            }
            if (el.statsBtn) {
                el.statsBtn.addEventListener('click', async () => {
                    const stats = await this.api.stats(this.store.state.dirPath);
                    this.renderStats(stats);
                    this.notify('统计已刷新');
                });
            }
            // 状态订阅 → 统计/引擎态刷新
            this._unsubs.push(this.store.subscribe(() => this._refreshStats()));
            this._unsubs.push(this.store.subscribe(() => this._refreshBuildStatus()));
        }

        /** 渲染构建状态 */
        _refreshBuildStatus() {
            const s = this.store.state.buildStatus;
            const el = this.el.buildStatus;
            if (!el) return;
            if (!s) {
                el.textContent = '检测中…';
                return;
            }
            if (s.building) {
                el.textContent = `后台构建中 ${s.progress_pct ?? 0}%`;
                if (this.el.progressBar) this.el.progressBar.style.width = `${s.progress_pct ?? 0}%`;
            } else if (s.node_count > 0) {
                el.textContent = `已构建：${s.node_count} 节点 / ${s.edge_count} 边`;
            } else {
                el.textContent = '尚未构建（等待后台任务）';
            }
        }

        /** 渲染底部统计 */
        _refreshStats() {
            const st = this.store.state;
            if (this.el.statNodes) this.el.statNodes.textContent = String(this.store.data.nodeCount);
            if (this.el.statEdges) this.el.statEdges.textContent = String(this.store.data.edgeCount);
            if (this.el.statEngine) this.el.statEngine.textContent = `引擎: ${st.engineReady ? 'Sigma' : '未就绪'}`;
        }

        /** 更新 LOD 徽标 */
        updateLodBadge(lod) {
            const labels = { 0: 'L0 概览', 1: 'L1 局部', 2: 'L2 焦点' };
            if (this.el.lodBadge) this.el.lodBadge.textContent = labels[lod] || `L${lod}`;
            if (this.el.statLod) this.el.statLod.textContent = `LOD ${labels[lod] || lod}`;
        }

        /** 渲染类型过滤（按当前图数据类型动态生成 checkbox，多选联动） */
        renderTypeFilters() {
            const el = this.el.typeFilters;
            if (!el) return;
            const types = Array.from(new Set(Array.from(this.store.data.nodes.values()).map((n) => n.type)));
            const html = types.map((t) => `
                <label class="kg-filter-item">
                    <input type="checkbox" class="kg-filter-check" data-type="${t}" checked />
                    <span>${TYPE_LABELS[t] || t}</span>
                </label>`).join('');
            el.innerHTML = html;
            el.querySelectorAll('.kg-filter-check').forEach((cb) => {
                cb.addEventListener('change', () => {
                    const checked = Array.from(el.querySelectorAll('.kg-filter-check'))
                        .filter((c) => c.checked)
                        .map((c) => c.dataset.type);
                    // null = 全部可见；非空数组 = 多选白名单
                    this.store.set({ typeFilter: checked.length === types.length ? null : checked });
                    this.interaction.refresh();
                });
            });
        }

        /** 渲染统计卡（sidebar） */
        renderStats(stats) {
            const el = this.el.stats;
            if (!el) return;
            if (!stats) {
                el.innerHTML = '<div class="kg-muted">暂无统计</div>';
                return;
            }
            const byType = stats.by_type || {};
            const rows = Object.entries(byType).map(([t, c]) =>
                `<div class="kg-stat-row"><span>${TYPE_LABELS[t] || t}</span><b>${c}</b></div>`).join('');
            el.innerHTML = rows || '<div class="kg-muted">暂无统计</div>';
        }

        /** 展示节点详情卡 */
        showNodeDetail(node) {
            const el = this.el;
            if (!el.detailCard || !node) return;
            el.detailTitle.textContent = node.name;
            const meta = node.meta ? `<div class="kg-detail-meta">${JSON.stringify(node.meta)}</div>` : '';
            el.detailBody.innerHTML = `
                <div class="kg-detail-row"><span>类型</span><b>${TYPE_LABELS[node.type] || node.type}</b></div>
                <div class="kg-detail-row"><span>ID</span><code>${escapeHtml(node.id)}</code></div>
                ${node.path ? `<div class="kg-detail-row"><span>路径</span><code>${escapeHtml(node.path)}</code></div>` : ''}
                ${typeof node.degree === 'number' ? `<div class="kg-detail-row"><span>度数</span><b>${node.degree}</b></div>` : ''}
                ${meta}
                <div class="kg-detail-row"><span>邻居</span><b>${this.store.data.neighbors(node.id).size}</b></div>`;
            el.detailActions.innerHTML = `
                <button class="kg-btn kg-btn-sm" id="kg-detail-expand">展开二跳</button>
                ${node.path ? '<button class="kg-btn kg-btn-sm" id="kg-detail-open">打开文件</button>' : ''}`;
            const expandBtn = el.detailActions.querySelector('#kg-detail-expand');
            if (expandBtn) expandBtn.addEventListener('click', () => this.interaction.expandNode(node.id));
            const openBtn = el.detailActions.querySelector('#kg-detail-open');
            if (openBtn) openBtn.addEventListener('click', () => this.openFile(node));
            el.detailCard.hidden = false;
        }

        /** 刷新详情卡（展开后调用） */
        refreshNodeDetail(nodeId) {
            const node = this.store.getNode(nodeId);
            if (node && this.store.state.selectedNodeId === nodeId) this.showNodeDetail(node);
        }

        /** 隐藏详情卡 */
        hideNodeDetail() {
            if (this.el.detailCard) this.el.detailCard.hidden = true;
        }

        /** 打开文件：postMessage 通知主页面（复用现有打开链路） */
        openFile(node) {
            if (node.path && window.parent) {
                window.parent.postMessage({ type: 'graph:open-node', payload: { nodeId: node.id, path: node.path } }, '*');
            }
        }

        /** 轻提示（骨架：console + 状态栏闪示） */
        notify(msg, level = 'info') {
            console.log(`[graph-panel] [${level}]`, msg);
            const el = this.el.statLod;
            if (el && level === 'warn') {
                const prev = el.textContent;
                el.textContent = msg;
                setTimeout(() => { if (el.textContent === msg) el.textContent = prev; }, 2500);
            }
        }

        /** 销毁：解绑订阅 */
        dispose() {
            this._unsubs.forEach((u) => { try { u(); } catch (e) { /* ignore */ } });
            this._unsubs = [];
        }
    }

    /** HTML 转义（最小实现，防注入） */
    function escapeHtml(str) {
        return String(str ?? '').replace(/[&<>"']/g, (c) => ({
            '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
        }[c]));
    }

    // ─── 对外暴露 ───
    window.GraphPanel = GraphPanel;
})();
