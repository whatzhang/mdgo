/**
 * ===== Markdown 业务模块（css_js/modules/markdown.js） =====
 *
 * 【职责】从 main.html 抽取的 Markdown 渲染/解析业务（P0-7 分期落地）：
 *   1. marked 渲染（Markdown.md，兼容旧全局 markedMd）
 *   2. Obsidian wiki 双链解析（Markdown.parseWiki，兼容旧全局 parseWiki）
 *   3. 后续轮次继续迁入：parseObsidianToHTML / postProcessMarkdown /
 *      renderMarkdownFile / 实时预览 / 选区工具条 / 导出相关
 * 【依赖】marked（css_js/cdn/marked.min.js，必须先于本文件加载）
 * 【加载】必须位于 main.html 主脚本之前（<head> 中 marked.min.js 之后）：
 *         主脚本与 css_js/modules/* 大量按全局函数名调用本模块函数，
 *         只有先于主脚本加载，任意时机的调用才有定义。
 * 【对外暴露】window.Markdown 命名空间（新代码推荐）+ window.markedMd /
 *            window.parseWiki（兼容既有调用点，主脚本与 agent/canvas/skill/
 *            schedule/mcp 等模块均按此调用）
 */
(function () {
    'use strict';

    // ===== 命名空间 =====
    const Markdown = {};

    // ===== marked 渲染（原 main.html markedMd，行为逐字保持一致） =====
    Markdown.md = function (str) {
        if (!str) return '';
        try {
            return marked.parse(str);
        } catch (error) {
            console.error('Markdown 解析错误:', error);
            return str;
        }
    };

    // ===== Obsidian wiki 双链解析（原 main.html parseWiki，行为逐字保持一致） =====
    Markdown.parseWiki = function (html) {
        return html.replace(/\[\[([^\|#\]]+)(?:\|([^\]]+))?\]\]([\r\n]*)/g, (match, link, alias, newlines) => {
            const displayText = alias || link;
            return `<a href="${link}" target="_blank" class="ob-internal-link">${displayText}</a>${newlines}`;
        });
    };

    // ===== 兼容旧全局调用点（行为不变：主脚本内同名函数声明已移除） =====
    window.markedMd = Markdown.md;
    window.parseWiki = Markdown.parseWiki;
    window.Markdown = Markdown;
})();
