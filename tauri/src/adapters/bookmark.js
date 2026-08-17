/**
 * Tauri 书签知识资产适配层
 *
 * 封装 bookmark_* 命令（invoke），统一通过 window.__mdgoBookmark 暴露，
 * 供 main.html 书签页面（导入/列表/搜索/统计/树）调用。
 *
 * 边界：导入是 UI 行为；Agent 只经 search_bookmarks / get_bookmark
 * 只读访问（Rust 侧工具，不经本适配层）。
 *
 * 约定：所有函数带 bookmark 前缀，与现有业务隔离。
 */

(function () {
    if (typeof window.__TAURI__ === 'undefined') {
        console.log('[BookmarkAdapter] 非 Tauri 环境，跳过适配');
        return;
    }

    const invoke = window.__TAURI__.core.invoke;

    const bookmarkApi = {
        /** 导入书签（前端 parseBookmarkHtml 解析后的结构化 JSON；按 URL 去重，已存在跳过） */
        bookmarkImport: (dirPath, entries, sourceFile) =>
            invoke('bookmark_import', { dirPath, entries, sourceFile: sourceFile || null }),

        /** 书签列表（可选过滤；failed/dead 也返回，由页面按 status/dead 渲染） */
        bookmarkList: (dirPath, opts) =>
            invoke('bookmark_list', {
                dirPath,
                folder: (opts && opts.folder) || null,
                category: (opts && opts.category) || null,
                status: (opts && opts.status) || null,
                limit: (opts && opts.limit) || 100,
            }),

        /** 书签检索（LIKE ∪ 向量补位） */
        bookmarkSearch: (dirPath, query, opts) =>
            invoke('bookmark_search', {
                dirPath,
                query,
                limit: (opts && opts.limit) || 10,
                category: (opts && opts.category) || null,
                folder: (opts && opts.folder) || null,
            }),

        /** 书签统计（UI 统计卡） */
        bookmarkStat: (dirPath) => invoke('bookmark_stat', { dirPath }),

        /** 书签详情 */
        bookmarkGet: (dirPath, id) => invoke('bookmark_get', { dirPath, id }),

        /** 书签目录树（页面直读 DB；叶子带 status/dead 标记） */
        bookmarkTree: (dirPath) => invoke('bookmark_tree', { dirPath }),

        /** 分析扫描：启动（或继续）书签 Enrichment Worker（已在运行则无操作） */
        bookmarkWorkerStart: () => invoke('bookmark_worker_start'),
    };

    window.__mdgoBookmark = bookmarkApi;
    console.log('[BookmarkAdapter] 书签适配层就绪');
})();
