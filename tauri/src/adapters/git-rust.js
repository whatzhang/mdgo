/**
 * Tauri 模式 Git 适配器（唯一实现：Rust `commands/git.rs`）
 *
 * 只暴露「记录 / 对比 / 暂存 / 提交」四条主链路所需的最小接口。
 * 设计原则：
 *  - 不做结果缓存（写操作后不会读到旧数据），只做并发请求去重；
 *  - 所有错误统一抛 `Error(String)`，不再把失败吞成 null；
 *  - 参数名按 Tauri 约定用 camelCase（Rust 侧 snake_case）。
 */
(function () {
    if (typeof window.__TAURI__ === 'undefined') {
        return;
    }

    const { invoke } = window.__TAURI__.core;

    /** 并发去重：同一 key 的请求在飞行中只发一次 */
    const inflight = new Map();
    function dedupe(key, factory) {
        if (inflight.has(key)) return inflight.get(key);
        const p = factory().finally(() => inflight.delete(key));
        inflight.set(key, p);
        return p;
    }

    async function call(cmd, args) {
        try {
            return await invoke(cmd, args);
        } catch (err) {
            const msg = err && typeof err === 'object' && 'message' in err ? err.message : String(err);
            throw new Error(msg || `${cmd} 执行失败`);
        }
    }

    function toBlobPayload(raw) {
        if (!raw) throw new Error('读取文件内容失败');
        return {
            data: raw.data instanceof Uint8Array ? raw.data : new Uint8Array(raw.data || []),
            size: raw.size || 0,
            isBinary: !!raw.is_binary,
        };
    }

    window.GitRustAdapter = {
        /** 历史记录（分页） */
        async log(options) {
            const { dir, depth, skip, ref, filepath } = options || {};
            if (!dir) throw new Error('仓库路径为空');
            const args = {
                dir,
                depth: depth || 100,
                skip: skip || 0,
                refName: ref || null,
                filepath: filepath || null,
            };
            return dedupe(`log:${JSON.stringify(args)}`, () => call('git_log', args));
        },

        /** 某次提交的变更文件列表 */
        async files(dir, commit, parent) {
            if (!dir || !commit) throw new Error('缺少仓库路径或提交号');
            const args = { dir, commit, parent: parent || null };
            return dedupe(`files:${JSON.stringify(args)}`, () => call('git_commit_files', args));
        },

        /** 读取提交内文件内容 */
        async readBlob(dir, oid, filepath) {
            const raw = await call('git_read_blob', { dir, oid, filepath });
            return toBlobPayload(raw);
        },

        /** 读取已暂存文件内容（index:stage 0） */
        async readIndexBlob(dir, filepath) {
            const raw = await call('git_read_index_blob', { dir, filepath });
            return toBlobPayload(raw);
        },

        /** 分支 / 远程 / 标签 / HEAD 状态 */
        async parseRefs(dir) {
            return dedupe(`refs:${dir}`, () => call('git_parse_refs', { dir }));
        },

        /** 工作区状态（原始 XY 码） */
        async status(dir) {
            return call('git_status', { dir });
        },

        async stage(dir, paths) {
            return call('git_stage', { dir, paths: paths || [] });
        },

        async unstage(dir, paths) {
            return call('git_unstage', { dir, paths: paths || [] });
        },

        async discard(dir, paths) {
            return call('git_discard', { dir, paths: paths || [] });
        },

        async identity(dir) {
            return call('git_identity', { dir });
        },

        async setIdentity(dir, name, email) {
            return call('git_set_identity', { dir, name, email });
        },

        /** 提交，返回完整 SHA */
        async commit(dir, message, authorName, authorEmail) {
            return call('git_commit', {
                dir,
                message,
                authorName: authorName || null,
                authorEmail: authorEmail || null,
            });
        },
    };
})();
