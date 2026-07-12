/**
 * Tauri 存储适配层
 *
 * 将 localStorage 替换为 Tauri Store 插件。
 * 保持原有 API 接口不变：getItem / setItem / removeItem / clear
 *
 * 策略：
 * 1. 所有读写操作先在内存中同步完成（保证兼容性）
 * 2. 写操作通过防抖批量异步持久化到 Store（无阻塞、高性能）
 * 3. 页面加载时，从 Store 恢复数据到内存
 */

(function () {
  if (typeof window.__TAURI__ === 'undefined') {
    console.log('[TauriStore] 非 Tauri 环境，跳过适配');
    return;
  }

  const { Store } = window.__TAURI__.pluginStore;

  const memStorage = {};
  let _store = null;
  let _storeReady = false;
  let _saveTimer = null;
  const SAVE_DEBOUNCE_MS = 200;

  async function _getStore() {
    if (_store) return _store;
    _store = await Store.load('app-settings.json');
    _storeReady = true;
    return _store;
  }

  function _scheduleSave() {
    if (!_storeReady) return;
    if (_saveTimer) clearTimeout(_saveTimer);
    const token = {};
    _saveTimer = token;
    setTimeout(async () => {
      try {
        const store = await _getStore();
        await store.save();
      } catch (e) {
        console.warn('[TauriStore] 持久化失败:', e);
      }
      if (_saveTimer === token) _saveTimer = null;
    }, SAVE_DEBOUNCE_MS);
  }

  /**
   * 暴露就绪 Promise，供 initFromStorage 等关键初始化逻辑等待。
   * 因为 localStorage Proxy 的 getItem 是同步读取 memStorage，
   * 而 memStorage 需要从 Tauri Store 异步加载，直接读取可能返回 null。
   */
  window.__tauriStorageReady = (async () => {
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

  const syncStorageHandler = {
    get(target, prop) {
      if (prop === 'getItem') {
        return (key) => memStorage[key] !== undefined ? memStorage[key] : null;
      }
      if (prop === 'setItem') {
        return (key, value) => {
          memStorage[key] = String(value);
          _getStore().then(async (store) => {
            try {
              await store.set(key, String(value));
              _scheduleSave();
            } catch (e) {
              console.warn('[TauriStore] set 失败:', e);
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
              _scheduleSave();
            } catch (e) {
              console.warn('[TauriStore] delete 失败:', e);
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
              _scheduleSave();
            } catch (e) {
              console.warn('[TauriStore] clear 失败:', e);
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
      if (typeof prop === 'string' && !prop.startsWith('__')) {
        return memStorage[prop];
      }
      return Reflect.get(target, prop);
    },
    set(target, prop, value) {
      if (typeof prop === 'string' && !prop.startsWith('__')) {
        memStorage[prop] = String(value);
        _getStore().then(async (store) => {
          try {
            await store.set(prop, String(value));
            _scheduleSave();
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
            _scheduleSave();
          } catch (e) {
            console.warn('[TauriStore] 删除失败:', e);
          }
        }).catch(() => {});
        return true;
      }
      return Reflect.deleteProperty(target, prop);
    },
  };

  Object.defineProperty(window, 'localStorage', {
    value: new Proxy({}, syncStorageHandler),
    writable: false,
    configurable: true,
  });

  console.log('[TauriStore] 存储适配层已加载');
})();
