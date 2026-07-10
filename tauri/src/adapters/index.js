/**
 * Tauri 适配器入口
 *
 * 加载 Tauri API 并初始化所有适配器。
 * 以 <script type="module"> 方式加载，确保在所有业务逻辑之前运行。
 */

(async () => {
  // 导入 Tauri API
  const [
    { invoke },
    { open, save },
    { Store }
  ] = await Promise.all([
    import('@tauri-apps/api/core'),
    import('@tauri-apps/plugin-dialog'),
    import('@tauri-apps/plugin-store'),
  ]);

  // 挂载到全局，供 adapter scripts (非 module) 使用
  window.__TAURI__ = {
    core: { invoke },
    dialog: { open, save },
    pluginStore: { Store },
  };

  // 加载文件系统适配器
  await import('./file-system.js');

  // 加载存储适配器
  await import('./storage.js');

  console.log('[TauriAdapter] 所有适配器已初始化');
})();
