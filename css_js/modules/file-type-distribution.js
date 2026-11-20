/**
 * ===== 文件类型分布视图（v2）=====
 *
 * 信息优先：KPI + Treemap（面积=数量/体积）+ 排行 + 明细表 + 选中类型文件列表。
 * 不依赖后端；数据来自 `_getFilesByDirPath()`，分类由宿主注入的 categoryOf 提供。
 *
 * 对外：window.FileTypeDistribution = { mount(container, files, opts), destroy(), aggregate(files, categoryOf) }
 */
(function () {
    'use strict';
    if (window.FileTypeDistribution) return;

    const CATEGORY_ORDER = ['前端', '后端', '脚本', '数据库', '文档', '媒体', '压缩文件', '图表', '字体', '其他'];
    const CATEGORY_COLORS = {
        前端: '#2f6bff',
        后端: '#7c5cff',
        脚本: '#12a5a5',
        数据库: '#0ea5e9',
        文档: '#22a06b',
        媒体: '#f2994a',
        压缩文件: '#e5484d',
        图表: '#a855f7',
        字体: '#64748b',
        其他: '#94a3b8',
    };
    const UNKNOWN_EXT = '__none__';
    const TOP_PER_CATEGORY = 8;
    const MIN_SHARE = 0.01;

    let state = null;
    let rootEl = null;
    let tooltipEl = null;
    let resizeObserver = null;
    let resizeTimer = 0;
    let searchTimer = 0;
    let controller = null;

    function esc(value) {
        return String(value == null ? '' : value)
            .replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;')
            .replace(/"/g, '&quot;')
            .replace(/'/g, '&#39;');
    }

    function fmtSize(bytes) {
        const n = Number(bytes) || 0;
        if (n <= 0) return '0 B';
        const units = ['B', 'KB', 'MB', 'GB', 'TB'];
        let value = n;
        let i = 0;
        while (value >= 1024 && i < units.length - 1) {
            value /= 1024;
            i++;
        }
        const digits = value >= 100 || i === 0 ? 0 : 1;
        return value.toFixed(digits) + ' ' + units[i];
    }

    function fmtInt(n) {
        const value = Number(n) || 0;
        return value >= 10000 ? (value / 10000).toFixed(1) + ' 万' : String(value);
    }

    function fmtPct(part, total) {
        if (!total) return '0%';
        const pct = part / total * 100;
        return (pct >= 10 ? pct.toFixed(0) : pct.toFixed(1)) + '%';
    }

    function extKey(rawExt) {
        const ext = String(rawExt || '').toLowerCase();
        return ext || UNKNOWN_EXT;
    }

    function extDisplay(key) {
        return key === UNKNOWN_EXT ? '无扩展名' : '.' + key;
    }

    function categoryOf(key, fallback) {
        const ext = key === UNKNOWN_EXT ? '' : key;
        try {
            return (typeof fallback === 'function' && fallback(ext)) || '其他';
        } catch (e) {
            return '其他';
        }
    }

    function metricValue(row, metric) {
        return metric === 'size' ? (Number(row.size) || 0) : (Number(row.count) || 0);
    }

    /**
     * 聚合文件：
     * rows: [{ key, ext, category, count, size }]
     * categories: [{ name, count, size, rows }]
     */
    function aggregate(files, categoryFn) {
        const map = new Map();
        let totalSize = 0;
        for (const file of files || []) {
            const key = extKey(file && file.ext);
            let row = map.get(key);
            if (!row) {
                row = { key, ext: key, category: categoryOf(key, categoryFn), count: 0, size: 0 };
                map.set(key, row);
            }
            row.count += 1;
            row.size += Number(file && file.size) || 0;
            totalSize += Number(file && file.size) || 0;
        }
        const rows = [...map.values()].sort((a, b) => b.count - a.count || b.size - a.size || a.key.localeCompare(b.key));
        const cmap = new Map();
        for (const row of rows) {
            let cat = cmap.get(row.category);
            if (!cat) {
                cat = { name: row.category, count: 0, size: 0, rows: [] };
                cmap.set(row.category, cat);
            }
            cat.count += row.count;
            cat.size += row.size;
            cat.rows.push(row);
        }
        const categories = [...cmap.values()].sort((a, b) => {
            const order = CATEGORY_ORDER.indexOf(a.name) - CATEGORY_ORDER.indexOf(b.name);
            if (order !== 0) return order;
            return b.count - a.count;
        });
        const total = rows.reduce((sum, r) => sum + r.count, 0);
        const top = rows[0] || null;
        return {
            rows,
            categories,
            total,
            totalSize,
            typeCount: rows.length,
            top,
        };
    }

    function filteredRows() {
        if (!state) return [];
        const q = state.query.trim().toLowerCase();
        return state.rows.filter((row) => {
            if (state.categoryFilter && row.category !== state.categoryFilter) return false;
            if (!q) return true;
            return row.ext.toLowerCase().includes(q) || row.category.toLowerCase().includes(q);
        });
    }

    function buildTree(rows) {
        const metric = state.metric;
        const total = rows.reduce((sum, r) => sum + metricValue(r, metric), 0);
        const grouped = new Map();
        for (const row of rows) {
            if (!grouped.has(row.category)) grouped.set(row.category, []);
            grouped.get(row.category).push(row);
        }
        const children = [];
        for (const [name, list] of grouped) {
            list.sort((a, b) => metricValue(b, metric) - metricValue(a, metric));
            const kept = [];
            let restCount = 0;
            let restSize = 0;
            list.forEach((row, index) => {
                const share = total > 0 ? metricValue(row, metric) / total : 0;
                if (index < TOP_PER_CATEGORY || share >= MIN_SHARE) {
                    kept.push({
                        key: row.key,
                        label: extDisplay(row.key),
                        category: name,
                        count: row.count,
                        size: row.size,
                        isOther: false,
                    });
                } else {
                    restCount += row.count;
                    restSize += row.size;
                }
            });
            if (restCount > 0) {
                kept.push({
                    key: name + '__other',
                    label: name === '其他' ? '其他类型' : '其他',
                    category: name,
                    count: restCount,
                    size: restSize,
                    isOther: true,
                });
            }
            const catCount = kept.reduce((sum, r) => sum + r.count, 0);
            const catSize = kept.reduce((sum, r) => sum + r.size, 0);
            children.push({
                name,
                category: name,
                count: catCount,
                size: catSize,
                children: kept,
            });
        }
        children.sort((a, b) => {
            const va = metric === 'size' ? a.size : a.count;
            const vb = metric === 'size' ? b.size : b.count;
            return vb - va;
        });
        return { name: 'root', children };
    }

    function renderKpis() {
        const kpis = rootEl.querySelector('[data-ref="kpis"]');
        if (!kpis) return;
        const top = state.summary.top;
        const topLabel = top ? `${extDisplay(top.key)}` : '—';
        kpis.innerHTML = `
            <div class="ftd-kpi">
                <div class="ftd-kpi-label">文件总数</div>
                <div class="ftd-kpi-value">${fmtInt(state.summary.total)}</div>
                <div class="ftd-kpi-foot">当前目录全部文件</div>
            </div>
            <div class="ftd-kpi">
                <div class="ftd-kpi-label">类型数量</div>
                <div class="ftd-kpi-value">${fmtInt(state.summary.typeCount)}</div>
                <div class="ftd-kpi-foot">按扩展名去重</div>
            </div>
            <div class="ftd-kpi">
                <div class="ftd-kpi-label">最多类型</div>
                <div class="ftd-kpi-value" title="${esc(topLabel)}">${esc(topLabel)}</div>
                <div class="ftd-kpi-foot">${top ? `${fmtInt(top.count)} 个文件 · ${fmtPct(top.count, state.summary.total)}` : '暂无数据'}</div>
            </div>
            <div class="ftd-kpi">
                <div class="ftd-kpi-label">文件总大小</div>
                <div class="ftd-kpi-value">${fmtSize(state.summary.totalSize)}</div>
                <div class="ftd-kpi-foot">按扫描数据汇总</div>
            </div>`;
    }

    function truncate(text, maxChars) {
        const chars = [...String(text)];
        if (chars.length <= maxChars) return String(text);
        return chars.slice(0, Math.max(1, maxChars - 1)).join('') + '…';
    }

    function renderTreemap() {
        if (!state) return;
        const host = rootEl.querySelector('[data-ref="treemap"]');
        if (!host) return;
        const width = Math.max(320, host.clientWidth || 640);
        const height = Math.max(240, host.clientHeight || 400);
        const rows = filteredRows();
        host.innerHTML = '';
        if (!rows.length) {
            host.innerHTML = '<div class="ftd-empty">没有匹配的文件类型</div>';
            return;
        }

        const tree = buildTree(rows);
        const root = d3.hierarchy(tree).sum((d) => (d.children ? 0 : metricValue(d, state.metric)));
        d3.treemap()
            .size([width, height])
            .paddingOuter(3)
            .paddingInner(4)
            .paddingTop((d) => (d.depth === 1 ? 20 : 0))
            .round(true)
            .tile(d3.treemapSquarify.ratio(1.3))(root);

        const svg = d3.select(host).append('svg')
            .attr('viewBox', `0 0 ${width} ${height}`)
            .attr('width', width)
            .attr('height', height);
        const layer = svg.append('g');

        const catNodes = root.children || [];
        const catRank = new Map(catNodes.map((n, i) => [n.data.name, i]));

        // 分类组：外框 + 标题
        layer.selectAll('g.ftd-cat')
            .data(catNodes)
            .join('g')
            .attr('class', 'ftd-cat')
            .attr('transform', (d) => `translate(${d.x0},${d.y0})`)
            .each(function (d) {
                const g = d3.select(this);
                const w = d.x1 - d.x0;
                const h = d.y1 - d.y0;
                g.append('rect')
                    .attr('width', w)
                    .attr('height', h)
                    .attr('rx', 5)
                    .attr('fill', CATEGORY_COLORS[d.data.name] || CATEGORY_COLORS['其他'])
                    .attr('fill-opacity', 0.08)
                    .attr('stroke', CATEGORY_COLORS[d.data.name] || CATEGORY_COLORS['其他'])
                    .attr('stroke-opacity', 0.22);
                if (w > 56 && h > 26) {
                    g.append('text')
                        .attr('x', 6)
                        .attr('y', 13)
                        .attr('font-size', 12)
                        .attr('font-weight', 600)
                        .attr('fill', CATEGORY_COLORS[d.data.name] || CATEGORY_COLORS['其他'])
                        .text(truncate(d.data.name, Math.floor((w - 12) / 12)))
                        .style('cursor', 'pointer')
                        .on('click', (event) => {
                            event.stopPropagation();
                            toggleCategory(d.data.name);
                        })
                        .append('title')
                        .text(d.data.name + '（点击筛选）');
                }
            });

        const leaves = root.leaves();
        const leaf = layer.selectAll('g.ftd-leaf')
            .data(leaves)
            .join('g')
            .attr('class', 'ftd-leaf')
            .attr('transform', (d) => `translate(${d.x0},${d.y0})`);

        leaf.append('rect')
            .attr('class', 'ftd-cell')
            .attr('width', (d) => Math.max(0, d.x1 - d.x0))
            .attr('height', (d) => Math.max(0, d.y1 - d.y0))
            .attr('rx', 4)
            .attr('fill', (d) => {
                const base = d3.color(CATEGORY_COLORS[d.data.category] || CATEGORY_COLORS['其他']);
                const rank = catRank.get(d.data.category) || 0;
                return base.brighter(Math.min(0.45, rank * 0.05)).formatHex();
            })
            .attr('fill-opacity', 0.9)
            .attr('stroke', (d) => (state.selectedExt === d.data.key ? '#165bfa' : 'rgba(255,255,255,.65)'))
            .attr('stroke-width', (d) => (state.selectedExt === d.data.key ? 2.5 : 1))
            .attr('tabindex', 0)
            .attr('role', 'button')
            .attr('aria-label', (d) => `${d.data.label}，${d.data.count} 个文件`)
            .style('cursor', 'pointer')
            .on('mousemove', (event, d) => showTip(event, d))
            .on('mouseleave', hideTip)
            .on('click', (event, d) => {
                event.stopPropagation();
                if (d.data.isOther) {
                    toggleCategory(d.data.category);
                } else {
                    selectExt(d.data.key);
                }
            })
            .on('keydown', (event, d) => {
                if (event.key === 'Enter' || event.key === ' ') {
                    event.preventDefault();
                    if (d.data.isOther) {
                        toggleCategory(d.data.category);
                    } else {
                        selectExt(d.data.key);
                    }
                }
            });

        leaf.each(function (d) {
            const g = d3.select(this);
            const w = d.x1 - d.x0;
            const h = d.y1 - d.y0;
            if (w < 26 || h < 18) return;
            const dark = isDarkColor(CATEGORY_COLORS[d.data.category] || CATEGORY_COLORS['其他']);
            const color = dark ? '#ffffff' : '#0f172a';
            const shadow = dark ? 'rgba(0,0,0,.35)' : 'rgba(255,255,255,.5)';
            const maxChars = Math.max(2, Math.floor((w - 10) / 6.2));
            const showCount = w >= 64 && h >= 44;
            g.append('text')
                .attr('x', 6)
                .attr('y', showCount ? 17 : Math.min(h - 5, 17))
                .attr('font-size', 12)
                .attr('font-weight', 600)
                .attr('fill', color)
                .attr('stroke', shadow)
                .attr('stroke-width', 0.6)
                .attr('paint-order', 'stroke')
                .text(truncate(d.data.label, maxChars));
            if (showCount) {
                g.append('text')
                    .attr('x', 6)
                    .attr('y', 33)
                    .attr('font-size', 12)
                    .attr('fill', color)
                    .attr('fill-opacity', 0.88)
                    .attr('stroke', shadow)
                    .attr('stroke-width', 0.4)
                    .attr('paint-order', 'stroke')
                    .text(`${fmtInt(d.data.count)} · ${fmtPct(d.data.count, state.summary.total)}`);
            }
        });
    }

    function isDarkColor(hex) {
        try {
            const c = d3.color(hex);
            const rgb = c.rgb();
            return (rgb.r * 299 + rgb.g * 587 + rgb.b * 114) / 1000 < 150;
        } catch (e) {
            return false;
        }
    }

    function renderRank() {
        const host = rootEl.querySelector('[data-ref="rank"]');
        if (!host) return;
        const rows = filteredRows().slice().sort((a, b) => metricValue(b, state.metric) - metricValue(a, state.metric));
        const max = rows.length ? Math.max(1, metricValue(rows[0], state.metric)) : 1;
        const visible = state.showAllRank ? rows : rows.slice(0, 10);
        host.innerHTML = '';
        if (!rows.length) {
            host.innerHTML = '<div class="ftd-empty" style="height:8rem">没有匹配的类型</div>';
            return;
        }
        visible.forEach((row, index) => {
            const value = metricValue(row, state.metric);
            const div = document.createElement('div');
            div.className = 'ftd-rank-row' + (state.selectedExt === row.key ? ' selected' : '');
            div.dataset.ext = row.key;
            div.tabIndex = 0;
            div.setAttribute('role', 'button');
            div.setAttribute('aria-label', `${extDisplay(row.key)}，${row.count} 个文件`);
            div.innerHTML = `
                <span class="ftd-rank-index">${index + 1}</span>
                <span class="ftd-rank-main">
                    <span class="ftd-rank-label"><b>${esc(extDisplay(row.key))}</b><span>${esc(row.category)}</span></span>
                    <span class="ftd-rank-track"><span class="ftd-rank-bar" style="width:${(value / max) * 100}%;background:${CATEGORY_COLORS[row.category] || CATEGORY_COLORS['其他']}"></span></span>
                </span>
                <span class="ftd-rank-value">${state.metric === 'size' ? fmtSize(value) : fmtInt(value)}</span>`;
            div.addEventListener('click', () => selectExt(row.key));
            div.addEventListener('keydown', (event) => {
                if (event.key === 'Enter' || event.key === ' ') {
                    event.preventDefault();
                    selectExt(row.key);
                }
            });
            host.appendChild(div);
        });
        if (rows.length > 10) {
            const more = document.createElement('button');
            more.type = 'button';
            more.className = 'ftd-rank-more';
            more.textContent = state.showAllRank ? '收起' : `显示全部 ${rows.length} 项`;
            more.addEventListener('click', () => {
                state.showAllRank = !state.showAllRank;
                renderRank();
            });
            host.appendChild(more);
        }
    }

    function renderTable() {
        const host = rootEl.querySelector('[data-ref="table"]');
        if (!host) return;
        const rows = filteredRows().slice();
        const key = state.sortKey;
        const dir = state.sortDir;
        rows.sort((a, b) => {
            let va;
            let vb;
            if (key === 'ext') {
                va = a.key;
                vb = b.key;
                return va.localeCompare(vb) * dir;
            }
            if (key === 'category') {
                va = a.category;
                vb = b.category;
                return va.localeCompare(vb) * dir;
            }
            va = key === 'size' ? a.size : key === 'avg' ? (a.count ? a.size / a.count : 0) : a.count;
            vb = key === 'size' ? b.size : key === 'avg' ? (b.count ? b.size / b.count : 0) : b.count;
            return (va - vb) * dir;
        });
        const total = state.summary.total;
        const headers = [
            ['ext', '扩展名'],
            ['category', '分类'],
            ['count', '文件数'],
            ['size', '总大小'],
            ['avg', '平均大小'],
        ];
        host.innerHTML = '';
        if (!rows.length) {
            host.innerHTML = '<div class="ftd-empty" style="height:6rem">没有匹配的类型</div>';
            return;
        }
        const wrap = document.createElement('div');
        wrap.className = 'ftd-table-wrap';
        const table = document.createElement('table');
        table.className = 'ftd-table';
        const thead = document.createElement('thead');
        const tr = document.createElement('tr');
        headers.forEach(([k, label]) => {
            const th = document.createElement('th');
            th.textContent = label + (state.sortKey === k ? (state.sortDir > 0 ? ' ↑' : ' ↓') : '');
            th.className = 'sortable' + (k === 'count' || k === 'size' || k === 'avg' ? ' num' : '');
            th.addEventListener('click', () => {
                if (state.sortKey === k) state.sortDir = -state.sortDir;
                else {
                    state.sortKey = k;
                    state.sortDir = k === 'ext' || k === 'category' ? 1 : -1;
                }
                renderTable();
            });
            tr.appendChild(th);
        });
        const thShare = document.createElement('th');
        thShare.textContent = '占比';
        thShare.className = 'num';
        tr.appendChild(thShare);
        thead.appendChild(tr);
        table.appendChild(thead);
        const tbody = document.createElement('tbody');
        rows.forEach((row) => {
            const trb = document.createElement('tr');
            trb.dataset.ext = row.key;
            trb.tabIndex = 0;
            if (state.selectedExt === row.key) trb.className = 'selected';
            trb.innerHTML = `
                <td><span class="ftd-dot" style="background:${CATEGORY_COLORS[row.category] || CATEGORY_COLORS['其他']}"></span>${esc(extDisplay(row.key))}</td>
                <td>${esc(row.category)}</td>
                <td class="num">${fmtInt(row.count)}</td>
                <td class="num">${fmtSize(row.size)}</td>
                <td class="num">${fmtSize(row.count ? row.size / row.count : 0)}</td>
                <td class="num">${fmtPct(row.count, total)}</td>`;
            trb.addEventListener('click', () => selectExt(row.key));
            trb.addEventListener('keydown', (event) => {
                if (event.key === 'Enter' || event.key === ' ') {
                    event.preventDefault();
                    selectExt(row.key);
                }
            });
            tbody.appendChild(trb);
        });
        table.appendChild(tbody);
        wrap.appendChild(table);
        host.appendChild(wrap);
    }

    function renderFiles() {
        const card = rootEl.querySelector('[data-ref="files-card"]');
        if (!card) return;
        if (!state.selectedExt) {
            card.hidden = true;
            card.innerHTML = '';
            return;
        }
        const row = state.rows.find((r) => r.key === state.selectedExt);
        const files = state.files
            .filter((f) => extKey(f.ext) === state.selectedExt)
            .sort((a, b) => (Number(b.size) || 0) - (Number(a.size) || 0));
        if (!row) {
            card.hidden = true;
            return;
        }
        card.hidden = false;
        card.innerHTML = `
            <div class="ftd-files">
                <div class="ftd-files-head">
                    <div class="ftd-card-title">${esc(extDisplay(row.key))} · ${fmtInt(row.count)} 个文件</div>
                    <button type="button" class="ftd-link" data-ref="clear-ext">清除选择</button>
                </div>
                <div class="ftd-files-list"></div>
            </div>`;
        card.querySelector('[data-ref="clear-ext"]').addEventListener('click', () => selectExt(null));
        const list = card.querySelector('.ftd-files-list');
        const limit = 80;
        files.slice(0, limit).forEach((file) => {
            const item = document.createElement('div');
            item.className = 'ftd-file';
            item.title = file.path || file.name || '';
            item.innerHTML = `<span class="ftd-file-name">${esc(file.name || file.path || '(未命名)')}</span><span class="ftd-file-size">${fmtSize(file.size)}</span>`;
            if (typeof state.onOpenFile === 'function') {
                item.addEventListener('click', () => state.onOpenFile(file));
            }
            list.appendChild(item);
        });
        if (files.length > limit) {
            const more = document.createElement('div');
            more.className = 'ftd-file';
            more.style.cursor = 'default';
            more.innerHTML = `<span class="ftd-file-name">还有 ${files.length - limit} 个文件未显示</span>`;
            list.appendChild(more);
        }
    }

    function renderAll() {
        if (!state) return;
        hideTip();
        const sub = rootEl.querySelector('[data-ref="treemap-sub"]');
        if (sub) sub.textContent = state.metric === 'size' ? '面积＝文件总大小' : '面积＝文件数量';
        renderKpis();
        renderTreemap();
        renderRank();
        renderTable();
        renderFiles();
        const badge = rootEl.querySelector('[data-ref="filter-badge"]');
        if (badge) {
            if (state.categoryFilter) {
                badge.hidden = false;
                badge.textContent = `分类：${state.categoryFilter} ×`;
                badge.title = '点击清除分类筛选';
            } else {
                badge.hidden = true;
                badge.textContent = '';
            }
        }
    }

    function selectExt(key) {
        if (!state) return;
        if (key && !state.rows.some((r) => r.key === key)) return;
        state.selectedExt = state.selectedExt === key ? null : key;
        if (state.selectedExt) {
            const row = state.rows.find((r) => r.key === state.selectedExt);
            if (row && state.categoryFilter && row.category !== state.categoryFilter) {
                state.categoryFilter = null;
            }
        }
        renderAll();
    }

    function toggleCategory(name) {
        if (!state) return;
        state.categoryFilter = state.categoryFilter === name ? null : name;
        state.selectedExt = null;
        renderAll();
    }

    function showTip(event, d) {
        if (!tooltipEl) return;
        const row = state.rows.find((r) => r.key === d.data.key);
        const count = row ? row.count : d.data.count;
        const size = row ? row.size : d.data.size;
        const pct = fmtPct(count, state.summary.total);
        tooltipEl.innerHTML = `
            <div><b>${esc(d.data.label)}</b></div>
            <div class="muted">${esc(d.data.category)}</div>
            <div>${fmtInt(count)} 个文件 · ${pct}</div>
            <div>${fmtSize(size)} · 平均 ${fmtSize(count ? size / count : 0)}</div>`;
        tooltipEl.style.display = 'block';
        const pad = 12;
        const tw = tooltipEl.offsetWidth || 200;
        const th = tooltipEl.offsetHeight || 80;
        let left = event.clientX + pad;
        let top = event.clientY + pad;
        if (left + tw > window.innerWidth - 8) left = event.clientX - tw - pad;
        if (top + th > window.innerHeight - 8) top = event.clientY - th - pad;
        tooltipEl.style.left = Math.max(8, left) + 'px';
        tooltipEl.style.top = Math.max(8, top) + 'px';
        highlightExt(d.data.key, true);
    }

    function hideTip() {
        if (tooltipEl) tooltipEl.style.display = 'none';
        highlightExt(null, false);
    }

    function highlightExt(key, on) {
        if (!key) {
            rootEl.querySelectorAll('.ftd-rank-row.hover, .ftd-table tbody tr.hover').forEach((el) => el.classList.remove('hover'));
            return;
        }
        rootEl.querySelectorAll(`[data-ext="${cssEscape(key)}"]`).forEach((el) => {
            el.classList.toggle('hover', on);
        });
    }

    function cssEscape(value) {
        if (window.CSS && typeof window.CSS.escape === 'function') return window.CSS.escape(value);
        return String(value).replace(/["\\]/g, '\\$&');
    }

    function bind() {
        controller = new AbortController();
        const { signal } = controller;

        rootEl.querySelectorAll('[data-metric]').forEach((btn) => {
            btn.addEventListener('click', () => {
                if (state.metric === btn.dataset.metric) return;
                state.metric = btn.dataset.metric;
                rootEl.querySelectorAll('[data-metric]').forEach((b) => b.classList.toggle('active', b === btn));
                state.showAllRank = false;
                renderAll();
            }, { signal });
        });

        const search = rootEl.querySelector('[data-ref="search"]');
        if (search) {
            search.addEventListener('input', () => {
                clearTimeout(searchTimer);
                searchTimer = setTimeout(() => {
                    if (!state) return;
                    state.query = search.value || '';
                    renderAll();
                }, 150);
            }, { signal });
        }

        const badge = rootEl.querySelector('[data-ref="filter-badge"]');
        if (badge) {
            badge.addEventListener('click', () => {
                if (!state.categoryFilter) return;
                state.categoryFilter = null;
                renderAll();
            }, { signal });
        }

        const more = rootEl.querySelector('[data-ref="rank-more"]');
        if (more) {
            more.addEventListener('click', () => {
                state.showAllRank = !state.showAllRank;
                renderRank();
            }, { signal });
        }
    }

    function mount(container, files, opts) {
        if (!container || !window.d3) return;
        destroy();
        const options = opts || {};
        const categoryFn = options.categoryOf;
        state = {
            files: Array.isArray(files) ? files.slice() : [],
            rows: [],
            summary: null,
            metric: 'count',
            query: '',
            categoryFilter: null,
            selectedExt: null,
            sortKey: 'count',
            sortDir: -1,
            showAllRank: false,
            onOpenFile: options.onOpenFile,
        };
        const summary = aggregate(state.files, categoryFn);
        state.rows = summary.rows;
        state.summary = summary;

        rootEl = container;
        rootEl.classList.add('ftd-active');
        rootEl.innerHTML = `
            <div class="ftd-page">
                <div class="ftd-header">
                    <div>
                        <div class="ftd-title">文件类型分布</div>
                        <div class="ftd-sub">面积/长度＝文件数量；点击类型可查看对应文件</div>
                    </div>
                    <div class="ftd-controls">
                        <button type="button" class="ftd-link" data-ref="filter-badge" hidden></button>
                        <div class="ftd-seg" role="group" aria-label="统计维度">
                            <button type="button" data-metric="count" class="active">按数量</button>
                            <button type="button" data-metric="size">按体积</button>
                        </div>
                        <label class="ftd-search">
                            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="11" cy="11" r="7"></circle><path d="m20 20-3.5-3.5"></path></svg>
                            <input type="search" data-ref="search" placeholder="搜索扩展名 / 分类" aria-label="搜索扩展名或分类">
                        </label>
                    </div>
                </div>
                <div class="ftd-kpis" data-ref="kpis"></div>
                <div class="ftd-main">
                    <div class="ftd-card">
                        <div class="ftd-card-head">
                            <div class="ftd-card-title">类型分布</div>
                            <div class="ftd-card-sub" data-ref="treemap-sub">面积＝文件数量</div>
                        </div>
                        <div class="ftd-treemap-wrap">
                            <div class="ftd-treemap" data-ref="treemap"></div>
                        </div>
                    </div>
                    <div class="ftd-card">
                        <div class="ftd-card-head">
                            <div class="ftd-card-title">类型排行</div>
                            <div class="ftd-card-sub">Top 10</div>
                        </div>
                        <div class="ftd-rank" data-ref="rank"></div>
                    </div>
                </div>
                <div class="ftd-card ftd-table-card">
                    <div class="ftd-card-head">
                        <div class="ftd-card-title">类型明细</div>
                        <div class="ftd-card-sub">点击表头排序，点击行查看文件</div>
                    </div>
                    <div data-ref="table"></div>
                </div>
                <div class="ftd-card" data-ref="files-card" hidden></div>
            </div>`;

        tooltipEl = document.createElement('div');
        tooltipEl.className = 'ftd-tooltip';
        tooltipEl.setAttribute('role', 'tooltip');
        rootEl.querySelector('.ftd-page').appendChild(tooltipEl);

        bind();
        renderAll();

        const wrap = rootEl.querySelector('.ftd-treemap-wrap');
        if (wrap && typeof ResizeObserver !== 'undefined') {
            resizeObserver = new ResizeObserver(() => {
                clearTimeout(resizeTimer);
                resizeTimer = setTimeout(() => {
                    if (state) renderTreemap();
                }, 150);
            });
            resizeObserver.observe(wrap);
        }
        requestAnimationFrame(() => {
            if (state) renderTreemap();
        });
    }

    function destroy() {
        if (controller) {
            controller.abort();
            controller = null;
        }
        if (resizeObserver) {
            resizeObserver.disconnect();
            resizeObserver = null;
        }
        clearTimeout(resizeTimer);
        clearTimeout(searchTimer);
        if (tooltipEl && tooltipEl.parentNode) tooltipEl.parentNode.removeChild(tooltipEl);
        tooltipEl = null;
        if (rootEl) {
            rootEl.classList.remove('ftd-active');
            rootEl.innerHTML = '';
        }
        rootEl = null;
        state = null;
    }

    window.FileTypeDistribution = { mount, destroy, aggregate };
})();


