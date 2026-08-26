/**
 * ===== 幻灯片模块（css_js/modules/editor/slides.js） =====
 *
 * 【职责】P2-7 Markdown → 幻灯片：
 *   - 按独立一行 `---`（水平线）分页；跳过 YAML frontmatter
 *   - 全屏放映：黑色 overlay + 每页渲染（markdown-body 大字号），
 *     ←/→/Space 翻页、Esc 退出、底部页码与导航、requestFullscreen
 *   - 导出：打印时 @media print 每页一屏（print 样式见 markdown.css）
 * 【依赖】运行时主脚本全局：MdgoDocument / markedParse / parseObsidianToHTML /
 *         showNotification
 */
(function () {
    'use strict';

    function splitSlides(mdText) {
        let text = String(mdText || '');
        // 跳过 frontmatter（容忍 BOM/前导空行）
        text = text.replace(/^\uFEFF?/, '');
        const fm = text.match(/^---\n[\s\S]*?\n---\n?/);
        if (fm) text = text.slice(fm[0].length);
        // 修复(S7)：逐行扫描并跟踪 ``` 围栏状态，围栏内的 `---` 行不切分
        const parts = [];
        let cur = [];
        let inFence = false;
        for (const line of text.split('\n')) {
            const fenceMatch = line.match(/^\s*(```|~~~)/);
            if (fenceMatch) {
                inFence = !inFence;
                cur.push(line);
                continue;
            }
            if (!inFence && /^---\s*$/.test(line)) {
                if (cur.length > 0) parts.push(cur.join('\n').trim());
                cur = [];
            } else {
                cur.push(line);
            }
        }
        if (cur.length > 0) parts.push(cur.join('\n').trim());
        return parts.filter(Boolean).length > 0 ? parts : [text];
    }

    async function renderSlide(md) {
        try {
            return await window.markedParse(window.parseObsidianToHTML(md));
        } catch (e) {
            // 修复：错误路径转义，避免渲染失败时自 XSS
            return '<pre>' + escapeHtml2(String(md)) + '</pre>';
        }
    }

    function escapeHtml2(s) {
        return String(s).replace(/[&<>"']/g, m => ({
            '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
        })[m]);
    }

    function notify(msg, type) {
        if (typeof window.showNotification === 'function') window.showNotification(msg, type, 2000);
    }

    async function present(mdText) {
        const slides = splitSlides(mdText);
        if (slides.length === 0) { notify('无可放映的内容', 'warning'); return; }
        const overlay = document.createElement('div');
        overlay.className = 'mdgo-slides-overlay';
        let idx = 0;
        overlay.innerHTML = `
            <div class="mdgo-slides-page markdown-body"></div>
            <div class="mdgo-slides-nav">
                <span class="mdgo-slides-count"></span>
                <button type="button" class="mdgo-slides-prev"><svg xmlns="http://www.w3.org/2000/svg" style="transform: rotate(180deg);"
                                        width="10" height="10" fill="currentColor" aria-label="chevron"
                                        viewBox="-30.0 -0.0 201.1 201.1">
                                        <g transform="translate(0.000000,201.000000) scale(0.100000,-0.100000)">
                                            <path d="M332 1821 c-84 -21 -144 -97 -145 -185 -1 -85 -21 -62 421 -489 72
                                        -70 132 -136 132 -145 0 -9 -117 -133 -261 -276 -156 -154 -267 -272 -275
                                        -292 -28 -67 -9 -160 42 -205 59 -51 155 -64 222 -30 66 34 700 651 732 711
                                        27 53 32 92 19 146 -11 43 -27 63 -128 165 -227 227 -590 566 -623 582 -44 21
                                        -96 27 -136 18z"></path>
                                        </g>
                                    </svg></button>
                <button type="button" class="mdgo-slides-next"><svg xmlns="http://www.w3.org/2000/svg" width="10" height="10" fill="currentColor"
                                        aria-label="chevron" viewBox="-30.0 -0.0 201.1 201.1">
                                        <g transform="translate(0.000000,201.000000) scale(0.100000,-0.100000)">
                                            <path d="M332 1821 c-84 -21 -144 -97 -145 -185 -1 -85 -21 -62 421 -489 72
                                        -70 132 -136 132 -145 0 -9 -117 -133 -261 -276 -156 -154 -267 -272 -275
                                        -292 -28 -67 -9 -160 42 -205 59 -51 155 -64 222 -30 66 34 700 651 732 711
                                        27 53 32 92 19 146 -11 43 -27 63 -128 165 -227 227 -590 566 -623 582 -44 21
                                        -96 27 -136 18z"></path>
                                        </g>
                                    </svg></button>
                <button type="button" class="mdgo-slides-exit"><svg  width="10" height="10" viewBox="0 0 16 16" version="1.1" xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink">
<path fill="currentColor" d="M15.1 3.1l-2.2-2.2-4.9 5-4.9-5-2.2 2.2 5 4.9-5 4.9 2.2 2.2 4.9-5 4.9 5 2.2-2.2-5-4.9z"></path>
</svg></button>
            </div>`;
        document.body.appendChild(overlay);
        const pageEl = overlay.querySelector('.mdgo-slides-page');
        const countEl = overlay.querySelector('.mdgo-slides-count');
        const prevBtn = overlay.querySelector('.mdgo-slides-prev');
        const nextBtn = overlay.querySelector('.mdgo-slides-next');
        const exitBtn = overlay.querySelector('.mdgo-slides-exit');

        async function show() {
            pageEl.innerHTML = await renderSlide(slides[idx]);
            countEl.textContent = `${idx + 1} / ${slides.length}`;
            prevBtn.disabled = idx === 0;
            nextBtn.disabled = idx === slides.length - 1;
            // 每个幻灯页内的图表/公式懒渲染
            if (typeof window.postProcessMarkdown === 'function') {
                try { window.postProcessMarkdown(pageEl); } catch (e) { }
            }
        }
        function go(delta) {
            const n = idx + delta;
            if (n >= 0 && n < slides.length) { idx = n; show(); }
        }
        async function close() {
            document.removeEventListener('keydown', onKey, true);
            try { await exitFullAppscreen(); } catch (e) { }
            overlay.remove();
        }
        async function onKey(e) {
            if (e.key === 'Escape') { e.preventDefault(); await close(); }
            else if (e.key === 'ArrowRight' || e.key === ' ' || e.key === 'PageDown') { e.preventDefault(); go(1); }
            else if (e.key === 'ArrowLeft' || e.key === 'PageUp') { e.preventDefault(); go(-1); }
        }
        prevBtn.addEventListener('click', () => go(-1));
        nextBtn.addEventListener('click', () => go(1));
        exitBtn.addEventListener('click', close);
        overlay.addEventListener('click', (e) => {
            if (e.target === overlay || e.target === pageEl) go(1);
        });
        document.addEventListener('keydown', onKey, true);
        await enterFullAppscreen();
        await show();
    }

    window.markdownSlides = async function (text) {
        if (!text.trim()) { notify('当前文档为空', 'warning'); return; }
        await present(text);
    };
})();
