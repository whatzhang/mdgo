/**
 * ===== 全局搜索页（css_js/modules/global-search.js） =====
 *
 * 【定位】Tauri 桌面端（main.html）主内容区的独立视图，入口：
 *   1. 仪表盘工具网格第一格「全局搜索」（DASHBOARD_BUTTON）
 *   2. 全局快捷键 Ctrl/Cmd + K
 *
 * 【检索分层】渐进增强，避免"输入即卡"：
 *   L1 文件名/路径 —— 内存索引（getScanFileList），输入即出（<50ms）
 *   L2 全文 + 语义 —— kb_search_hybrid（向量+BM25+符号+RRF+精排），停顿 350ms 或回车触发
 *   L3 精确子串 —— 对 L2 结果做二次判定，供「精确」通道过滤（0 成本）
 *
 * 【结果维度】文件（聚合）/ 命中（片段）× 类型（文档/代码/图表/媒体）× 通道（语义/BM25/符号/精确）
 *   × 目录 / 标签 / 时间 / 页码 / 符号名。左侧分面 + 中间分组卡片 + 右侧预览，另有「平铺」视图。
 *
 * 【诚实边界】`kb_search_hybrid` 当前无过滤参数 → 分面只在"本次返回的候选集"内过滤（UI 明示）；
 *   索引里没有行号 → 打开后按文本定位（不是精确到行）。
 *
 * 【依赖宿主】main.html 注入的 window.__mdgoHost（见 main.html「全局搜索/反向链接宿主桥」）。
 */
