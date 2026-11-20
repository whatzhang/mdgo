/**
 * ===== 文档库分析页（行为分析）· 图表与 KPI 渲染 =====
 *
 * 【职责】把 `.mdgo/mdgo.db` 里的 `usage_daily / usage_hourly / usage_file_daily`
 *         渲染成 KPI + 6 张图；AI 分析区块仍由 main.html 负责（渲染/存储/重算）。
 * 【依赖】window.__TAURI__.core.invoke（usage_summary / usage_files）
 *         window.__mdgoHost（files / openFile / notify / basename）
 *         echarts（main.html 已加载）
 * 【交互】时间范围 chips（默认近 30 天）、Top10 指标切换、点击文件打开、点击目录筛选榜单。
 */
(function () {
    if (window.__mdgoBehaviorInited) return;
    window.__mdgoBehaviorInited = true;

    const DEFAULT_DAYS = 30;
    const MAX_TREND_DAYS = 62;
    const TOP_LIMIT = 500;

    const COLOR = {
        view: '#2F6BFF',
        edit: '#F2994A',
        delete: '#E5484D',
        ai: '#7C5CFF',
        create: '#22A06B',
        axis: '#5B6472',
        split: '#EEF0F3',
        text: '#1F2329',
        sub: '#5B6472',
    };

    // 图表字号：以 rem 计算、12px 为可读下限（窄屏根字号 14px 时仍保持 12px）
    const ROOT_FONT_PX = (typeof window !== 'undefined' && window.getComputedStyle)
        ? (parseFloat(window.getComputedStyle(document.documentElement).fontSize) || 16)
        : 16;
    const CHART_FS = Math.max(12, Math.round(ROOT_FONT_PX * 0.75));
    const CHART_FS_LG = Math.max(18, Math.round(ROOT_FONT_PX * 1.125));

    const DIR_COLORS = ['#2F6BFF', '#12A5A5', '#7C5CFF', '#F2994A', '#22A06B', '#E5484D', '#9AA4B2'];

    const state = {
        days: DEFAULT_DAYS,
        topMetric: 'view_ms',
        dirFilter: null,
        summary: null,
        files: null,
        totalFiles: 0,
        loading: false,
        errors: [],
    };

    const charts = {};

    // Node/测试环境没有 rAF 时退化为 setTimeout，避免渲染中断
    const raf = (typeof window !== 'undefined' && typeof window.requestAnimationFrame === 'function')
        ? window.requestAnimationFrame.bind(window)
        : (fn) => setTimeout(fn, 16);

    // ─── 基础工具 ───

    function H() {
        return window.__mdgoHost || null;
    }

    /**
     * 当前知识库根路径。
     * 宿主桥把 `rootPath` 暴露成 **getter 函数**（`() => getDirPath(...)`），不是字符串；
     * 直接当字符串用会得到 `undefined`，导致 usage_* 调用缺 `dirPath` 而全部失败
     * （表现：AI 分析有结果，但 KPI/图表全空）。
     */
    function rootPath() {
        const h = H();
        if (!h) return '';
        try {
            const v = h.rootPath;
            return typeof v === 'function' ? String(v() || '') : String(v || '');
        } catch (e) {
            return '';
        }
    }

    function canRun() {
        return !!(window.__TAURI__ && window.__TAURI__.core && rootPath());
    }

    function invoke(name, args) {
        return window.__TAURI__.core.invoke(name, args);
    }

    function pad2(n) {
        return String(n).padStart(2, '0');
    }

    function fmtDate(d) {
        return d.getFullYear() + '-' + pad2(d.getMonth() + 1) + '-' + pad2(d.getDate());
    }

    function parseDate(s) {
        const p = String(s || '').split('-').map(Number);
        return new Date(p[0], (p[1] || 1) - 1, p[2] || 1);
    }

    function addDays(d, n) {
        const x = new Date(d.getTime());
        x.setDate(x.getDate() + n);
        return x;
    }

    /** 依据 days 计算 [from,to] 与上一等长周期 */
    function rangeFor(days) {
        const today = new Date();
        today.setHours(0, 0, 0, 0);
        if (days === 1) {
            const from = fmtDate(today);
            const prevTo = fmtDate(addDays(today, -1));
            const prevFrom = fmtDate(addDays(today, -2));
            return { from, to: from, prevFrom, prevTo };
        }
        if (days === 0) {
            return { from: '2000-01-01', to: fmtDate(today), prevFrom: '', prevTo: '' };
        }
        const fromD = addDays(today, -(days - 1));
        const prevToD = addDays(fromD, -1);
        const prevFromD = addDays(prevToD, -(days - 1));
        return {
            from: fmtDate(fromD),
            to: fmtDate(today),
            prevFrom: fmtDate(prevFromD),
            prevTo: fmtDate(prevToD),
        };
    }

    function fmtNum(n) {
        const v = Number(n) || 0;
        if (Math.abs(v) >= 10000) return (v / 10000).toFixed(1) + ' 万';
        return v.toLocaleString('zh-CN');
    }

    function fmtDuration(ms) {
        const v = Number(ms) || 0;
        if (v < 1000) return '0 秒';
        const sec = Math.floor(v / 1000);
        if (sec < 60) return sec + ' 秒';
        const min = Math.floor(sec / 60);
        if (min < 60) return min + ' 分钟';
        const h = min / 60;
        return (h >= 10 ? Math.round(h) : Math.round(h * 10) / 10) + ' 小时';
    }

    function fmtMinutes(ms) {
        return Math.round((Number(ms) || 0) / 60000 * 10) / 10;
    }

    function pct(part, total) {
        if (!total) return '0%';
        return Math.round(part / total * 100) + '%';
    }

    function esc(s) {
        const h = H();
        if (h && typeof h.escapeHtml === 'function') return h.escapeHtml(String(s == null ? '' : s));
        return String(s == null ? '' : s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
    }

    function baseName(p) {
        const h = H();
        if (h && typeof h.basename === 'function') return h.basename(p);
        return String(p || '').split('/').pop();
    }

    function dirOf(p) {
        const parts = String(p || '').split('/');
        return parts.length > 1 ? parts[0] : '(根目录)';
    }

    function notify(msg, type) {
        const h = H();
        if (h && typeof h.notify === 'function') h.notify(msg, type || 'info');
    }

    function emptyText(n) {
        return n > 0 ? '' : '该区间暂无数据，先阅读或编辑一些文件吧';
    }

    // ─── 图表容器 ───

    function ensureChart(id) {
        const el = document.getElementById(id);
        if (!el || typeof echarts === 'undefined') return null;
        if (charts[id] && !charts[id].isDisposed()) {
            charts[id].dispose();
        }
        const chart = echarts.init(el, null, { renderer: 'canvas' });
        charts[id] = chart;
        return chart;
    }

    function resizeAll() {
        Object.keys(charts).forEach((k) => {
            const c = charts[k];
            if (c && !c.isDisposed()) c.resize();
        });
    }

    function baseOption(extra) {
        const opt = {
            animationDuration: 260,
            textStyle: { fontFamily: 'inherit', color: COLOR.text },
            tooltip: {
                backgroundColor: 'rgba(255,255,255,0.98)',
                borderColor: '#E6E8EC',
                borderWidth: 1,
                textStyle: { color: COLOR.text, fontSize: CHART_FS },
                extraCssText: 'box-shadow:0 4px 16px rgba(0,0,0,.08);border-radius:8px;',
            },
            grid: { left: 8, right: 12, top: 34, bottom: 6, containLabel: true },
        };
        return Object.assign(opt, extra || {});
    }

    function categoryAxis(data, extra) {
        return Object.assign({
            type: 'category',
            data: data,
            axisTick: { show: false },
            axisLine: { lineStyle: { color: COLOR.split } },
            axisLabel: { color: COLOR.axis, fontSize: CHART_FS },
        }, extra || {});
    }

    function valueAxis(extra) {
        return Object.assign({
            type: 'value',
            axisLabel: { color: COLOR.axis, fontSize: CHART_FS },
            splitLine: { lineStyle: { color: COLOR.split } },
        }, extra || {});
    }

    function setEmpty(id, message) {
        const el = document.getElementById(id);
        if (!el) return;
        const wrap = el.parentElement;
        if (!wrap) return;
        let hint = wrap.querySelector('.ba-empty');
        if (!message) {
            if (hint) hint.remove();
            return;
        }
        if (!hint) {
            hint = document.createElement('div');
            hint.className = 'ba-empty';
            wrap.appendChild(hint);
        }
        hint.textContent = message;
    }

    // ─── 数据 ───

    async function load() {
        state.errors = [];
        const r = rangeFor(state.days);
        const dirPath = rootPath();
        const [summary, fileList] = await Promise.all([
            invoke('usage_summary', Object.assign({ dirPath: dirPath }, r)).catch((e) => {
                console.warn('[behavior] usage_summary 失败:', e);
                state.errors.push('统计汇总：' + ((e && (e.message || e)) || '未知错误'));
                return { daily: [], hourly: [], totals: {}, previous: {} };
            }).then((s) => Object.assign({ daily: [], hourly: [], totals: {}, previous: {} }, s || {})),
            invoke('usage_files', { dirPath: dirPath, from: r.from, to: r.to, limit: TOP_LIMIT }).catch((e) => {
                console.warn('[behavior] usage_files 失败:', e);
                state.errors.push('文件统计：' + ((e && (e.message || e)) || '未知错误'));
                return { rows: [], opened_total: 0 };
            }).then((f) => Object.assign({ rows: [], opened_total: 0 }, f || {})),
        ]);
        state.summary = summary;
        state.files = fileList;
        state.range = r;
        try {
            state.totalFiles = (H().files() || []).length;
        } catch (e) {
            state.totalFiles = 0;
        }
        return { summary, fileList };
    }

    // ─── 旧数据回填（一次性） ───
    // 改造前只有 localStorage 的 20 条浏览记录，没有按天序列。这里把「文件累计时长」
    // 按它的 lastViewTime 归一天回填一次，让首次打开页面不至于全空；历史完整性有限，
    // 因此只在统计表为空时执行，并写标记避免重复。
    const BACKFILL_FLAG = 'mdgo_usage_backfill_v1';

    function readLegacyViewRecords() {
        try {
            const raw = (typeof safeLocalStorageGet === 'function')
                ? safeLocalStorageGet('index_view_sum')
                : window.localStorage.getItem('index_view_sum');
            if (!raw) return [];
            const data = JSON.parse(raw);
            return Array.isArray(data.viewRecords) ? data.viewRecords : [];
        } catch (e) {
            return [];
        }
    }

    async function maybeBackfill() {
        const t = (state.summary && state.summary.totals) || {};
        const nonZero = ['view_ms', 'opens', 'edits', 'creates', 'deletes', 'ai_calls']
            .some((k) => Number(t[k]) > 0);
        if (nonZero) return false;
        try {
            const flag = (typeof safeLocalStorageGet === 'function')
                ? safeLocalStorageGet(BACKFILL_FLAG)
                : window.localStorage.getItem(BACKFILL_FLAG);
            if (flag) return false;
        } catch (e) { /* 读取失败则按未回填处理 */ }

        const records = readLegacyViewRecords();
        if (!records.length) return false;

        const events = [];
        records.forEach((r) => {
            const ts = Number(r.lastViewTime) || 0;
            if (!ts) return;
            const d = new Date(ts);
            const date = fmtDate(d);
            const hour = d.getHours();
            const path = r.path || null;
            if (Number(r.durationMs) > 0) {
                events.push({ kind: 'view_ms', date, hour, path, n: Math.round(Number(r.durationMs)), ts });
            }
            if (Number(r.count) > 0) {
                events.push({ kind: 'open', date, hour, path, n: Math.max(1, Math.round(Number(r.count))), ts });
            }
        });
        if (!events.length) return false;

        try {
            await invoke('usage_record', {
                dirPath: rootPath(),
                events,
                pruneBefore: '',
            });
            const set = (typeof safeLocalStorageSet === 'function')
                ? safeLocalStorageSet
                : (k, v) => window.localStorage.setItem(k, v);
            set(BACKFILL_FLAG, '1');
            console.info('[behavior] 已回填旧浏览记录 ' + records.length + ' 条（一次性）');
            return true;
        } catch (e) {
            console.warn('[behavior] 旧记录回填失败（忽略）:', e);
            return false;
        }
    }
    // ─── KPI ───

    function deltaHtml(cur, prev, unit) {
        const c = Number(cur) || 0;
        const p = Number(prev) || 0;
        if (!p && !c) return '<span class="ba-delta-flat">—</span>';
        if (!p) return '<span class="ba-delta-up">新增</span>';
        const d = Math.round((c - p) / p * 100);
        if (d === 0) return '<span class="ba-delta-flat">持平</span>';
        const cls = d > 0 ? 'ba-delta-up' : 'ba-delta-down';
        const arrow = d > 0 ? '▲' : '▼';
        return '<span class="' + cls + '">' + arrow + ' ' + Math.abs(d) + '%</span>' + (unit ? ' ' + unit : '');
    }

    function streakOf(daily) {
        const active = new Set((daily || []).filter((d) => Object.keys(d).some((k) => k !== 'date' && Number(d[k]) > 0)).map((d) => d.date));
        let streak = 0;
        let cursor = new Date();
        cursor.setHours(0, 0, 0, 0);
        // 今天还没活跃则从昨天起算（不打断连续记录）
        if (!active.has(fmtDate(cursor))) cursor = addDays(cursor, -1);
        while (active.has(fmtDate(cursor))) {
            streak++;
            cursor = addDays(cursor, -1);
        }
        // 历史最长
        let longest = 0;
        let run = 0;
        let prev = null;
        const dates = Array.from(active).sort();
        for (const ds of dates) {
            if (prev && fmtDate(addDays(parseDate(prev), 1)) === ds) run++;
            else run = 1;
            longest = Math.max(longest, run);
            prev = ds;
        }
        return { streak, longest: Math.max(longest, streak) };
    }

    function renderKpi() {
        const el = document.getElementById('ba-kpi');
        if (!el) return;
        const t = (state.summary && state.summary.totals) || {};
        const p = (state.summary && state.summary.previous) || {};
        const opened = (state.files && state.files.opened_total) || 0;
        const totalFiles = state.totalFiles || 0;
        const st = streakOf(state.summary && state.summary.daily);
        const coverage = totalFiles ? Math.min(100, Math.round(opened / totalFiles * 100)) : 0;

        const cards = [
            {
                label: '浏览时长',
                value: fmtDuration(t.view_ms),
                foot: deltaHtml(t.view_ms, p.view_ms) + ' 较上期',
            },
            {
                label: '打开文件',
                value: fmtNum(t.opens || 0) + '<span class="ba-kpi-unit">次</span>',
                foot: deltaHtml(t.opens, p.opens) + ' 较上期',
            },
            {
                label: '编辑次数',
                value: fmtNum(t.edits || 0) + '<span class="ba-kpi-unit">次</span>',
                foot: deltaHtml(t.edits, p.edits) + ' 较上期',
            },
            {
                label: '阅读覆盖率',
                value: coverage + '<span class="ba-kpi-unit">%</span>',
                foot: opened + ' / ' + (totalFiles || '?') + ' 个文件',
            },
            {
                label: '连续活跃',
                value: st.streak + '<span class="ba-kpi-unit">天</span>',
                foot: '历史最长 ' + st.longest + ' 天',
            },
            {
                label: 'AI 调用',
                value: fmtNum(t.ai_calls || 0) + '<span class="ba-kpi-unit">次</span>',
                foot: deltaHtml(t.ai_calls, p.ai_calls) + ' 较上期',
            },
        ];

        let html = cards.map((c) => (
            '<div class="ba-kpi-card">' +
            '<div class="ba-kpi-label">' + esc(c.label) + '</div>' +
            '<div class="ba-kpi-value">' + c.value + '</div>' +
            '<div class="ba-kpi-foot">' + c.foot + '</div>' +
            '</div>'
        )).join('');

        // 加载失败不再静默（否则表现为"有 AI 结果但数据栏空白"）
        if (state.errors.length) {
            html = '<div class="ba-banner ba-banner-error">统计加载失败：' + esc(state.errors.join('；')) + '</div>' + html;
        } else if (!['view_ms', 'opens', 'edits', 'ai_calls'].some((k) => Number(t[k]) > 0)) {
            html = '<div class="ba-banner">该区间还没有使用记录 —— 打开或编辑文件后会自动开始统计</div>' + html;
        }
        el.innerHTML = html;
    }

    // ─── 图表 ───

    function trendSeries() {
        const daily = (state.summary && state.summary.daily) || [];
        const map = {};
        daily.forEach((d) => { map[d.date] = d; });
        const days = state.days;
        let dates = [];
        if (days > 0) {
            const today = new Date();
            today.setHours(0, 0, 0, 0);
            const n = Math.min(days, MAX_TREND_DAYS);
            for (let i = n - 1; i >= 0; i--) dates.push(fmtDate(addDays(today, -i)));
        } else {
            dates = daily.map((d) => d.date).slice(-MAX_TREND_DAYS);
        }
        return dates.map((ds) => {
            const row = map[ds] || {};
            return {
                date: ds,
                label: ds.slice(5),
                view: fmtMinutes(row.view_ms),
                edits: Number(row.edits) || 0,
                opens: Number(row.opens) || 0,
            };
        });
    }

    function renderTrend() {
        const chart = ensureChart('ba-trend-chart');
        if (!chart) return;
        const rows = trendSeries();
        const hasData = rows.some((r) => r.view > 0 || r.edits > 0 || r.opens > 0);
        setEmpty('ba-trend-chart', hasData ? '' : '该区间暂无浏览/编辑记录');
        chart.setOption(baseOption({
            legend: { top: 2, right: 4, itemWidth: 10, itemHeight: 8, textStyle: { color: COLOR.sub, fontSize: CHART_FS } },
            tooltip: Object.assign({}, baseOption().tooltip, {
                trigger: 'axis',
                formatter: (ps) => {
                    if (!ps || !ps.length) return '';
                    const r = rows[ps[0].dataIndex] || {};
                    return '<div style="font-size: 12px; line-height: 1.6;"><b>' + esc(r.date) + '</b><br/>浏览时长：' + r.view + ' 分钟<br/>打开次数：' + r.opens + ' 次<br/>编辑次数：' + r.edits + ' 次</div>';
                },
            }),
            grid: { left: 8, right: 14, top: 34, bottom: 4, containLabel: true },
            xAxis: categoryAxis(rows.map((r) => r.label)),
            yAxis: [
                valueAxis({ name: '分钟', nameTextStyle: { color: COLOR.axis, fontSize: CHART_FS } }),
                // 右侧「编辑次数」已由 legend 表达，不再重复渲染轴名，避免与图例重叠
                valueAxis({ splitLine: { show: false } }),
            ],
            series: [
                {
                    name: '浏览时长',
                    type: 'bar',
                    data: rows.map((r) => r.view),
                    itemStyle: { color: COLOR.view, borderRadius: [3, 3, 0, 0] },
                    barMaxWidth: 18,
                },
                {
                    name: '编辑次数',
                    type: 'line',
                    yAxisIndex: 1,
                    data: rows.map((r) => r.edits),
                    smooth: true,
                    symbolSize: 5,
                    lineStyle: { color: COLOR.edit, width: 1.6 },
                    itemStyle: { color: COLOR.edit },
                },
            ],
        }), true);
    }

    function renderHourly() {
        const chart = ensureChart('ba-hourly-chart');
        if (!chart) return;
        const rows = (state.summary && state.summary.hourly) || [];
        const byHour = (kind) => {
            const arr = new Array(24).fill(0);
            rows.filter((r) => r.kind === kind).forEach((r) => {
                const h = Number(r.hour);
                if (h >= 0 && h < 24) arr[h] += Number(r.n) || 0;
            });
            return arr;
        };
        const series = [
            { name: '浏览', kind: 'open', color: COLOR.view },
            { name: '编辑', kind: 'edit', color: COLOR.edit },
            { name: '删除', kind: 'delete', color: COLOR.delete },
            { name: 'AI', kind: 'ai', color: COLOR.ai },
        ].map((s) => ({
            name: s.name,
            type: 'bar',
            stack: 'activity',
            data: byHour(s.kind),
            itemStyle: { color: s.color },
            barMaxWidth: 14,
            emphasis: { focus: 'series' },
        }));
        const total = series.reduce((sum, s) => sum + s.data.reduce((a, b) => a + b, 0), 0);
        setEmpty('ba-hourly-chart', total > 0 ? '' : '该区间暂无操作记录');
        chart.setOption(baseOption({
            legend: { top: 2, right: 4, itemWidth: 10, itemHeight: 8, textStyle: { color: COLOR.sub, fontSize: CHART_FS } },
            tooltip: Object.assign({}, baseOption().tooltip, { trigger: 'axis', axisPointer: { type: 'shadow' } }),
            grid: { left: 8, right: 12, top: 34, bottom: 4, containLabel: true },
            xAxis: categoryAxis(Array.from({ length: 24 }, (_, i) => i + '时'), { axisLabel: { color: COLOR.axis, fontSize: CHART_FS, interval: 3 } }),
            yAxis: valueAxis({ name: '次数', nameTextStyle: { color: COLOR.axis, fontSize: CHART_FS } }),
            series,
        }), true);
    }

    function topRows() {
        let rows = (state.files && state.files.rows) || [];
        if (state.dirFilter) {
            rows = rows.filter((r) => dirOf(r.path) === state.dirFilter);
        }
        const key = state.topMetric === 'opens' ? 'opens' : 'view_ms';
        return rows
            .slice()
            .sort((a, b) => (Number(b[key]) || 0) - (Number(a[key]) || 0))
            .slice(0, 10);
    }

    function renderTop10() {
        const chart = ensureChart('ba-top10-chart');
        if (!chart) return;
        const rows = topRows();
        const metric = state.topMetric === 'opens' ? 'opens' : 'view_ms';
        const values = rows.map((r) => (metric === 'opens' ? Number(r.opens) || 0 : fmtMinutes(r.view_ms)));
        const names = rows.map((r) => baseName(r.path));
        const hasData = rows.length > 0;
        setEmpty('ba-top10-chart', hasData ? '' : (state.dirFilter ? '该目录下暂无浏览记录' : '该区间暂无文件浏览记录'));
        chart.setOption(baseOption({
            grid: { left: 6, right: 34, top: 8, bottom: 4, containLabel: true },
            tooltip: Object.assign({}, baseOption().tooltip, {
                trigger: 'axis',
                axisPointer: { type: 'shadow' },
                formatter: (ps) => {
                    if (!ps || !ps.length) return '';
                    const r = rows[ps[0].dataIndex] || {};
                    return '<div style="font-size: 12px; line-height: 1.6;"><b>' + esc(r.path) + '</b><br/>浏览时长：' + fmtDuration(r.view_ms) +
                        '<br/>打开次数：' + (r.opens || 0) + ' 次<br/>编辑次数：' + (r.edits || 0) + ' 次</div>';
                },
            }),
            xAxis: valueAxis({ name: metric === 'opens' ? '次数' : '分钟', nameTextStyle: { color: COLOR.axis, fontSize: CHART_FS } }),
            yAxis: categoryAxis(names.slice().reverse(), {
                axisLabel: { color: COLOR.sub, fontSize: CHART_FS, width: CHART_FS * 8, overflow: 'truncate' },
                axisLine: { show: false },
            }),
            series: [{
                type: 'bar',
                data: values.slice().reverse(),
                itemStyle: { color: COLOR.view, borderRadius: [0, 3, 3, 0] },
                barMaxWidth: 14,
                label: {
                    show: true,
                    position: 'right',
                    fontSize: CHART_FS,
                    color: COLOR.sub,
                    formatter: (p) => (metric === 'opens' ? p.value + ' 次' : p.value + ' 分'),
                },
            }],
        }), true);

        chart.off('click');
        chart.on('click', (p) => {
            const r = rows[rows.length - 1 - p.dataIndex];
            if (!r || !r.path) return;
            if (H() && typeof H().openFile === 'function') {
                H().openFile(r.path);
            }
        });
    }

    function renderTreemap() {
        const chart = ensureChart('ba-treemap-chart');
        if (!chart) return;
        const rows = (state.files && state.files.rows) || [];
        const map = {};
        rows.forEach((r) => {
            const d = dirOf(r.path);
            map[d] = (map[d] || 0) + (Number(r.view_ms) || 0);
        });
        const data = Object.keys(map)
            .map((name, i) => ({
                name,
                value: fmtMinutes(map[name]),
                itemStyle: { color: DIR_COLORS[i % DIR_COLORS.length] },
            }))
            .filter((d) => d.value > 0)
            .sort((a, b) => b.value - a.value)
            .slice(0, 12);
        setEmpty('ba-treemap-chart', data.length ? '' : '该区间暂无目录浏览数据');
        chart.setOption(baseOption({
            tooltip: Object.assign({}, baseOption().tooltip, {
                formatter: (p) => '<div style="font-size: 11px; line-height: 1.6;"><b>' + esc(p.name) + '</b><br/>浏览时长：' + p.value + ' 分钟</div>'
            }),
            series: [{
                type: 'treemap',
                roam: false,
                breadcrumb: { show: false },
                nodeClick: false,
                width: '100%',
                height: '100%',
                top: 4,
                left: 4,
                right: 4,
                bottom: 4,
                label: { fontSize: CHART_FS, color: '#fff', formatter: (p) => p.name + '\n' + p.value + ' 分' },
                itemStyle: { borderColor: '#fff', borderWidth: 2, gapWidth: 2 },
                data,
            }],
        }), true);

        chart.off('click');
        chart.on('click', (p) => {
            if (!p || !p.name) return;
            state.dirFilter = state.dirFilter === p.name ? null : p.name;
            renderTop10();
            if (state.dirFilter) notify('已按目录筛选：' + p.name, 'info');
        });
    }

    function renderColdHot() {
        const chart = ensureChart('ba-coldhot-chart');
        if (!chart) return;
        const rows = (state.files && state.files.rows) || [];
        const opened = (state.files && state.files.opened_total) || 0;
        const total = state.totalFiles || 0;
        const cold = Math.max(0, total - opened);
        const once = rows.filter((r) => Number(r.opens) === 1).length;
        const mid = rows.filter((r) => Number(r.opens) >= 2 && Number(r.opens) <= 5).length;
        const hot = rows.filter((r) => Number(r.opens) > 5).length;
        const sum = cold + once + mid + hot;
        const data = [
            { name: '从未浏览', value: cold, itemStyle: { color: '#C9D3E3' } },
            { name: '仅 1 次', value: once, itemStyle: { color: COLOR.view } },
            { name: '2–5 次', value: mid, itemStyle: { color: '#12A5A5' } },
            { name: '5 次以上', value: hot, itemStyle: { color: COLOR.edit } },
        ];
        setEmpty('ba-coldhot-chart', sum > 0 ? '' : '还没有文件被浏览');
        chart.setOption(baseOption({
            tooltip: Object.assign({}, baseOption().tooltip, {
                trigger: 'item',
                formatter: (p) => '<div style="font-size: 12px; line-height: 1.6;">'+ p.name + '：' + p.value + ' 个（' + pct(p.value, sum) + '）</div>',
            }),
            legend: { bottom: 0, left: 'center', itemWidth: 8, itemHeight: 8, textStyle: { color: COLOR.sub, fontSize: CHART_FS } },
            series: [{
                type: 'pie',
                radius: ['48%', '70%'],
                center: ['50%', '44%'],
                avoidLabelOverlap: true,
                label: { show: false },
                emphasis: { label: { show: true, fontSize: 12, fontWeight: 600, formatter: '{b}\n' + total + ' 个文件中的 {d}%' } },
                data,
            }],
            graphic: [{
                type: 'text',
                left: 'center',
                top: '38%',
                style: { text: String(total || 0), fontSize: CHART_FS_LG, fontWeight: 600, fill: COLOR.text, textAlign: 'center' },
            }, {
                type: 'text',
                left: 'center',
                top: '47%',
                style: { text: '文件总数', fontSize: CHART_FS, fill: COLOR.axis, textAlign: 'center' },
            }],
        }), true);
    }

    function renderScatter() {
        const chart = ensureChart('ba-scatter-chart');
        if (!chart) return;
        const rows = ((state.files && state.files.rows) || []).filter((r) => (Number(r.view_ms) || 0) > 0 || (Number(r.edits) || 0) > 0);
        const points = rows.slice(0, 200).map((r) => ({
            name: baseName(r.path),
            value: [fmtMinutes(r.view_ms), Number(r.edits) || 0, Number(r.opens) || 0, r.path],
        }));
        const xVals = points.map((p) => p.value[0]).sort((a, b) => a - b);
        const yVals = points.map((p) => p.value[1]).sort((a, b) => a - b);
        const med = (arr) => (arr.length ? arr[Math.floor(arr.length / 2)] : 0);
        const mx = Math.max(1, med(xVals));
        const my = Math.max(1, med(yVals));
        setEmpty('ba-scatter-chart', points.length ? '' : '该区间暂无可对比的文件');
        chart.setOption(baseOption({
            tooltip: Object.assign({}, baseOption().tooltip, {
                formatter: (p) => '<div style="font-size: 12px; line-height: 1.6;"><b>' + esc(p.value[3]) + '</b><br/>浏览：' + p.value[0] + ' 分钟<br/>编辑：' + p.value[1] + ' 次<br/>打开：' + p.value[2] + ' 次</div>',
            }),
            grid: { left: 8, right: 16, top: 30, bottom: 4, containLabel: true },
            xAxis: valueAxis({ name: '浏览(分钟)', nameTextStyle: { color: COLOR.axis, fontSize: CHART_FS }, scale: true }),
            yAxis: valueAxis({ name: '编辑(次)', nameTextStyle: { color: COLOR.axis, fontSize: CHART_FS }, scale: true }),
            series: [{
                type: 'scatter',
                symbolSize: (v) => Math.min(18, 7 + (Number(v[2]) || 0)),
                data: points,
                itemStyle: { color: 'rgba(47,107,255,0.65)', borderColor: 'rgba(47,107,255,0.9)' },
                emphasis: { focus: 'self' },
                markLine: {
                    silent: true,
                    symbol: 'none',
                    lineStyle: { color: '#C9D3E3', type: 'dashed', width: 1 },
                    label: { show: false },
                    data: [{ xAxis: mx }, { yAxis: my }],
                },
            }],
        }), true);

        chart.off('click');
        chart.on('click', (p) => {
            const path = p && p.value && p.value[3];
            if (path && H() && typeof H().openFile === 'function') H().openFile(path);
        });
    }

    // ─── 交互 ───

    function bindRange() {
        const el = document.getElementById('ba-range');
        if (!el || el.dataset.bound === '1') return;
        el.dataset.bound = '1';
        el.querySelectorAll('button[data-days]').forEach((btn) => {
            btn.addEventListener('click', () => {
                const days = Number(btn.getAttribute('data-days'));
                if (days === state.days) return;
                setRange(days);
            });
        });
    }

    function setRange(days) {
        state.days = days;
        state.dirFilter = null;
        const el = document.getElementById('ba-range');
        if (el) {
            el.querySelectorAll('button[data-days]').forEach((b) => {
                b.classList.toggle('active', Number(b.getAttribute('data-days')) === days);
            });
        }
        render();
    }

    function bindTopMetric() {
        const el = document.getElementById('ba-top-metric');
        if (!el || el.dataset.bound === '1') return;
        el.dataset.bound = '1';
        el.querySelectorAll('button[data-metric]').forEach((btn) => {
            btn.addEventListener('click', () => {
                const m = btn.getAttribute('data-metric');
                if (m === state.topMetric) return;
                state.topMetric = m;
                el.querySelectorAll('button[data-metric]').forEach((b) => {
                    b.classList.toggle('active', b.getAttribute('data-metric') === m);
                });
                renderTop10();
            });
        });
    }

    // ─── 入口 ───

    async function render() {
        if (!canRun()) {
            const kpi = document.getElementById('ba-kpi');
            if (kpi) kpi.innerHTML = '<div class="ba-empty" style="position:static">仅桌面模式支持行为统计，请先打开知识库</div>';
            return;
        }
        if (state.loading) return;
        state.loading = true;
        bindRange();
        bindTopMetric();
        try {
            await load();
            // 首次升级：把旧 localStorage 浏览记录回填一次，再重新取数
            if (await maybeBackfill()) {
                await load();
            }
            renderKpi();
            renderTrend();
            renderHourly();
            renderTop10();
            renderTreemap();
            renderColdHot();
            renderScatter();
            raf(resizeAll);
        } catch (e) {
            console.error('[behavior] 渲染失败:', e);
            notify('行为统计加载失败：' + (e && (e.message || e)), 'error');
        } finally {
            state.loading = false;
        }
    }

    function refresh() {
        return render();
    }

    function getRangeDays() {
        return state.days;
    }

    window.addEventListener('resize', () => {
        raf(resizeAll);
    });

    window.mdgoBehavior = {
        render,
        refresh,
        setRange,
        getRangeDays,
        rangeFor,
    };
})();


