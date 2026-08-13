/**
 * Tauri MCP 适配层
 *
 * 封装 mcp_* 命令（invoke），统一通过 window.__mdgoMcp 暴露，
 * 供 index.html 的 MCP 管理页面（UI/交互参考 Skill 模块）调用。
 *
 * 约定：所有函数带 mcp 前缀，与现有业务隔离。
 */

(function () {
    if (typeof window.__TAURI__ === 'undefined') {
        console.log('[McpAdapter] 非 Tauri 环境，跳过适配');
        return;
    }

    const invoke = window.__TAURI__.core.invoke;

    const mcpApi = {
        /** 服务器列表（自动加载目录配置并连接启用的服务器） */
        mcpList: (dirPath) => invoke('mcp_list', { dirPath }),
        /** 服务器详情（配置 + 工具清单；不含运行日志） */
        mcpGet: (dirPath, name) => invoke('mcp_get', { dirPath, name }),
        /** 运行日志（按需加载：详情页不随 mcp_get 返回，点击后单独拉取） */
        mcpLogs: (dirPath, name) => invoke('mcp_logs', { dirPath, name }),
        /** 新增/更新服务器（写 .mdgo/mcp.json + 重连） */
        mcpUpsert: (dirPath, name, config) => invoke('mcp_upsert', { dirPath, name, config }),
        /** 删除服务器 */
        mcpDelete: (dirPath, name) => invoke('mcp_delete', { dirPath, name }),
        /** 连接 / 断开 / 重启 */
        mcpConnect: (dirPath, name) => invoke('mcp_connect', { dirPath, name }),
        mcpDisconnect: (dirPath, name) => invoke('mcp_disconnect', { dirPath, name }),
        mcpRestart: (dirPath, name) => invoke('mcp_restart', { dirPath, name }),
        /** 测试连接（不落盘），返回工具数 */
        mcpTest: (config) => invoke('mcp_test', { config }),
    };

    window.__mdgoMcp = mcpApi;
    console.log('[McpAdapter] MCP 适配器已挂载');
})();
