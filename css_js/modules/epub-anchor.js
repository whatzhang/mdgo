/**
 * ===== EPUB 阅读增强（css_js/modules/epub-anchor.js） =====
 *
 * 【职责】三件与「后端 EPUB 富化通路」配套的前端能力，集中在本文件：
 *
 * 1. `mdgoHeadingSlug` —— anydoc 同款 GFM 标题 slug（缺口 G3）。
 *    后端 anydoc 的围栏锚点契约是：**跨章引用/脚注等内链指向"标题的 GFM slug"**
 *    （`anydoc/src/render/markdown/anchors.rs::gfm_slug`）。而本仓 `marked v15` 全局配了
 *    `headerIds: false`（main.html），渲染出的 `<h1..h6>` **没有 id**；随后
 *    `addHeadingIDs()` 又把标题 id 统一改写成 `heading-{n}`。
 *    两个后果叠加 → 后端产出的 `[见 2.1](#21-子节)` 之类链接**必然指向不存在的 id**，
 *    EPUB 的交叉引用、目录内链、`回到目录` 全部点不动。
 *    修法：把标题 id 直接生成为**与 anydoc 完全一致**的 slug，两边自然对齐。
 *
 * 2. `mdgoSlugAllocator` —— 与 anydoc `UniqueIds::claim` 一致的去重分配器。
 *    anydoc 对重名 slug 的分配顺序是 `base`、`base-1`、`base-2`…，且**按渲染顺序**
 *    先给标题占位。本分配器镜像同一规则，保证第 n 个同名标题两边拿到同一个 id。
 *
 * 3. `mdgoResolveAssetSrc` —— 解析后端导出的 EPUB 内嵌图片（缺口 G1）。
 *    后端把 zip 里的图片导出到缓存目录，并在 Markdown 里写成
 *    `mdgoasset://local/<base64url(绝对路径)>`。这里把它还原成绝对路径，
 *    再交给 `convertFileSrc()` 走 Tauri asset 协议。
 *    **安全约束**：只接受落在后端本次返回的 `asset_root` 目录内的路径，
 *    否则一个构造过的 EPUB 就能借该 scheme 让前端去读任意本地文件。
 *
 * 【加载顺序】须在 main.html 主脚本之后（与 support.js 同级即可）。
 */

