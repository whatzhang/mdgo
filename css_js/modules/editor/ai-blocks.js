/**
 * ===== AI 块运行时（css_js/modules/editor/ai-blocks.js） =====
 *
 * 【职责】P2-3 动态 AI 块（fenced ai-block 表达，落盘为合法 Markdown，动态视图不写回）：
 *   ```ai-block
 *   { "type": "summary|todos|semantic-search|tags", "query": "可选", "refresh": "auto|manual" }
 *   ```
 *   - summary：当前文档动态摘要（变更后防抖刷新）
 *   - todos：扫描文档 - [ ] / - [x] 提取任务列表
 *   - semantic-search：kb_search_hybrid 前 5 条 + 引用链接（点击跳转）
 *   - tags：frontmatter tags + 高频词建议
 * 【入口】main.html postProcessMarkdown 调用 bindAiBlocks(dom)
 * 【依赖】运行时主脚本全局：currentRootPath / showNotification / markedParse /
 *         callAIAPI；window.MdgoDocument（core.js）；Tauri invoke kb_search_hybrid
 * 【对外暴露】window.bindAiBlocks / window.MdgoAiBlocks
 */
(function () {
    'use strict';

    const TITLES = {
        summary: '📊 动态摘要',
        todos: '✅ 待办提取',
        'semantic-search': '🔍 语义搜索',
        tags: '🏷 标签建议'
    };

    // ===== 解析 ai-block fenced 块 =====
    function parseConfig(codeText) {
        const text = String(codeText || '').trim();
        try {
            const obj = JSON.parse(text);
            if (obj && typeof obj === 'object') return obj;
        } catch (e) { /* 非 JSON 直接忽略 */ }
        return { type: 'summary', refresh: 'manual' };
    }

    // ===== 当前文档全文（块所在文档 = 活动文档） =====
    function currentDocText() {
        const d = window.MdgoDocument;
        if (d && d.getValue) return d.getValue();
        return '';
    }

    // 移除代码块/行内代码，避免代码块内的 - [ ]/#tag 被误统计
    function stripFence(text) {
        return String(text).replace(/```[\s\S]*?```/g, '').replace(/`[^`]*`/g, '');
    }

    // ===== 各类型刷新 =====
    async function refreshSummary(bodyEl, query) {
        const text = currentDocText();
        if (!text.trim()) { bodyEl.textContent = '（空文档）'; return; }
        const prompt = '请对以下文档生成简洁摘要（3-5 条要点），保留关键数据，直接输出 Markdown 列表：\n\n' + text.slice(0, 12000);
        const result = await window.callAIAPI(prompt, '');
        // 修复：AI 输出经 DOMPurify 清洗（与 renderChatMarkdown 一致），防恶意 HTML
        const raw = await window.markedParse(result || '');
        bodyEl.innerHTML = (typeof window.DOMPurify !== 'undefined') ? window.DOMPurify.sanitize(raw) : raw;
    }

    async function refreshTodos(bodyEl, query) {
        const text = stripFence(currentDocText());
        const lines = text.split('\n');
        const todos = lines.filter(l => /^\s*[-*]\s*\[[ xX]\]/.test(l)).map(l => l.trim());
        if (todos.length === 0) { bodyEl.textContent = '（未发现待办项）'; return; }
        bodyEl.innerHTML = '<div class="mdgo-ai-block-list">' + todos.map(t =>
            '<div class="mdgo-ai-block-todo">' + (t.includes('[x]') || t.includes('[X]') ? '☑ ' : '☐ ') + escapeHtml2(t.replace(/^\s*[-*]\s*\[[ xX]\]\s*/, '')) + '</div>'
        ).join('') + '</div>';
    }

    async function refreshSemantic(bodyEl, query) {
        // 修复：config.query 为空时回退当前文档标题（去扩展名），避免向
        // kb_search_hybrid 传空查询
        const q = (query || '').trim() ||
            String(currentFileName || '').replace(/\.md$/i, '').trim();
        if (!q) { bodyEl.textContent = '（未指定查询内容）'; return; }
        const invoke = window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke;
        if (!invoke || !currentRootPath) {
            bodyEl.textContent = '（语义搜索需要知识库索引，请先在设置中启用）';
            return;
        }
        try {
            const hits = await invoke('kb_search_hybrid', { dirPath: currentRootPath, query: q, topK: 5 });
            if (!hits || hits.length === 0) { bodyEl.textContent = '（未找到相关内容）'; return; }
            const rows = hits.map(h => {
                const name = h.doc_name || h.path || '未知';
                return `<div class="mdgo-ai-block-link" data-path="${escapeHtml2(name)}" title="${escapeHtml2(name)}">📄 ${escapeHtml2(name)} <span class="mdgo-ai-block-score">${(h.score || 0).toFixed(2)}</span></div>`;
            }).join('');
            bodyEl.innerHTML = '<div class="mdgo-ai-block-list">' + rows + '</div>';
            bodyEl.querySelectorAll('.mdgo-ai-block-link').forEach(el => {
                el.addEventListener('click', () => {
                    const p = el.dataset.path;
                    if (typeof window.openFileFromPath === 'function') window.openFileFromPath(p);
                });
            });
        } catch (e) {
            bodyEl.textContent = '语义搜索失败: ' + (e && e.message ? e.message : e);
        }
    }

    async function refreshTags(bodyEl, query) {
        const text = stripFence(currentDocText());
        const tags = new Set();
        // frontmatter tags
        const fm = text.match(/^---\n([\s\S]*?)\n---/);
        if (fm) {
            const b = fm[1].match(/tags:\s*\[([^\]]+)\]/);
            if (b) b[1].split(',').map(t => t.trim()).filter(Boolean).forEach(t => tags.add(t));
            const h = fm[1].match(/tags:\s*(#\S+(?:\s*,\s*#\S+)*)/);
            if (h) h[1].split(',').map(t => t.trim().replace(/^#/, '')).filter(Boolean).forEach(t => tags.add(t));
        }
        // 行内 #tag（排除标题）
        const re = /(^|\s)#([\w\u4e00-\u9fa5\-_\/]+)/g;
        let m;
        while ((m = re.exec(text))) {
            const lineStart = text.lastIndexOf('\n', m.index);
            const line = text.substring(lineStart + 1, text.indexOf('\n', m.index) < 0 ? text.length : text.indexOf('\n', m.index));
            if (/^#{1,6}\s/.test(line.trim())) continue; // 跳过标题
            tags.add(m[2]);
        }
        if (tags.size === 0) { bodyEl.textContent = '（未发现标签）'; return; }
        bodyEl.innerHTML = '<div class="mdgo-ai-block-list">' + [...tags].map(t =>
            `<span class="mdgo-ai-block-tag">#${escapeHtml2(t)}</span>`
        ).join(' ') + '</div>';
    }

    const REFRESHERS = {
        summary: refreshSummary,
        todos: refreshTodos,
        'semantic-search': refreshSemantic,
        tags: refreshTags
    };

    // ===== 渲染卡片并替换原 fenced 块 =====
    function renderBlock(codeEl, config) {
        const type = REFRESHERS[config.type] ? config.type : 'summary';
        const card = document.createElement('div');
        card.className = 'mdgo-ai-block';
        card.dataset.type = type;
        card.dataset.query = config.query || '';
        card.dataset.docTitle = '';
        const title = TITLES[type] || '🤖 AI 块';
        card.innerHTML = `
            <div class="mdgo-ai-block-head">
                <span class="mdgo-ai-block-title">${title}</span>
                <button type="button" class="mdgo-ai-block-refresh" title="刷新">↻ 刷新</button>
            </div>
            <div class="mdgo-ai-block-body">加载中…</div>`;
        const pre = codeEl.closest('pre');
        if (pre && pre.parentNode) pre.parentNode.replaceChild(card, pre);
        else if (codeEl.parentNode) codeEl.parentNode.replaceChild(card, codeEl);
        card.querySelector('.mdgo-ai-block-refresh').addEventListener('click', () => refreshCard(card));
        // auto 刷新一次（防抖）；修复：实时预览每次输入会整块重渲重建卡片，
        // 用内容指纹节流——同文档内容 10 秒内复用上次结果，避免 AI 调用风暴
        if (config.refresh !== 'manual') {
            setTimeout(() => refreshCard(card, true), 300);
        }
        return card;
    }

    let refreshChain = Promise.resolve();
    // 内容指纹 → 上次渲染 HTML + 时间（auto 刷新节流，10s）
    const autoCache = new Map();
    function docFingerprint() {
        const t = currentDocText();
        let h = 5381;
        for (let i = 0; i < t.length && i < 4000; i++) h = ((h << 5) + h + t.charCodeAt(i)) | 0;
        return String(h) + ':' + t.length;
    }
    async function refreshCard(card, isAuto) {
        const type = card.dataset.type;
        const bodyEl = card.querySelector('.mdgo-ai-block-body');
        if (!bodyEl || !REFRESHERS[type]) return;
        if (isAuto) {
            // auto：同文档内容 10 秒内复用缓存，不调 LLM
            const key = type + '|' + (card.dataset.query || '') + '|' + docFingerprint();
            const hit = autoCache.get(key);
            if (hit && Date.now() - hit.ts < 10000) {
                bodyEl.innerHTML = hit.html;
                return;
            }
        }
        bodyEl.textContent = '加载中…';
        refreshChain = refreshChain.then(async () => {
            try {
                await REFRESHERS[type](bodyEl, card.dataset.query);
                if (isAuto) {
                    const key = type + '|' + (card.dataset.query || '') + '|' + docFingerprint();
                    autoCache.set(key, { html: bodyEl.innerHTML, ts: Date.now() });
                    if (autoCache.size > 50) {
                        const first = autoCache.keys().next().value;
                        if (first !== undefined) autoCache.delete(first);
                    }
                }
            } catch (e) {
                bodyEl.textContent = '刷新失败: ' + (e && e.message ? e.message : e);
            }
        });
        await refreshChain;
    }

    // ===== 入口：postProcessMarkdown 调用 =====
    window.bindAiBlocks = function (dom) {
        if (!dom) return;
        dom.querySelectorAll('pre code.language-ai-block').forEach(codeEl => {
            if (codeEl.closest('.mdgo-ai-block')) return;
            const config = parseConfig(codeEl.textContent);
            renderBlock(codeEl, config);
        });
    };

    function escapeHtml2(s) {
        return String(s).replace(/[&<>"']/g, m => ({
            '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
        })[m]);
    }

    window.MdgoAiBlocks = {
        REFRESHERS,
        refresh: refreshCard,
        parseConfig
    };
})();
