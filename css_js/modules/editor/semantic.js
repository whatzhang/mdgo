/**
 * ===== 语义推荐模块（css_js/modules/editor/semantic.js） =====
 *
 * 【职责】P2-4 [[ 语义推荐 / 反向链接 / 文档关联提示：
 *   1. wikilinkCandidates(query)：kb_search_hybrid 语义候选（suggest.js [[ 分支合并）
 *   2. backlinks()：读 index_link_graph.json → 引用当前文件的笔记列表
 *   3. related()：当前文档（标题+前 200 字符）语义检索 → 非自身的相关笔记
 *   4. UI：initSemanticButtons() 注入 footer「反链」按钮（弹窗列表，点击跳转）；
 *      maybeSuggestRelated() 文档打开时后台提示"与 N 篇旧笔记相关"
 * 【依赖】运行时主脚本全局：currentRootPath / currentFileName / loadLinkGraphData /
 *         openFileFromPath / showNotification；window.MdgoDocument（core.js）
 * 【对外暴露】window.MdgoSemantic / window.initSemanticButtons / window.maybeSuggestRelated
 */
(function () {
    'use strict';

    const NOOP = () => { };

    async function searchHybrid(query, topK) {
        const invoke = window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke;
        if (!invoke || !currentRootPath || !query) return [];
        try {
            const hits = await invoke('kb_search_hybrid', { dirPath: currentRootPath, query, topK });
            return (hits || []).map(h => ({
                name: String(h.doc_name || h.path || '').replace(/\.md$/i, ''),
                path: String(h.doc_name || h.path || ''),
                score: h.score || 0
            }));
        } catch (e) {
            console.warn('[mdgo] 语义检索失败:', e);
            return [];
        }
    }

    const MdgoSemantic = {
        /** [[ 语义候选（供 suggest.js 合并） */
        wikilinkCandidates: (query, limit = 8) => searchHybrid(query, limit),

        /** 反向链接：图谱中指向当前文件的节点 */
        async backlinks() {
            if (typeof window.loadLinkGraphData !== 'function') return [];
            let g = null;
            try { g = await window.loadLinkGraphData(); } catch (e) { return []; }
            if (!g || !Array.isArray(g.nodes) || !Array.isArray(g.edges)) return [];
            const cur = (currentFileName || '').replace(/\.md$/i, '');
            if (!cur) return [];
            const curNode = g.nodes.find(n =>
                String(n.title || '').replace(/\.md$/i, '') === cur ||
                String(n.path || '').replace(/\.md$/i, '') === cur
            );
            if (!curNode) return [];
            const result = [];
            const seen = new Set();
            // 修复(S3)：图谱边字段是 {source, target}（buildLinkGraphData 写入），
            // 原实现读 toNode/fromNode 恒 undefined 导致反链恒空
            for (const e of g.edges) {
                if (e.target === curNode.id) {
                    const from = g.nodes.find(n => n.id === e.source);
                    if (from && from.path !== curNode.path && !seen.has(from.id)) {
                        seen.add(from.id);
                        result.push(from);
                    }
                }
            }
            return result;
        },

        /** 相关笔记：语义检索当前文档主题，返回非自身 top3 */
        async related() {
            const doc = (window.MdgoDocument && window.MdgoDocument.getValue) ? window.MdgoDocument.getValue() : '';
            const title = (currentFileName || '').replace(/\.md$/i, '');
            if (!title && !doc.trim()) return [];
            const query = (title + ' ' + doc.slice(0, 200)).slice(0, 300);
            const cands = await searchHybrid(query, 6);
            // 修复(S4)：c.path 是相对路径（doc_name），currentFileName 是 basename，
            // 双方都取 basename 后小写比对，排除当前文档自身
            const selfBase = String(currentFileName || '').split('/').pop().toLowerCase();
            return cands.filter(c => {
                const base = String(c.path || '').split('/').pop().toLowerCase();
                return base !== selfBase;
            }).slice(0, 3);
        }
    };

    // ===== 反链面板（footer 按钮 → 弹窗） =====
    function showBacklinksPanel(links) {
        const overlay = document.createElement('div');
        overlay.className = 'mdgo-semantic-overlay';
        overlay.innerHTML = `
            <div class="mdgo-semantic-panel">
                <div class="mdgo-semantic-head">
                    <span>🔗 反向链接（${links.length}）</span>
                    <button type="button" class="mdgo-semantic-close">✕</button>
                </div>
                <div class="mdgo-semantic-body">
                    ${links.length === 0 ? '<div class="mdgo-semantic-empty">暂无其他笔记引用当前文档</div>' :
                        links.map(l => `<div class="mdgo-semantic-item" data-path="${escapeHtml2(l.path || l.title || '')}">📄 ${escapeHtml2(l.title || l.path || '未知')}</div>`).join('')}
                </div>
            </div>`;
        document.body.appendChild(overlay);
        const close = () => overlay.remove();
        overlay.querySelector('.mdgo-semantic-close').addEventListener('click', close);
        overlay.addEventListener('click', (e) => { if (e.target === overlay) close(); });
        overlay.querySelectorAll('.mdgo-semantic-item').forEach(el => {
            el.addEventListener('click', () => {
                const p = el.dataset.path;
                close();
                if (p && typeof window.openFileFromPath === 'function') window.openFileFromPath(p);
            });
        });
    }

    window.initSemanticButtons = function () {
        const footer = document.getElementById('editor-footer');
        if (!footer || footer.querySelector('.mdgo-semantic-toggle')) return;
        const btn = document.createElement('button');
        btn.type = 'button';
        btn.className = 'mdgo-export-btn mdgo-semantic-toggle';
        btn.textContent = '反链';
        btn.title = '显示引用当前文档的反向链接（P2-4）';
        btn.addEventListener('click', async () => {
            btn.disabled = true;
            try {
                const links = await MdgoSemantic.backlinks();
                showBacklinksPanel(links);
            } catch (e) {
                if (typeof window.showNotification === 'function') window.showNotification('反链查询失败: ' + (e && e.message ? e.message : e), 'error');
            } finally {
                btn.disabled = false;
            }
        });
        const right = footer.querySelector('.status-right');
        if (right) right.appendChild(btn);
    };

    // ===== 文档关联提示（打开文档后后台提示，防抖 + 幂等） =====
    let relatedTimer = null;
    let lastSuggestedPath = '';
    window.maybeSuggestRelated = function () {
        if (relatedTimer) clearTimeout(relatedTimer);
        relatedTimer = setTimeout(async () => {
            try {
                const path = currentFileName || '';
                // 修复：无论有无结果都记录路径（无相关笔记的文件避免每次打开重复检索）
                if (!path) return;
                lastSuggestedPath = path;
                const rel = await MdgoSemantic.related();
                if (rel && rel.length > 0) {
                    if (typeof window.showNotification === 'function') {
                        const names = rel.map(r => r.name || r.path).slice(0, 3).join('、');
                        window.showNotification('💡 与 ' + rel.length + ' 篇旧笔记相关：' + names + '（可在 [[ 补全中查看）', 'info', 5000);
                    }
                }
            } catch (e) { /* 静默 */ }
        }, 2000);
    };

    function escapeHtml2(s) {
        return String(s).replace(/[&<>"']/g, m => ({
            '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
        })[m]);
    }

    window.MdgoSemantic = MdgoSemantic;
})();
