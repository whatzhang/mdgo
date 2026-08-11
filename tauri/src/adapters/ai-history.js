/**
 * AI 历史记录 Tauri 适配层
 *
 * 在 Tauri 环境下拦截全局 AI 历史操作函数，改为通过 Tauri invoke
 * 调用 Rust 后端的 AiHistoryStore（SQLite）。
 *
 * 在非 Tauri 环境下自动跳过，保持现有 File System Access API 逻辑不变。
 */
(function () {
    if (typeof window.__TAURI__ === 'undefined') {
        return; // 非 Tauri 环境，跳过
    }

    const { invoke } = window.__TAURI__.core;

    /**
     * 将 Rust 后端的 AiHistoryItem（snake_case + boolean）转换为
     * 前端全局变量 aiHistory[] 所需的格式（camelCase + 0/1 + date 字符串）
     */
    function _toFrontendFormat(rustItem) {
        const date = new Date(rustItem.created_at);
        const y = date.getFullYear();
        const m = String(date.getMonth() + 1).padStart(2, '0');
        const d = String(date.getDate()).padStart(2, '0');
        const h = String(date.getHours()).padStart(2, '0');
        const min = String(date.getMinutes()).padStart(2, '0');
        const s = String(date.getSeconds()).padStart(2, '0');
        return {
            id: rustItem.id,
            type: rustItem.type,
            label: rustItem.label,
            prompt: rustItem.prompt,
            result: rustItem.result,
            fileName: rustItem.file_name,
            filePath: rustItem.file_path,
            date: `${y}-${m}-${d} ${h}:${min}:${s}`,
            lastAccess: rustItem.last_access_at,
            favorite: rustItem.favorite ? 1 : 0,
        };
    }

    /** 前端 LRU 上限（略大于后端，避免加载后立即淘汰） */
    const LOCAL_MAX_NON_FAV = 1050;
    const LOCAL_MAX_FAV = 210;

    /**
     * 本地 LRU 淘汰：与后端策略保持一致，防止前端数组无限增长
     */
    function _localLruEvict() {
        if (!window.aiHistory) return;
        // 非收藏记录淘汰
        const nonFav = window.aiHistory.filter(i => !i.favorite);
        if (nonFav.length > LOCAL_MAX_NON_FAV) {
            const excess = nonFav.length - LOCAL_MAX_NON_FAV;
            nonFav.sort((a, b) => (a.lastAccess || 0) - (b.lastAccess || 0));
            const toRemove = new Set(nonFav.slice(0, excess).map(i => i.id));
            window.aiHistory = window.aiHistory.filter(i => !toRemove.has(i.id));
        }
        // 收藏记录淘汰
        const fav = window.aiHistory.filter(i => i.favorite);
        if (fav.length > LOCAL_MAX_FAV) {
            const excess = fav.length - LOCAL_MAX_FAV;
            fav.sort((a, b) => (a.lastAccess || 0) - (b.lastAccess || 0));
            const toRemove = new Set(fav.slice(0, excess).map(i => i.id));
            window.aiHistory = window.aiHistory.filter(i => !toRemove.has(i.id));
        }
    }

    /**
     * 获取当前项目目录路径，惰性缓存避免重复获取
     * 当路径为空时，不缓存结果，允许下次重新尝试
     */
    function _getDirPath() {
        const handle = window.getRootHandle && window.getRootHandle();
        return handle ? handle.path : '';
    }

    // ─── 替换全局函数 ───

    /**
     * getAIHistoryData — 从 SQLite 查询 AI 历史记录列表
     *
     * Rust 后端返回顺序：收藏全部在前、非收藏最近 10 条在后，各组内按
     * created_at DESC（新在前）。渲染由 renderAIHistory 显式排序
     * （收藏置顶 + 组内时间降序），此处保持后端顺序即可，无需再反转。
     */
    window.getAIHistoryDataTauri = async function () {
        // 如果已有数据，直接返回
        if (window.aiHistory && window.aiHistory.length > 0) {
            return window.aiHistory;
        }
        const path = _getDirPath();
        // 没有路径时，返回空数组但不缓存（允许下次重新尝试）
        if (!path) {
            return [];
        }
        try {
            // 只加载「收藏全部 + 最近 10 条非收藏」（面板空间有限，不需要 200 条）
            const items = await invoke('ai_history_list', {
                dirPath: path,
                limit: 10,
                offset: 0,
            });
            window.aiHistory = (items || []).map(_toFrontendFormat);
            return window.aiHistory;
        } catch (e) {
            console.error('[TauriAIHistory] 读取失败:', e);
            return [];
        }
    };

    /**
     * saveAIHistory — Tauri 模式下无操作
     *
     * 每次 add / delete / toggle / show 已通过 invoke 实时写入 SQLite，
     * 无需全量序列化写回 JSON 文件。
     */
    window.saveAIHistoryTauri = async function () {
        // Tauri 模式下无需全量保存
    };

    /**
     * addAIHistoryItem — 通过 invoke 写入 SQLite
     *
     * 用 push 追加到尾部（保持 oldest-first 顺序），
     * 与 renderAIHistory 的 reverse() 配合显示最新在前。
     */
    window.addAIHistoryItemTauri = async function (item) {
        // 优先使用调用方显式传入的 dirPath（如 currentRootPath），
        // 避免依赖 _getDirPath() 可能因句柄状态异常返回空路径导致数据丢失
        const path = item.dirPath || _getDirPath();
        if (!path) {
            console.error('[TauriAIHistory] 添加失败: 无法获取根目录路径');
            return;
        }
        // 移除 dirPath 字段，不发送到后端
        const { dirPath: _dirPath, ...rest } = item;
        try {
            const newItem = await invoke('ai_history_add', {
                dirPath: path,
                item: {
                    type: rest.type || '',
                    label: rest.label || '',
                    prompt: rest.prompt || '',
                    result: rest.result || '',
                    file_name: rest.fileName || '',
                    file_path: rest.filePath || '',
                    token_count: rest.tokenCount || 0,
                },
            });
            const frontendItem = _toFrontendFormat(newItem);
            if (!window.aiHistory) {
                window.aiHistory = [];
            }
            // push 追加到数组末尾；渲染时由 renderAIHistory 显式排序，此处顺序无关紧要
            window.aiHistory.push(frontendItem);
            // 本地 LRU 淘汰防止内存无限增长
            _localLruEvict();

            const listEl = document.getElementById('ai-history-list');
            if (listEl && typeof renderAIHistory === 'function') {
                renderAIHistory();
            }
        } catch (e) {
            console.error('[TauriAIHistory] 添加失败:', e);
        }
    };

    /**
     * toggleAIFavorite — 通过 invoke 切换收藏状态
     */
    window.toggleAIFavoriteTauri = async function (id) {
        const path = _getDirPath();
        if (!path) {
            console.error('[TauriAIHistory] 切换收藏失败: 无法获取根目录路径');
            return;
        }
        try {
            const newFav = await invoke('ai_history_toggle_favorite', {
                dirPath: path,
                id: id,
            });
            // 更新内存缓存
            if (window.aiHistory) {
                const item = window.aiHistory.find(i => i.id === id);
                if (item) {
                    item.favorite = newFav ? 1 : 0;
                }
            }
            // 重新渲染：收藏状态变化后条目需在「收藏置顶」分组间移动
            const listEl = document.getElementById('ai-history-list');
            if (listEl && typeof renderAIHistory === 'function') {
                renderAIHistory();
            }
            // 统一经全局通知通道（Notify）发送
            window.Notify.show(
                newFav ? '⭐ 收藏成功' : '已取消收藏',
                newFav ? 'success' : 'info',
                1000
            );
        } catch (e) {
            console.error('[TauriAIHistory] 切换收藏失败:', e);
        }
    };

    /**
     * showAIHistoryItem — 覆盖原始函数以同步 lastAccess 到 SQLite
     */
    window.showAIHistoryItemTauri = async function (id) {
        const item = (window.aiHistory || []).find(i => i.id === id);
        if (!item) return;
        const now = Date.now();
        item.lastAccess = now;
        // 同步 lastAccess 到 SQLite（保持 LRU 淘汰数据准确）
        const path = _getDirPath();
        if (path) {
            try {
                await invoke('ai_history_update_access_time', { dirPath: path, id });
            } catch (e) {
                console.warn('[TauriAIHistory] 更新时间失败:', e);
            }
        }
        if (item.type === 'custom') {
            const promptInput = document.getElementById('ai-prompt-input');
            if (promptInput) promptInput.value = item.prompt;
        }
        await showAIResultModal(item.type, item.label, item.result, item.fileName, item.filePath);
    };

    /**
     * deleteAIHistoryItem — 通过 invoke 删除
     */
    window.deleteAIHistoryItemTauri = async function (id) {
        const path = _getDirPath();
        if (!path) {
            console.error('[TauriAIHistory] 删除失败: 无法获取根目录路径');
            return;
        }
        try {
            await invoke('ai_history_delete', { dirPath: path, id: id });
            // 更新内存缓存
            if (window.aiHistory) {
                const index = window.aiHistory.findIndex(i => i.id === id);
                if (index !== -1) window.aiHistory.splice(index, 1);
            }
            const listEl = document.getElementById('ai-history-list');
            if (listEl && typeof renderAIHistory === 'function') {
                renderAIHistory();
            }
        } catch (e) {
            console.error('[TauriAIHistory] 删除失败:', e);
        }
    };

    /**
     * updateFileNameInAIHistory — 通过 invoke 同步文件重命名
     */
    window.updateFileNameInAIHistoryTauri = async function (oldRelativePath, newName, newRelativePath) {
        const path = _getDirPath();
        if (!path) {
            console.warn('[TauriAIHistory] 更新文件路径失败: 无法获取根目录路径');
            return;
        }
        try {
            await invoke('ai_history_update_file_path', {
                dirPath: path,
                oldFilePath: oldRelativePath || '',
                newFileName: newName || '',
                newFilePath: newRelativePath || '',
            });
            // 同步更新本地缓存中的文件名/路径
            if (window.aiHistory) {
                for (const item of window.aiHistory) {
                    if (item.filePath === oldRelativePath) {
                        item.fileName = newName || '';
                        item.filePath = newRelativePath || '';
                    }
                }
            }
        } catch (e) {
            console.warn('[TauriAIHistory] 更新文件路径失败:', e);
        }
    };
    console.log('[TauriAIHistory] AI 历史适配层已加载（SQLite 模式）');
})();
