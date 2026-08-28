/**
 * Tauri 适配器入口
 *
 * 加载 Tauri API 并初始化所有适配器。
 * 以 <script type="module"> 方式加载，确保在所有业务逻辑之前运行。
 *
 * 优化：file-system + storage 并行加载，git-rust 后置懒加载不阻塞启动。
 *
 * 同步暴露 __tauriInitPromise，供 initFromStorage 等待适配器初始化完成。
 */

window.__tauriInitPromise = (async () => {
    try {
        const [{ invoke, convertFileSrc }, { listen }, { getCurrentWindow }, { open, save }, { Store }] = await Promise.all([
            import('@tauri-apps/api/core'),
            import('@tauri-apps/api/event'),
            import('@tauri-apps/api/window'),
            import('@tauri-apps/plugin-dialog'),
            import('@tauri-apps/plugin-store'),
        ]);

        window.__TAURI__ = {
            core: { invoke, convertFileSrc },
            event: { listen },
            window: { getCurrentWindow },
            dialog: { open, save },
            pluginStore: { Store },
        };

        // 挂载浏览器打开功能（通过 invoke → Rust open crate）
        window.__tauriOpenUrl = async (url) => {
            try {
                await invoke('open_url', { url });
            } catch (e) {
                console.error('[TauriShell] 打开 URL 失败:', url, e);
            }
        };

        console.log('[TauriAdapter] Tauri API 已挂载');
    } catch (e) {
        console.error('[TauriAdapter] 加载 Tauri API 失败:', e);
        return;
    }

    // 文件系统 + 存储适配器并行加载，互无依赖
    try {
        await Promise.all([
            import('./file-system.js').then(() => console.log('[TauriAdapter] 文件系统适配器已加载')),
            import('./storage.js').then(() => console.log('[TauriAdapter] 存储适配器已加载')),
            import('./ai-history.js').then(() => console.log('[TauriAdapter] AI 历史适配器已加载')),
            import('./skill.js').then(() => console.log('[TauriAdapter] Skill 适配器已加载')),
            import('./prompt.js').then(() => console.log('[TauriAdapter] Prompt 适配器已加载')),
            import('./mcp.js').then(() => console.log('[TauriAdapter] MCP 适配器已加载')),
            import('./bookmark.js').then(() => console.log('[TauriAdapter] 书签适配器已加载')),
        ]);
    } catch (e) {
        console.error('[TauriAdapter] 加载核心适配器失败:', e);
    }

    // Git 适配器非启动关键，后置加载
    import('./git-rust.js')
        .then(() => console.log('[TauriAdapter] Git Rust 适配器已加载'))
        .catch((e) => console.error('[TauriAdapter] 加载 Git Rust 适配器失败:', e));

    console.log('[TauriAdapter] 所有适配器初始化完成');
})();
