/**
 * Tauri Prompt 适配层
 *
 * 封装 prompt_* 命令（invoke）与 prompt:changed 事件订阅。
 * 统一通过 window.__mdgoPrompt 暴露，供 prompt 管理界面调用。
 *
 * 三层体系：
 * - system：resources/prompt/*.md（只读）
 * - global：用户数据目录 prompts.db（跨项目共享）
 * - project：{dir}/.mdgo/mdgo.db 的 prompts 表（随项目走）
 */

(function () {
    if (typeof window.__TAURI__ === 'undefined') {
        console.log('[PromptAdapter] 非 Tauri 环境，跳过适配');
        return;
    }

    const invoke = window.__TAURI__.core.invoke;
    const listen = window.__TAURI__.event.listen;

    const promptApi = {
        /** Prompt 列表（scope 可选: '' | system | global | project） */
        promptList: (dirPath, scope) => invoke('prompt_list', { dirPath, scope: scope || null }),
        /** 新建（scope: global / project） */
        promptCreate: (dirPath, scope, name, prompt) => invoke('prompt_create', { dirPath, scope, name, prompt }),
        /** 更新（scope: global / project） */
        promptUpdate: (dirPath, scope, id, name, prompt) => invoke('prompt_update', { dirPath, scope, id, name, prompt }),
        /** 删除（scope: global / project；system 只读拒绝） */
        promptDelete: (dirPath, scope, id) => invoke('prompt_delete', { dirPath, scope, id }),
        /** 变更订阅（返回取消函数） */
        promptOnChanged: async (callback) => {
            const unlisten = await listen('prompt:changed', () => callback());
            return unlisten;
        },
    };

    window.__mdgoPrompt = promptApi;
    console.log('[PromptAdapter] Prompt 适配器已挂载');
})();
