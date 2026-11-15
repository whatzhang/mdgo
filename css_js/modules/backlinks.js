/**
 * ===== 反向链接面板（css_js/modules/backlinks.js） =====
 *
 * 【定位】Tauri 桌面端（main.html）TOC 右下面板第 4 个 tab：显示"谁链接到了当前文档"。
 *   正向链接已由「Links列表」tab 提供，本模块只做反向链接（inbound）。
 *
 * 【数据来源 / 性能设计】（对应评审结论"复用文档关联图谱数据，但要高性能低内存"）
 *   1. 首次构建优先复用 `文档关联图谱` 的缓存 `.mdgo/index_link_graph.json`（不重复扫盘）；
 *      缓存缺失/过期时退化为一次自己的分块扫描（并发 8，按 md 文件）。
 *   2. 无论来源，落地时都转成**紧凑结构**并写侧车缓存 `.mdgo/index_backlinks.json`：
 *        src:[相对路径] 、mt:[mtime 秒] 、e:[srcIdx,dstIdx,...] 、ln:[行号,...]
 *      内存中再用 **CSR（压缩稀疏行）** 双向索引：
 *        fOff/fDst（出链）、rOff/rSrc（入链，本面板查的就是它）
 *      → 1 万 md / 10 万链接约 1.7MB，查询 O(入度)，不再持有 {nodes,edges} 对象图。
 *   3. 失效：保存 md 时增量重算该文件的出链（`onFileSaved`），打开面板时按 mtime 全量对账，
 *      只重扫变更文件；侧车缓存落后于图谱缓存时也自动重建。
 *   4. 片段/行号按需补全（每个源文件只读一次，并发 ≤3），不阻塞列表首屏。
 *
 * 【依赖宿主】main.html 注入的 window.__mdgoHost（见 main.html「全局搜索/反向链接宿主桥」）。
 */
