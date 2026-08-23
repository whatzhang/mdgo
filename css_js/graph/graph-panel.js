/**
 * ===== 图谱面板 UI（css_js/graph/graph-panel.js） =====
 * 【职责】左侧面板（概览/分类过滤/关系过滤/布局模式/AI 分析）+ 右侧节点详情（四 Tab）。
 *        只做展示与用户输入收集（S），业务指令转发给 store/interaction。
 * 【PRD 对齐】§7 图谱概览 / §8 分类过滤 / §9 关系过滤 / §10 布局模式 /
 *            §16-20 节点详情四 Tab（概览/内容/关系/分析）/ §29 AI 智能分析
 */
(function () {
    'use strict';

    /** 布局模式（PRD §10：力导向/层级/放射/聚类）→ graph-layout 预设名 */
    const LAYOUT_MODES = [
        { key: 'force', label: '力导向', desc: '自由探索关系' },
        { key: 'hierarchy', label: '层级', desc: '目录/领域/知识体系' },
        { key: 'radial', label: '放射', desc: '中心节点关系' },
        { key: 'cluster', label: '聚类', desc: '簇间分离（默认推荐）' },
    ];

    /** 节点类型中文标签 */
    const TYPE_LABELS = {
        doc: '文档', folder: '目录', chunk: '语义块', section: '章节',
        entity: '实体', experience: '经验', memory: '记忆', cluster: '知识簇',
    };

    /** 关系中文标签（PRD §9 + 内容层语义关系） */
    const RELATION_LABELS = {
        CONTAINS: '包含', REFERENCES: '引用', IMPORTS: '导入', DERIVED_FROM: '派生',
        SAME_TOPIC: '相关', SOLVED_BY: '解决', IMPLEMENTED_IN: '实现于',
        VALIDATED_BY: '验证', REPLACED_BY: '替代', DEPRECATED: '废弃',
        PREFERS: '偏好', AVOIDS: '回避', USES: '使用', BELONGS_TO: '属于',
        SIMILAR_TO: '语义相似', DEPENDS_ON: '依赖',
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
            /** 视图控制器（compose 后置注入；刷新按钮 → 重跑当前视图） */
            this.views = null;
            /** 收藏节点 id 集合（懒加载缓存） */
            this._favSet = new Set();
            /** 当前详情目标 id（推荐/收藏联动） */
            this._detailTargetId = null;
            /** DOM 引用（懒取） */
            this.el = {
                // 顶部
                search: document.getElementById('kg-search-input'),
                refresh: document.getElementById('kg-refresh-btn'),
                // 左侧概览
                statNodes: document.getElementById('kg-stat-nodes'),
                statEdges: document.getElementById('kg-stat-edges'),
                overviewCats: document.getElementById('kg-overview-cats'),
                // 过滤
                typeFilters: document.getElementById('kg-type-filters'),
                relationFilters: document.getElementById('kg-relation-filters'),
                layoutModes: document.getElementById('kg-layout-modes'),
                aiAnalysis: document.getElementById('kg-ai-analysis'),
                // 状态
                buildStatus: document.getElementById('kg-build-status'),
                lodBadge: document.getElementById('kg-lod-badge'),
                statLod: document.getElementById('kg-stat-lod'),
                statEngine: document.getElementById('kg-stat-engine'),
                statVersion: document.getElementById('kg-stat-version'),
                progressBar: document.getElementById('kg-progress-bar'),
                // 详情
                detail: document.getElementById('kg-detail'),
                detailIcon: document.getElementById('kg-detail-icon'),
                detailTitle: document.getElementById('kg-detail-title'),
                detailSubtitle: document.getElementById('kg-detail-subtitle'),
                detailTabs: document.getElementById('kg-detail-tabs'),
                detailBody: document.getElementById('kg-detail-body'),
                detailClose: document.getElementById('kg-detail-close'),
                detailStar: document.getElementById('kg-detail-star'),
                // 画布
                canvasToolbar: document.getElementById('kg-canvas-toolbar'),
            };
            this._unsubs = [];
        }

        /**
         * 绑定 document 级事件委托（按钮无反应防御修复）：
         * 侧栏容器若被任何视图/渲染逻辑替换重建，绑定在容器上的 delegate 会失效；
         * document 级委托永不失效。返回解绑函数（dispose 时移除）。
         * @param {string} eventType
         * @param {string} selector
         * @param {(target: Element, ev: Event) => void} handler
         */
        _bindDelegate(eventType, selector, handler) {
            const listener = (e) => {
                const target = e.target && typeof e.target.closest === 'function'
                    ? e.target.closest(selector)
                    : null;
                if (!target) return;
                handler(target, e);
            };
            document.addEventListener(eventType, listener);
            return () => document.removeEventListener(eventType, listener);
        }

        /** 初始化：绑定控件事件 + 订阅 store（单点异常不中断整页绑定） */
        init() {
            try {
                this._initBindings();
            } catch (e) {
                console.error('[graph-panel] init 绑定异常（已降级继续）:', e);
            }
        }

        /** init 具体实现（try/catch 包裹，避免单点异常导致后续按钮全部失效） */
        _initBindings() {
            const el = this.el;
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
                    if (this.views) {
                        this.views.switchView(this.store.state.view);
                    } else if (this.interaction && this.store.state.dirPath) {
                        this.interaction.refresh();
                    }
                });
            }
            // AI 分析动作（document 级委托：抽取/摘要/缺口/冲突/重复/待确认/演化/
            // 聚类/收藏/偏好/问答/经验/重新分析 —— 容器重建不失效）
            this._unsubs.push(this._bindDelegate('click', '[data-ai-action]', (btn) => {
                this.runAiAction(btn.dataset.aiAction);
            }));
            // 布局模式（document 级委托：同上防御）
            this._unsubs.push(this._bindDelegate('click', '[data-layout]', (btn) => {
                const container = this.el.layoutModes;
                if (container) {
                    container.querySelectorAll('[data-layout]').forEach((b) => b.classList.toggle('active', b === btn));
                }
                this.interaction.applyLayout(btn.dataset.layout);
            }));
            // 详情收藏（PRD §50：My Knowledge）
            if (el.detailStar) {
                el.detailStar.addEventListener('click', async () => {
                    const nodeId = this._detailTargetId;
                    if (!nodeId || !this.store.state.dirPath) return;
                    const on = !this._favSet.has(nodeId);
                    try {
                        await this.api.favorite(this.store.state.dirPath, nodeId, on);
                        if (on) this._favSet.add(nodeId); else this._favSet.delete(nodeId);
                        this._updateStar();
                        this.notify(on ? '已收藏到 My Knowledge' : '已取消收藏');
                    } catch (e) {
                        this.notify('收藏操作失败: ' + String(e), 'warn');
                    }
                });
            }
            // 布局模式已迁移到 document 级委托（见 _initBindings 开头；此处删除容器级绑定）
            // 画布工具栏（PRD §44）
            if (el.canvasToolbar) {
                el.canvasToolbar.addEventListener('click', (e) => {
                    const btn = e.target.closest('[data-action]');
                    if (!btn) return;
                    const action = btn.dataset.action;
                    if (action === 'fit') this.interaction.fitView();
                    else if (action === 'in') this.interaction.zoomIn();
                    else if (action === 'out') this.interaction.zoomOut();
                    else if (action === 'reset') this.interaction.resetView();
                });
            }
            // 详情 Tab 切换（PRD §16.2）
            if (el.detailTabs) {
                el.detailTabs.addEventListener('click', (e) => {
                    const tab = e.target.closest('[data-tab]');
                    if (!tab) return;
                    el.detailTabs.querySelectorAll('[data-tab]').forEach((t) => t.classList.toggle('active', t === tab));
                    const body = el.detailBody;
                    if (body && body.querySelectorAll('[data-tab-body]').length) {
                        body.querySelectorAll('[data-tab-body]').forEach((b) => {
                            b.hidden = b.dataset.tabBody !== tab.dataset.tab;
                        });
                    }
                });
            }
            // 状态订阅 → 统计/引擎态刷新
            this._unsubs.push(this.store.subscribe(() => this._refreshStats()));
            this._unsubs.push(this.store.subscribe(() => this._refreshBuildStatus()));
            // 数据变化 → 重渲染分类/关系过滤（视图层装载数据后自动联动）
            this._unsubs.push(this.store.subscribe((state, change) => {
                if (change && change.type === 'data') {
                    this.renderTypeFilters();
                    this.renderRelationFilters();
                    this._refreshStats();
                }
            }));
        }

        // ─── 左侧：图谱概览（PRD §7） ───

        /** 概览统计：总节点/总边 + 分类计数（点击过滤） */
        _refreshStats() {
            const st = this.store.state;
            const data = this.store.data;
            if (this.el.statNodes) this.el.statNodes.textContent = String(data.nodeCount);
            if (this.el.statEdges) this.el.statEdges.textContent = String(data.edgeCount);
            if (this.el.statEngine) this.el.statEngine.textContent = `引擎: ${st.engineReady ? 'Sigma' : '未就绪'}`;
            if (this.el.statLod) this.el.statLod.textContent = `LOD ${this._lodLabel(st.lod)}`;
            if (this.el.statVersion) this.el.statVersion.textContent = `v${st.graphVersion ?? 0}`;

            // 分类计数（PRD §8：文档/目录/代码/配置/图片/概念/实体/脚本/项目/其他）
            if (this.el.overviewCats) {
                const cats = ['doc', 'folder', 'code', 'concept', 'entity'];
                const counts = {};
                data.nodes.forEach((n) => {
                    const cat = this.model.categoryOf(n);
                    counts[cat] = (counts[cat] || 0) + 1;
                });
                this.el.overviewCats.innerHTML = cats
                    .map((c) => {
                        const label = this.model.CATEGORY_LABELS[c] || c;
                        const color = this.model.CATEGORY_COLORS[c] || '#9aa1a9';
                        return `<span class="kg-ov-chip" data-cat="${c}" style="--chip:${color}">
                            ${label} <b>${counts[c] || 0}</b></span>`;
                    })
                    .join('');
                this.el.overviewCats.querySelectorAll('.kg-ov-chip').forEach((chip) => {
                    chip.addEventListener('click', () => {
                        const cat = chip.dataset.cat;
                        this.store.set({ typeFilter: this.store.state.typeFilter && this.store.state.typeFilter.length === 1 && this.store.state.typeFilter[0] === cat ? null : [cat] });
                        if (this.interaction) this.interaction.applyFilters();
                    });
                });
            }
        }

        /** 分类过滤（PRD §8：11 类，每类显示数量，多选联动） */
        renderTypeFilters() {
            const el = this.el.typeFilters;
            if (!el) return;
            const model = this.model;
            const counts = {};
            const catSet = new Set();
            this.store.data.nodes.forEach((n) => {
                const cat = model.categoryOf(n);
                counts[cat] = (counts[cat] || 0) + 1;
                catSet.add(cat);
            });
            const cats = model.CATEGORIES.filter((c) => c === 'all' || counts[c] > 0);
            const html = cats.map((c) => `
                <label class="kg-filter-item" data-cat="${c}">
                    <input type="checkbox" class="kg-filter-check" data-cat="${c}" ${c === 'all' ? 'checked' : ''} />
                    <span class="kg-filter-dot" style="--dot:${model.CATEGORY_COLORS[c] || '#9aa1a9'}"></span>
                    <span>${model.CATEGORY_LABELS[c] || c}</span>
                    <b class="kg-filter-count">${c === 'all' ? this.store.data.nodeCount : (counts[c] || 0)}</b>
                </label>`).join('');
            el.innerHTML = html;
            const allCb = el.querySelector('[data-cat="all"] .kg-filter-check');
            el.querySelectorAll('.kg-filter-check').forEach((cb) => {
                cb.addEventListener('change', () => {
                    if (cb.dataset.cat === 'all') {
                        // 全选 → 清除过滤
                        el.querySelectorAll('.kg-filter-check').forEach((c) => { c.checked = c.dataset.cat === 'all'; });
                        this.store.set({ typeFilter: null });
                    } else {
                        if (allCb) allCb.checked = false;
                        const checked = Array.from(el.querySelectorAll('.kg-filter-check:checked'))
                            .map((c) => c.dataset.cat)
                            .filter((c) => c !== 'all');
                        if (!checked.length) {
                            el.querySelectorAll('.kg-filter-check').forEach((c) => { c.checked = true; });
                            this.store.set({ typeFilter: null });
                        } else {
                            this.store.set({ typeFilter: checked });
                        }
                    }
                    if (this.interaction) this.interaction.applyFilters();
                });
            });
        }

        /** 关系过滤（PRD §9：名称/数量/颜色；点击只显示该类型关系） */
        renderRelationFilters() {
            const el = this.el.relationFilters;
            if (!el) return;
            const counts = {};
            this.store.data.edges.forEach((e) => {
                counts[e.relation] = (counts[e.relation] || 0) + 1;
            });
            const rels = Object.keys(counts).sort();
            if (!rels.length) {
                el.innerHTML = '<div class="kg-muted">暂无关系</div>';
                return;
            }
            el.innerHTML = rels.map((r) => `
                <button class="kg-rel-filter" data-rel="${r}" title="${RELATION_LABELS[r] || r}">
                    <span class="kg-rel-line" style="--rel:${this._relationColor(r)}"></span>
                    <span class="kg-rel-name">${RELATION_LABELS[r] || r}</span>
                    <b>${counts[r]}</b>
                </button>`).join('');
            el.querySelectorAll('.kg-rel-filter').forEach((btn) => {
                btn.addEventListener('click', () => {
                    const r = btn.dataset.rel;
                    const active = this.store.state.relationFilter === r;
                    this.store.set({ relationFilter: active ? null : r });
                    el.querySelectorAll('.kg-rel-filter').forEach((b) => b.classList.toggle('active', !active && b.dataset.rel === r));
                    if (this.interaction) this.interaction.applyFilters();
                });
            });
        }

        /** 关系颜色（与 renderer RELATION_STYLES 对齐的展示色） */
        _relationColor(relation) {
            const map = {
                CONTAINS: '#9aa1a9', REFERENCES: '#6b9be8', IMPORTS: '#4caf78',
                DERIVED_FROM: '#d9a04b', SAME_TOPIC: '#a08ad8', SOLVED_BY: '#4c8bf5',
                IMPLEMENTED_IN: '#4caf78', VALIDATED_BY: '#d9a04b', REPLACED_BY: '#e07a6a',
                DEPRECATED: '#9aa1a9', PREFERS: '#e8749a', AVOIDS: '#9aa1a9',
                USES: '#4caf78', BELONGS_TO: '#8a6fd8', SIMILAR_TO: '#3aa8b8', DEPENDS_ON: '#c26f4a',
            };
            return map[relation] || '#9aa1a9';
        }

        /** 布局模式激活态（交互层切换后调用） */
        setLayoutMode(mode) {
            const el = this.el.layoutModes;
            if (!el) return;
            el.querySelectorAll('[data-layout]').forEach((b) => b.classList.toggle('active', b.dataset.layout === mode));
        }

        // ─── 左侧：AI 智能分析（PRD §29/§52-54） ───

        /** AI 分析动作分发（按钮点击入口） */
        async runAiAction(action) {
            const el = this.el.aiAnalysis;
            if (!el) return;
            const dirPath = this.store.state.dirPath;
            if (!dirPath) {
                // 按钮无反应防御：不再静默 return —— 明确提示根因（此前若 dirPath 为空，
                // 所有 AI 动作点击"无任何反应"且无任何提示）
                el.innerHTML = '<div class="kg-ai-item kg-ai-err">未检测到知识库目录 —— 请先在主界面打开知识库并进入图谱，再使用 AI 分析</div>';
                return;
            }
            const render = (html) => { el.innerHTML = html; };
            try {
                switch (action) {
                    case 're-enqueue': {
                        // D4：全库重新入队（done 不重复、failed 重试）→ 后台 worker 重新处理
                        render('<div class="kg-ai-item">按最新重要度重新排队并触发后台分析…</div>');
                        const n = await this.api.aiEnqueueAll(dirPath);
                        render(`<div class="kg-ai-item">已更新 AI 队列（<b>${n || 0}</b> 个文档参与排队）<br><small>后台将自动抽取：规则抽取始终执行；LLM 已配置时对高价值文档执行</small></div>`);
                        break;
                    }
                    case 'extract': {
                        render('<div class="kg-ai-item">AI 实体关系抽取中（高价值文档优先）…</div>');
                        const n = await this.api.aiExtract(dirPath, null, 10);
                        render(`<div class="kg-ai-item">抽取完成：新增 <b>${n || 0}</b> 条候选关系<br><small>（打开「待确认」查看并确认落图）</small></div>`);
                        this.interaction.applyFilters();
                        break;
                    }
                    case 'summarize': {
                        render('<div class="kg-ai-item">AI 簇摘要生成中…</div>');
                        const n = await this.api.aiSummarizeClusters(dirPath, 20);
                        render(`<div class="kg-ai-item">已为 <b>${n || 0}</b> 个知识簇生成 AI 描述</div>`);
                        break;
                    }
                    case 'gaps': {
                        render('<div class="kg-ai-item">分析知识缺口中…</div>');
                        const clusterId = (this.store.clusters[0] && this.store.clusters[0].id) || this.store.state.clusterId;
                        if (!clusterId) { render('<div class="kg-muted">暂无知识簇可分析</div>'); return; }
                        const gaps = (await this.api.aiGaps(dirPath, clusterId)) || [];
                        if (!gaps.length) { render('<div class="kg-ai-item">该簇未发现明显知识缺口（或需配置 LLM 深度分析）</div>'); return; }
                        render(gaps.map((g) => `
                            <div class="kg-ai-item">
                                <b>${escapeHtml(g.cluster_name)}</b>
                                <div>已覆盖：${escapeHtml(g.covered.slice(0, 8).join('、'))}</div>
                                <div>建议补充：<b class="kg-ai-missing">${escapeHtml(g.missing.join('、'))}</b></div>
                            </div>`).join(''));
                        break;
                    }
                    case 'conflicts': {
                        render('<div class="kg-ai-item">检测知识冲突中…</div>');
                        const conflicts = (await this.api.aiConflicts(dirPath)) || [];
                        if (!conflicts.length) { render('<div class="kg-ai-item">未检测到明显知识冲突（或需配置 LLM）</div>'); return; }
                        render(conflicts.map((c) => `
                            <div class="kg-ai-item">
                                <b>⚠ ${escapeHtml(c.topic)}</b>
                                <div>A（${escapeHtml(c.source_a)}）：${escapeHtml(c.claim_a.slice(0, 80))}</div>
                                <div>B（${escapeHtml(c.source_b)}）：${escapeHtml(c.claim_b.slice(0, 80))}</div>
                                <div class="kg-ai-missing">${escapeHtml(c.analysis)}</div>
                            </div>`).join(''));
                        break;
                    }
                    case 'duplicates': {
                        render('<div class="kg-ai-item">检测重复概念中…</div>');
                        const dups = (await this.api.aiDuplicates(dirPath)) || [];
                        if (!dups.length) { render('<div class="kg-ai-item">未发现高度相似的概念</div>'); return; }
                        render(`<div class="kg-ai-item"><b>发现 ${dups.length} 组相似概念</b></div>` + dups.slice(0, 10).map((d) => `
                            <div class="kg-ai-item">「${escapeHtml(d.name_a)}」≈「${escapeHtml(d.name_b)}」（${(d.similarity * 100).toFixed(0)}%）</div>`).join(''));
                        break;
                    }
                    case 'candidates': {
                        render('<div class="kg-ai-item">加载待确认关系…</div>');
                        const list = (await this.api.aiCandidates(dirPath, null, 100)) || [];
                        const pending = list.filter((c) => c.status === 'candidate');
                        if (!pending.length) { render('<div class="kg-ai-item">暂无待确认的 AI 关系（先运行「AI 抽取」）</div>'); return; }
                        render(pending.slice(0, 12).map((c) => `
                            <div class="kg-ai-item kg-ai-cand" data-id="${escapeHtml(c.id)}">
                                <div><b>${escapeHtml(c.source)}</b> —${escapeHtml(c.relation)}→ <b>${escapeHtml(c.target)}</b>
                                    <span class="kg-ai-conf">${Math.round(c.confidence * 100)}%</span></div>
                                ${c.evidence ? `<div class="kg-ai-ev-text">${escapeHtml(c.evidence.slice(0, 90))}</div>` : ''}
                                <div class="kg-ai-cand-actions">
                                    <button class="kg-btn kg-btn-sm kg-ai-ok" data-id="${escapeHtml(c.id)}">确认</button>
                                    <button class="kg-btn kg-btn-sm kg-ai-no" data-id="${escapeHtml(c.id)}">忽略</button>
                                </div>
                            </div>`).join(''));
                        const container = this.el.aiAnalysis;
                        container.querySelectorAll('.kg-ai-ok').forEach((b) => b.addEventListener('click', () => this._confirmCandidate(b.dataset.id)));
                        container.querySelectorAll('.kg-ai-no').forEach((b) => b.addEventListener('click', () => this._rejectCandidate(b.dataset.id)));
                        break;
                    }
                    case 'evolution': {
                        render('<div class="kg-ai-item">演化分析中…</div>');
                        const res = await this.api.evolution(dirPath, true);
                        const evo = res && res.evolution;
                        const monthly = evo && evo.monthly_nodes ? evo.monthly_nodes.slice(-6).reverse() : [];
                        const rows = monthly.map(([m, c]) => `${m}: +${c}`).join('<br>');
                        render(`
                            ${res && res.insight ? `<div class="kg-ai-item">${escapeHtml(res.insight)}</div>` : '<div class="kg-ai-item">（未配置 LLM，以下为演化统计）</div>'}
                            ${rows ? `<div class="kg-ai-item" style="margin-top:6px"><b>近 6 个月节点增长</b><br>${rows}</div>` : ''}`);
                        break;
                    }
                    case 'recluster-directory': {
                        render('<div class="kg-ai-item">目录结构聚类重算中…</div>');
                        const n = await this.api.recluster(dirPath, 'directory');
                        render(`<div class="kg-ai-item">聚类完成：<b>${n || 0}</b> 个知识簇（目录结构）</div>`);
                        if (this.views) this.views.switchView(this.store.state.view);
                        break;
                    }
                    case 'recluster-embedding': {
                        render('<div class="kg-ai-item">语义聚类中（本地 Embedding，可能需数秒）…</div>');
                        const n = await this.api.recluster(dirPath, 'embedding');
                        render(`<div class="kg-ai-item">语义聚类完成：<b>${n || 0}</b> 个知识簇</div>`);
                        if (this.views) this.views.switchView(this.store.state.view);
                        break;
                    }
                    case 'favorites': {
                        render('<div class="kg-ai-item">加载收藏中…</div>');
                        const list = (await this.api.favorites(dirPath, 100)) || [];
                        if (!list.length) {
                            render('<div class="kg-ai-item">暂无收藏 —— 在节点详情右上角点击 ☆ 收藏到 My Knowledge</div>');
                            return;
                        }
                        render(list.map((n) => `
                            <div class="kg-ai-item kg-fav-item" data-id="${escapeHtml(n.id)}" ${n.path ? `data-path="${escapeHtml(n.path)}"` : ''}>
                                <b>${escapeHtml(n.name)}</b> <small>${escapeHtml(n.type || '')}</small>
                            </div>`).join(''));
                        const container = this.el.aiAnalysis;
                        container.querySelectorAll('.kg-fav-item').forEach((item) => {
                            item.addEventListener('click', () => {
                                const id = item.dataset.id;
                                const path = item.dataset.path;
                                if (path) { this.openPath(path); }
                                else if (id && this.store.getNode(id)) { this.interaction.onNodeClick(id); }
                                else if (id) { this.notify('该节点未装载，请先在图谱中打开', 'warn'); }
                            });
                        });
                        break;
                    }
                    case 'chunk-build': {
                        render('<div class="kg-ai-item">重建文档结构层（chunk/section）…</div>');
                        const res = await this.api.buildChunks(dirPath);
                        render(`<div class="kg-ai-item">内容层重建完成：<b>${res && res.docs || 0}</b> 篇文档 / <b>${res && res.chunks || 0}</b> 个 chunk / <b>${res && res.sections || 0}</b> 个章节<br><small>放大到 L4 细粒度查看内容块</small></div>`);
                        if (this.views) this.views.switchView(this.store.state.view);
                        break;
                    }
                    case 'chunk-sim': {
                        render('<div class="kg-ai-item">构建 chunk 语义相似边中（本地 Embedding，可能需数十秒）…</div>');
                        const n = await this.api.chunkSimilarity(dirPath, 3);
                        render(`<div class="kg-ai-item">语义相似边构建完成：新增 <b>${n || 0}</b> 条 SIMILAR_TO 关系</div>`);
                        if (this.views) this.views.switchView(this.store.state.view);
                        break;
                    }
                    case 'memory': {
                        render('<div class="kg-ai-item">加载我的知识偏好…</div>');
                        const list = (await this.api.memoryPreferences(dirPath)) || [];
                        if (!list.length) {
                            render('<div class="kg-ai-item">暂无偏好记录 —— 偏好由 Agent/AI 操作自动沉淀（P2）</div>');
                            return;
                        }
                        render(list.map((r) => `
                            <div class="kg-ai-item kg-fav-item" data-id="${escapeHtml(r.node.id)}">
                                <b>${escapeHtml(r.node.name)}</b> <small>${escapeHtml(r.reason)}</small>
                            </div>`).join(''));
                        const container = this.el.aiAnalysis;
                        container.querySelectorAll('.kg-fav-item').forEach((item) => {
                            item.addEventListener('click', () => {
                                const id = item.dataset.id;
                                if (id && this.store.getNode(id)) { this.interaction.onNodeClick(id); }
                            });
                        });
                        break;
                    }
                    case 'query': {
                        // GraphRAG 图谱问答（PRD §22-23）：答案 + 可定位的 chunk 证据
                        render(`
                            <div class="kg-ai-item kg-muted">向知识图谱提问（图证据 + 文档检索 + LLM 回答）</div>
                            <div class="kg-ai-row">
                                <input class="kg-ai-input" id="kg-ai-query-input" type="text" placeholder="例如：Redis 的持久化方案有哪些？" />
                                <button class="kg-btn kg-btn-sm kg-ai-query-go">提问</button>
                            </div>`);
                        const doQuery = async () => {
                            const input = document.getElementById('kg-ai-query-input');
                            const q = input && input.value.trim();
                            if (!q) return;
                            render('<div class="kg-ai-item">检索图证据并生成回答…</div>');
                            const res = await this.api.query(dirPath, q, 20);
                            if (!res) {
                                render('<div class="kg-ai-item kg-ai-err">问答失败：需要 LLM 配置（未配置时可用「搜索/相似边」代替）</div>');
                                return;
                            }
                            const ev = (res.evidence || []).filter((x) => x && x.source_doc);
                            render(`
                                ${res.answer ? `<div class="kg-ai-item kg-ai-answer">${escapeHtml(res.answer)}</div>` : '<div class="kg-ai-item">（未生成回答）</div>'}
                                ${res.used_llm === false ? '<div class="kg-ai-item kg-muted">未配置 LLM：以下为图证据而非 AI 回答</div>' : ''}
                                ${ev.length
                                    ? `<div class="kg-ai-item kg-muted"><b>证据（${ev.length}）</b> —— 点击「定位段落」直达语义块</div>` +
                                      ev.slice(0, 8).map((e) => `
                                        <div class="kg-ai-item kg-ai-ev" data-doc="${escapeHtml(e.source_doc)}" data-chunk="${e.chunk_id ? escapeHtml(e.chunk_id) : ''}">
                                            <div class="kg-ai-ev-src">${escapeHtml(e.source_doc)}</div>
                                            <div class="kg-ai-ev-text">${escapeHtml(e.snippet)}</div>
                                            ${e.chunk_id ? '<button class="kg-btn kg-btn-sm kg-ai-locate">定位段落</button>' : ''}
                                        </div>`).join('')
                                    : '<div class="kg-ai-item kg-muted">无文档证据（仅有图关系）</div>'}`);
                            const container = this.el.aiAnalysis;
                            container.querySelectorAll('.kg-ai-ev').forEach((item) => {
                                const chunk = item.dataset.chunk;
                                const locateBtn = item.querySelector('.kg-ai-locate');
                                const jump = () => {
                                    if (chunk && this.interaction && typeof this.interaction.locateNode === 'function') {
                                        this.interaction.locateNode(chunk);
                                    } else if (item.dataset.doc) {
                                        this.openPath(item.dataset.doc);
                                    }
                                };
                                if (locateBtn) locateBtn.addEventListener('click', jump);
                                else item.addEventListener('click', jump);
                            });
                        };
                        const goBtn = document.getElementById('kg-ai-query-go');
                        const qInput = document.getElementById('kg-ai-query-input');
                        if (goBtn) goBtn.addEventListener('click', doQuery);
                        if (qInput) qInput.addEventListener('keydown', (e) => { if (e.key === 'Enter') doQuery(); });
                        break;
                    }
                    case 'experience': {
                        // 经验库检索（Experience Graph：描述问题 → 匹配历史解决方案）
                        render(`
                            <div class="kg-ai-item kg-muted">经验库检索 —— 描述问题，匹配历史解决方案（来自 Git 提交 / AI 操作）</div>
                            <div class="kg-ai-row">
                                <input class="kg-ai-input" id="kg-ai-exp-input" type="text" placeholder="例如：缓存穿透怎么解决" />
                                <button class="kg-btn kg-btn-sm kg-ai-exp-go">检索</button>
                            </div>`);
                        const doExp = async () => {
                            const input = document.getElementById('kg-ai-exp-input');
                            const q = input && input.value.trim();
                            if (!q) return;
                            render('<div class="kg-ai-item">匹配经验中…</div>');
                            const hits = (await this.api.experienceSearch(dirPath, q, 10)) || [];
                            if (!hits.length) {
                                render('<div class="kg-ai-item">暂无相关经验 —— 经验由 Git 提交 / AI 操作自动沉淀，LLM 已配置时自动富化 P/S</div>');
                                return;
                            }
                            render(hits.map((h) => `
                                <div class="kg-ai-item kg-exp-item" data-doc="${escapeHtml(h.doc_path || '')}">
                                    <div><b>${escapeHtml(h.problem)}</b> <span class="kg-ai-conf">${Math.round(h.score * 100)}%</span></div>
                                    <div class="kg-ai-ev-text">→ ${escapeHtml(h.solution)}</div>
                                    ${h.doc_path ? `<div class="kg-ai-ev-src">${escapeHtml(h.doc_path)}</div>` : ''}
                                </div>`).join(''));
                            const container = this.el.aiAnalysis;
                            container.querySelectorAll('.kg-exp-item').forEach((item) => {
                                item.addEventListener('click', () => {
                                    if (item.dataset.doc) this.openPath(item.dataset.doc);
                                });
                            });
                        };
                        const expGo = document.getElementById('kg-ai-exp-go');
                        const expInput = document.getElementById('kg-ai-exp-input');
                        if (expGo) expGo.addEventListener('click', doExp);
                        if (expInput) expInput.addEventListener('keydown', (e) => { if (e.key === 'Enter') doExp(); });
                        break;
                    }
                    default:
                        render('<div class="kg-muted">未知操作</div>');
                }
            } catch (e) {
                render(`<div class="kg-ai-item kg-ai-err">操作失败: ${escapeHtml(String(e))}</div>`);
            }
        }

        /** 确认候选关系（落正式边 + 重载视图闭环） */
        async _confirmCandidate(id) {
            try {
                await this.api.aiConfirm(this.store.state.dirPath, id);
                this.notify('已确认关系（落图）');
                await this.runAiAction('candidates');
                if (this.views) this.views.switchView(this.store.state.view);
            } catch (e) {
                this.notify('确认失败: ' + String(e), 'warn');
            }
        }

        /** 忽略候选关系 */
        async _rejectCandidate(id) {
            try {
                await this.api.aiReject(this.store.state.dirPath, id);
                this.notify('已忽略候选');
                await this.runAiAction('candidates');
                if (this.views) this.views.switchView(this.store.state.view);
            } catch (e) {
                this.notify('忽略失败: ' + String(e), 'warn');
            }
        }

        /** 默认 AI 分析概览（本地规则：簇数量/跨域关系/分类分布） */
        _runAiAnalysis() {
            const el = this.el.aiAnalysis;
            const clusters = this.store.clusters;
            const data = this.store.data;
            if (!el) return;
            const insights = [];
            if (clusters.length) {
                insights.push(`发现 <b>${clusters.length}</b> 个知识聚类`);
                const top = clusters[0];
                insights.push(`建议查看：<b>${escapeHtml(top.name)}</b>（${top.node_count} 节点）`);
                const links = clusters.reduce((s, c) => s + (c.links || []).length, 0);
                insights.push(`簇间存在 <b>${links}</b> 条跨域关系`);
            }
            const catCount = {};
            data.nodes.forEach((n) => {
                const cat = this.model.categoryOf(n);
                catCount[cat] = (catCount[cat] || 0) + 1;
            });
            const topCat = Object.entries(catCount).sort((a, b) => b[1] - a[1])[0];
            if (topCat) {
                insights.push(`知识最多的分类：<b>${this.model.CATEGORY_LABELS[topCat[0]] || topCat[0]}</b>（${topCat[1]}）`);
            }
            el.innerHTML = insights.length
                ? insights.map((i) => `<div class="kg-ai-item">${i}</div>`).join('')
                : '<div class="kg-muted">构建图谱后 AI 将在此给出知识聚类 / 缺口 / 冲突分析</div>';
        }

        // ─── 状态 ───

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
                const clusters = s.cluster_count != null ? ` / ${s.cluster_count} 簇` : '';
                const mode = s.cluster_mode === 'embedding' ? '（语义聚类）' : (s.cluster_mode === 'directory' ? '（目录聚类）' : '');
                el.textContent = `已构建：${s.node_count} 节点 / ${s.edge_count} 边${clusters}${mode}`;
            } else {
                el.textContent = '尚未构建（等待后台任务）';
            }
        }

        /** 更新 LOD 徽标（PRD §12：L0-L4） */
        updateLodBadge(lod) {
            if (this.el.lodBadge) this.el.lodBadge.textContent = this._lodLabel(lod);
            if (this.el.statLod) this.el.statLod.textContent = `LOD ${this._lodLabel(lod)}`;
        }

        _lodLabel(lod) {
            const labels = { 0: 'L0 聚类', 1: 'L1 核心', 2: 'L2 重要', 3: 'L3 全量', 4: 'L4 细粒度' };
            return labels[lod] || `L${lod}`;
        }

        /** 加载态（视图层调用） */
        setLoading(loading) {
            this.store.set({ loading });
            const el = this.el.buildStatus;
            if (el && loading) el.textContent = '加载中…';
        }

        // ─── 右侧：节点详情（PRD §16-20，四 Tab） ───

        /**
         * 展示详情：普通节点（GraphNode）或知识簇（GraphCluster）。
         * Tab：概览（描述/Tags/关键文件）/ 内容（关联文档）/ 关系（入边出边）/ 分析。
         */
        showNodeDetail(target) {
            const el = this.el;
            if (!el.detail || !target) return;
            el.detail.hidden = false;
            const isCluster = target.type === 'cluster' || target.node_type === 'cluster'
                || (target.id && target.id.startsWith('cluster:') && target.node_count != null);
            const name = target.name || target.id;
            const typeLabel = isCluster ? '概念聚类' : (TYPE_LABELS[target.type || target.node_type] || '节点');
            const color = this.model.CATEGORY_COLORS.cluster || '#6b7b8f';
            el.detailIcon.textContent = isCluster ? '◈' : '●';
            el.detailIcon.style.color = color;
            el.detailTitle.textContent = name;
            const nodeCount = isCluster ? (target.node_count || 0) : 1;
            const edgeCount = isCluster ? (target.edge_count || 0) : (this._nodeEdgeCount(target.id));
            el.detailSubtitle.textContent = `${typeLabel} · ${nodeCount} 节点 · ${edgeCount} 边`;

            // Tab 内容
            const overview = isCluster
                ? this._clusterOverview(target)
                : this._nodeOverview(target);
            const content = isCluster
                ? this._clusterContent(target)
                : this._nodeContent(target);
            const relations = isCluster
                ? this._clusterRelations(target)
                : this._nodeRelations(target);
            const analysis = isCluster
                ? this._clusterAnalysis(target)
                : this._nodeAnalysis(target);

            el.detailBody.innerHTML = `
                <div data-tab-body="overview">${overview}</div>
                <div data-tab-body="content" hidden>${content}</div>
                <div data-tab-body="relations" hidden>${relations}</div>
                <div data-tab-body="analysis" hidden>${analysis}</div>`;
            // 默认激活「概览」Tab
            if (el.detailTabs) {
                el.detailTabs.querySelectorAll('[data-tab]').forEach((t) => t.classList.toggle('active', t.dataset.tab === 'overview'));
            }
            // 内容 Tab 打开文件
            el.detailBody.querySelectorAll('[data-open]').forEach((item) => {
                item.addEventListener('click', () => {
                    const path = item.dataset.open;
                    if (path) this.openPath(path);
                });
            });
            // 收藏状态 + 推荐（PRD §50/§51）
            this._detailTargetId = target.id || null;
            this._updateStar();
            if (target.id) this._loadRecommendations(target.id);
        }

        /** 收藏星标状态（PRD §50） */
        _updateStar() {
            const star = this.el.detailStar;
            if (!star) return;
            const fav = this._detailTargetId && this._favSet.has(this._detailTargetId);
            star.textContent = fav ? '★' : '☆';
            star.classList.toggle('active', !!fav);
        }

        /** 懒加载收藏集合（My Knowledge） */
        async _loadFavSet() {
            if (this._favSet.size || this._favLoaded) return;
            this._favLoaded = true;
            try {
                const list = (await this.api.favorites(this.store.state.dirPath, 500)) || [];
                this._favSet = new Set(list.map((n) => n.id));
                this._updateStar();
            } catch (e) { /* ignore */ }
        }

        /** 加载「你可能还需要了解」推荐（PRD §51） */
        async _loadRecommendations(nodeId) {
            this._loadFavSet();
            const targetId = this._detailTargetId;
            try {
                const recs = (await this.api.recommend(this.store.state.dirPath, nodeId, 6)) || [];
                const box = document.getElementById('kg-detail-recs');
                if (!box || targetId !== this._detailTargetId) return;
                box.innerHTML = recs.length
                    ? `<div class="kg-detail-section-label">你可能还需要了解（AI 推荐）</div>
                       <ul class="kg-rel-list">${recs.map((r) => `
                        <li class="kg-rel-item kg-rec-item" data-id="${escapeHtml(r.node.id)}">
                            <span class="kg-rel-dot"></span>
                            <span><b>${escapeHtml(r.node.name)}</b></span>
                            <small>${escapeHtml(r.reason)}</small>
                        </li>`).join('')}</ul>`
                    : '<div class="kg-muted">暂无推荐（图谱关系丰富后自动出现）</div>';
                box.querySelectorAll('.kg-rec-item').forEach((item) => {
                    item.addEventListener('click', () => {
                        const id = item.dataset.id;
                        if (this.interaction && this.store.getNode(id)) {
                            this.interaction.onNodeClick(id);
                        } else {
                            this._loadFavSet();
                            this.views && this.views.switchView('document').then(() => {
                                setTimeout(() => this.interaction.onNodeClick(id), 150);
                            });
                        }
                    });
                });
            } catch (e) { /* 推荐失败不阻断 */ }
        }

        _nodeEdgeCount(nodeId) {
            let count = 0;
            this.store.data.edges.forEach((e) => {
                if (e.source === nodeId || e.target === nodeId) count++;
            });
            return count;
        }

        _nodeOverview(node) {
            const meta = node.meta && typeof node.meta === 'object' ? node.meta : {};
            const metaStr = Object.keys(meta).length ? `<div class="kg-detail-meta">${escapeHtml(JSON.stringify(meta))}</div>` : '';
            // chunk/section 内容层：展示正文内容（L4 细粒度）
            const contentHtml = node.content
                ? `<div class="kg-detail-section-label">内容</div>
                   <div class="kg-detail-desc" style="white-space:pre-wrap">${escapeHtml(node.content)}</div>`
                : '';
            return `
                <div class="kg-detail-row"><span>类型</span><b>${TYPE_LABELS[node.type] || node.type}</b></div>
                <div class="kg-detail-row"><span>ID</span><code>${escapeHtml(node.id)}</code></div>
                ${node.path ? `<div class="kg-detail-row"><span>路径</span><code class="kg-openable" data-open="${escapeHtml(node.path)}">${escapeHtml(node.path)} ↗</code></div>` : ''}
                ${typeof node.degree === 'number' ? `<div class="kg-detail-row"><span>度数</span><b>${node.degree}</b></div>` : ''}
                ${typeof node.created_at === 'number' ? `<div class="kg-detail-row"><span>首次入库</span><b>${new Date(node.created_at).toLocaleString()}</b></div>` : ''}
                ${metaStr}
                ${contentHtml}`;
        }

        _clusterOverview(cluster) {
            const files = (cluster.top_files || []).map((f) => `<span class="kg-tag">${escapeHtml(f.name)}</span>`).join('');
            const filesList = (cluster.top_files || []).map((f, i) => `
                <li class="kg-file-item" ${f.path ? `data-open="${escapeHtml(f.path)}"` : ''}>
                    <span>${i + 1}. ${escapeHtml(f.name)}</span>
                    <small>${escapeHtml(f.path || '')}</small>
                </li>`).join('');
            return `
                <div class="kg-detail-section-label">描述</div>
                <div class="kg-detail-desc">${escapeHtml(cluster.description || 'AI 尚未生成该知识簇的描述。')}</div>
                <div class="kg-detail-section-label">Tags</div>
                <div class="kg-tag-row">${files || '<span class="kg-muted">暂无</span>'}</div>
                <div class="kg-detail-section-label">关键文件（Top ${Math.min(5, (cluster.top_files || []).length || 5)}）</div>
                <ul class="kg-file-list">${filesList || '<li class="kg-muted">暂无</li>'}</ul>`;
        }

        _clusterContent(cluster) {
            // 成员文档（store 中 meta.clusterId === cluster.id 的节点；或按 path 顶层匹配）
            const members = [];
            this.store.data.nodes.forEach((n) => {
                if (n.id === cluster.id) return;
                let hit = false;
                try {
                    const meta = typeof n.meta === 'string' ? JSON.parse(n.meta) : (n.meta || {});
                    hit = meta && meta.clusterId === cluster.id;
                } catch (e) { /* ignore */ }
                if (!hit && n.path) {
                    const top = n.path.split('/')[0];
                    const cid = `cluster:${top}`;
                    hit = cid === cluster.id;
                }
                if (hit) members.push(n);
            });
            if (!members.length) return '<div class="kg-muted">暂无成员数据 —— 在全局视图中点击该簇展开。</div>';
            return `<ul class="kg-file-list">${members.slice(0, 100).map((m) => `
                <li class="kg-file-item" ${m.path ? `data-open="${escapeHtml(m.path)}"` : ''}>
                    <span>${escapeHtml(m.name)}</span>
                    <small>${escapeHtml(m.path || m.type || '')} · ${m.degree || 0} 度</small>
                </li>`).join('')}</ul>`;
        }

        _clusterRelations(cluster) {
            const links = (cluster.links || []).slice(0, 50);
            if (!links.length) return '<div class="kg-muted">暂无跨簇关系。</div>';
            return `<ul class="kg-rel-list">${links.map((l) => {
                const other = l.source === cluster.id ? l.target : l.source;
                return `<li class="kg-rel-item">
                    <span class="kg-rel-dot"></span>
                    <span>${escapeHtml(other)}</span>
                    <small>× ${l.count} 条边</small>
                </li>`;
            }).join('')}</ul>`;
        }

        _nodeContent(node) {
            // 关联文档（邻居中含 path 的节点）
            const neighbors = this.store.data.neighbors(node.id) || new Set();
            const docs = [];
            neighbors.forEach((nbId) => {
                const nb = this.store.getNode(nbId);
                if (nb && (nb.path || nb.type === 'doc')) docs.push(nb);
            });
            if (!docs.length) return '<div class="kg-muted">暂无关联文档。</div>';
            return `<ul class="kg-file-list">${docs.slice(0, 100).map((m) => `
                <li class="kg-file-item" ${m.path ? `data-open="${escapeHtml(m.path)}"` : ''}>
                    <span>${escapeHtml(m.name)}</span>
                    <small>${escapeHtml(m.path || '')}</small>
                </li>`).join('')}</ul>`;
        }

        _nodeRelations(node) {
            const edges = [];
            this.store.data.edges.forEach((e) => {
                if (e.source === node.id) edges.push({ dir: 'out', label: `${RELATION_LABELS[e.relation] || e.relation} →`, other: e.target });
                else if (e.target === node.id) edges.push({ dir: 'in', label: `← ${RELATION_LABELS[e.relation] || e.relation}`, other: e.source });
            });
            if (!edges.length) return '<div class="kg-muted">暂无关系。</div>';
            return `<ul class="kg-rel-list">${edges.slice(0, 100).map((r) => `
                <li class="kg-rel-item">
                    <span class="kg-rel-dot"></span>
                    <span class="kg-rel-dir">${r.dir === 'out' ? '↓' : '↑'}</span>
                    <span>${r.label} <b>${escapeHtml(this.store.getNode(r.other)?.name || r.other)}</b></span>
                </li>`).join('')}</ul>`;
        }

        _nodeAnalysis(node) {
            const degree = node.degree || 0;
            // 中心性排名（按当前装载数据）
            let rank = 1;
            const degs = [];
            this.store.data.nodes.forEach((n) => { if (n.degree != null) degs.push(n.degree); });
            degs.sort((a, b) => b - a);
            rank = degs.findIndex((d) => d <= degree) + 1;
            return `
                <div class="kg-detail-row"><span>知识规模</span><b>${degree}</b></div>
                <div class="kg-detail-row"><span>核心度排名</span><b>Top ${rank} / ${degs.length || 1}</b></div>
                <div class="kg-detail-row"><span>增长率</span><b>—（AI 分析待接入）</b></div>
                <div class="kg-detail-section-label">发现</div>
                <div class="kg-muted">接入 LLM 后将在此给出知识密度 / 缺口 / 冲突分析（PRD §20）。</div>
                <div class="kg-recs" id="kg-detail-recs"></div>`;
        }

        _clusterAnalysis(cluster) {
            return `
                <div class="kg-detail-row"><span>知识规模</span><b>${cluster.node_count} 节点</b></div>
                <div class="kg-detail-row"><span>簇内关系</span><b>${cluster.edge_count} 条</b></div>
                <div class="kg-detail-row"><span>核心节点</span><b>${escapeHtml(cluster.centroid || '—')}</b></div>
                <div class="kg-detail-row"><span>聚类算法</span><b>${escapeHtml(cluster.algorithm || 'directory')}</b></div>
                <div class="kg-detail-section-label">发现</div>
                <div class="kg-muted">接入 LLM 后将在此给出知识缺口 / 冲突分析（PRD §20/§52）。</div>
                <div class="kg-recs" id="kg-detail-recs"></div>`;
        }

        /** 隐藏详情 */
        hideNodeDetail() {
            if (this.el.detail) this.el.detail.hidden = true;
        }

        /** 刷新详情（展开后调用） */
        refreshNodeDetail(nodeId) {
            const node = this.store.getNode(nodeId);
            if (node && this.store.state.selectedNodeId === nodeId) this.showNodeDetail(node);
        }

        /** 打开文件：postMessage 通知主页面（复用现有打开链路） */
        openPath(path) {
            if (path && window.parent) {
                window.parent.postMessage({ type: 'graph:open-node', payload: { nodeId: 'doc:' + path.replace(/\\/g, '/'), path } }, '*');
            }
        }

        /** 轻提示 */
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