(function () {
    'use strict';

    // ═══════════════ 1+2. 标题 slug（对齐 anydoc） ═══════════════

    /**
     * anydoc `is_combining_mark()` 的镜像：`is_alphanumeric` 覆盖不到的组合记号区段
     * （元音符号、变音符号、重音/吟诵记号等）。
     */
    function _isCombiningMark(code) {
        return (
            (code >= 0x0300 && code <= 0x036f) ||
            (code >= 0x0483 && code <= 0x0489) ||
            (code >= 0x0900 && code <= 0x0903) ||
            (code >= 0x093a && code <= 0x094f) ||
            (code >= 0x0951 && code <= 0x0957) ||
            (code >= 0x0962 && code <= 0x0963) ||
            (code >= 0x1ab0 && code <= 0x1aff) ||
            (code >= 0x1dc0 && code <= 0x1dff) ||
            (code >= 0x20d0 && code <= 0x20ff) ||
            (code >= 0xfe20 && code <= 0xfe2f)
        );
    }

    /** anydoc `is_connector_punctuation()` 的镜像（Unicode 分类 Pc，像 `_` 一样连接词）。 */
    function _isConnectorPunctuation(code) {
        return (
            code === 0x005f || code === 0x203f || code === 0x2040 || code === 0x2054 ||
            code === 0xfe33 || code === 0xfe34 ||
            (code >= 0xfe4d && code <= 0xfe4f) || code === 0xff3f
        );
    }

    /**
     * 把标题的**行内 Markdown** 近似还原成纯文本，逼近 anydoc 的
     * `inlines_to_plain_text()`：anydoc 是在**结构化 inline 节点**上取文本，
     * 而这里只有原始 Markdown 字符串，所以要先剥掉行内语法。
     * 典型差异来源：`## [链接](https://x.com)` → 纯文本应为「链接」，
     * 若不剥掉 URL 会得到「链接httpsex.com」，与后端的 slug 不一致。
     */
    function _mdInlineToPlain(text) {
        let s = String(text == null ? '' : text);
        // 实体先解码：anydoc 是在**结构化 inline 节点**上取纯文本，
        // 若它把 `&` 写成 `&amp;` 而我们不解码，slug 就会多出 "amp" 这类词。
        s = s.replace(/&#x([0-9a-f]+);/gi, (_, h) => {
            try { return String.fromCodePoint(parseInt(h, 16)); } catch (_) { return ''; }
        });
        s = s.replace(/&#(\d+);/g, (_, d) => {
            try { return String.fromCodePoint(parseInt(d, 10)); } catch (_) { return ''; }
        });
        s = s.replace(/&(amp|lt|gt|quot|apos|nbsp|mdash|ndash|hellip|shy);/gi, (m, n) => {
            const map = {
                amp: '&', lt: '<', gt: '>', quot: '"', apos: "'",
                nbsp: ' ', mdash: '\u2014', ndash: '\u2013', hellip: '\u2026', shy: '',
            };
            return Object.prototype.hasOwnProperty.call(map, n.toLowerCase()) ? map[n.toLowerCase()] : m;
        });
        s = s.replace(/!\[([^\]]*)\]\([^)]*\)/g, '$1');   // 图片 → alt
        s = s.replace(/\[([^\]]*)\]\([^)]*\)/g, '$1');     // 链接 → 标签
        s = s.replace(/\[([^\]]*)\]\[[^\]]*\]/g, '$1');    // 引用式链接
        s = s.replace(/`([^`]*)`/g, '$1');                 // 行内代码
        s = s.replace(/<[^>]*>/g, '');                     // 行内 HTML
        s = s.replace(/\\([\\`*_{}\[\]()#+\-.!~>])/g, '$1'); // 反转义（anydoc 会转义 Markdown 元字符）
        s = s.replace(/[*_~]+/g, '');                      // 强调标记
        return s;
    }

    /**
     * anydoc `gfm_slug()` 的镜像。
     *
     * 规则（逐字对齐）：trim → 逐字符**全量 Unicode 小写** → 空格转 `-` →
     * 仅保留「字母/数字、组合记号、连接符标点、`-`」 → 其余全部丢弃 →
     * 结果为空则回落为 `section`。
     *
     * 注：Rust 的 `char::is_alphanumeric()` 是 `Alphabetic ∪ N*`，与 JS 的
     * `\p{L}∪\p{N}` 在极少数稀有文字上可能有一两个码位的差异；受影响的范围仅限于
     * 这些文字的标题锚点，且两边都会**同样地丢字符**，不影响常见语言（中/日/韩/拉丁）。
     */
    function mdgoHeadingSlug(text) {
        const plain = _mdInlineToPlain(text).trim();
        let slug = '';
        for (const ch of plain) {
            // 与 Rust `flat_map(char::to_lowercase)` 等价：一次小写可能展开成多个码位
            for (const c of ch.toLowerCase()) {
                if (c === ' ' || c === '-') {
                    slug += '-';
                    continue;
                }
                const code = c.codePointAt(0);
                if (/[\p{L}\p{N}]/u.test(c) || _isCombiningMark(code) || _isConnectorPunctuation(code)) {
                    slug += c;
                }
            }
        }
        return slug === '' ? 'section' : slug;
    }

    /**
     * 与 anydoc `UniqueIds::claim` 等价的去重分配器。
     *
     * 注意：anydoc 在**渲染顺序**上用同一个分配器给标题占位，因此这里也必须在
     * 文档顺序上调用一次、依次分配，才能得到逐字相同的 id。
     */
    function mdgoSlugAllocator() {
        const used = new Set();
        const nextSuffix = new Map();
        return function claim(text) {
            const base = mdgoHeadingSlug(text);
            if (!used.has(base)) {
                used.add(base);
                if (!nextSuffix.has(base)) nextSuffix.set(base, 1);
                return base;
            }
            let n = nextSuffix.get(base) || 1;
            for (;;) {
                const candidate = base + '-' + n;
                n += 1;
                if (!used.has(candidate)) {
                    used.add(candidate);
                    nextSuffix.set(base, n);
                    if (!nextSuffix.has(candidate)) nextSuffix.set(candidate, 1);
                    return candidate;
                }
            }
        };
    }

    // ═══════════════ 3. 后端导出的 EPUB 图片资源 ═══════════════

    const ASSET_URL_PREFIX = 'mdgoasset://local/';

    // 当前预览文档的授权资源目录（由 main.html 在拿到 document_preview 结果后调用 setter 写入）
    let _assetRoot = '';

    function _norm(p) {
        return String(p == null ? '' : p).replace(/\\/g, '/').replace(/\/+$/, '');
    }

    /** 由 main.html 在每次 `document_preview` 返回后设置（非 EPUB 传空串即可）。 */
    function mdgoSetAssetRoot(root) {
        _assetRoot = _norm(root);
    }

    /** 当前形态：`<sha256 前 32 位>.<扩展名>`。 */
    const ASSET_NAME_RE = /^[0-9a-f]{32}\.[a-z0-9]{1,8}$/;

    /** 同一类失败只告警一次，避免一本书几十张图刷屏。 */
    const _warnedKeys = new Set();
    function _warnOnce(key, msg) {
        if (_warnedKeys.has(key)) return;
        _warnedKeys.add(key);
        console.warn('[EpubAnchor] ' + msg);
    }

    function _b64urlDecodeToText(s) {
        let t = String(s).replace(/-/g, '+').replace(/_/g, '/');
        while (t.length % 4 !== 0) t += '=';
        const bin = atob(t);
        const bytes = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
        return new TextDecoder('utf-8').decode(bytes);
    }

    /**
     * **兼容形态**（本方案早期版本，以及"前端已更新、后端未重建"的版本错配）：
     * `mdgoasset://local/<base64url(绝对路径)>`。
     *
     * 解码后必须**落在本次授权的 `asset_root` 内**且不含 `..`——即与早期实现同一套校验。
     * 保留它不是为了好看，而是因为一次真实事故：URL 形态变更后，只要前后端有一半是旧的，
     * 该书的图片就会**全部消失**。容忍旧形态让版本错配退化为"照常显示"，
     * 而不是用户可见的功能中断。
     */
    function _resolveLegacyAbsolute(payload) {
        let abs;
        try {
            abs = _norm(_b64urlDecodeToText(payload));
        } catch (_) {
            return null;
        }
        if (!abs) return null;
        if (abs.split('/').some((seg) => seg === '..')) return null;
        if (abs !== _assetRoot && !abs.startsWith(_assetRoot + '/')) return null;
        return abs;
    }

    /**
     * 解析 `mdgoasset://local/<载荷>`，返回可交给 `convertFileSrc()` 的绝对路径；
     * 不是该 scheme、两种形态都不匹配、或尚未设置资源目录 → 返回 `null`（调用方保持原样）。
     *
     * 优先按**当前形态**（裸文件名）解析，安全模型最强：URL 里没有路径，
     * 路径由「后端给的 `asset_root` + 文件名」拼出，构造过的 EPUB 连表达绝对路径或 `..`
     * 的机会都没有。旧形态则退化为"解码后必须在 `asset_root` 内"的校验。
     */
    function mdgoResolveAssetSrc(src) {
        if (typeof src !== 'string' || !src.startsWith(ASSET_URL_PREFIX)) return null;
        const payload = src.slice(ASSET_URL_PREFIX.length);
        if (!_assetRoot) {
            _warnOnce('no-root', '尚未拿到资源目录（asset_root），EPUB 图片无法解析');
            return null;
        }
        if (!payload) return null;
        // 当前形态：裸文件名
        if (ASSET_NAME_RE.test(payload)) return _assetRoot + '/' + payload;
        // 兼容形态：base64url(绝对路径)
        const legacy = _resolveLegacyAbsolute(payload);
        if (legacy) return legacy;
        _warnOnce(
            'bad-payload',
            '资源 URL 载荷两种形态都不匹配，图片不显示。常见原因：前后端版本不一致，' +
            '或正文来自更早版本的转换缓存。url=' + src
        );
        return null;
    }

    /** 该 src 是否是后端导出的 EPUB 资源（不校验授权，仅判前缀）。 */
    function mdgoIsAssetSrc(src) {
        return typeof src === 'string' && src.startsWith(ASSET_URL_PREFIX);
    }

    // ── 暴露 ──
    window.mdgoHeadingSlug = mdgoHeadingSlug;
    window.mdgoSlugAllocator = mdgoSlugAllocator;
    window.mdgoSetAssetRoot = mdgoSetAssetRoot;
    window.mdgoResolveAssetSrc = mdgoResolveAssetSrc;
    window.mdgoIsAssetSrc = mdgoIsAssetSrc;
    window.MDGO_ASSET_URL_PREFIX = ASSET_URL_PREFIX;

    console.log('[EpubAnchor] EPUB 锚点/资源解析模块已就绪');
})();
