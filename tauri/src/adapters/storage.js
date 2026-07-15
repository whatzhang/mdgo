/**
 * Tauri 存储适配层
 *
 * 将 localStorage 替换为 Tauri Store 插件。
 * 保持原有 API 接口不变：getItem / setItem / removeItem / clear
 *
 * 策略：
 * 1. 所有读写操作先在内存中同步完成（保证兼容性）
 * 2. 写操作通过 Promise 链串行持久化到 Store（保证写入顺序和完成追踪）
 * 3. 暴露 __tauriStorageFlush() 供关键数据写入后强制刷盘
 * 4. 页面加载时，从 Store 恢复数据到内存
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

    async function _getStore() {
        if (_store) return _store;
        _store = await Store.load('app-settings.json');
        _storeReady = true;
        return _store;
    }

    // Promise 链：保证所有写操作串行执行，且可追踪完成状态
    let _saveChain = Promise.resolve();

    function _scheduleSave() {
        if (!_storeReady) return;
        // 将 save 操作追加到链尾，确保串行执行
        _saveChain = _saveChain
            .then(async () => {
                const store = await _getStore();
                await store.save();
            })
            .catch((e) => {
                console.warn('[TauriStore] 持久化失败:', e);
            });
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
            console.log('[TauriStore] 已从 Store 恢复', keys.length, '条数据');
        } catch (e) {
            console.warn('[TauriStore] 恢复数据失败:', e);
        }
    })();

    /**
     * 强制等待所有待处理的持久化操作完成。
     * 在关键数据（如 ROOT_DIR）写入后调用，确保数据已落盘。
     */
    window.__tauriStorageFlush = async () => {
        // 确保 store 已初始化
        try {
            await _getStore();
        } catch (e) {
            console.warn('[TauriStore] flush: 获取 Store 失败:', e);
            return;
        }
        // 如果链上还有 pending 的 save，等待它完成
        // 再额外执行一次 save 兜底（处理链上刚追加但尚未开始的 save）
        _scheduleSave();
        await _saveChain;
    };

    const syncStorageHandler = {
        get(target, prop) {
            if (prop === 'getItem') {
                return (key) => (memStorage[key] !== undefined ? memStorage[key] : null);
            }
            if (prop === 'setItem') {
                return (key, value) => {
                    memStorage[key] = String(value);
                    _getStore()
                        .then(async (store) => {
                            try {
                                await store.set(key, String(value));
                                _scheduleSave();
                            } catch (e) {
                                console.warn('[TauriStore] set 失败:', e);
                            }
                        })
                        .catch(() => {});
                };
            }
            if (prop === 'removeItem') {
                return (key) => {
                    delete memStorage[key];
                    _getStore()
                        .then(async (store) => {
                            try {
                                await store.delete(key);
                                _scheduleSave();
                            } catch (e) {
                                console.warn('[TauriStore] delete 失败:', e);
                            }
                        })
                        .catch(() => {});
                };
            }
            if (prop === 'clear') {
                return () => {
                    Object.keys(memStorage).forEach((k) => delete memStorage[k]);
                    _getStore()
                        .then(async (store) => {
                            try {
                                const keys = await store.keys();
                                for (const key of keys) {
                                    await store.delete(key);
                                }
                                _scheduleSave();
                            } catch (e) {
                                console.warn('[TauriStore] clear 失败:', e);
                            }
                        })
                        .catch(() => {});
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
                _getStore()
                    .then(async (store) => {
                        try {
                            await store.set(prop, String(value));
                            _scheduleSave();
                        } catch (e) {
                            console.warn('[TauriStore] 持久化失败:', e);
                        }
                    })
                    .catch(() => {});
                return true;
            }
            return Reflect.set(target, prop, value);
        },
        deleteProperty(target, prop) {
            if (typeof prop === 'string' && !prop.startsWith('__')) {
                delete memStorage[prop];
                _getStore()
                    .then(async (store) => {
                        try {
                            await store.delete(prop);
                            _scheduleSave();
                        } catch (e) {
                            console.warn('[TauriStore] 删除失败:', e);
                        }
                    })
                    .catch(() => {});
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
