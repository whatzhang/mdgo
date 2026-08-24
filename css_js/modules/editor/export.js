/**
 * ===== 导出模块（css_js/modules/editor/export.js） =====
 *
 * 【职责】P0-4 导出（不做命令面板，入口为编辑器 footer 按钮组 + 全局 API）：
 *   1. copyMarkdown：复制当前文件 Markdown 源码到剪贴板
 *   2. exportHtml：当前预览内容快照 + 内联 CSS → 独立 HTML 文件（所见即所得，离线可用）
 *   3. exportPdf：渲染内容到打印容器 → window.print()（WebView 打印对话框另存 PDF，
 *      @media print 隐藏 UI 只显示内容，样式见 markdown.css）
 * 【入口】main.html enterEditMode / enterLivePreviewMode 调用 initExportFooter()
 * 【依赖】运行时主脚本全局：markedParse / parseObsidianToHTML / copyToClipboard /
 *         currentEditor / originalContent / previewFileText / showNotification；
 *         Tauri invoke：dialog.save / write_file
 * 【对外暴露】window.mdgoExport（API）/ window.initExportFooter（注入 footer 按钮组，幂等）
 */
(function () {
    'use strict';

    function getMdText() {
        if (currentEditor && currentEditor.getValue) {
            const v = currentEditor.getValue();
            if (v) return v;
        }
        return originalContent || previewFileText || '';
    }

    async function fetchCssText(path) {
        try {
            const res = await fetch(path, { cache: 'no-cache' });
            if (!res.ok) return '';
            return await res.text();
        } catch (e) {
            console.warn('[mdgo] 读取样式失败(降级):', path, e);
            return '';
        }
    }

    async function renderToHtml() {
        const text = getMdText();
        if (!text) return '';
        // 与主内容/实时预览同管线（含 Obsidian 语法）。注意：mermaid/KaTeX 等
        // JS 渲染不在此执行（exportHtml 通过内联 CDN script 自渲染，见 exportHtml）
        const html = await window.markedParse(window.parseObsidianToHTML(text));
        return html;
    }

    /** 复制任意文本：优先 main.html 的 copyTextToClipboard(text)（有参），
     *  注意不能用无参的 copyToClipboard()（其语义是复制当前文件磁盘内容） */
    async function copyText(text) {
        if (typeof window.copyTextToClipboard === 'function') {
            await window.copyTextToClipboard(text);
            return true;
        }
        if (navigator.clipboard && navigator.clipboard.writeText) {
            await navigator.clipboard.writeText(text);
            return true;
        }
        return false;
    }

    const mdgoExport = {
        /** 复制当前文件 Markdown 源码 */
        async copyMarkdown() {
            const text = getMdText();
            if (!text) { showNotify('当前没有可复制的 Markdown 内容', 'warning'); return; }
            const ok = await copyText(text);
            showNotify(ok ? '✓ Markdown 源码已复制' : '复制功能不可用', ok ? 'success' : 'error');
        },

        /** 导出当前预览内容为独立 HTML（内联样式快照） */
        async exportHtml() {
            const bodyHtml = await renderToHtml();
            if (!bodyHtml) { showNotify('当前没有可导出的 Markdown 内容', 'warning'); return; }
            const cssParts = [];
            // 顺序：正文样式 → Markdown 业务样式 → 公式样式 → 代码高亮
            for (const p of [
                'css_js/cdn/github-markdown-light.min.css',
                'css_js/modules/markdown.css',
                'css_js/cdn/katex.min.css',
                'css_js/cdn/atom-one-dark.min.css'
            ]) {
                const t = await fetchCssText(p);
                if (t) cssParts.push(t);
            }
            const styleHtml = cssParts.length
                ? `<style>\n${cssParts.join('\n')}\n</style>`
                : '<!-- 样式内联失败：导出为无样式 HTML -->';
            // 修复：导出内容为渲染态快照（mermaid/KaTeX 未执行 JS 渲染），
            // 引入 CDN script 让导出文件在联网时自渲染图表/公式/代码高亮
            const selfRenderScript = `
<script src="https://cdn.jsdelivr.net/npm/katex@0.16.9/dist/katex.min.js"></script>
<script src="https://cdn.jsdelivr.net/npm/katex@0.16.9/dist/contrib/auto-render.min.js"></script>
<script src="https://cdn.jsdelivr.net/gh/highlightjs/cdn-release@11/build/highlight.min.js"></script>
<script src="https://cdn.jsdelivr.net/npm/mermaid@10/dist/mermaid.min.js"></script>
<script>
document.addEventListener('DOMContentLoaded', function () {
  if (window.renderMathInElement) {
    try { renderMathInElement(document.body, { delimiters: [{left:'$$',right:'$$',display:true},{left:'$',right:'$',display:false}] }); } catch (e) {}
  }
  if (window.hljs) {
    document.querySelectorAll('pre code:not(.language-mermaid)').forEach(function (b) {
      try { hljs.highlightElement(b); } catch (e) {}
    });
  }
  if (window.mermaid) {
    try {
      mermaid.initialize({ startOnLoad: false });
      document.querySelectorAll('pre code.language-mermaid').forEach(function (el) {
        var src = el.textContent;
        var key = 'mmd' + Math.random().toString(36).slice(2);
        mermaid.render(key, src).then(function (res) {
          var wrap = el.closest('pre');
          if (wrap) { wrap.outerHTML = '<div class="mermaid">' + res.svg + '</div>'; }
        }).catch(function () {});
      });
    } catch (e) {}
  }
});
</script>`;
            const doc = `<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>${escapeHtml2(currentFileName || 'mdgo 导出')}</title>
${styleHtml}
</head>
<body style="margin:0;padding:0;background:var(--color-bg-secondary,#fff);">
<div class="markdown-body" style="max-width:860px;margin:0 auto;padding:24px;box-sizing:border-box;">
${bodyHtml}
</div>
${selfRenderScript}
</body>
</html>`;
            await saveTextFile('export.html', doc, 'text/html', 'html');
        },

        /** 渲染到打印容器并调起系统打印（可另存为 PDF） */
        async exportPdf() {
            const bodyHtml = await renderToHtml();
            if (!bodyHtml) { showNotify('当前没有可打印的 Markdown 内容', 'warning'); return; }
            let root = document.getElementById('mdgo-print-root');
            if (!root) {
                root = document.createElement('div');
                root.id = 'mdgo-print-root';
                root.style.display = 'none';
                document.body.appendChild(root);
            }
            root.innerHTML = `<div class="markdown-body">${bodyHtml}</div>`;
            // 让 @media print 生效：显示打印容器、隐藏其余 UI（样式见 markdown.css）
            try {
                // 等待图表/图片就绪（尽力而为）
                await new Promise(r => setTimeout(r, 300));
                window.print();
            } catch (e) {
                showNotify('打印失败: ' + (e && e.message ? e.message : e), 'error');
            }
        }
    };

    /** Tauri dialog.save + write_file 保存文本文件；非 Tauri 环境降级下载 */
    async function saveTextFile(defaultName, content, mime, ext) {
        if (window.__TAURI__ && window.__TAURI__.dialog && window.__TAURI__.core) {
            try {
                const p = await window.__TAURI__.dialog.save({
                    defaultPath: defaultName,
                    filters: [{ name: mime, extensions: [ext] }]
                });
                if (!p) return; // 用户取消
                await window.__TAURI__.core.invoke('write_file', { path: p, content });
                showNotify('✓ 已导出: ' + p, 'success');
                return;
            } catch (e) {
                showNotify('✗ 导出失败: ' + (e && e.message ? e.message : e), 'error');
                return;
            }
        }
        // 降级：浏览器下载
        const blob = new Blob([content], { type: mime + ';charset=utf-8' });
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url; a.download = defaultName; a.click();
        setTimeout(() => URL.revokeObjectURL(url), 3000);
        showNotify('✓ 已下载: ' + defaultName, 'success');
    }

    function escapeHtml2(s) {
        return String(s).replace(/[&<>"']/g, m => ({
            '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
        })[m]);
    }

    function showNotify(msg, type) {
        if (typeof window.showNotification === 'function') {
            window.showNotification(msg, type, 2000);
        } else {
            console.log('[mdgo]', msg);
        }
    }

    /** 向编辑器 footer 注入导出按钮组（幂等） */
    window.initExportFooter = function () {
        const footer = document.getElementById('editor-footer');
        if (!footer) return;
        if (footer.querySelector('.mdgo-export-group')) return;
        const right = footer.querySelector('.status-right');
        if (!right) return;
        const group = document.createElement('span');
        group.className = 'mdgo-export-group';
        group.style.cssText = 'display:inline-flex;gap:4px;margin-right:8px;align-items:center;';
        group.innerHTML =
            '<button type="button" class="mdgo-export-btn" data-act="copy" title="复制 Markdown 源码">复制MD</button>' +
            '<button type="button" class="mdgo-export-btn" data-act="html" title="导出为独立 HTML（内联样式）">HTML</button>' +
            '<button type="button" class="mdgo-export-btn" data-act="pdf" title="打印 / 另存为 PDF">PDF</button>';
        group.addEventListener('click', async (e) => {
            const btn = e.target.closest('.mdgo-export-btn');
            if (!btn) return;
            const act = btn.dataset.act;
            btn.disabled = true;
            try {
                if (act === 'copy') await mdgoExport.copyMarkdown();
                else if (act === 'html') await mdgoExport.exportHtml();
                else if (act === 'pdf') await mdgoExport.exportPdf();
            } finally {
                btn.disabled = false;
            }
        });
        right.prepend(group);
    };

    window.mdgoExport = mdgoExport;
})();
