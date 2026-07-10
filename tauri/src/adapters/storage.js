/**
 * Tauri 存储适配层
 *
 * 将 localStorage 和 IndexedDB 替换为 Tauri Store 插件。
 * 保持原有 API 接口不变：getItem / setItem / removeItem / clear
 *
 * 注意：IndexedDB 在 Tauri webview 中仍然可以工作（基于 WebKit），
 * 但对于简单的 KV 存储，推荐替换为 Store 以获得更好的持久性和性能。
 */

(function () {
  // 仅在 Tauri 环境下生效
  if (typeof window.__TAURI__ === 'undefined') {
    console.log('[TauriStore] 非 Tauri 环境，跳过适配');
    return;
  }

  const { Store } = window.__TAURI__.pluginStore;

  let _store = null;
  let _ready = false;
  let _queue = []; // 存储初始化前的操作队列

  async function _getStore() {
    if (_store) return _store;
    _store = await Store.load('app-settings.json');
    _ready = true;
    // 处理队列
    for (const op of _queue) {
      try { await op(); } catch (e) { console.warn('[TauriStore] 队列操作失败:', e); }
    }
    _queue = [];
    return _store;
  }

  // 延迟初始化，不阻塞页面加载
  _getStore();

  // =====================================================
  // localStorage 替换
  // =====================================================
  const originalStorage = Object.create(null);

  const tauriStorage = {
    async getItem(key) {
      try {
        const store = await _getStore();
        const val = await store.get(key);
        return val !== undefined && val !== null ? String(val) : null;
      } catch {
        // 降级到内存存储
        return originalStorage[key] !== undefined ? String(originalStorage[key]) : null;
      }
    },

    async setItem(key, value) {
      try {
        const store = await _getStore();
        await store.set(key, value);
        await store.save();
      } catch {
        originalStorage[key] = value;
      }
    },

    async removeItem(key) {
      try {
        const store = await _getStore();
        await store.delete(key);
        await store.save();
      } catch {
        delete originalStorage[key];
      }
    },

    async clear() {
      try {
        const store = await _getStore();
        // Store 没有 clear 方法，遍历所有 key 删除
        const keys = await store.keys();
        for (const key of keys) {
          await store.delete(key);
        }
        await store.save();
      } catch {
        for (const key of Object.keys(originalStorage)) {
          delete originalStorage[key];
        }
      }
    },

    get length() {
      return Object.keys(originalStorage).length;
    },

    key(index) {
      return Object.keys(originalStorage)[index] || null;
    },
  };

  /**
   * 由于 localStorage 是同步 API，而 Tauri Store 是异步的，
   * 我们提供一个同步回退 + 后台异步持久化的策略：
   *
   * 1. 所有读写操作先在内存中同步完成（保证兼容性）
   * 2. 写操作同时异步持久化到 Store（无阻塞）
   * 3. 页面加载时，尝试从 Store 恢复数据到内存
   */
  const memStorage = {};

  // 启动时从 Store 恢复数据
  (async () => {
    try {
      const store = await _getStore();
      const keys = await store.keys();
      for (const key of keys) {
        const val = await store.get(key);
        if (val !== undefined && val !== null) {
          memStorage[key] = String(val);
        }
      }
    } catch (e) {
      console.warn('[TauriStore] 恢复数据失败:', e);
    }
  })();

  // 定义同步 localStorage 代理
  const syncStorageHandler = {
    get(target, prop) {
      if (prop === 'getItem') {
        return (key) => memStorage[key] !== undefined ? memStorage[key] : null;
      }
      if (prop === 'setItem') {
        return (key, value) => {
          memStorage[key] = String(value);
          // 异步持久化
          _getStore().then(async (store) => {
            try {
              await store.set(key, String(value));
              await store.save();
            } catch (e) {
              console.warn('[TauriStore] 持久化失败:', e);
            }
          }).catch(() => {});
        };
      }
      if (prop === 'removeItem') {
        return (key) => {
          delete memStorage[key];
          _getStore().then(async (store) => {
            try {
              await store.delete(key);
              await store.save();
            } catch (e) {
              console.warn('[TauriStore] 删除失败:', e);
            }
          }).catch(() => {});
        };
      }
      if (prop === 'clear') {
        return () => {
          Object.keys(memStorage).forEach(k => delete memStorage[k]);
          _getStore().then(async (store) => {
            try {
              const keys = await store.keys();
              for (const key of keys) {
                await store.delete(key);
              }
              await store.save();
            } catch (e) {
              console.warn('[TauriStore] 清空失败:', e);
            }
          }).catch(() => {});
        };
      }
      if (prop === 'length') {
        return Object.keys(memStorage).length;
      }
      if (prop === 'key') {
        return (index) => Object.keys(memStorage)[index] || null;
      }
      // 允许直接属性访问
      if (typeof prop === 'string' && !prop.startsWith('__')) {
        return memStorage[prop];
      }
      return Reflect.get(target, prop);
    },
    set(target, prop, value) {
      if (typeof prop === 'string' && !prop.startsWith('__')) {
        memStorage[prop] = String(value);
        // 异步持久化
        _getStore().then(async (store) => {
          try {
            await store.set(prop, String(value));
            await store.save();
          } catch (e) {
            console.warn('[TauriStore] 持久化失败:', e);
          }
        }).catch(() => {});
        return true;
      }
      return Reflect.set(target, prop, value);
    },
    deleteProperty(target, prop) {
      if (typeof prop === 'string' && !prop.startsWith('__')) {
        delete memStorage[prop];
        _getStore().then(async (store) => {
          try {
            await store.delete(prop);
            await store.save();
          } catch (e) {
            console.warn('[TauriStore] 删除失败:', e);
          }
        }).catch(() => {});
        return true;
      }
      return Reflect.deleteProperty(target, prop);
    },
  };

  // 替换全局 localStorage
  Object.defineProperty(window, 'localStorage', {
    value: new Proxy({}, syncStorageHandler),
    writable: false,
    configurable: true,
  });

  console.log('[TauriStore] 存储适配层已加载');
})();
