/**
 * ===== 表格转数据库视图（css_js/modules/editor/db-view.js） =====
 *
 * 【职责】P2-2 块转数据库：Markdown 表格 → 数据库视图（筛选/排序/字段统计）。
 *   - postProcessMarkdown 中为带表头的表格添加「📊 数据库视图」按钮
 *   - 点击弹出数据库面板：列头点击排序、筛选框过滤行、行数统计、CSV 复制
 *   - 数据仍来自表格内容（Markdown 为事实源，视图不另存，检索可索引原表格）
 * 【入口】main.html postProcessMarkdown 调用 bindDbViews(dom)
 * 【依赖】运行时主脚本全局：showNotification / copyToClipboard
 * 【对外暴露】window.bindDbViews / window.MdgoDbView
 */
(function () {
    'use strict';

    function parseTable(tableEl) {
        const rows = [];
        const trs = tableEl.querySelectorAll('tr');
        if (trs.length === 0) return { headers: [], rows: [] };
        const headers = [...trs[0].querySelectorAll('th, td')].map(c => (c.textContent || '').trim());
        for (let i = 1; i < trs.length; i++) {
            const cells = [...trs[i].querySelectorAll('th, td')].map(c => (c.textContent || '').trim());
            if (cells.length > 0) rows.push(cells);
        }
        return { headers, rows };
    }

    function openDbView(tableEl) {
        const { headers, rows } = parseTable(tableEl);
        if (headers.length === 0) return;
        const overlay = document.createElement('div');
        overlay.className = 'mdgo-semantic-overlay';
        let sortIdx = -1, sortDir = 1, filterText = '';
        const state = { rows };

        function render() {
            let data = state.rows;
            if (filterText) {
                const q = filterText.toLowerCase();
                data = data.filter(r => r.some(c => String(c).toLowerCase().includes(q)));
            }
            if (sortIdx >= 0) {
                data = [...data].sort((a, b) => {
                    const va = a[sortIdx], vb = b[sortIdx];
                    const na = parseFloat(va), nb = parseFloat(vb);
                    const cmp = (!isNaN(na) && !isNaN(nb)) ? na - nb : String(va).localeCompare(String(vb), 'zh-CN');
                    return cmp * sortDir;
                });
            }
            // 修复(S5)：只重建表格区与计数，输入框保持不重建（否则每键丢焦点只能输 1 字符）
            const thead = headers.map((h, i) =>
                `<th data-i="${i}" class="mdgo-db-th${i === sortIdx ? ' sorted' : ''}">${escapeHtml2(h)}${i === sortIdx ? (sortDir > 0 ? ' ▲' : ' ▼') : ''}</th>`).join('');
            const tbody = data.map(r =>
                `<tr>${r.map(c => `<td>${escapeHtml2(c)}</td>`).join('')}</tr>`).join('') ||
                `<tr><td colspan="${headers.length}" class="mdgo-db-empty">无匹配行</td></tr>`;
            countEl.textContent = `${data.length} / ${state.rows.length} 行`;
            tableWrap.innerHTML = `<table class="mdgo-db-table"><thead><tr>${thead}</tr></thead><tbody>${tbody}</tbody></table>`;
            tableWrap.querySelectorAll('.mdgo-db-th').forEach(th => {
                th.addEventListener('click', () => {
                    const i = Number(th.dataset.i);
                    if (sortIdx === i) sortDir *= -1; else { sortIdx = i; sortDir = 1; }
                    render();
                });
            });
        }

        overlay.innerHTML = `
            <div class="mdgo-semantic-panel mdgo-db-panel">
                <div class="mdgo-semantic-head">
                    <span>📊 数据库视图（${headers.length} 字段）</span>
                    <button type="button" class="mdgo-semantic-close">✕</button>
                </div>
                <div class="mdgo-db-body">
                    <div class="mdgo-db-toolbar">
                        <input type="text" class="mdgo-db-filter" placeholder="筛选…">
                        <span class="mdgo-db-count"></span>
                        <button type="button" class="mdgo-export-btn mdgo-db-copy">复制CSV</button>
                    </div>
                    <div class="mdgo-db-table-wrap"></div>
                </div>
            </div>`;
        document.body.appendChild(overlay);
        const filterEl = overlay.querySelector('.mdgo-db-filter');
        const countEl = overlay.querySelector('.mdgo-db-count');
        const tableWrap = overlay.querySelector('.mdgo-db-table-wrap');
        const close = () => overlay.remove();
        overlay.querySelector('.mdgo-semantic-close').addEventListener('click', close);
        overlay.addEventListener('click', (e) => { if (e.target === overlay) close(); });
        filterEl.addEventListener('input', (e) => { filterText = e.target.value; render(); });
        overlay.querySelector('.mdgo-db-copy').addEventListener('click', () => {
            let data = state.rows;
            if (filterText) {
                const q = filterText.toLowerCase();
                data = data.filter(r => r.some(c => String(c).toLowerCase().includes(q)));
            }
            const csv = [headers, ...data].map(r => r.map(c => `"${String(c).replace(/"/g, '""')}"`).join(',')).join('\n');
            if (typeof window.copyTextToClipboard === 'function') window.copyTextToClipboard(csv);
            if (typeof window.showNotification === 'function') window.showNotification('✓ CSV 已复制', 'success');
        });
        render();
    }

    window.bindDbViews = function (dom) {
        if (!dom) return;
        dom.querySelectorAll('table').forEach(tableEl => {
            if (tableEl.closest('.mdgo-db-panel') || tableEl.closest('.mdgo-ai-block')) return;
            if (tableEl.closest('.mdgo-db-wrap')) return; // 已包裹
            const firstRow = tableEl.querySelector('tr');
            if (!firstRow || !firstRow.querySelector('th')) return; // 仅带表头的表格
            // H4 修复：按钮 absolute 定位需要 positioned 祖先。把 table 包进
            // .mdgo-db-wrap（position:relative），按钮作为其子元素定位才可靠
            const wrap = document.createElement('div');
            wrap.className = 'mdgo-db-wrap';
            const btn = document.createElement('button');
            btn.type = 'button';
            btn.className = 'mdgo-db-btn';
            btn.textContent = '📊 数据库';
            btn.title = 'P2-2：表格转数据库视图（筛选/排序）';
            btn.addEventListener('click', (e) => { e.stopPropagation(); openDbView(tableEl); });
            tableEl.parentNode.insertBefore(wrap, tableEl);
            wrap.appendChild(tableEl);
            wrap.appendChild(btn);
        });
    };

    function escapeHtml2(s) {
        return String(s).replace(/[&<>"']/g, m => ({
            '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
        })[m]);
    }

    window.MdgoDbView = { openDbView, parseTable };
})();
