/**
 * Tauri 适配器入口
 *
 * 加载 Tauri API 并初始化所有适配器。
 * 以 <script type="module"> 方式加载，确保在所有业务逻辑之前运行。
 */

(async () => {
    try {
        const [{ invoke, convertFileSrc }, { open, save }, { Store }] = await Promise.all([
            import('@tauri-apps/api/core'),
            import('@tauri-apps/plugin-dialog'),
            import('@tauri-apps/plugin-store'),
        ]);

        window.__TAURI__ = {
            core: { invoke, convertFileSrc },
            dialog: { open, save },
            pluginStore: { Store },
        };

        console.log('[TauriAdapter] Tauri API 已挂载');
    } catch (e) {
        console.error('[TauriAdapter] 加载 Tauri API 失败:', e);
        return;
    }

    try {
        await import('./file-system.js');
        console.log('[TauriAdapter] 文件系统适配器已加载');
    } catch (e) {
        console.error('[TauriAdapter] 加载文件系统适配器失败:', e);
    }

    try {
        await import('./storage.js');
        console.log('[TauriAdapter] 存储适配器已加载');
    } catch (e) {
        console.error('[TauriAdapter] 加载存储适配器失败:', e);
    }

    try {
        await import('./git-rust.js');
        console.log('[TauriAdapter] Git Rust 适配器已加载');
    } catch (e) {
        console.error('[TauriAdapter] 加载 Git Rust 适配器失败:', e);
    }

    console.log('[TauriAdapter] 所有适配器初始化完成');
})();