(function () {
    'use strict';

    const H = () => window.__mdgoHost;
    const SIDECAR = 'index_backlinks.json';
    const SIDECAR_V = 1;

    // ─────────────────────────── 状态 ───────────────────────────
    const BK = {
        ready: false,
        building: null,
        root: '',
        at: 0,
        builtFrom: '',           // '' | 'graph' | 'scan'：本次索引来源（诊断用）
        src: [],                 // string[]：md 相对路径（两端口径一致）
        idx: new Map(),          // lower(path) -> srcIdx（查询用，大小写不敏感）
        exact: new Map(),        // path -> srcIdx（解析用，大小写敏感）
        byName: new Map(),       // lower(文件名去掉 .md) -> srcIdx（同名取首个）
        pathSet: new Set(),      // lower(path) 集合（判断源文件是否仍存在，O(1)）
        mt: null,                // Int32Array：与 src 对齐的 mtime（秒）
        e: null,                 // Int32Array：扁平 [sIdx,tIdx, ...]
        ln: null,                // Int32Array：每条边在源文件中的行号（0=未知）
        fOff: null, fDst: null,  // 出链 CSR
        rOff: null, rSrc: null, rLn: null, // 入链 CSR
        files: null,             // 由 src 延迟构造的 [{path,name}]（宿主解析兜底用）

        pending: null,           // 当前面板上下文
        readErrors: 0,           // 扫描时读取失败的文件数（诊断用）
        lastError: '',
    };

    const lower = (p) => String(p || '').replace(/\\/g, '/').toLowerCase();
    const isMd = (p) => /\.(md|markdown)$/i.test(String(p || ''));
    const baseName = (p) => String(p || '').replace(/\\/g, '/').split('/').pop() || '';
    const stripExt = (p) => baseName(p).replace(/\.[^.]+$/, '');

    // ─────────────────────── 构建 / hydrate ───────────────────────

    /** 从图谱缓存构建（不重复扫盘）：只保留 md→md 的边 */
    async function buildFromGraph(root) {
        let g;
        try {
            g = await H().loadLinkGraph();
        } catch (e) {
            return false;
        }
        if (!g || !Array.isArray(g.nodes) || !Array.isArray(g.edges) || !g.nodes.length) return false;
        // 图谱缓存属于另一个知识库时不可复用（否则会把两个库的路径混进同一索引）
        if (g.rootPath && String(g.rootPath) !== String(root)) return false;

        // 新鲜度校验：图谱生成时间必须不早于"最新的 md 修改时间"，
        // 否则边可能已过期（用户改过链接但没刷新图谱）→ 宁可自己重建，也不能展示错的入链。
        try {
            const list = H().files();
            let maxMt = 0;
            for (let i = 0; i < list.length; i++) {
                const f = list[i];
                if (!isMd(f.path || f.name)) continue;
                const t = Number(f.mtime) || 0;
                if (t > maxMt) maxMt = t;
            }
            const genAt = Number(g.generatedAt) || 0;
            if (genAt && maxMt && genAt < maxMt * 1000 - 60000) {
                console.info('[backlinks] 文档关联图谱早于最新修改（' + new Date(genAt).toLocaleString() +
                    ' < ' + new Date(maxMt * 1000).toLocaleString() + '），改为自建索引');
                return false;
            }
        } catch (e) { /* 拿不到 mtime 就继续用图谱 */ }

        const id2i = new Map();
        const src = [];
        for (let i = 0; i < g.nodes.length; i++) {
            const n = g.nodes[i];
            if (!n || !isMd(n.path)) continue;
            id2i.set(String(n.id), src.length);
            src.push(String(n.path).replace(/\\/g, '/'));
        }
        if (!src.length) return false;

        const eArr = [];
        const lArr = [];
        for (let i = 0; i < g.edges.length; i++) {
            const ed = g.edges[i];
            if (!ed) continue;
            const s = id2i.get(String(ed.source));
            const t = id2i.get(String(ed.target));
            if (s === undefined || t === undefined || s === t) continue;
            eArr.push(s, t);
            lArr.push(0);
        }
        adopt(root, src, eArr, lArr);
        BK.at = Number(g.generatedAt) || Date.now();
        BK.builtFrom = 'graph';
        return true;
    }
    /** 兜底：自己扫一遍 md 文件（并发 8），解析 [[ ]] 与 [](x.md) 并解析目标 */
    async function buildFromScan(root, onProgress) {
        const list = H().files().filter((f) => isMd(f.path || f.name));
        const src = list.map((f) => String(f.path || f.name).replace(/\\/g, '/'));
        const mt = list.map((f) => Number(f.mtime) || 0);
        adopt(root, src, [], []);
        BK.mt = Int32Array.from(mt);
        BK.builtFrom = 'scan';
        BK.readErrors = 0;

        const eArr = [];
        const lArr = [];
        const CONC = 8;
        let done = 0;
        for (let i = 0; i < src.length; i += CONC) {
            const batch = src.slice(i, i + CONC);
            const results = await Promise.all(batch.map(async (p, k) => {
                const si = i + k;
                try {
                    const text = await H().readTextFile(p);

                    return extractEdges(text, p, si);
                } catch (err) {
                    BK.readErrors++;
                    return null;
                }
            }));
            for (const r of results) {
                if (!r) continue;
                for (let j = 0; j < r.pairs.length; j += 2) {
                    if (r.pairs[j] === r.pairs[j + 1]) continue;
                    eArr.push(r.pairs[j], r.pairs[j + 1]);
                    lArr.push(r.lines[j / 2]);
                }
            }
            done += batch.length;
            if (onProgress) onProgress(done, src.length);
        }
        BK.e = Int32Array.from(eArr);
        BK.ln = Int32Array.from(lArr);
        console.info('[backlinks] 自建索引完成：' + src.length + ' 篇 / ' + (eArr.length >> 1) + ' 条链接' +
            (BK.readErrors ? '（' + BK.readErrors + ' 篇读取失败）' : ''));
    }

    /** 归一化相对路径：折叠 ./ 与 ../，统一分隔符 */
    function normRel(p) {
        const parts = String(p || '').replace(/\\/g, '/').split('/');
        const out = [];
        for (let i = 0; i < parts.length; i++) {
            const seg = parts[i];
            if (!seg || seg === '.') continue;
            if (seg === '..') { out.pop(); continue; }
            out.push(seg);
        }
        return out.join('/');
    }

    /**
     * 目标解析（模块内自足，不依赖宿主共享缓存）：
     *   1) 原样路径（大小写敏感 → 不敏感）
     *   2) 相对源文件目录
     *   3) 文件名（去 .md、去目录）唯一匹配
     * 最后仍失败时，退回宿主 resolveLinkTargetAll（兼容宿主历史行为）。
     */
    function resolveTarget(target, srcPath) {
        let t = String(target || '').trim().replace(/\\/g, '/');
        if (!t) return null;
        const h = t.indexOf('#');
        if (h !== -1) t = t.slice(0, h);
        if (!t) return null;
        const hasMd = /\.md$/i.test(t);
        if (!hasMd && /\.[a-z0-9]{1,8}$/i.test(t)) return null;   // 指向图片/其它附件
        const base = hasMd ? t : t + '.md';
        const cands = [normRel(base)];
        const slash = String(srcPath || '').lastIndexOf('/');
        if (slash > 0) cands.push(normRel(srcPath.slice(0, slash + 1) + base));
        for (const c of cands) {
            const i = BK.exact.get(c);
            if (i !== undefined) return i;
        }
        for (const c of cands) {
            const i = BK.idx.get(c.toLowerCase());
            if (i !== undefined) return i;
        }
        const name = base.split('/').pop().replace(/\.md$/i, '').toLowerCase();
        if (name) {
            const i = BK.byName.get(name);
            if (i !== undefined) return i;
        }
        // 兜底：宿主解析（历史/特殊写法）
        try {
            if (typeof H().resolveLinkTarget === 'function') {
                const i = H().resolveLinkTarget(target, srcPath, BK.exact, BK.files);
                if (i !== null && i !== undefined) return i;
            }
        } catch (e) { /* 忽略 */ }
        return null;
    }

    /** 解析一段 markdown 的链接 → { pairs:[s,t,...], lines:[n,...] } */
    function extractEdges(text, srcPath, srcIdx) {
        const pairs = [];
        const lines = [];
        if (!text || text.length === 0) return { pairs, lines };
        // 合并两种链接语法，保持匹配顺序（用于行号递增计数）
        const RE = /\[\[([^\[\]|#\n]+)(?:\|[^\]\n]*)?\]\]|!?\[[^\]\n]*\]\(\s*<?([^)>\s]+)>?(?:\s+"[^"]*")?\s*\)/g;
        let m;
        let lineNo = 1;
        let scanPos = 0;
        while ((m = RE.exec(text)) !== null) {
            // 行号：按已消费区间数换行
            for (let p = scanPos; p < m.index; p++) {
                if (text.charCodeAt(p) === 10) lineNo++;
            }
            scanPos = m.index + 1;
            const target = (m[1] !== undefined ? m[1] : m[2] || '').trim();
            const idx = resolveTarget(target, srcPath);
            if (idx === null || idx === undefined || idx === srcIdx) continue;
            pairs.push(srcIdx, idx);
            lines.push(lineNo);
        }
        return { pairs, lines };
    }

    /** 建立 src 数组与索引；e/ln 由调用方填充 */
    function adopt(root, src, eArr, lArr) {
        BK.root = root;
        BK.src = src;
        BK.idx = new Map();
        BK.exact = new Map();
        BK.byName = new Map();
        BK.pathSet = new Set();
        for (let i = 0; i < src.length; i++) {
            const p = src[i];
            BK.idx.set(lower(p), i);
            BK.exact.set(p, i);
            BK.pathSet.add(lower(p));
            const name = baseName(p).replace(/\.md$/i, '').toLowerCase();
            if (name && !BK.byName.has(name)) BK.byName.set(name, i);
        }
        BK.files = src.map((p) => ({ path: p, name: baseName(p) }));
        BK.e = Int32Array.from(eArr || []);
        BK.ln = Int32Array.from(lArr || []);
        if (!BK.mt || BK.mt.length !== src.length) {
            const ms = mtimeMap();
            BK.mt = Int32Array.from(src.map((p) => Number(ms.get(lower(p))) || 0));
        }
    }

    function hydrate(side) {
        BK.root = String(side.root || '');
        BK.at = Number(side.at) || 0;
        BK.src = Array.isArray(side.src) ? side.src.map((p) => String(p).replace(/\\/g, '/')) : [];
        BK.idx = new Map();
        BK.exact = new Map();
        BK.byName = new Map();
        BK.pathSet = new Set();
        for (let i = 0; i < BK.src.length; i++) {
            const p = BK.src[i];
            BK.idx.set(lower(p), i);
            BK.exact.set(p, i);
            BK.pathSet.add(lower(p));
            const name = baseName(p).replace(/\.md$/i, '').toLowerCase();
            if (name && !BK.byName.has(name)) BK.byName.set(name, i);
        }
        BK.files = BK.src.map((p) => ({ path: p, name: baseName(p) }));
        BK.mt = Int32Array.from(side.mt || []);
        BK.e = Int32Array.from(side.e || []);
        BK.ln = Int32Array.from(side.ln || []);
        buildCSR();
        BK.ready = true;
    }

    function persist() {
        if (!BK.ready && !BK.e) return;
        // 空索引不落盘：宁可下次重建，也不要把「0 文件」缓存固化下来
        if (!BK.src || !BK.src.length) return;
        const payload = {
            v: SIDECAR_V,
            root: BK.root,
            at: BK.at || Date.now(),
            src: BK.src,
            mt: Array.from(BK.mt || []),
            e: Array.from(BK.e || []),
            ln: Array.from(BK.ln || []),
        };
        // 侧车缓存写失败不影响功能（下次重建即可）
        Promise.resolve()
            .then(() => H().writeJSON(SIDECAR, payload))
            .catch((e) => console.warn('[backlinks] 写侧车缓存失败:', e));
    }

    /** 计数排序建立双向 CSR；O(n+m)，10 万边 < 10ms */
    function buildCSR() {
        const n = BK.src.length;
        const m = BK.e ? (BK.e.length >> 1) : 0;
        const fOff = new Int32Array(n + 1);
        const rOff = new Int32Array(n + 1);
        for (let i = 0; i < m; i++) {
            fOff[BK.e[i * 2] + 1]++;
            rOff[BK.e[i * 2 + 1] + 1]++;
        }
        for (let i = 0; i < n; i++) {
            fOff[i + 1] += fOff[i];
            rOff[i + 1] += rOff[i];
        }
        const fDst = new Int32Array(m);
        const rSrc = new Int32Array(m);
        const rLn = new Int32Array(m);
        const c1 = fOff.slice(0, n);
        const c2 = rOff.slice(0, n);
        for (let i = 0; i < m; i++) {
            const s = BK.e[i * 2];
            const t = BK.e[i * 2 + 1];
            fDst[c1[s]++] = t;
            const p = c2[t]++;
            rSrc[p] = s;
            rLn[p] = BK.ln ? BK.ln[i] : 0;
        }
        BK.fOff = fOff; BK.fDst = fDst;
        BK.rOff = rOff; BK.rSrc = rSrc; BK.rLn = rLn;
    }

    // ─────────────────────── 查询 ───────────────────────

    /** mtime 表：path(lower) -> 秒 */
    function mtimeMap() {
        const map = new Map();
        const list = H().files();
        for (let i = 0; i < list.length; i++) {
            const f = list[i];
            const p = f.path || f.name;
            if (p) map.set(lower(p), Number(f.mtime) || 0);
        }
        return map;
    }


    /**
     * 把"当前文件路径"解析为索引下标。
     * 兼容三种来源差异：相对路径、绝对路径、仅文件名（dataset/filePath 在不同入口下不保证一致）。
     */
    function indexOfPath(relPath) {
        if (!relPath) return -1;
        const p = String(relPath).replace(/\\/g, '/');
        let i = BK.idx.get(p.toLowerCase());
        if (i !== undefined) return i;
        const name = baseName(p).replace(/\.md$/i, '').toLowerCase();
        if (name) {
            i = BK.byName.get(name);
            if (i !== undefined) return i;
        }
        return -1;
    }

    /** 反向查询：返回 [{src, line, count}]，按引用次数降序 */
    function backlinksOf(relPath) {
        if (!BK.ready || !BK.rOff) return [];
        const i = indexOfPath(relPath);
        if (i < 0) return [];
        const from = BK.rOff[i];
        const to = BK.rOff[i + 1];
        if (from === to) return [];
        const bySrc = new Map();
        for (let p = from; p < to; p++) {
            const s = BK.rSrc[p];
            const line = BK.rLn[p] || 0;
            const cur = bySrc.get(s);
            if (cur) {
                cur.count++;
                if (!cur.line && line) cur.line = line;
            } else {
                bySrc.set(s, { src: s, line, count: 1 });
            }
        }
        return [...bySrc.values()].sort((a, b) => b.count - a.count);
    }

    // ─────────────────────── 构建调度 ───────────────────────

    async function ensureIndex(onProgress) {
        if (BK.ready) return BK;
        if (BK.building) return BK.building;
        BK.building = (async () => {
            BK.lastError = '';
            const root = H().rootName();
            // 1) 侧车缓存（最快）
            let side = null;
            try {
                side = await H().loadJSON(SIDECAR);
            } catch (e) {
                side = null;
            }
            if (side && Number(side.v) === SIDECAR_V && String(side.root || '') === root &&
                Array.isArray(side.src) && side.src.length) {
                hydrate(side);
                BK.builtFrom = 'sidecar';


            } else {
                // 2) 复用文档关联图谱缓存
                const ok = await buildFromGraph(root).catch(() => false);
                if (!ok) {
                    // 3) 兜底自扫
                    await buildFromScan(root, onProgress);
                }
                buildCSR();
                BK.at = BK.at || Date.now();
                BK.ready = true;
                persist();
            }
            // 4) 后台对账（不阻塞首屏）
            setTimeout(() => { refreshChanged().catch(() => {}); }, 1200);
            return BK;
        })();
        try {
            return await BK.building;
        } finally {
            BK.building = null;
        }
    }

    /** 按 mtime 对账，只重算变更文件；新增/删除 md 也在此同步 */
    async function refreshChanged() {
        if (!BK.ready) return 0;
        if (String(BK.root || '') !== String(H().rootName() || '')) return 0;   // 期间切换了知识库
        const ms = mtimeMap();
        const changed = [];
        for (let i = 0; i < BK.src.length; i++) {
            const m = ms.get(lower(BK.src[i]));
            if (m !== undefined && m !== (BK.mt[i] || 0)) changed.push(i);
        }
        const known = new Set(BK.src.map(lower));
        const added = [];
        for (const [p, m] of ms) {
            if (!isMd(p) || known.has(p)) continue;
            added.push(p);
        }
        if (!changed.length && !added.length) return 0;
        // 变更/新增过多时直接全量重建（避免 O(k*m) 的反复过滤）
        if (changed.length + added.length > Math.max(30, Math.floor(BK.src.length * 0.1))) {
            await rebuild();
            return changed.length + added.length;
        }
        for (const i of changed) {
            let text = '';
            try {
                text = await H().readTextFile(BK.src[i]);
            } catch (e) {
                text = '';
            }
            applySourceEdges(i, text, ms);
        }
        for (const p of added) {
            const i = BK.src.length;
            BK.src.push(p);
            BK.idx.set(lower(p), i);
            BK.exact.set(p, i);
            BK.pathSet.add(lower(p));
            BK.files.push({ path: p, name: baseName(p) });
            const n = new Int32Array(BK.mt.length + 1);
            n.set(BK.mt); n[i] = Number(ms.get(lower(p))) || 0;
            BK.mt = n;
            try {
                applySourceEdges(i, await H().readTextFile(p), ms);
            } catch (e) { /* 忽略单文件失败 */ }
        }
        buildCSR();
        persist();
        return changed.length + added.length;
    }

    /** 用最新内容重算某个源文件的出链（移除旧边 + 追加新边），随后需 buildCSR */
    function applySourceEdges(srcIdx, text, mtMap) {
        const pairs = [];
        const lines = [];
        if (text) {
            const r = extractEdges(text, BK.src[srcIdx], srcIdx);
            pairs.push(...r.pairs);
            lines.push(...r.lines);
        }
        // 过滤掉该源的所有旧边
        const eOld = BK.e, lOld = BK.ln || new Int32Array(eOld.length >> 1);
        const keepE = [];
        const keepL = [];
        for (let i = 0; i < eOld.length; i += 2) {
            if (eOld[i] === srcIdx) continue;
            keepE.push(eOld[i], eOld[i + 1]);
            keepL.push(lOld[i >> 1]);
        }
        for (let j = 0; j < pairs.length; j += 2) {
            keepE.push(pairs[j], pairs[j + 1]);
            keepL.push(lines[j / 2]);
        }
        BK.e = Int32Array.from(keepE);
        BK.ln = Int32Array.from(keepL);
        const m = mtMap ? Number(mtMap.get(lower(BK.src[srcIdx]))) || 0 : 0;
        if (m) BK.mt[srcIdx] = m;

    }

    async function rebuild() {
        BK.readErrors = 0;
        const root = H().rootName();
        const t0 = performance.now();
        const withGraph = await buildFromGraph(root).catch(() => false);
        if (!withGraph) await buildFromScan(root, null);
        buildCSR();
        BK.ready = true;
        BK.at = Date.now();

        persist();
        console.info('[backlinks] 重建完成', Math.round(performance.now() - t0) + 'ms',
            BK.src.length + ' 文件 / ' + (BK.e.length >> 1) + ' 链接');
        return BK;
    }

    // ─────────────────────── 宿主回调 ───────────────────────

    /** 保存 md 后调用：立即增量更新（不阻塞保存流程，失败静默） */
    function onFileSaved(relPath) {
        if (!BK.ready || !relPath) return;
        const i = BK.idx.get(lower(relPath));
        if (i === undefined) return;
        Promise.resolve()
            .then(() => H().readTextFile(relPath))
            .then((text) => {
                applySourceEdges(i, text, null);
                buildCSR();
                BK.at = Date.now();
                schedulePersist();
            })
            .catch(() => {});
    }

    let _persistTimer = null;
    function schedulePersist() {
        clearTimeout(_persistTimer);
        _persistTimer = setTimeout(persist, 2000);
    }

    // ─────────────────────── 渲染 ───────────────────────

    function esc(s) {
        return H().escapeHtml(String(s === null || s === undefined ? '' : s));
    }

    function panelEl() {
        const all = document.querySelectorAll('#toc-link .toc-tab-panel[data-tab="toc-backlinks"]');
        return all.length ? all[0] : null;
    }

    /**
     * tab 渲染入口（由 main.html 的 tocLinkTabs 调用）。
     * 立即返回骨架（不阻塞 TOC），随后异步补齐列表。
     */
    function renderPanel() {
        if (!window.__mdgoHost) return '<div class="toc-link-empty">宿主持久化桥未就绪</div>';
        const relPath = H().currentRelPath() || '';
        BK.pending = { relPath };
        const token = String(Date.now()) + Math.random().toString(36).slice(2, 7);
        BK.pending.token = token;
        setTimeout(() => { try { paint(token); } catch (e) { console.error('[backlinks] paint 失败', e); } }, 0);
        return '<div class="bk-wrap" data-bk-token="' + token + '" data-bk-file="' + esc(relPath) + '">' +
            '<div class="bk-empty">正在准备链接索引…</div>' +
            '</div>';
    }

    /** 统一日志：所有数量与索引时间只进控制台，不在 UI 展示 */
    function logStats(relPath, total) {
        const links = BK.e ? (BK.e.length >> 1) : 0;
        const from = { sidecar: '缓存', graph: '图谱', scan: '自建' }[BK.builtFrom] || '索引';
        const at = BK.at ? new Date(BK.at).toLocaleString() : '未知';
        console.info('[backlinks] 文档=' + (relPath || '(空)') +
            ' | 反向链接=' + total + ' 篇' +
            ' | 索引文件=' + BK.src.length + ' 篇' +
            ' | 索引链接=' + links + ' 条' +
            ' | 来源=' + from +
            ' | 更新时间=' + at +
            (BK.readErrors ? ' | 读取失败=' + BK.readErrors + ' 篇' : ''));
    }

    async function paint(token) {
        const wrap = document.querySelector('.bk-wrap[data-bk-token="' + token + '"]');
        if (!wrap) return;
        const ctx = BK.pending;
        if (!ctx || ctx.token !== token) return;
        const relPath = ctx.relPath;

        if (!relPath) {
            wrap.innerHTML = '<div class="bk-empty">当前没有打开文档</div>';
            return;
        }

        if (!BK.ready) {
            const t0 = performance.now();
            try {
                await ensureIndex((done, total) => {
                    const w = document.querySelector('.bk-wrap[data-bk-token="' + token + '"]');
                    if (!w) return;
                    const el = w.querySelector('.bk-empty');
                    if (el) el.textContent = '正在建立链接索引 ' + done + ' / ' + total + ' …';
                });
            } catch (e) {
                wrap.innerHTML = '<div class="bk-empty">链接索引建立失败<br>' + esc(e && e.message) + '</div>';
                return;
            }
            const cost = Math.round(performance.now() - t0);
            console.info('[backlinks] 首次索引耗时 ' + cost + 'ms');
        }

        // 索引里没有当前文件（新建 / 索引落后）→ 重建一次再查
        if (indexOfPath(relPath) < 0) {
            try {
                wrap.innerHTML = '<div class="bk-empty">正在更新链接索引…</div>';
                await rebuild();
            } catch (e) { /* 重建失败则按空结果展示 */ }
        }

        if (!BK.src.length) {
            BK.ready = false;
            logStats(relPath, 0);
            wrap.innerHTML = '<div class="bk-empty">没有可索引的 Markdown 文件</div>';
            return;
        }

        try {
            const items = backlinksOf(relPath);
            logStats(relPath, items.length);
            renderList(wrap, relPath, items);
        } catch (e) {
            console.error('[backlinks] 渲染失败', e);
            wrap.innerHTML = '<div class="bk-empty">反向链接渲染失败<br>' + esc(e && e.message) + '</div>';
        }
    }

    /**
     * 面板内容 = 一个「刷新」按钮 + 与「Links列表」同款的无序链接列表。
     * 数量/更新时间等只输出到控制台（logStats），不在 UI 展示。
     */
    function renderList(wrap, relPath, items) {
        let html = '<div class="bk-bar">' +
            '<button class="bk-refresh" data-bk-act="refresh" title="重建链接索引">刷新</button>' +
            '</div>';

        if (!items.length) {
            html += '<div class="bk-empty">暂无反向链接</div>';
            wrap.innerHTML = html;
            return;
        }

        html += '<ul class="bk-list">';
        for (let i = 0; i < items.length; i++) {
            const it = items[i];
            const p = BK.src[it.src] || '';
            if (!p) continue;
            html += '<li><a data-bk-s="' + it.src + '" data-bk-l="' + (it.line || 0) + '" title="' + esc(p) + '">' +
                esc(baseName(p)) + '</a></li>';
        }
        html += '</ul>';
        wrap.innerHTML = html;
    }
    // ─────────────────────── 交互 ───────────────────────

    async function openBacklink(srcIdx, line, targetPath) {
        const srcPath = BK.src[srcIdx];
        if (!srcPath) return;
        const base = stripExt(targetPath || (BK.pending ? BK.pending.relPath : ''));
        const ok = await H().openFile(srcPath);
        if (!ok || !base) return;
        // 文件切换有 150ms 防抖 + 异步渲染：轮询等正文就绪后交给宿主统一定位（与全局搜索共用一套定位逻辑）
        let tries = 0;
        const tick = () => {
            tries++;
            if (H().revealText && H().revealText(base)) return;
            if (tries < 16) setTimeout(tick, 110);
        };
        setTimeout(tick, 120);
    }

    // 事件委托：面板会被 TOC 反复重建，绑定在常驻的 #toc-link 上
    function bindDelegates() {
        const root = document.getElementById('toc-link');
        if (!root) return;
        root.addEventListener('click', (e) => {
            const act = e.target.closest('[data-bk-act]');
            if (act) {
                if (act.getAttribute('data-bk-act') === 'refresh') {
                    act.textContent = '刷新中…';
                    act.disabled = true;
                    rebuild()
                        .then(() => {
                            const wrap = document.querySelector('.bk-wrap[data-bk-token]');
                            if (wrap && BK.pending) {
                                const items = backlinksOf(BK.pending.relPath);
                                logStats(BK.pending.relPath, items.length);
                                renderList(wrap, BK.pending.relPath, items);
                            }
                            H().notify('✓ 链接索引已刷新', 'success', 1800);
                        })
                        .catch((err) => H().notify('✗ 刷新失败: ' + (err && err.message), 'error'));
                }
                e.preventDefault();
                e.stopPropagation();
                return;
            }
            const link = e.target.closest('a[data-bk-s]');
            if (!link) return;
            const s = Number(link.getAttribute('data-bk-s'));
            const l = Number(link.getAttribute('data-bk-l')) || 0;
            const holder = link.closest('.bk-wrap');
            const file = holder ? holder.getAttribute('data-bk-file') : '';
            e.preventDefault();
            openBacklink(s, l, file || '');
        });
    }

    if (document.readyState === 'loading') {
        window.addEventListener('DOMContentLoaded', bindDelegates);
    } else {
        bindDelegates();
    }

    // ─────────────────────── 对外 ───────────────────────
    window.mdgoBacklinks = {
        renderPanel: renderPanel,
        onFileSaved: onFileSaved,
        prefetch: () => ensureIndex().catch(() => {}),
        refresh: () => rebuild(),
        stats: () => ({
            ready: BK.ready,
            files: BK.src.length,
            links: BK.e ? (BK.e.length >> 1) : 0,
            at: BK.at,
            root: BK.root,
            from: BK.builtFrom,
            readErrors: BK.readErrors,
        }),
    };
})();