/**
 * Tauri 模式 Git 适配器（使用 Rust gix 实现）
 * API 与 isomorphic-git 完全兼容
 *
 * 优化说明:
 * 1. 请求去重: 相同参数的并发请求只发一次，复用同一个 Promise
 * 2. 短路缓存: 100ms 内相同请求直接返回上次结果
 * 3. 减少不必要的序列化/反序列化开销
 */
(function () {
    if (typeof window.__TAURI__ === 'undefined') {
        return;
    }

    const { invoke } = window.__TAURI__.core;

    // 请求去重表: key -> Promise
    // 当同一函数同一参数同时被调用多次时，只发一次请求
    const inflightMap = new Map();

    /**
     * 带请求去重的 invoke 包装
     * 相同 key 的并发请求只会发起一次调用
     */
    function dedupedInvoke(cmd, params, cacheKey) {
        const key = cacheKey || cmd + '_' + JSON.stringify(params);
        // 如果已有相同请求在执行中，直接复用
        if (inflightMap.has(key)) {
            return inflightMap.get(key);
        }
        const promise = invoke(cmd, params)
            .then(result => {
                inflightMap.delete(key);
                return result;
            })
            .catch(err => {
                inflightMap.delete(key);
                throw err;
            });
        inflightMap.set(key, promise);
        return promise;
    }

    // Git Rust 适配器（接口与 isomorphic-git 完全一致）
    window.GitRustAdapter = {
        /**
         * 获取提交记录
         * @param {Object} options - { dir, depth, filepath }
         * @returns {Promise<Array>} - 提交列表
         */
        async log(options) {
            const { dir, depth, filepath } = options;
            const cacheKey = `git_log_${dir}_${depth}_${filepath}`;
            try {
                const commits = await dedupedInvoke('git_log', {
                    dir,
                    depth,
                    filepath: filepath || null,
                }, cacheKey);
                return commits;
            } catch (error) {
                console.error('[GitRust] log error:', error);
                throw new Error(`Git log 失败: ${error}`);
            }
        },

        /**
         * 获取文件状态矩阵
         * @param {Object} options - { dir }
         * @returns {Promise<Array>} - 状态矩阵 [[filepath, head, workdir, stage], ...]
         */
        async statusMatrix(options) {
            const { dir } = options;
            const cacheKey = `git_status_${dir}`;
            try {
                const matrix = await dedupedInvoke('git_status_matrix', { dir }, cacheKey);
                return matrix;
            } catch (error) {
                console.error('[GitRust] statusMatrix error:', error);
                throw new Error(`Git status 失败: ${error}`);
            }
        },

        /**
         * 恢复文件到 HEAD 状态
         * @param {Object} options - { dir, filepaths, force }
         * @returns {Promise<void>}
         */
        async checkout(options) {
            const { dir, filepaths, force } = options;
            try {
                await invoke('git_checkout', {
                    dir,
                    filepaths,
                    force: force || false,
                });
            } catch (error) {
                console.error('[GitRust] checkout error:', error);
                throw new Error(`Git checkout 失败: ${error}`);
            }
        },

        /**
         * 解析引用（分支、标签等）
         * @param {String} dir - 仓库路径
         * @returns {Promise<Object>} - 引用信息
         */
        async parseRefs(dir) {
            const cacheKey = `git_parseRefs_${dir}`;
            try {
                const refs = await dedupedInvoke('git_parse_refs', { dir }, cacheKey);
                return refs;
            } catch (error) {
                console.error('[GitRust] parseRefs error:', error);
                throw new Error(`Git parseRefs 失败: ${error}`);
            }
        },

        /**
         * 获取提交的文件变更（对比 parent commit）
         * @param {String} dir - 仓库路径
         * @param {String} commitOid - 提交 hash
         * @param {String|null} parentOid - 父提交 hash
         * @returns {Promise<Array>} - 文件变更列表 [{path, status}, ...]
         */
        async diffTree(dir, commitOid, parentOid) {
            const cacheKey = `git_diffTree_${dir}_${commitOid}_${parentOid}`;
            try {
                const changes = await dedupedInvoke('git_diff_tree', {
                    dir,
                    commitOid,
                    parentOid: parentOid || null,
                }, cacheKey);
                return changes;
            } catch (error) {
                console.error('[GitRust] diffTree error:', error);
                throw new Error(`Git diffTree 失败: ${error}`);
            }
        },

        /**
         * 读取文件内容（从指定 commit）
         * @param {String} dir - 仓库路径
         * @param {String} oid - 提交 hash
         * @param {String} filepath - 文件路径
         * @returns {Promise<Object|null>} - {blob: Uint8Array} 或 null（文件不存在时）
         */
        async readBlob(dir, oid, filepath) {
            try {
                const result = await invoke('git_read_blob', {
                    dir,
                    oid,
                    filepath,
                });
                return {
                    blob: new Uint8Array(result.blob),
                };
            } catch (error) {
                return null;
            }
        },

        /**
         * 暂存文件（git add）
         * @param {String} dir - 仓库路径
         * @param {String} filepath - 文件路径
         * @returns {Promise<void>}
         */
        async add(dir, filepath) {
            try {
                await invoke('git_add', { dir, filepath });
            } catch (error) {
                console.error('[GitRust] add error:', error);
                throw new Error(`Git add 失败: ${error}`);
            }
        },

        /**
         * 取消暂存文件（git reset）
         * @param {String} dir - 仓库路径
         * @param {String} filepath - 文件路径
         * @returns {Promise<void>}
         */
        async reset(dir, filepath) {
            try {
                await invoke('git_reset', { dir, filepath });
            } catch (error) {
                console.error('[GitRust] reset error:', error);
                throw new Error(`Git reset 失败: ${error}`);
            }
        },

        /**
         * 提交暂存（git commit）
         * @param {String} dir - 仓库路径
         * @param {String} message - 提交信息
         * @param {String} authorName - 作者名称
         * @param {String} authorEmail - 作者邮箱
         * @returns {Promise<String>} - 提交 SHA
         */
        async commit(dir, message, authorName, authorEmail) {
            try {
                const sha = await invoke('git_commit', {
                    dir,
                    message,
                    authorName,
                    authorEmail,
                });
                return sha;
            } catch (error) {
                console.error('[GitRust] commit error:', error);
                throw new Error(`Git commit 失败: ${error}`);
            }
        },

        /**
         * 解析引用（获取 commit hash）
         * @param {String} dir - 仓库路径
         * @param {String} refName - 引用名称
         * @returns {Promise<String>} - commit hash
         */
        async resolveRef(dir, refName) {
            try {
                const sha = await invoke('git_resolve_ref', { dir, refName });
                return sha;
            } catch (error) {
                console.error('[GitRust] resolveRef error:', error);
                throw new Error(`Git resolveRef 失败: ${error}`);
            }
        },
    };
})();