(function () {
    'use strict';

    const H = () => window.__mdgoHost;
    const RECENT_KEY = 'mdgo_global_search_recent_v1';
    const DEBOUNCE_HYBRID = 350;
    const TOP_K = 60;                 // 候选集（分面在其内过滤，UI 明示）
    const MAX_NAME_ROWS = 120;
    const TYPE_ORDER = ['文档', '代码', '图表', '媒体', '其他'];
    const TYPE_ICON = { 文档: 'MD', 代码: '{}', 图表: '图', 媒体: '▶', 其他: '·' };
    const TYPE_CLS = { 文档: 'gs-ico-doc', 代码: 'gs-ico-code', 图表: 'gs-ico-chart', 媒体: 'gs-ico-media', 其他: 'gs-ico-other' };

    // ─────────────────────────── 状态 ───────────────────────────
    const S = {
        q: '',
        mode: 'auto',                 // auto | name | hybrid
        view: 'grouped',              // grouped | flat
        sort: 'relevance',            // relevance | time | name
        filters: { type: '', channel: '', dir: '', tag: '', time: 'any' },
        name: [],                     // L1 结果
        hits: [],                     // L2 结果（归一化后）
        groups: [],                   // 分组后的最终展示数据
        flat: [],                     // 平铺行
        status: 'idle',               // idle | loading | done | empty | error | noindex
        errorMsg: '',
        seq: 0,
        sel: '',                      // 选中键 path#idx
        timings: { l1: 0, l2: 0 },
        index: null,                  // kb_status
        indexAt: 0,
        buildingIndex: false,
        fileIdx: null,
        fileIdxRoot: '',
        pathMeta: null,               // lower(path) -> {mtime,size,type,ext,name}
        recent: [],
        expanded: new Set(),
        hybridPending: false,
        shell: null,
    };

    // ─────────────────────────── 工具 ───────────────────────────
    const esc = (s) => H().escapeHtml(String(s === null || s === undefined ? '' : s));
    const lower = (p) => String(p || '').replace(/\\/g, '/').toLowerCase();
    const norm = (p) => String(p || '').replace(/\\/g, '/');
    const extOf = (p) => (H().getExt ? H().getExt(p) : String(p || '').split('.').pop() || '').toLowerCase();
    const baseOf = (p) => (H().basename ? H().basename(p) : String(p || '').split('/').pop());

    function escapeRe(s) {
        return String(s).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    }

    function fmtSize(n) {
        const b = Number(n) || 0;
        if (b < 1024) return b + 'B';
        if (b < 1024 * 1024) return (b / 1024).toFixed(1) + 'KB';
        return (b / 1048576).toFixed(1) + 'MB';
    }

    function fmtTime(sec) {
        const t = Number(sec) || 0;
        if (!t) return '';
        const d = new Date(t * 1000);
        const now = Date.now();
        const diff = now - d.getTime();
        if (diff < 86400e3) return '今天 ' + String(d.getHours()).padStart(2, '0') + ':' + String(d.getMinutes()).padStart(2, '0');
        if (diff < 2 * 86400e3) return '昨天';
        if (diff < 7 * 86400e3) return Math.floor(diff / 86400e3) + ' 天前';
        return d.getFullYear() + '-' + String(d.getMonth() + 1).padStart(2, '0') + '-' + String(d.getDate()).padStart(2, '0');
    }

    function withinTime(sec) {
        const f = S.filters.time;
        if (f === 'any' || !f) return true;
        const days = Number(f);
        if (!days) return true;
        return (Number(sec) || 0) * 1000 >= Date.now() - days * 86400e3;
    }

    /** 高亮：先转义再包 <mark>（顺序不能反，否则破坏 DOM） */
    function markOrigin(text, q) {
        const safe = esc(text);
        const raw = String(q || '').trim();
        if (!raw) return safe;
        const terms = [...new Set(raw.split(/\s+/).filter((t) => t.length >= 2))];
        if (!terms.length) return safe;
        let out = safe;
        for (const t of terms) {
            const re = new RegExp(escapeRe(esc(t)), 'gi');
            out = out.replace(re, (m) => '<mark>' + m + '</mark>');
        }
        return out;
    }

    // ─────────────────────── 本地文件索引（L1） ───────────────────────

    function buildFileIndex() {
        const root = H().rootName();
        const files = H().files();
        if (S.fileIdx && S.fileIdxRoot === root && S.fileIdx.length === files.length) return;
        const idx = new Array(files.length);

        const meta = new Map();
        for (let i = 0; i < files.length; i++) {
            const f = files[i];
            const p = norm(f.path || f.name);
            const name = String(f.name || baseOf(p));
            const ext = String(f.ext || extOf(p)).toLowerCase();
            const type = H().fileType(ext);
            idx[i] = {
                p,
                name,
                n: name.toLowerCase(),
                l: p.toLowerCase(),
                ext,
                type,
                mt: Number(f.mtime) || 0,
                sz: Number(f.size) || 0,
            };
            meta.set(lower(p), { mtime: idx[i].mt, size: idx[i].sz, type, ext, name });
        }
        S.fileIdx = idx;
        S.pathMeta = meta;
        S.fileIdxRoot = root;
    }

    function searchNames(q) {
        const nq = String(q || '').trim().toLowerCase();
        if (!nq) return [];
        const out = [];
        const list = S.fileIdx || [];
        for (let i = 0; i < list.length; i++) {
            const f = list[i];
            let sc = 0;
            if (f.n === nq) sc = 130;
            else if (f.n.startsWith(nq)) sc = 110;
            else if (f.n.includes(nq)) sc = 84;
            else if (f.l.includes(nq)) sc = 52;
            else continue;
            // 扩展名相等 / 词首匹配略加分（"rs" 优先 rust 文件）
            if (f.ext && nq === f.ext) sc += 6;
            out.push({ kind: 'name', path: f.p, name: f.name, ext: f.ext, type: f.type, mtime: f.mt, size: f.sz, sc });
        }
        out.sort((a, b) => b.sc - a.sc || b.mtime - a.mtime || a.path.localeCompare(b.path));
        return out.slice(0, MAX_NAME_ROWS);
    }

    // ─────────────────────── 混合检索（L2） ───────────────────────

    function parseTagList(raw) {
        if (!raw) return [];
        try {
            const arr = JSON.parse(raw);
            return Array.isArray(arr) ? arr.filter((x) => typeof x === 'string').slice(0, 6) : [];
        } catch (e) {
            return [];
        }
    }

    function channelsOf(h) {
        const ch = [];
        if (h.symbol_name) ch.push('symbol');
        if ((Number(h.score_bm25) || 0) > 0) ch.push('bm25');
        if ((Number(h.score_vec) || 0) > 0) ch.push('dense');
        if (typeof h.score_rerank === 'number') ch.push('rerank');
        return ch;
    }

    function normalizeHits(hits) {
        const out = [];
        for (let i = 0; i < hits.length; i++) {
            const h = hits[i] || {};
            const path = norm(h.doc_name || '');
            if (!path) continue;
            const ext = extOf(path);
            const meta = (S.pathMeta && S.pathMeta.get(lower(path))) || {};
            const text = String(h.text || '');
            const ch = channelsOf(h);
            // 精确通道：查询串在片段中真出现（不是语义漂移）
            const q = S.q.trim().toLowerCase();
            const exact = q.length >= 2 && text.toLowerCase().includes(q);
            if (exact) ch.push('exact');
            out.push({
                kind: 'hit',
                uid: i,
                path,
                name: meta.name || baseOf(path),
                ext,
                type: meta.type || H().fileType(ext),
                mtime: meta.mtime || 0,
                size: meta.size || 0,
                text,
                ch,
                exact,
                score: typeof h.score_rerank === 'number' ? h.score_rerank : (Number(h.score) || 0),
                page: h.page_start
                    ? (h.page_end && h.page_end !== h.page_start ? h.page_start + '-' + h.page_end : String(h.page_start))
                    : '',
                symbol: h.symbol_name || '',
                symbolKind: h.symbol_kind || '',
                chunkType: h.chunk_type || '',
                tags: parseTagList(h.tags),
            });
        }
        return out;
    }

    async function runHybrid() {
        if (!hybridReady()) return;
        const q = S.q.trim();
        if (q.length < 2) return;
        const seq = ++S.seq;
        S.hybridPending = true;
        renderMeta();
        const t0 = performance.now();
        try {
            const raw = await H().searchHybrid(q, TOP_K);
            if (seq !== S.seq) return;                  // 旧响应丢弃
            S.hits = normalizeHits(Array.isArray(raw) ? raw : []);
            S.timings.l2 = Math.round(performance.now() - t0);
            S.status = 'done';
        } catch (e) {
            if (seq !== S.seq) return;
            S.hits = [];
            S.timings.l2 = Math.round(performance.now() - t0);
            S.status = 'error';
            S.errorMsg = (e && (e.message || e)) + '';
        } finally {
            if (seq === S.seq) {
                S.hybridPending = false;
                rebuildResults();
                renderAll();
            }
        }
    }

    let _hybridTimer = null;
    function scheduleHybrid(immediate) {
        clearTimeout(_hybridTimer);
        if (S.mode === 'name') return;
        if (!hybridReady()) return;
        const q = S.q.trim();
        if (q.length < 2) return;
        if (immediate) { runHybrid(); return; }
        _hybridTimer = setTimeout(runHybrid, DEBOUNCE_HYBRID);
    }

    function hybridReady() {
        return !!(S.index && S.index.status === 'indexed' && (Number(S.index.chunk_count) || 0) > 0);
    }

    async function ensureIndexStatus(force) {
        if (!force && S.index && Date.now() - S.indexAt < 60000) return S.index;
        try {
            S.index = await H().kbStatus();
            S.indexAt = Date.now();
        } catch (e) {
            S.index = { status: 'unknown', error: String((e && e.message) || e) };
            S.indexAt = Date.now();
        }
        return S.index;
    }

    // ─────────────────────── 聚合 / 分面 ───────────────────────

    function rebuildResults() {
        // 1) 合并：文件名命中（L1）+ 片段命中（L2）
        const nameRows = S.name.filter((n) => withinTime(n.mtime));
        const hitRows = S.hits.filter((h) => withinTime(h.mtime));

        const groups = new Map();
        for (const n of nameRows) {
            if (!groups.has(n.path)) {
                groups.set(n.path, {
                    path: n.path, name: n.name, ext: n.ext, type: n.type,
                    mtime: n.mtime, size: n.size, hits: [], best: 120,
                    channels: new Set(['name']), nameHit: true,
                });
            }
        }
        for (const h of hitRows) {
            let g = groups.get(h.path);
            if (!g) {
                g = {
                    path: h.path, name: h.name, ext: h.ext, type: h.type,
                    mtime: h.mtime, size: h.size, hits: [], best: h.score,
                    channels: new Set(), nameHit: false,
                };
                groups.set(h.path, g);
            }
            g.hits.push(h);
            g.best = Math.max(g.best, h.score);
            for (const c of h.ch) g.channels.add(c);
        }

        // 2) 过滤
        const { type, channel, dir, tag } = S.filters;
        let arr = [...groups.values()];
        if (type) arr = arr.filter((g) => g.type === type);
        if (channel) {
            arr = arr.filter((g) => {
                if (channel === 'name') return g.nameHit;
                return g.hits.some((h) => h.ch.includes(channel));
            });
            if (channel !== 'name') {
                arr = arr.map((g) => ({ ...g, hits: g.hits.filter((h) => h.ch.includes(channel)) }));
            }
        }
        if (dir) arr = arr.filter((g) => dirOf(g.path) === dir);
        if (tag) arr = arr.filter((g) => g.hits.some((h) => h.tags.includes(tag)) || (g.tagHit && g.tagHit.includes(tag)));

        // 3) 排序
        if (S.sort === 'time') arr.sort((a, b) => b.mtime - a.mtime);
        else if (S.sort === 'name') arr.sort((a, b) => a.name.localeCompare(b.name, 'zh-Hans-CN'));
        else if (channel === 'name') arr.sort((a, b) => (b.hits.length + (b.nameHit ? 1 : 0)) - (a.hits.length + (a.nameHit ? 1 : 0)) || b.best - a.best);
        else arr.sort((a, b) => b.best - a.best || b.hits.length - a.hits.length);

        // 4) 类型分区（分组视图）
        const sections = new Map(TYPE_ORDER.map((t) => [t, []]));
        for (const g of arr) {
            const t = TYPE_ORDER.includes(g.type) ? g.type : '其他';
            sections.get(t).push(g);
        }
        S.groups = [...sections.entries()].filter(([, list]) => list.length).map(([t, list]) => ({ type: t, list }));

        // 5) 平铺行
        const flat = [];
        for (const g of arr) {
            if (g.nameHit) flat.push({ path: g.path, name: g.name, type: g.type, mtime: g.mtime, kind: 'name', text: '', tags: [] });
            for (const h of g.hits) flat.push(h);
        }
        S.flat = flat;

        // 6) 状态
        const total = arr.length;
        if (!S.q.trim()) S.status = 'idle';
        else if (total === 0 && !S.hybridPending) S.status = (S.hits.length || S.name.length) ? 'empty' : (S.status === 'error' ? 'error' : 'empty');
        else S.status = 'done';

        // 7) 选中项校正（支持 path#file 这种"整篇文件"选中键）
        if (S.sel) {
            const fileKey = S.sel.endsWith('#file');
            const ok = fileKey ? !!findGroup(S.sel.slice(0, -5)) : flat.some((r) => rowKey(r) === S.sel);
            if (!ok) S.sel = '';
        }
    }

    function rowKey(row) {
        return row.kind === 'name' ? row.path + '#name' : row.path + '#h' + (row.uid === undefined ? (row.text || '').slice(0, 16) : row.uid);
    }

    function dirOf(path) {
        const i = String(path || '').lastIndexOf('/');
        if (i <= 0) return '/';
        return path.slice(0, i) + '/';
    }

    function topDirs(limit) {
        const map = new Map();
        for (const g of allGroups()) {
            const d = dirOf(g.path);
            map.set(d, (map.get(d) || 0) + 1);
        }
        return [...map.entries()].sort((a, b) => b[1] - a[1]).slice(0, limit);
    }

    function allGroups() {
        const out = [];
        for (const sec of S.groups) for (const g of sec.list) out.push(g);
        return out;
    }

    function topTags(limit) {
        const map = new Map();
        for (const g of allGroups()) {
            for (const h of g.hits) for (const t of h.tags) map.set(t, (map.get(t) || 0) + 1);
        }
        return [...map.entries()].sort((a, b) => b[1] - a[1]).slice(0, limit);
    }

    function facetCounts() {
        const counts = { type: new Map(), channel: new Map() };
        for (const g of allGroups()) {
            counts.type.set(g.type, (counts.type.get(g.type) || 0) + 1);
            if (g.nameHit) counts.channel.set('name', (counts.channel.get('name') || 0) + 1);
            for (const h of g.hits) for (const c of h.ch) counts.channel.set(c, (counts.channel.get(c) || 0) + 1);
        }
        return counts;
    }

    // ─────────────────────── 渲染 ───────────────────────

    function buildShell() {
        const root = H().container();
        if (!root) return false;
        root.innerHTML = `
<div class="gs-wrap">
  <div class="gs-header">
    <div class="gs-input-row">
      <span class="gs-input-ico">
        <svg viewBox="0 0 24 24" width="17" height="17" fill="currentColor"><path d="M15.5 14h-.79l-.28-.27A6.471 6.471 0 0 0 16 9.5 6.5 6.5 0 1 0 9.5 16c1.61 0 3.09-.59 4.23-1.57l.27.28v.79l5 4.99L20.49 19l-4.99-5zm-6 0C7.01 14 5 11.99 5 9.5S7.01 5 9.5 5 14 7.01 14 9.5 11.99 14 9.5 14z"/></svg>
      </span>
      <input id="gs-input" type="text" autocomplete="off" spellcheck="false"
             placeholder="搜索文件名、正文、语义…">
      <button class="gs-clear" id="gs-clear" title="清空">✕</button>

    </div>
    <div class="gs-chips" id="gs-chips"></div>
    <div class="gs-meta" id="gs-meta"></div>
  </div>
  <div class="gs-body">
    <aside class="gs-facets" id="gs-facets"></aside>
    <main class="gs-results" id="gs-results"></main>
    <aside class="gs-preview" id="gs-preview"></aside>
  </div>
  <div class="gs-status" id="gs-status"></div>
</div>`;

        S.shell = {
            input: root.querySelector('#gs-input'),
            clear: root.querySelector('#gs-clear'),
            chips: root.querySelector('#gs-chips'),
            meta: root.querySelector('#gs-meta'),
            facets: root.querySelector('#gs-facets'),
            results: root.querySelector('#gs-results'),
            preview: root.querySelector('#gs-preview'),
            status: root.querySelector('#gs-status'),
        };
        bindShellEvents();
        return true;
    }

    function bindShellEvents() {
        const { input, clear, chips, facets, results, status } = S.shell;

        input.addEventListener('input', () => {
            S.q = input.value;
            clear.style.display = S.q ? 'block' : 'none';
            onQueryChanged();
        });

        input.addEventListener('keydown', (e) => {
            // 仅保留 Esc（清空 / 退出）；上下键选择、Enter 打开、Tab 切换视图已按需求移除
            if (e.key === 'Escape') {
                e.preventDefault();
                if (S.q) { input.value = ''; S.q = ''; clear.style.display = 'none'; onQueryChanged(); }
                else H().exitSearch();
            }
        });

        clear.addEventListener('click', () => {
            input.value = '';
            S.q = '';
            clear.style.display = 'none';
            input.focus();
            onQueryChanged();
        });

        chips.addEventListener('click', (e) => {
            const chip = e.target.closest('[data-gs-chip]');
            if (!chip) return;
            const group = chip.getAttribute('data-gs-chip');
            const value = chip.getAttribute('data-gs-val') || '';
            if (group === 'mode') {
                S.mode = value;
                if (value !== 'name' && hybridReady()) scheduleHybrid(true);
            } else if (group === 'view') {
                S.view = value;
            } else if (group === 'sort') {
                S.sort = value;
            } else if (group === 'time') {
                S.filters.time = value;
            } else if (group === 'type') {
                S.filters.type = S.filters.type === value ? '' : value;
            }
            rebuildResults();
            renderAll();
            input.focus();
        });

        facets.addEventListener('click', (e) => {
            const item = e.target.closest('[data-gs-facet]');
            if (!item) return;
            const kind = item.getAttribute('data-gs-facet');
            const value = item.getAttribute('data-gs-val') || '';
            if (kind === 'type') S.filters.type = S.filters.type === value ? '' : value;
            else if (kind === 'channel') S.filters.channel = S.filters.channel === value ? '' : value;
            else if (kind === 'dir') S.filters.dir = S.filters.dir === value ? '' : value;
            else if (kind === 'tag') S.filters.tag = S.filters.tag === value ? '' : value;
            rebuildResults();
            renderAll();
            input.focus();
        });

        // 单击 = 选中预览；双击（同一项 400ms 内两次点击）= 进入文件页面。
        let _lastPick = { key: '', at: 0 };
        /**
         * 就地更新选中态。
         * 注意：不能用 querySelector('[data-gs-row="…"]') —— 键里含 # / . / 中文，
         * CSS 转义在不同实现下可能匹配失败（表现为"点了没有蓝框"）。
         * 这里改为遍历属性做**字符串相等**比较，并保留"找不到就重绘一次"的兜底。
         */
        function selectKey(key) {
            S.sel = key || '';
            const box = S.shell.results;
            const olds = box.querySelectorAll('.sel');
            for (let i = 0; i < olds.length; i++) olds[i].classList.remove('sel');
            if (!key) { renderPreview(); return; }

            const cards = box.querySelectorAll('[data-gs-file]');
            const rows = box.querySelectorAll('[data-gs-row]');
            if (key.endsWith('#file')) {
                const path = key.slice(0, -5);
                for (let i = 0; i < cards.length; i++) {
                    if (cards[i].getAttribute('data-gs-file') === path) { cards[i].classList.add('sel'); break; }
                }
            } else {
                let hit = null;
                for (let i = 0; i < rows.length; i++) {
                    if (rows[i].getAttribute('data-gs-row') === key) { hit = rows[i]; break; }
                }
                if (hit) {
                    hit.classList.add('sel');
                    const card = hit.closest('.gs-file');
                    if (card) card.classList.add('sel');
                } else {
                    // 兜底：让模板重绘一次（模板同时支持 path#file 与命中键）
                    renderResults();
                }
            }
            renderPreview();
        }

        /** 同一项快速点两次 → 打开；否则只选中 */
        function pick(key, opener) {
            const now = Date.now();
            if (key && _lastPick.key === key && now - _lastPick.at <= 400) {
                _lastPick = { key: '', at: 0 };
                opener();
                return;
            }
            _lastPick = { key: key || '', at: now };
            selectKey(key);
        }

        results.addEventListener('click', (e) => {
            const more = e.target.closest('[data-gs-more]');
            if (more) {
                const key = more.getAttribute('data-gs-more');
                if (S.expanded.has(key)) S.expanded.delete(key); else S.expanded.add(key);
                renderResults();
                return;
            }
            const head = e.target.closest('[data-gs-open]');
            if (head) {
                const path = head.getAttribute('data-gs-open') || '';
                const g = findGroup(path);
                const key = path + '#file';
                pick(key, () => openPath(path));
                return;
            }
            const row = e.target.closest('[data-gs-row]');
            if (row) {
                const key = row.getAttribute('data-gs-row');
                pick(key, () => openRow(findRow(key)));
                return;
            }
            const empty = e.target.closest('[data-gs-act]');
            if (empty) {
                const act = empty.getAttribute('data-gs-act');
                if (act === 'retry') { runHybrid(); }
                else if (act === 'build') { startIndexing(); }
                else if (act === 'recent') {
                    const q = empty.getAttribute('data-gs-q') || '';
                    S.shell.input.value = q;
                    S.q = q;
                    S.shell.clear.style.display = 'block';
                    onQueryChanged();
                } else if (act === 'dashboard') { H().exitSearch(); }
                else if (act === 'relax') {
                    S.filters = { type: '', channel: '', dir: '', tag: '', time: 'any' };
                    rebuildResults();
                    renderAll();
                }
            }
        });

        status.addEventListener('click', (e) => {
            const btn = e.target.closest('[data-gs-act]');
            if (!btn) return;
            const act = btn.getAttribute('data-gs-act');
            if (act === 'build') startIndexing();
            else if (act === 'refresh') { ensureIndexStatus(true).then(() => { rebuildResults(); renderAll(); }); }
        });

        S.shell.preview.addEventListener('click', (e) => {
            const btn = e.target.closest('[data-gs-act]');
            if (!btn) return;
            const act = btn.getAttribute('data-gs-act');
            const row = findRow(S.sel);
            if (!row) return;
            if (act === 'open') openRow(row);
            else if (act === 'copy-path') {
                copyText(row.path);
                H().notify('✓ 已复制路径', 'success', 1400);
            } else if (act === 'copy-snippet') {
                copyText(row.text || row.name || '');
                H().notify('✓ 已复制片段', 'success', 1400);
            } else if (act === 'locate') {
                openRow(row);
            }
        });
    }

    function copyText(t) {
        try {
            if (navigator.clipboard && navigator.clipboard.writeText) navigator.clipboard.writeText(t);
        } catch (e) { /* 忽略 */ }
    }

    function onQueryChanged() {
        S.seq++;
        S.hybridPending = false;
        const q = S.q.trim();
        if (!q) {
            S.name = []; S.hits = []; S.groups = []; S.flat = []; S.status = 'idle'; S.sel = '';
            renderAll();
            return;
        }
        const t0 = performance.now();
        S.name = searchNames(q);
        S.timings.l1 = Math.round(performance.now() - t0);
        S.timings.l2 = 0;
        S.hits = [];                 // 旧结果立即失效，避免"输入新词却显示旧命中"
        S.status = 'loading';
        rebuildResults();
        // 深度检索待发出时保持 loading，避免"暂无结果"闪现后又被结果顶掉
        if (!allGroups().length && hybridReady() && S.mode !== 'name') S.status = 'loading';
        renderAll();
        scheduleHybrid(false);
        if (!hybridReady()) ensureIndexStatus().then(() => { renderStatus(); renderMeta(); if (hybridReady()) scheduleHybrid(false); });
    }

    function renderAll() {
        if (!S.shell) return;
        renderChips();
        renderMeta();
        renderFacets();
        renderResults();
        renderPreview();
        renderStatus();
    }

    function chip(label, group, value, on, extra) {
        return `<button class="gs-chip${on ? ' on' : ''}" data-gs-chip="${group}" data-gs-val="${esc(value)}">${esc(label)}${extra || ''}</button>`;
    }

    function renderChips() {
        const c = S.shell.chips;
        const hasHybrid = hybridReady();
        c.innerHTML =
            '<span class="gs-chip-group"><span class="gs-chip-label">检索</span>' +
            chip('自动', 'mode', 'auto', S.mode === 'auto') +
            chip('仅文件名', 'mode', 'name', S.mode === 'name') +
            chip('深度', 'mode', 'hybrid', S.mode === 'hybrid') +
            '</span>' +
            '<span class="gs-chip-group"><span class="gs-chip-label">视图</span>' +
            chip('分组', 'view', 'grouped', S.view === 'grouped') +
            chip('平铺', 'view', 'flat', S.view === 'flat') +
            '</span>' +
            '<span class="gs-chip-group"><span class="gs-chip-label">排序</span>' +
            chip('相关度', 'sort', 'relevance', S.sort === 'relevance') +
            chip('时间', 'sort', 'time', S.sort === 'time') +
            chip('名称', 'sort', 'name', S.sort === 'name') +
            '</span>' +
            '<span class="gs-chip-group"><span class="gs-chip-label">时间</span>' +
            chip('不限', 'time', 'any', S.filters.time === 'any') +
            chip('7 天', 'time', '7', S.filters.time === '7') +
            chip('30 天', 'time', '30', S.filters.time === '30') +
            '</span>' +
            (hasHybrid ? '' : '<span class="gs-chip-group"><span class="gs-chip-label gs-meta-warn">深度检索需先建立知识库索引</span></span>');
    }

    function renderMeta() {
        const m = S.shell.meta;
        if (!S.q.trim()) {
            m.innerHTML = '';
            return;
        }
        const groups = allGroups();
        const hitCount = groups.reduce((n, g) => n + g.hits.length, 0);
        const nameCount = groups.filter((g) => g.nameHit).length;
        const ch = new Set();
        for (const g of groups) for (const c of g.channels) ch.add(c);
        const chLabel = { name: '文件名', dense: '语义', bm25: 'BM25', symbol: '符号', exact: '精确', rerank: '精排' };
        const usedCh = [...ch].filter((x) => chLabel[x]).map((x) => chLabel[x]);
        const ms = S.timings.l1 + S.timings.l2;
        let html = '<span>命中 <b>' + groups.length + '</b> 个文件 / <b>' + hitCount + '</b> 处片段</span>';
        if (nameCount) html += '<span class="gs-dot">文件名 <b>' + nameCount + '</b></span>';
        if (ms) html += '<span class="gs-dot"><b>' + ms + 'ms</b></span>';
        if (usedCh.length) html += '<span class="gs-dot">通道 ' + usedCh.join('+') + '</span>';
        if (S.hybridPending) html += '<span class="gs-dot">深度检索中…</span>';
        if (S.filters.type || S.filters.channel || S.filters.dir || S.filters.tag || S.filters.time !== 'any') {
            html += '<span class="gs-dot gs-meta-warn">已过滤（在候选集 ' + TOP_K + ' 内）</span>';
        }
        m.innerHTML = html;
    }

    function renderFacets() {
        const box = S.shell.facets;
        const counts = facetCounts();
        const chLabel = { name: '文件名', dense: '语义', bm25: 'BM25', symbol: '符号', exact: '精确' };
        let html = '';

        const typeEntries = TYPE_ORDER.filter((t) => counts.type.get(t)).map((t) => [t, counts.type.get(t)]);
        if (typeEntries.length) {
            html += '<div class="gs-facet-sec"><div class="gs-facet-title">类型</div>';
            for (const [t, n] of typeEntries) {
                html += `<div class="gs-facet-item${S.filters.type === t ? ' on' : ''}" data-gs-facet="type" data-gs-val="${esc(t)}">
                    <span class="gs-facet-name">${esc(t)}</span><span class="gs-facet-count">${n}</span></div>`;
            }
            html += '</div>';
        }

        const chEntries = [...counts.channel.entries()].filter(([k]) => chLabel[k]);
        if (chEntries.length) {
            html += '<div class="gs-facet-sec"><div class="gs-facet-title">命中通道</div>';
            for (const [k, n] of chEntries) {
                html += `<div class="gs-facet-item${S.filters.channel === k ? ' on' : ''}" data-gs-facet="channel" data-gs-val="${esc(k)}">
                    <span class="gs-facet-name">${esc(chLabel[k])}</span><span class="gs-facet-count">${n}</span></div>`;
            }
            html += '</div>';
        }

        const dirs = topDirs(6);
        if (dirs.length > 1) {
            html += '<div class="gs-facet-sec"><div class="gs-facet-title">目录</div>';
            for (const [d, n] of dirs) {
                html += `<div class="gs-facet-item${S.filters.dir === d ? ' on' : ''}" data-gs-facet="dir" data-gs-val="${esc(d)}" title="${esc(d)}">
                    <span class="gs-facet-name">${esc(d === '/' ? '（根目录）' : d)}</span><span class="gs-facet-count">${n}</span></div>`;
            }
            html += '</div>';
        }

        const tags = topTags(8);
        if (tags.length) {
            html += '<div class="gs-facet-sec"><div class="gs-facet-title">标签</div>';
            for (const [t, n] of tags) {
                html += `<div class="gs-facet-item${S.filters.tag === t ? ' on' : ''}" data-gs-facet="tag" data-gs-val="${esc(t)}" title="${esc(t)}">
                    <span class="gs-facet-name">#${esc(t)}</span><span class="gs-facet-count">${n}</span></div>`;
            }
            html += '</div>';
        }

        box.innerHTML = html || '<div class="gs-facet-title">暂无分面</div>';
    }

    function renderResults() {
        const box = S.shell.results;
        const q = S.q.trim();
        if (!q) { box.innerHTML = emptyIdle(); return; }
        if (S.status === 'error') { box.innerHTML = emptyError(); return; }
        if (!allGroups().length) {
            if (S.hybridPending || S.status === 'loading') {
                box.innerHTML = '<div class="gs-loading"><span class="gs-spin"></span><span>检索中…</span></div>' + emptyMaybe();
                return;
            }
            box.innerHTML = emptyNoResult();
            return;
        }
        box.innerHTML = S.view === 'flat' ? renderFlat() : renderGrouped();
    }

    function renderGrouped() {
        let html = '';
        for (const sec of S.groups) {
            html += `<div class="gs-section-title">${esc(sec.type)}<span class="gs-sec-count">${sec.list.length}</span></div>`;
            for (const g of sec.list) html += cardHtml(g);
        }
        return html;
    }

    function cardHtml(g) {
        const key = g.path + '#name';
        const sel = (S.sel === g.path + '#file' || (S.sel && S.sel.startsWith(g.path + '#'))) ? ' sel' : '';
        const shown = S.expanded.has(g.path) ? g.hits : g.hits.slice(0, 3);
        const badges = [];
        if (g.hits.length) badges.push('<span class="gs-badge gs-badge-hit">' + g.hits.length + ' 处</span>');
        if (g.nameHit) badges.push('<span class="gs-badge">文件名</span>');
        const chLabel = { dense: '语义', bm25: 'BM25', symbol: '符号', exact: '精确' };
        const chBadges = [...g.channels].filter((c) => chLabel[c]).slice(0, 3)
            .map((c) => '<span class="gs-badge">' + chLabel[c] + '</span>').join('');

        let html = `<div class="gs-file${sel}" data-gs-file="${esc(g.path)}">
  <div class="gs-file-head" data-gs-open="${esc(g.path)}">
    <span class="gs-file-ico ${TYPE_CLS[g.type] || 'gs-ico-other'}">${esc(TYPE_ICON[g.type] || '·')}</span>
    <span class="gs-file-main">
      <span class="gs-file-name">${markOrigin(g.name, S.q)}</span>
      <span class="gs-file-sub"><span>${esc(g.path)}</span>${g.size ? '<span>' + fmtSize(g.size) + '</span>' : ''}${g.mtime ? '<span>' + fmtTime(g.mtime) + '</span>' : ''}</span>
    </span>
    <span class="gs-file-badges">${chBadges}${badges.join('')}</span>
  </div>`;

        if (shown.length) {
            html += '<div class="gs-hits">';
            for (const h of shown) {
                const hk = rowKey(h);
                const page = h.page ? '<span class="gs-ch">P' + esc(h.page) + '</span>' : '';
                const sym = h.symbol ? '<span class="gs-ch gs-ch-symbol">' + esc(h.symbol) + (h.symbolKind ? ' · ' + esc(h.symbolKind) : '') + '</span>' : '';
                const tagHtml = h.tags.slice(0, 3).map((t) => '<span class="gs-ch">#' + esc(t) + '</span>').join('');
                html += `<div class="gs-hit${S.sel === hk ? ' sel' : ''}" data-gs-row="${esc(hk)}">
  <span class="gs-hit-body">
    <span class="gs-snippet">${markOrigin(snippetOf(h.text), S.q)}</span>
    <span class="gs-hit-tags">${chBadgesOf(h)}${page}${sym}${tagHtml}</span>
  </span>
</div>`;
            }
            if (g.hits.length > 3) {
                const open = S.expanded.has(g.path);
                html += `<div class="gs-hit-more" data-gs-more="${esc(g.path)}">${open ? '收起' : '展开全部 ' + g.hits.length + ' 处'}</div>`;
            }
            html += '</div>';
        }
        html += '</div>';
        return html;
    }

    function chBadgesOf(h) {
        const map = { dense: ['语义', 'gs-ch-dense'], bm25: ['BM25', 'gs-ch-bm25'], symbol: ['符号', 'gs-ch-symbol'], exact: ['精确', 'gs-ch-exact'] };
        return h.ch.filter((c) => map[c]).map((c) => `<span class="gs-ch ${map[c][1]}">${map[c][0]}</span>`).join('');
    }

    function renderFlat() {
        const rows = S.flat;
        if (!rows.length) return emptyNoResult();
        let html = '<div class="gs-section-title">平铺命中<span class="gs-sec-count">' + rows.length + '</span></div>';
        for (const r of rows) {
            const key = rowKey(r);
            const text = r.kind === 'name' ? '（文件名命中）' : snippetOf(r.text);
            html += `<div class="gs-flat-row${S.sel === key ? ' sel' : ''}" data-gs-row="${esc(key)}">
  <span class="gs-flat-file">${markOrigin(baseOf(r.path), S.q)}</span>
  <span class="gs-flat-text">${markOrigin(text, S.q)}</span>
</div>`;
        }
        return html;
    }

    function snippetOf(text) {
        const t = String(text || '').replace(/\s+/g, ' ').trim();
        return t.length > 220 ? t.slice(0, 220) + '…' : t;
    }

    function renderPreview() {
        const box = S.shell.preview;
        const row = findRow(S.sel);
        if (!row) {
            box.innerHTML = '<div class="gs-pv-empty"></div>';



            return;
        }
        const path = row.path;
        const name = baseOf(path);
        const ext = extOf(path);
        const type = row.type || H().fileType(ext);
        const meta = (S.pathMeta && S.pathMeta.get(lower(path))) || row;
        const badges = [];
        badges.push('<span class="gs-badge">' + esc(type) + '</span>');
        badges.push('<span class="gs-badge">' + esc(ext || '?') + '</span>');
        if (meta.size) badges.push('<span class="gs-badge">' + fmtSize(meta.size) + '</span>');
        if (meta.mtime) badges.push('<span class="gs-badge">' + fmtTime(meta.mtime) + '</span>');
        if (row.page) badges.push('<span class="gs-badge">第 ' + esc(row.page) + ' 页</span>');
        if (row.symbol) badges.push('<span class="gs-badge">' + esc(row.symbol) + '</span>');
        const tags = (row.tags || []).map((t) => '<span class="gs-badge">#' + esc(t) + '</span>').join('');

        const body = row.kind === 'name'
            ? '<div class="gs-pv-text">' + esc('文件名命中，打开后可查看内容。') + '</div>'
            : `<div class="gs-pv-text">${row.text ? markOrigin(row.text, S.q) : '（无片段文本）'}</div>`;

        box.innerHTML = `
<div class="gs-pv-title">${esc(name)}</div>
<div class="gs-pv-path">${esc(path)}</div>
<div class="gs-pv-meta">${badges.join('')}${tags}</div>
<div class="gs-pv-sec">命中片段</div>
${body}
<div class="gs-pv-actions">
  <button class="gs-btn gs-btn-primary" data-gs-act="open">打开文件</button>
  <button class="gs-btn" data-gs-act="locate">打开并定位</button>
  <button class="gs-btn" data-gs-act="copy-path">复制路径</button>
  ${row.kind === 'hit' ? '<button class="gs-btn" data-gs-act="copy-snippet">复制片段</button>' : ''}
</div>`;
    }

    function renderStatus() {
        const box = S.shell.status;
        const st = S.index || {};
        let left = '';
        if (S.buildingIndex) {
            left = '<span>正在建立索引…</span><span class="gs-progress on"><span class="gs-progress-bar" id="gs-bar"></span></span><span id="gs-bar-text"></span>';
        } else if (!st.status || st.status === 'unknown' || !(Number(st.chunk_count) > 0)) {
            left = '<span class="gs-meta-warn">知识库尚未建索引：当前仅文件名检索</span>' +
                '<button class="gs-btn" data-gs-act="build">立即建立索引</button>';
        } else {
            const stale = st.stale ? '<span class="gs-meta-warn">索引已过期' +
                (Array.isArray(st.stale_kinds) && st.stale_kinds.length ? '（' + st.stale_kinds.join('/') + '）' : '') + '</span>' : '';
            left = '<span>索引 <b>' + (st.file_count || 0) + '</b> 文件 / <b>' + (st.chunk_count || 0) + '</b> 文本块</span>' +
                stale + '<button class="gs-btn" data-gs-act="refresh">刷新状态</button>';
        }
        box.innerHTML = left + '<span class="gs-spacer"></span>';
    }

    function emptyIdle() {
        const recent = S.recent.slice(0, 10);
        return `<div class="gs-empty">
  <div class="gs-empty-title">全局搜索</div>

  ${recent.length ? '<div class="gs-pv-sec" style="margin-top:6px">最近搜索</div><div class="gs-recent">' +
            recent.map((q) => `<span class="gs-recent-item" data-gs-act="recent" data-gs-q="${esc(q)}">${esc(q)}</span>`).join('') +
            '</div>' : ''}



</div>`;
    }

    function emptyMaybe() {
        return '';
    }

    function emptyNoResult() {
        return `<div class="gs-empty">
  <div class="gs-empty-title">没有匹配结果</div>

  <div class="gs-empty-actions">
    <button class="gs-btn" data-gs-act="relax">清除过滤条件</button>
    ${hybridReady() ? '' : '<button class="gs-btn" data-gs-act="build">建立索引后可语义检索</button>'}
  </div>
</div>`;
    }

    function emptyError() {
        return `<div class="gs-empty">
  <div class="gs-empty-title">检索失败</div>
  <div>${esc(S.errorMsg || '未知错误')}</div>
  <div class="gs-empty-actions"><button class="gs-btn" data-gs-act="retry">重试</button></div>
</div>`;
    }

    // ─────────────────────── 选择 / 打开 ───────────────────────

    /** 按文件路径取分组（卡片头单击时定位到该组首个命中做预览） */
    function findGroup(path) {
        const list = allGroups();
        for (let i = 0; i < list.length; i++) if (list[i].path === path) return list[i];
        return null;
    }

    function findRow(key) {
        if (!key) return null;
        const isFile = key.endsWith('#file');
        const filePath = isFile ? key.slice(0, -5) : '';
        for (const g of allGroups()) {
            if (isFile) {
                if (g.path !== filePath) continue;
                // 整篇文件被选中：优先用首个命中做预览，没有命中则用文件信息
                if (g.hits.length) return g.hits[0];
                return { kind: 'name', path: g.path, name: g.name, type: g.type, mtime: g.mtime, size: g.size, tags: [] };
            }
            if (g.path + '#name' === key) {
                return { kind: 'name', path: g.path, name: g.name, type: g.type, mtime: g.mtime, size: g.size, tags: [] };
            }
            for (const h of g.hits) if (rowKey(h) === key) return h;
        }
        return null;
    }


    async function openRow(row) {
        if (!row) return;
        rememberQuery(S.q.trim());
        const ok = await H().openFile(row.path);
        if (!ok) return;
        const needle = row.kind === 'hit' ? needleOf(row.text) : '';
        if (needle) {
            let tries = 0;
            const tick = () => {
                tries++;
                if (H().revealText(needle)) return;
                if (tries < 16) setTimeout(tick, 110);
            };
            setTimeout(tick, 120);
        }
    }

    function openPath(path) { openRow({ kind: 'name', path, name: baseOf(path) }); }

    function needleOf(text) {
        const raw = String(text || '');
        const lines = raw.split('\n');
        for (const line of lines) {
            const t = line.trim();
            if (!t) continue;
            const cleaned = t.replace(/^[#>*\-+|`]+\s*/, '').replace(/[*_`~\[\]]/g, '').replace(/\s+/g, ' ').trim();
            if (cleaned.length >= 6) return cleaned.slice(0, 40);
        }
        return '';
    }


    // ─────────────────────── 索引建立（内联） ───────────────────────

    async function startIndexing() {
        if (S.buildingIndex) return;
        S.buildingIndex = true;
        renderStatus();
        try {
            await H().kbIndex((percent, message) => {
                const bar = document.getElementById('gs-bar');
                const txt = document.getElementById('gs-bar-text');
                if (bar) bar.style.width = (percent || 0) + '%';
                if (txt) txt.textContent = message || '';
            });
            await ensureIndexStatus(true);
            H().notify('✓ 索引已建立', 'success', 1800);
            if (S.q.trim()) scheduleHybrid(true);
        } catch (e) {
            H().notify('✗ 建立索引失败: ' + ((e && (e.message || e)) + ''), 'error', 4000);
        } finally {
            S.buildingIndex = false;
            rebuildResults();
            renderAll();
        }
    }

    // ─────────────────────── 最近搜索 ───────────────────────

    function loadRecent() {
        try {
            const raw = localStorage.getItem(RECENT_KEY);
            const arr = raw ? JSON.parse(raw) : [];
            S.recent = Array.isArray(arr) ? arr.filter((x) => typeof x === 'string').slice(0, 10) : [];
        } catch (e) { S.recent = []; }
    }

    function rememberQuery(q) {
        if (!q || q.length < 2) return;
        const arr = [q, ...S.recent.filter((x) => x !== q)].slice(0, 10);
        S.recent = arr;
        try { localStorage.setItem(RECENT_KEY, JSON.stringify(arr)); } catch (e) { /* 忽略 */ }
    }

    // ─────────────────────── 入口 ───────────────────────

    async function openGlobalSearch() {
        if (!H() || !H().showSearchView) {
            console.warn('[global-search] 宿主桥缺失，无法打开搜索页');
            return;
        }
        await H().showSearchView();
        if (!S.shell) buildShell();
        loadRecent();
        buildFileIndex();
        // 根目录切换后重置索引相关缓存
        renderAll();
        if (S.shell && S.shell.input) {
            S.shell.input.focus();
            S.shell.input.select();
        }
        ensureIndexStatus().then(() => {
            renderStatus();
            renderChips();
            renderMeta();
            if (hybridReady() && S.q.trim().length >= 2 && !S.hits.length) scheduleHybrid(false);
        });
    }

    window.openGlobalSearch = openGlobalSearch;
    window.mdgoSearch = {
        open: openGlobalSearch,
        stats: () => ({
            status: S.status,
            groups: allGroups().length,
            hits: S.hits.length,
            name: S.name.length,
            index: S.index,
        }),
    };
})();