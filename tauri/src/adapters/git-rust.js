/**
 * Tauri 模式 Git 适配器（使用 Rust gix 实现）
 * API 与 isomorphic-git 完全兼容
 */
(function () {
    if (typeof window.__TAURI__ === 'undefined') {
        return;
    }

    const { invoke } = window.__TAURI__.core;

    // Git Rust 适配器（接口与 isomorphic-git 完全一致）
    window.GitRustAdapter = {
        /**
         * 获取提交记录
         * @param {Object} options - { dir, depth, filepath }
         * @returns {Promise<Array>} - 提交列表
         */
        async log(options) {
            const { dir, depth, filepath } = options;

            try {
                const commits = await invoke('git_log', {
                    dir,
                    depth,
                    filepath,
                });
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

            try {
                const matrix = await invoke('git_status_matrix', { dir });
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
            try {
                const refs = await invoke('git_parse_refs', { dir });
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
            try {
                const changes = await invoke('git_diff_tree', {
                    dir,
                    commitOid,
                    parentOid,
                });
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
                    blob: new Uint8Array(result.blob)
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
