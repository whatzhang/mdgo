/**
 * ===== 图谱视图控制器（css_js/graph/graph-views.js） =====
 * 【职责】顶部 5 个一级视图（PRD §4/§6.2）：
 *   - 全局视图（global）：Cluster + Core Entity + Major Relationship（L0 聚合）
 *   - 文档关系（document）：真实文档结构关系（Document Graph）
 *   - 主题聚类（topics）：知识簇卡片列表
 *   - 领域地图（domains）：领域/知识量分布
 *   - 时间轴（timeline）：知识形成与演化时间线
 * global/document 渲染到 Sigma 画布；topics/domains/timeline 渲染到 HTML 面板。
 * 【依赖注入】{ store, api, renderer, layout, interaction, panel }
 */
(function () {
    'use strict';

    /** 视图元信息（PRD §6.2） */
    const VIEWS = [
        { key: 'global', label: '全局视图' },
        { key: 'document', label: '文档关系' },
        { key: 'topics', label: '主题聚类' },
        { key: 'domains', label: '领域地图' },
        { key: 'timeline', label: '时间轴' },
    ];

    class GraphViews {
        /**
         * @param {{ store: object, api: object, renderer: object, layout: object, interaction: object, panel: object }} deps
         */
        constructor({ store, api, renderer, layout, interaction, panel }) {
            this.store = store;
            this.api = api;
            this.renderer = renderer;
            this.layout = layout;
            this.interaction = interaction;
            this.panel = panel;
            this._panelEl = null; // #kg-view-panel
        }

        static definitions() { return VIEWS.slice(); }

        /** 绑定顶部导航（分段控件）+ ⌘K 搜索快捷键 */
        init() {
            const nav = document.getElementById('kg-views');
            if (nav) {
                nav.addEventListener('click', (e) => {
                    const btn = e.target.closest('[data-view]');
                    if (btn) this.switchView(btn.dataset.view);
                });
            }
            // ⌘K / Ctrl+K 聚焦搜索（PRD §6.3）
            document.addEventListener('keydown', (e) => {
                if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
                    e.preventDefault();
                    const input = document.getElementById('kg-search-input');
                    if (input) input.focus();
                }
            });
        }

        /** 视图切换入口 */
        async switchView(view) {
            if (!VIEWS.some((v) => v.key === view)) view = 'global';
            this.store.set({ view });
            // 顶部导航选中态（PRD §6.2：浅蓝背景 + 底部指示线）
            document.querySelectorAll('#kg-views [data-view]').forEach((btn) => {
                btn.classList.toggle('active', btn.dataset.view === view);
            });
            try {
                switch (view) {
                    case 'global': return await this.loadGlobal();
                    case 'document': return await this.loadDocument();
                    case 'topics': return await this.renderTopics();
                    case 'domains': return await this.renderDomains();
                    case 'timeline': return await this.renderTimeline();
                }
            } catch (e) {
                console.warn('[graph-views] 视图加载失败:', e);
                this.panel.notify('视图加载失败: ' + String(e), 'warn');
            }
        }

        // ─── 画布视图 ───

        /** 全局视图：Cluster 聚合图（PRD §4.1/§11/§12 L0） */
        async loadGlobal() {
            this._showCanvas();
            const dirPath = this.store.state.dirPath;
            const status = await this.api.status(dirPath);
            if (!status || status.node_count === 0) {
                // 浏览器预览（mock）：给出一组演示簇，保证页面可独立预览
                if (this.api.isMock) {
                    this._loadMockClusters();
                    return;
                }
                this._showEmpty(status);
                return;
            }
            this.panel.setLoading(true);
            try {
                const clusters = (await this.api.clusters(dirPath, 200)) || [];
                this.store.setClusters(clusters);
                if (!clusters.length) {
                    // 无聚类（图已建但未聚类）→ 触发一次重算
                    await this.api.rebuildClusters(dirPath);
                    const again = (await this.api.clusters(dirPath, 200)) || [];
                    this.store.setClusters(again);
                }
                this._renderClusterGraph();
            } finally {
                this.panel.setLoading(false);
            }
        }

        /** mock 演示簇（无 Tauri 浏览器预览用） */
        _loadMockClusters() {
            const mockClusters = [
                { id: 'cluster:docs', name: 'docs', node_count: 42, edge_count: 68, description: '演示聚类：文档目录', algorithm: 'directory', links: [{ source: 'cluster:docs', target: 'cluster:src', count: 12 }], top_files: [{ name: 'guide.md', path: 'docs/guide.md' }, { name: 'rag.md', path: 'docs/rag.md' }] },
                { id: 'cluster:src', name: 'src', node_count: 36, edge_count: 54, description: '演示聚类：源码目录', algorithm: 'directory', links: [{ source: 'cluster:src', target: 'cluster:docs', count: 12 }], top_files: [{ name: 'main.rs', path: 'src/main.rs' }] },
                { id: 'cluster:config', name: 'config', node_count: 8, edge_count: 6, description: '演示聚类：配置目录', algorithm: 'directory', links: [], top_files: [] },
            ];
            this.store.setClusters(mockClusters);
            this._renderClusterGraph();
        }

        /** 文档关系视图：Document Graph（PRD §4.2） */
        async loadDocument() {
            this._showCanvas();
            const dirPath = this.store.state.dirPath;
            this.panel.setLoading(true);
            try {
                let res = await this.api.overview(dirPath, 3000);
                if ((!res || !res.nodes || !res.nodes.length) && this.api.isMock) {
                    // 浏览器预览：mock 文档图
                    res = await this.api.related(dirPath, { depth: 2 });
                }
                if (res && res.nodes && res.nodes.length) {
                    this.store.clearData();
                    this.store.loadData(res);
                    this.store.set({ lod: this.store.model.LOD.FULL, clusterId: null });
                    this.renderer.setData(this.store.data, {
                        focusNodeId: null,
                        lod: this.store.state.lod,
                        visibleIds: () => this.store.visibleNodeIds(),
                    });
                    // 布局：沿用用户选择的模式（默认力导向）
                    await this.interaction.applyLayout(this.store.state.layoutPreset || 'force');
                } else {
                    this._showEmpty(null);
                }
            } finally {
                this.panel.setLoading(false);
            }
        }

        /** 把 clusters 渲染为簇超节点图（全局视图核心） */
        _renderClusterGraph() {
            const clusters = this.store.clusters;
            if (!clusters.length) {
                this._showEmpty(null);
                return;
            }
            // 簇超节点 + 簇间关系（PRD §13 Cluster Node / §15 聚合边）
            const nodes = clusters.map((c) => ({
                id: c.id,
                type: 'cluster',
                name: c.name,
                degree: (c.links || []).length,
                meta: { clusterId: c.id, nodeCount: c.node_count },
            }));
            const edges = [];
            clusters.forEach((c) => {
                (c.links || []).forEach((l) => {
                    if (l.source === c.id) {
                        edges.push({ source: l.source, target: l.target, relation: 'REFERENCES', weight: l.count });
                    }
                });
            });
            this.store.clearData();
            this.store.loadData({ nodes, edges });
            this.store.set({ lod: this.store.model.LOD.CLUSTERS, clusterId: null });
            this.renderer.setData(this.store.data, {
                focusNodeId: null,
                lod: this.store.state.lod,
                visibleIds: () => this.store.visibleNodeIds(),
            });
            // 聚类布局（簇间圆圈排布；PRD §11.2；沿用用户选择，默认 cluster）
            this.interaction.applyLayout(this.store.state.layoutPreset || 'cluster').catch((e) => {
                console.warn('[graph-views] 聚类布局失败:', e);
            });
        }

        // ─── HTML 面板视图 ───

        /** 主题聚类：知识簇卡片列表（PRD §4.3/§29） */
        async renderTopics() {
            const dirPath = this.store.state.dirPath;
            this._showPanel();
            this.panel.setLoading(true);
            try {
                let clusters = this.store.clusters;
                if (!clusters.length) {
                    clusters = (await this.api.clusters(dirPath, 200)) || [];
                    this.store.setClusters(clusters);
                }
                const el = this._panelEl;
                if (!clusters.length) {
                    el.innerHTML = '<div class="kg-panel-empty">暂无知识聚类 —— 先构建知识图谱。</div>';
                    return;
                }
                const html = clusters.map((c, i) => `
                    <div class="kg-topic-card" data-cluster="${c.id}" style="animation-delay:${Math.min(i * 30, 600)}ms">
                        <div class="kg-topic-head">
                            <span class="kg-topic-icon">◈</span>
                            <div class="kg-topic-name">${escapeHtml(c.name)}</div>
                            <div class="kg-topic-meta">${c.node_count} 节点 · ${c.edge_count} 关系</div>
                        </div>
                        <div class="kg-topic-desc">${escapeHtml(c.description || '')}</div>
                        ${(c.top_files || []).length ? `
                        <div class="kg-topic-files">
                            ${c.top_files.map((f) => `<span class="kg-tag">${escapeHtml(f.name)}</span>`).join('')}
                        </div>` : ''}
                    </div>`).join('');
                el.innerHTML = `<div class="kg-view-head">主题聚类 <small>AI 自动发现的知识主题</small></div>
                    <div class="kg-topic-grid">${html}</div>`;
                el.querySelectorAll('.kg-topic-card').forEach((card) => {
                    card.addEventListener('click', () => {
                        const clusterId = card.dataset.cluster;
                        this.store.setClusterId(clusterId);
                        this.switchView('global').then(() => {
                            // 全局视图渲染完成后展开该簇
                            setTimeout(() => this.interaction.expandCluster(clusterId), 100);
                        });
                    });
                });
            } finally {
                this.panel.setLoading(false);
            }
        }

        /** 领域地图：领域知识量分布（PRD §4.4/§13.5） */
        async renderDomains() {
            const dirPath = this.store.state.dirPath;
            this._showPanel();
            this.panel.setLoading(true);
            try {
                let clusters = this.store.clusters;
                if (!clusters.length) {
                    clusters = (await this.api.clusters(dirPath, 200)) || [];
                    this.store.setClusters(clusters);
                }
                const el = this._panelEl;
                if (!clusters.length) {
                    el.innerHTML = '<div class="kg-panel-empty">暂无领域数据 —— 先构建知识图谱。</div>';
                    return;
                }
                const max = Math.max(1, ...clusters.map((c) => c.node_count));
                const rows = clusters.map((c) => `
                    <div class="kg-domain-row" data-cluster="${c.id}">
                        <span class="kg-domain-name">${escapeHtml(c.name)}</span>
                        <div class="kg-domain-bar-track">
                            <div class="kg-domain-bar" style="width:${Math.max(4, (c.node_count / max) * 100)}%"></div>
                        </div>
                        <span class="kg-domain-count">${c.node_count}</span>
                    </div>`).join('');
                el.innerHTML = `<div class="kg-view-head">领域地图 <small>从知识体系角度理解你掌握的领域</small></div>
                    <div class="kg-domain-list">${rows}</div>`;
                el.querySelectorAll('.kg-domain-row').forEach((row) => {
                    row.addEventListener('click', () => {
                        const clusterId = row.dataset.cluster;
                        this.store.setClusterId(clusterId);
                        this.switchView('global').then(() => {
                            setTimeout(() => this.interaction.expandCluster(clusterId), 100);
                        });
                    });
                });
            } finally {
                this.panel.setLoading(false);
            }
        }

        /** 时间轴：知识形成与演化（PRD §4.5/§30-31） */
        async renderTimeline() {
            const dirPath = this.store.state.dirPath;
            this._showPanel();
            this.panel.setLoading(true);
            try {
                const el = this._panelEl;
                // 演化统计（withAi=false 避免长耗时；AI 洞察走「AI 分析」区）
                const evo = await this.api.evolution(dirPath, false);
                const nodesByMonth = evo && evo.evolution && evo.evolution.monthly_nodes
                    ? evo.evolution.monthly_nodes
                    : null;

                let sections = '';
                if (nodesByMonth && nodesByMonth.length) {
                    // 月度节点增长时间线
                    sections = nodesByMonth.slice().reverse().map(([month, count]) => `
                        <div class="kg-tl-month">
                            <div class="kg-tl-month-label">${month} <small>+${count} 节点</small></div>
                            <div class="kg-tl-bar"><div class="kg-tl-bar-fill" style="width:${Math.min(100, 20 + count * 6)}%"></div></div>
                        </div>`).join('');
                }
                // 簇月度增长（领域级演化）
                const growth = evo && evo.evolution && evo.evolution.cluster_growth
                    ? evo.evolution.cluster_growth.filter((g) => g.monthly.length).slice(0, 10)
                    : [];
                if (growth.length) {
                    sections += `<div class="kg-view-head" style="margin-top:18px">领域增长 <small>各知识簇累计节点</small></div>
                        <div class="kg-domain-list">${growth.map((g) => {
                            const total = g.monthly.reduce((s, m) => s + m[1], 0);
                            return `<div class="kg-domain-row">
                                <span class="kg-domain-name">${escapeHtml(g.cluster_name)}</span>
                                <div class="kg-domain-bar-track"><div class="kg-domain-bar" style="width:${Math.min(100, 10 + total * 2)}%"></div></div>
                                <span class="kg-domain-count">${total}</span>
                            </div>`;
                        }).join('')}</div>`;
                }
                if (!sections) {
                    // 回退：按节点 created_at 分组
                    const res = await this.api.overview(dirPath, 5000);
                    if (!res || !res.nodes || !res.nodes.length) {
                        el.innerHTML = '<div class="kg-panel-empty">暂无时间数据 —— 先构建知识图谱。</div>';
                        return;
                    }
                    const months = new Map();
                    res.nodes.forEach((n) => {
                        const t = typeof n.created_at === 'number' ? n.created_at : 0;
                        if (!t) return;
                        const d = new Date(t);
                        const key = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}`;
                        if (!months.has(key)) months.set(key, []);
                        months.get(key).push(n);
                    });
                    const keys = Array.from(months.keys()).sort().reverse();
                    sections = keys.map((key) => `
                        <div class="kg-tl-month">
                            <div class="kg-tl-month-label">${key} <small>+${months.get(key).length} 节点</small></div>
                            <div class="kg-tl-items">${months.get(key).slice(0, 8).map((n) => `
                                <div class="kg-tl-item" data-id="${escapeHtml(n.id)}">
                                    <span class="kg-tl-dot"></span>
                                    <span class="kg-tl-name">${escapeHtml(n.name)}</span>
                                    <span class="kg-tl-type">${escapeHtml(n.type || '')}</span>
                                </div>`).join('')}</div>
                        </div>`).join('');
                }
                el.innerHTML = `<div class="kg-view-head">时间轴 <small>知识如何形成与演化</small></div>
                    <div class="kg-tl">${sections || '<div class="kg-panel-empty">暂无时间数据。</div>'}</div>`;
                // 点击节点 → 文档视图聚焦
                el.querySelectorAll('.kg-tl-item').forEach((item) => {
                    item.addEventListener('click', () => {
                        const nodeId = item.dataset.id;
                        this.store.selectNode(nodeId);
                        this.switchView('document').then(() => {
                            setTimeout(() => this.interaction.onNodeClick(nodeId), 150);
                        });
                    });
                });
            } finally {
                this.panel.setLoading(false);
            }
        }

        // ─── 展示辅助 ───

        /** 切回画布视图：隐藏 HTML 面板，恢复画布与工具栏 */
        _showCanvas() {
            if (this._panelEl) this._panelEl.hidden = true;
            const canvas = document.getElementById('kg-canvas');
            if (canvas) canvas.style.display = '';
            const tb = document.getElementById('kg-canvas-toolbar');
            if (tb) tb.style.display = '';
        }

        /** 切到 HTML 面板视图（主题聚类/领域地图/时间轴/空态）。
         *  注意：#kg-view-panel 是 #kg-canvas 的子元素 —— 不能隐藏画布父节点，
         *  否则面板内容连同不可见（此前 bug：三个视图"没有任何数据"）。 */
        _showPanel() {
            if (!this._panelEl) this._panelEl = document.getElementById('kg-view-panel');
            if (!this._panelEl) return;
            this._panelEl.hidden = false;
            // 画布保持可见（面板 z-index 6 覆盖其上，背景不透明）；
            // 仅隐藏悬浮工具栏（z-index 10），避免盖在面板内容上
            const tb = document.getElementById('kg-canvas-toolbar');
            if (tb) tb.style.display = 'none';
        }

        /** 空状态（PRD §67） */
        _showEmpty(status) {
            this._showPanel();
            if (!this._panelEl) return;
            const docs = status && status.node_count ? status.node_count : '—';
            this._panelEl.innerHTML = `
                <div class="kg-empty">
                    <div class="kg-empty-title">还没有建立知识图谱</div>
                    <div class="kg-empty-desc">你的知识库中已有 <b>${docs}</b> 个知识资产</div>
                    <div class="kg-empty-desc">图谱会在索引/文件变更后自动构建；也可以点击下方按钮尝试构建。</div>
                    <button class="kg-btn kg-btn-primary" id="kg-build-cta">立即构建知识图谱</button>
                </div>`;
            const btn = document.getElementById('kg-build-cta');
            if (btn) {
                btn.addEventListener('click', async () => {
                    btn.disabled = true;
                    btn.textContent = '检测中…';
                    try {
                        const dirPath = this.store.state.dirPath;
                        // 图谱构建由主应用索引/watcher 自动完成；此处触发聚类重算并重新检测
                        const n = await this.api.rebuildClusters(dirPath);
                        this.panel.notify(n > 0 ? `已就绪：${n} 个知识簇` : '图谱构建由索引自动完成，请先在主界面完成索引后重试');
                    } catch (e) {
                        this.panel.notify('检测失败: ' + String(e), 'warn');
                    } finally {
                        btn.disabled = false;
                        btn.textContent = '立即构建知识图谱';
                        this.switchView(this.store.state.view);
                    }
                });
            }
        }
    }

    /** HTML 转义（防注入） */
    function escapeHtml(str) {
        return String(str ?? '').replace(/[&<>"']/g, (c) => ({
            '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
        }[c]));
    }

    // ─── 对外暴露 ───
    window.GraphViews = GraphViews;
})();
