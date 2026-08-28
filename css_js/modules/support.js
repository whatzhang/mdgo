/**
 * ===== 文件/能力支持判断集中管理（css_js/modules/support.js） =====
 *
 * 【职责】集中管理所有「某文件/语言/能力是否被支持」的判断逻辑，避免散落各处。
 * 【原则】单一事实源：main.html 等调用方统一经本模块判断，不重复实现规则。
 *
 * 现有三组能力判断：
 * 1. TOC 支持（isTocSupported）：哪些扩展名文件可生成目录；
 * 2. Prettier 格式化支持（hasPrettierSupport / getPrettierConfig）：
 *    哪些语言可用 Prettier 格式化（含插件是否就绪）；
 * 3. 编辑支持（supportsEdit / codeEditEnble）：哪些文件可进入编辑器。
 *
 * 【加载顺序】须在 main.html 主脚本之后（依赖 getExt/isCode/isImage/_TOC_EXT_SET/
 * CODE_EXT_SET 等全局，或本模块自带 fallback）。
 */

(function () {
    'use strict';

    // ── 依赖的全局（main.html 提供；缺失时给出默认值，避免模块单独加载报错） ──
    // 注意：_TOC_EXT_SET / CODE_EXT_SET 等为 main.html 顶层 const（非 window 属性），
    // 此处经裸标识符访问（共享全局词法环境），缺失时回退默认值。
    function getExt(name) {
        if (typeof window.getExt === 'function') return window.getExt(name);
        const s = String(name || '');
        const idx = s.lastIndexOf('.');
        return idx >= 0 ? s.slice(idx + 1) : '';
    }
    function isCode(ext) {
        if (typeof window.isCode === 'function') return window.isCode(ext);
        const set = typeof CODE_EXT_SET !== 'undefined' ? CODE_EXT_SET : new Set();
        return set.has(String(ext || '').toLowerCase());
    }
    function isImage(ext) {
        if (typeof window.isImage === 'function') return window.isImage(ext);
        const set = typeof _IMAGE_EXT_SET !== 'undefined' ? _IMAGE_EXT_SET : new Set();
        return set.has(String(ext || '').toLowerCase());
    }
    function tocExtSet() {
        return typeof _TOC_EXT_SET !== 'undefined' ? _TOC_EXT_SET : new Set(['md', 'html', 'opml']);
    }

    // ── Prettier 支持的语言配置（原 PRETTIER_LANG_CONFIG，集中管理） ──
    const PRETTIER_LANG_CONFIG = {
        javascript: { parser: 'babel', pluginKeys: ['babel', 'estree'] },
        typescript: { parser: 'babel-ts', pluginKeys: ['babel', 'estree'] },
        html: { parser: 'html', pluginKeys: ['html'] },
        css: { parser: 'css', pluginKeys: ['postcss'] },
        scss: { parser: 'scss', pluginKeys: ['postcss'] },
        less: { parser: 'less', pluginKeys: ['postcss'] },
        markdown: { parser: 'markdown', pluginKeys: ['markdown'] },
        yaml: { parser: 'yaml', pluginKeys: ['yaml'] },
        graphql: { parser: 'graphql', pluginKeys: ['graphql'] },
    };

    // ═══════════ 1. TOC 支持 ═══════════

    /** 文件名是否支持生成目录（TOC） */
    function isTocSupported(name) {
        return tocExtSet().has(getExt(name).toLowerCase());
    }

    // ═══════════ 2. Prettier 格式化支持 ═══════════

    /** 获取某语言的 Prettier 配置（parser + 已就绪插件） */
    function getPrettierConfig(language) {
        const cfg = PRETTIER_LANG_CONFIG[language];
        if (!cfg) return { parser: null, plugins: [] };
        const plugins = cfg.pluginKeys.map(key => window.prettierPlugins?.[key]).filter(Boolean);
        return { parser: cfg.parser, plugins };
    }

    /** 某语言是否支持 Prettier 格式化（语言在配置内且全部插件已加载） */
    function hasPrettierSupport(language) {
        const cfg = PRETTIER_LANG_CONFIG[language];
        if (!cfg || !window.prettier || !window.prettierPlugins) return false;
        return cfg.pluginKeys.every(key => window.prettierPlugins[key]);
    }

    // ═══════════ 3. 编辑支持 ═══════════

    /** 文件名是否属于可编辑的代码类（plantuml / 代码扩展名） */
    function codeEditEnble(filename) {
        return /\.(puml|plantuml)$/i.test(filename) || isCode(getExt(filename));
    }

    /** 文件名是否可进入编辑器 */
    function supportsEdit(filename) {
        // 不支持编辑: drawio, 非SVG图片, pdf, excalidraw, excel
        if (filename.endsWith('.drawio') || filename.endsWith('.excalidraw')) return false;
        if (isImageFile(filename) && !filename.endsWith('.svg')) return false;
        if (filename.endsWith('.pdf') || filename.endsWith('.xlsx') || filename.endsWith('.xls')) return false;
        // 支持所有文本/代码文件 (包括 canvas 和 svg)
        return codeEditEnble(filename) || /\.(txt|md|csv|canvas|svg|opml|mm)$/i.test(filename);
    }

    function isImageFile(filename) {
        return isImage(getExt(filename));
    }

    // ── 暴露（与 main.html 原函数同名，直接全局替换） ──
    window.isTocSupported = isTocSupported;
    window.getPrettierConfig = getPrettierConfig;
    window.hasPrettierSupport = hasPrettierSupport;
    window.codeEditEnble = codeEditEnble;
    window.supportsEdit = supportsEdit;
    window.isImageFile = isImageFile;

    // 供其它模块引用配置（如需要）
    window.SUPPORT_PRETTIER_LANG_CONFIG = PRETTIER_LANG_CONFIG;

    console.log('[Support] 能力支持判断模块已就绪');
})();
