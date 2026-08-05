/**
 * Tauri Skill 适配层
 *
 * 封装 skill_* 命令（invoke）与 skill:changed 事件订阅。
 * 统一通过 window.__mdgoSkill 暴露，供 index.html 技能管理界面调用。
 *
 * 约定：所有前端函数/常量/ID 均带 skill 前缀，与现有业务隔离。
 */

(function () {
    if (typeof window.__TAURI__ === 'undefined') {
        console.log('[SkillAdapter] 非 Tauri 环境，跳过适配');
        return;
    }

    const invoke = window.__TAURI__.core.invoke;
    const listen = window.__TAURI__.event.listen;

    const skillApi = {
        /** 技能列表（scope 可选: system/global/project） */
        skillList: (dirPath, scope) => invoke('skill_list', { dirPath, scope: scope || null }),
        /** 技能详情（含正文） */
        skillGet: (dirPath, scope, id) => invoke('skill_get', { dirPath, scope, id }),
        /** 新建技能（scope: global/project） */
        skillCreate: (dirPath, scope, input) => invoke('skill_create', { dirPath, scope, input }),
        /** 更新技能（version 自动自增） */
        skillUpdate: (dirPath, scope, id, input) => invoke('skill_update', { dirPath, scope, id, input }),
        /** 删除技能（仅用户级） */
        skillDelete: (dirPath, scope, id) => invoke('skill_delete', { dirPath, scope, id }),
        /** 线上启停 */
        skillSetEnabled: (dirPath, scope, id, enabled) => invoke('skill_set_enabled', { dirPath, scope, id, enabled }),
        /** 会话挂载/卸载/查询 */
        skillAttach: (dirPath, sessionId, scope, skillId) => invoke('skill_attach', { dirPath, sessionId, scope, skillId }),
        skillDetach: (dirPath, sessionId, scope, skillId) => invoke('skill_detach', { dirPath, sessionId, scope, skillId }),
        skillGetAttached: (dirPath, sessionId) => invoke('skill_get_attached', { dirPath, sessionId }),
        /** 技能执行指标聚合 */
        skillMetrics: (dirPath, skillId, since) => invoke('skill_metrics', { dirPath, skillId: skillId || null, since: since || null }),
        /** 注册表变更订阅（返回取消函数） */
        skillOnChanged: async (callback) => {
            const unlisten = await listen('skill:changed', () => callback());
            return unlisten;
        },
    };

    window.__mdgoSkill = skillApi;
    console.log('[SkillAdapter] Skill 适配器已挂载');
})();
