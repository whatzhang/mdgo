/**
 * ===== MCP 管理模块（模块化标杆 · css_js/modules/mcp.js） =====
 *
 * 【职责】MCP 服务器管理面板：列表 / 搜索 / 状态筛选 / 详情 / 表单+JSON 双模式编辑 /
 *         连接 / 断开 / 重启 / 删除 / 工具清单 / 运行日志轮询。
 * 【入口】主页面视图路由 type='mcp' → 调用全局函数 openMcpManager() 打开面板。
 * 【对外暴露】mcp 前缀全局函数，供面板 HTML 内联 onclick 调用。
 * 【依赖的全局服务】（来自 index.html 主脚本；加载顺序：主脚本 → 本模块）
 *   - window.__mdgoMcp      后端适配层（Tauri 命令封装），业务数据源（依赖倒置）
 *   - isTauriVisit()        运行环境判断
 *   - switchToView()        视图切换（隐藏其他主容器）
 *   - showNotification()    全局通知
 *   - getRootHandle()/getDirPath()  根目录句柄 / 目录路径
 *   - currentRootPath       当前根目录路径（只读回退值）
 *   - escapeHtml()          安全转义
 *   - initMonacoEditor()/monaco     JSON 编辑模式用（Monaco 全局）
 * 【样式依赖】css_js/modules/skill.css（布局类同构复用）+ index.html 主样式 CSS 变量。
 * 【SOLID 说明】
 *   - S 单一职责：本文件只负责 MCP 管理的展示与交互。
 *   - O 开闭原则：新增传输类型/能力优先扩展 window.__mdgoMcp 适配层，模块主体保持稳定。
 *   - D 依赖倒置：只依赖上述稳定全局服务接口，不依赖任何具体模块内部实现。
 */
// ====== MCP 管理（v2：独立页面，UI/交互参考 Skill） ======
let mcpAllList = [];
let mcpCurrent = null;
let mcpSearchTerm = '';
let mcpStatusFilter = '';
let mcpDirPath = '';
let mcpEditMode = 'form';   // 'form' | 'json'（编辑页双模式）
let mcpEditServer = null;   // 当前编辑的服务器（null = 新增）
let mcpJsonEditor = null;   // JSON 模式 Monaco 编辑器实例（退出编辑页时 dispose）
let mcpToolSearchTerm = ''; // 详情视图工具清单搜索词
let mcpPollTimer = null;    // 详情页定时轮询（状态/日志/工具数异步变化）
let mcpLogsExpanded = false; // 详情视图运行日志是否展开（默认关闭，点击后按需加载）
let mcpLogsData = [];        // 已加载的运行日志（展开期间随轮询刷新）

// ── MCP 配置 JSON Schema（draft-07，.mcp.json 官方格式） ──
const MCP_CONFIG_JSON_SCHEMA = {
    $schema: 'http://json-schema.org/draft-07/schema#',
    title: 'MCP 服务器配置（.mcp.json）',
    description: '官方格式：{ mcpServers: { 名称: { command/args/env | url/headers, enabled } } }',
    type: 'object',
    required: ['mcpServers'],
    properties: {
        mcpServers: {
            type: 'object',
            minProperties: 1,
            maxProperties: 1,
            additionalProperties: { $ref: '#/definitions/server' },
        },
    },
    additionalProperties: false,
    definitions: {
        server: {
            type: 'object',
            properties: {
                command: { type: 'string', minLength: 1, description: 'stdio 传输：可执行命令（如 npx / cmd）' },
                args: { type: 'array', items: { type: 'string' }, description: '命令参数列表' },
                env: { type: 'object', additionalProperties: { type: 'string' }, description: '环境变量（KEY=VALUE）' },
                url: { type: 'string', minLength: 1, description: 'HTTP/SSE 传输地址' },
                headers: { type: 'object', additionalProperties: { type: 'string' }, description: 'HTTP 请求头' },
                enabled: { type: 'boolean', description: '启用开关（保存后自动连接）' },
            },
            oneOf: [
                { required: ['command'], description: 'stdio 传输需提供 command' },
                { required: ['url'], description: 'HTTP/SSE 传输需提供 url' },
            ],
            additionalProperties: false,
        },
    },
};

/**
 * 轻量 JSON Schema（draft-07 子集）校验器：仅覆盖本配置所需关键字。
 * 支持：type / properties / required / additionalProperties / minProperties /
 * maxProperties / items / minItems / maxItems / minLength / maxLength /
 * pattern / enum / minimum / maximum / oneOf / anyOf / allOf / $ref(#/definitions)。
 * 错误按 JSON Pointer 路径收集：{ pointer, message }。
 */
function validateJsonSchema(instance, schema, errors, pointer, rootSchema) {
    const root = rootSchema || schema;
    const ptr = pointer || '';
    const push = (msg) => errors.push({ pointer: ptr, message: msg });
    // $ref 解析（#/definitions/xxx）
    if (typeof schema.$ref === 'string' && schema.$ref.startsWith('#/definitions/')) {
        const key = schema.$ref.slice('#/definitions/'.length);
        const def = (root.definitions || {})[key];
        if (def) validateJsonSchema(instance, def, errors, ptr, root);
        return;
    }
    if (schema.oneOf) {
        const subErrors = [];
        let ok = false;
        for (const sub of schema.oneOf) {
            const errs = [];
            validateJsonSchema(instance, sub, errs, ptr, root);
            if (errs.length === 0) { ok = true; break; }
            subErrors.push(errs);
        }
        if (!ok) {
            // 汇总最贴近的错误（取错误数最少的子 schema）
            const best = subErrors.reduce((a, b) => (b.length < a.length ? b : a), subErrors[0] || []);
            for (const e of best.slice(0, 2)) errors.push(e);
        }
        return;
    }
    if (schema.anyOf) {
        for (const sub of schema.anyOf) validateJsonSchema(instance, sub, errors, ptr, root);
        return;
    }
    if (schema.allOf) {
        for (const sub of schema.allOf) validateJsonSchema(instance, sub, errors, ptr, root);
        return;
    }
    if (schema.enum !== undefined && !schema.enum.some(v => JSON.stringify(v) === JSON.stringify(instance))) {
        push('取值不在允许范围内');
    }
    if (instance === null) {
        if (schema.type && schema.type !== 'null') push('期望类型 ' + schema.type + '，实际为 null');
        return;
    }
    const actualType = Array.isArray(instance) ? 'array' : typeof instance;
    if (schema.type && schema.type !== actualType) {
        push(`期望类型 ${schema.type}，实际为 ${actualType}`);
        return;
    }
    if (actualType === 'object') {
        if (schema.minProperties !== undefined && Object.keys(instance).length < schema.minProperties) {
            push(`属性数量不能少于 ${schema.minProperties}`);
        }
        if (schema.maxProperties !== undefined && Object.keys(instance).length > schema.maxProperties) {
            push(`属性数量不能多于 ${schema.maxProperties}`);
        }
        const props = schema.properties || {};
        for (const key of Object.keys(instance)) {
            if (props[key]) {
                validateJsonSchema(instance[key], props[key], errors, ptr + '/' + key, root);
            } else if (schema.additionalProperties === false) {
                push(`不允许的属性：${key}`);
            } else if (schema.additionalProperties && typeof schema.additionalProperties === 'object') {
                validateJsonSchema(instance[key], schema.additionalProperties, errors, ptr + '/' + key, root);
            }
        }
        for (const req of (schema.required || [])) {
            if (!Object.prototype.hasOwnProperty.call(instance, req)) {
                push(`缺少必填属性：${req}`);
            }
        }
    } else if (actualType === 'array') {
        if (schema.minItems !== undefined && instance.length < schema.minItems) push(`数组长度不能少于 ${schema.minItems}`);
        if (schema.maxItems !== undefined && instance.length > schema.maxItems) push(`数组长度不能多于 ${schema.maxItems}`);
        if (schema.items) {
            instance.forEach((item, idx) => {
                validateJsonSchema(item, schema.items, errors, `${ptr}/${idx}`, root);
            });
        }
    } else if (actualType === 'string') {
        if (schema.minLength !== undefined && instance.length < schema.minLength) push(`字符串长度不能少于 ${schema.minLength}`);
        if (schema.maxLength !== undefined && instance.length > schema.maxLength) push(`字符串长度不能多于 ${schema.maxLength}`);
        if (schema.pattern && !new RegExp(schema.pattern).test(instance)) push(`不匹配模式 ${schema.pattern}`);
    } else if (actualType === 'number') {
        if (schema.minimum !== undefined && instance < schema.minimum) push(`不能小于 ${schema.minimum}`);
        if (schema.maximum !== undefined && instance > schema.maximum) push(`不能大于 ${schema.maximum}`);
    }
}

/** 对配置 JSON 执行 schema 校验，返回错误数组（无错误返回空数组） */
function mcpValidateConfigJson(value) {
    const errors = [];
    try {
        validateJsonSchema(value, MCP_CONFIG_JSON_SCHEMA, errors, '', MCP_CONFIG_JSON_SCHEMA);
    } catch (e) {
        errors.push({ pointer: '', message: '校验器异常: ' + e.message });
    }
    return errors;
}

/** 打开 MCP 管理页（仅 Tauri 可用） */
async function openMcpManager() {
    if (!isTauriVisit() || !window.__mdgoMcp) {
        showNotification('MCP 管理仅在桌面版（Tauri）可用', 'error');
        return;
    }
    if (!getRootHandle()) {
        showNotification('请先打开根目录', 'error');
        return;
    }
    const container = document.getElementById('mcp-container');
    if (!container) return;
    // 兜底清理：进入 MCP 页时释放上次残留的 Monaco 编辑器实例
    mcpDisposeJsonEditor();
    await switchToView(container, 'flex');
    mcpDirPath = getDirPath(getRootHandle()) || currentRootPath;
    await mcpLoadList();
    mcpStartPolling();
}

/** 拉取服务器列表并渲染（silent=true 供轮询，失败不弹通知） */
async function mcpLoadList({ silent } = {}) {
    try {
        mcpAllList = await window.__mdgoMcp.mcpList(mcpDirPath);
    } catch (e) {
        if (!silent) showNotification('加载 MCP 服务器失败: ' + e, 'error');
        return;
    }
    mcpRenderList();
    if (mcpCurrent) {
        // 保持选中态：列表项（McpServerInfo）不含 config/tools/logs，
        // 必须重新拉详情再渲染，否则详情被降级为残缺数据
        const stillExists = mcpAllList.some(s => s.name === mcpCurrent.name);
        if (stillExists) {
            await mcpRefreshCurrentDetail({ silent });
        } else {
            // 服务器已被删除：清空选中态并隐藏详情
            mcpCurrent = null;
            const detail = document.getElementById('mcp-detail-view');
            if (detail) detail.style.display = 'none';
            mcpShowEmpty();
        }
    } else {
        mcpShowEmpty();
    }
}

/** 刷新当前选中服务器的详情（保留选中态；编辑视图打开时不打断） */
async function mcpRefreshCurrentDetail({ silent } = {}) {
    if (!mcpCurrent || mcpEditServer) return;
    const name = mcpCurrent.name;
    try {
        const server = await window.__mdgoMcp.mcpGet(mcpDirPath, name);
        mcpCurrent = server;
        mcpRenderList();
        // 工具搜索框正在输入时跳过整页重建（避免打断焦点/光标），仅更新数据源
        const focused = document.activeElement;
        const typingInSearch = focused && focused.id === 'mcp-tool-search';
        if (!typingInSearch) mcpRenderDetail(server);
    } catch (e) {
        if (!silent) showNotification('加载详情失败: ' + e, 'error');
    }
}

/** 定时轮询：服务端状态/工具数/日志异步变化，页面常驻时自动同步 */
function mcpStartPolling() {
    mcpStopPolling();
    mcpPollTimer = setInterval(async () => {
        // 编辑视图中不刷新（避免覆盖表单输入 / Monaco 内容）
        if (mcpEditServer) return;
        await mcpLoadList({ silent: true });
    }, 3000);
}

/** 停止定时轮询（离开 MCP 页时调用） */
function mcpStopPolling() {
    if (mcpPollTimer) {
        clearInterval(mcpPollTimer);
        mcpPollTimer = null;
    }
}

/** 空态/默认选中第一项 */
function mcpShowEmpty() {
    const first = mcpAllList[0];
    if (first) mcpSelectServer(first.name);
}

/** 渲染左侧列表（状态 + 关键词双重过滤） */
function mcpRenderList() {
    const kw = mcpSearchTerm.trim().toLowerCase();
    const filtered = mcpAllList.filter(s => {
        if (mcpStatusFilter && s.status !== mcpStatusFilter) return false;
        if (!kw) return true;
        return (s.name + ' ' + (s.error || '')).toLowerCase().includes(kw);
    });
    const listEl = document.getElementById('mcp-list');
    if (!listEl) return;
    if (!filtered.length) {
        listEl.innerHTML = `<div class="skill-empty-list">${mcpAllList.length ? '没有匹配的服务器' : '暂无服务器，点击下方按钮新增'}</div>`;
        return;
    }
    listEl.innerHTML = filtered.map(s => `
                <div class="skill-list-item ${mcpCurrent && mcpCurrent.name === s.name ? 'active' : ''}"
                    onclick="mcpSelectServer('${escapeHtml(s.name)}')">
                    <div class="skill-item-avatar">
                        <div class="skill-icon-box">
                            <svg class="skill-icon-svg" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 9l3 3-3 3m5 0h3M5 20h14a2 2 0 002-2V6a2 2 0 00-2-2H5a2 2 0 00-2 2v12a2 2 0 002 2z"></path>
                            </svg>
                        </div>
                    </div>
                    <div class="skill-item-info">
                        <div class="skill-item-name">${escapeHtml(s.name)}</div>
                        <div class="skill-item-meta">
                            <span class="skill-item-tag ${mcpStatusClass(s.status)}">${mcpStatusLabel(s.status)}</span>
                            <span>${s.tool_count || 0} 个工具</span>
                        </div>
                    </div>
                </div>`).join('');
}

function mcpStatusLabel(status) {
    return { connected: '已连接', connecting: '连接中', stopped: '已断开', failed: '失败' }[status] || status;
}
function mcpStatusClass(status) {
    if (status === 'connected') return 'enabled';
    if (status === 'failed') return 'disabled';
    return '';
}

function mcpSelectServer(name) {
    if (!window.__mdgoMcp) return;
    // 切换服务器：重置日志展开状态（新服务器默认关闭）
    mcpLogsExpanded = false;
    mcpLogsData = [];
    window.__mdgoMcp.mcpGet(mcpDirPath, name).then(server => {
        mcpCurrent = server;
        mcpRenderList();
        mcpRenderDetail(server);
    }).catch(e => showNotification('加载详情失败: ' + e, 'error'));
}

/** 渲染右侧详情 */
function mcpRenderDetail(server) {
    const view = document.getElementById('mcp-detail-view');
    if (!view) return;
    view.style.display = 'block';
    const cfg = server.config || {};
    const isConnected = server.status === 'connected';
    const tools = server.tools || [];
    const envEntries = Object.entries(cfg.env || {}).map(([k, v]) =>
        `<div class="skill-tool-row"><code>${escapeHtml(k)}</code> = ${escapeHtml(String(v))}</div>`).join('') || '<div style="color:var(--t3,#888);font-size:12px;">无环境变量</div>';
    // 运行日志：默认关闭（不随详情返回），点击「加载运行日志」后经 mcp_logs 按需拉取
    const logsExpanded = mcpLogsExpanded;
    view.innerHTML = `
                <div class="skill-detail-header">
                    <div style="flex:1;min-width:0;">
                        <div style="font-size:1rem;font-weight:600;display:flex;align-items:center;gap:8px;flex-wrap:wrap;">
                            ${escapeHtml(server.name)}
                            <span class="skill-item-tag ${mcpStatusClass(server.status)}">${mcpStatusLabel(server.status)}</span>
                            ${cfg.enabled ? '' : '<span class="skill-item-tag disabled">已停用</span>'}
                        </div>
                        <div style="color:var(--t3,#888);font-size:12px;margin-top:2px;">
                            ${escapeHtml(cfg.command || '')} ${escapeHtml((cfg.args || []).join(' '))}
                        </div>
                        ${server.error ? `<div style="color:var(--color-danger,#cf222e);font-size:12px;margin-top:4px;">✗ ${escapeHtml(server.error)}</div>` : ''}
                    </div>
                </div>
                <div style="display:flex;gap:6px;justify-content:flex-end;flex-wrap:wrap;padding:0.75rem 0;border-bottom:1px solid var(--border-color-light);">
                    ${isConnected
            ? `<button class="btn btn-sm btn-primary" onclick="mcpDisconnect('${escapeHtml(server.name)}')">断开</button>
                           <button class="btn btn-sm btn-primary" onclick="mcpRestart('${escapeHtml(server.name)}')">重启</button>`
            : `<button class="btn btn-sm btn-primary" onclick="mcpConnect('${escapeHtml(server.name)}')">连接</button>`}
                    <button class="btn btn-sm btn-primary" onclick="mcpOpenEdit('${escapeHtml(server.name)}')">编辑</button>
                    <button class="btn btn-sm btn-danger" onclick="mcpDeleteConfirm('${escapeHtml(server.name)}')">删除</button>
                </div>
                <div style="padding:0.75rem 0;">
                    <div id="mcp-tools-pane">${mcpBuildToolsPane(tools)}</div>
                    <div class="skill-detail-title" style="font-weight:600;margin:1rem 0 6px;">环境变量</div>
                    ${envEntries}
                    <div class="skill-detail-title" style="font-weight:600;margin:1rem 0 6px;">运行日志</div>
                    <button class="btn btn-sm" id="mcp-logs-toggle" onclick="mcpToggleLogs()">${logsExpanded ? '收起日志' : '加载运行日志'}</button>
                    <div id="mcp-logs-pane" style="${logsExpanded ? '' : 'display:none;'}">${logsExpanded ? mcpRenderLogsInner() : ''}</div>
                </div>`;
    // 日志已展开时保持数据新鲜（按需拉取，不阻塞详情渲染）
    if (logsExpanded) mcpLoadLogs();
}

/** 构建「工具清单」面板（搜索框 + 工具行 + schema 折叠），搜索输入时仅重建本区域 */
function mcpBuildToolsPane(tools) {
    const toolKw = mcpToolSearchTerm.trim().toLowerCase();
    const filteredTools = tools.filter(t => {
        if (!toolKw) return true;
        return (t.name || '').toLowerCase().includes(toolKw)
            || (t.description || '').toLowerCase().includes(toolKw);
    });
    const toolItems = filteredTools.length ? filteredTools.map(t => {
        const schema = t.input_schema && typeof t.input_schema === 'object' ? t.input_schema : null;
        const paramCount = schema && schema.properties ? Object.keys(schema.properties).length : 0;
        const hasSchema = schema && (paramCount > 0 || schema.type === 'object');
        return `<div class="skill-tool-row">
                    <code>${escapeHtml(t.name)}</code>
                    <div style="color:var(--t3,#888);font-size:11px;margin-top:2px;word-break:break-all;">${escapeHtml(t.description || '—')}</div>
                    ${hasSchema ? `<details style="margin-top:3px;">
                        <summary style="cursor:pointer;font-size:11px;color:var(--color-text-secondary,#888);">参数 schema${paramCount ? `（${paramCount} 个参数）` : ''}</summary>
                        <pre style="margin:4px 0 0;padding:6px 8px;background:var(--bg-code,rgba(0,0,0,0.03));border-radius:4px;font-size:11px;line-height:1.5;overflow:auto;max-height:14rem;">${escapeHtml(JSON.stringify(schema, null, 2))}</pre>
                    </details>` : ''}
                </div>`;
    }).join('') : `<div style="color:var(--t3,#888);font-size:12px;">${tools.length ? '没有匹配的工具' : '未连接或服务器未提供工具'}</div>`;
    return `<div class="skill-detail-title" style="font-weight:600;margin-bottom:6px;">工具清单（${tools.length}）</div>
                ${tools.length ? `<div style="margin-bottom:6px;">
                    <input type="text" id="mcp-tool-search" placeholder="搜索工具…" value="${escapeHtml(mcpToolSearchTerm)}"
                        oninput="mcpToolSearchInput(this.value)"
                        style="width:100%;box-sizing:border-box;padding:0.35rem 0.5rem;font-size:0.78rem;border:1px solid var(--color-border,#333);border-radius:0.375rem;background:transparent;color:var(--text-color,#eee);outline:none;">
                </div>` : ''}
                ${toolItems}`;
}

/** 工具清单搜索输入（仅重建工具面板，保留详情视图其它区域与输入焦点） */
function mcpToolSearchInput(value) {
    mcpToolSearchTerm = value || '';
    const pane = document.getElementById('mcp-tools-pane');
    if (pane && mcpCurrent) {
        pane.innerHTML = mcpBuildToolsPane(mcpCurrent.tools || []);
        const input = document.getElementById('mcp-tool-search');
        if (input) {
            input.focus();
            input.setSelectionRange(input.value.length, input.value.length);
        }
    }
}

/** 日志等级颜色 */
function mcpLogLevelColor(level) {
    const colors = { error: '#cf222e', warn: '#bf8700', info: '#1a7f37', debug: '#888' };
    return colors[level] || '#888';
}

/** 日志时间格式化（秒级时间戳 → HH:MM:SS） */
function mcpFormatLogTime(ts) {
    if (!ts) return '';
    const d = new Date(ts * 1000);
    const p = (n) => String(n).padStart(2, '0');
    return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

// ── 运行日志按需加载（默认关闭：不随详情返回，点击后经 mcp_logs 拉取） ──

/** 切换日志展开状态：展开时拉取，收起时清空数据（轻交互，数据获取在后端） */
async function mcpToggleLogs() {
    if (mcpLogsExpanded) {
        mcpLogsExpanded = false;
        mcpLogsData = [];
        const pane = document.getElementById('mcp-logs-pane');
        const btn = document.getElementById('mcp-logs-toggle');
        if (pane) pane.style.display = 'none';
        if (btn) btn.textContent = '加载运行日志';
        return;
    }
    mcpLogsExpanded = true;
    const pane = document.getElementById('mcp-logs-pane');
    const btn = document.getElementById('mcp-logs-toggle');
    if (pane) { pane.style.display = 'block'; pane.innerHTML = '<div style="color:var(--t3,#888);font-size:12px;">加载中…</div>'; }
    if (btn) btn.textContent = '收起日志';
    await mcpLoadLogs();
}

/** 拉取运行日志（展开期间由详情轮询/重建调用刷新；失败时展示错误行） */
async function mcpLoadLogs() {
    if (!mcpCurrent || !window.__mdgoMcp) return;
    try {
        mcpLogsData = (await window.__mdgoMcp.mcpLogs(mcpDirPath, mcpCurrent.name)) || [];
    } catch (e) {
        mcpLogsData = [{
            level: 'error',
            message: '加载运行日志失败: ' + (e && e.message ? e.message : e),
            ts: Math.floor(Date.now() / 1000),
        }];
    }
    const pane = document.getElementById('mcp-logs-pane');
    if (pane) pane.innerHTML = mcpRenderLogsInner();
}

/** 渲染日志列表 HTML（最新在前；连接事件 / message 通知 / stderr） */
function mcpRenderLogsInner() {
    const items = (mcpLogsData || []).slice().reverse().map(l =>
        `<div style="display:flex;gap:6px;font-size:11px;line-height:1.6;font-family:ui-monospace,Consolas,monospace;">
                    <span style="color:${mcpLogLevelColor(l.level)};flex-shrink:0;">[${escapeHtml(l.level || 'info')}]</span>
                    <span style="color:var(--t3,#888);flex-shrink:0;">${mcpFormatLogTime(l.ts)}</span>
                    <span style="word-break:break-all;">${escapeHtml(l.message || '')}</span>
                </div>`).join('') || '<div class="center-div" style="color:var(--t3,#888);font-size:0.75rem;">暂无日志</div>';
    return `<div style="display:flex;flex-direction:column;gap:2px;max-height:16rem;overflow:auto;">${items}</div>`;
}

/** 新增/编辑服务器模态 */
/** 打开新增/编辑服务器（右侧表单，仿 skill 编辑视图） */
function mcpOpenEdit(name) {
    const existing = name ? (mcpAllList.find(s => s.name === name) || null) : null;
    if (existing) {
        window.__mdgoMcp.mcpGet(mcpDirPath, existing.name)
            .then(server => mcpRenderEdit(server))
            .catch(() => mcpRenderEdit(null));
    } else {
        mcpRenderEdit(null);
    }
}

/** 渲染右侧编辑视图：JSON 配置编辑器（主体）+ 启用/测试（第二部分） */
function mcpRenderEdit(server) {
    document.getElementById('mcp-detail-view').style.display = 'none';
    const view = document.getElementById('mcp-edit-view');
    if (!view) return;
    view.style.display = 'block';
    // 重新渲染前释放上一个 Monaco 实例（容器将被 innerHTML 重写，防泄漏）
    mcpDisposeJsonEditor();
    mcpEditMode = 'form';
    mcpEditServer = server || null;
    const cfg = (server && server.config) || {};
    const name = server ? server.name : '';
    // 新增服务器默认不启用（保存后不自动连接，需用户手动连接）；编辑已有服务器保留原配置
    const enabled = server ? cfg.enabled !== false : false;
    const transport = (cfg.url && cfg.url.trim()) ? 'http' : 'stdio';
    view.innerHTML = `
                <div class="skill-edit-wrap">
                    <div class="skill-edit-header">
                        <div class="skill-edit-title">${server ? '编辑服务器' : '新增服务器'}</div>
                        <button class="btn" onclick="mcpBackToDetail()">取消</button>
                        <button class="btn btn-primary" onclick="mcpSaveEdit()">保存</button>
                    </div>
                    <div style="display:flex;gap:0.375rem;margin-bottom:0.75rem;">
                        <button class="btn btn-sm ${mcpEditMode === 'form' ? 'btn-primary' : ''}" id="mcp-mode-form-btn" onclick="mcpSetEditMode('form')">表单</button>
                        <button class="btn btn-sm ${mcpEditMode === 'json' ? 'btn-primary' : ''}" id="mcp-mode-json-btn" onclick="mcpSetEditMode('json')">JSON</button>
                    </div>
                    <div id="mcp-form-pane" style="display:flex;flex-direction:column;gap:0.75rem;">
                        <div class="skill-form-grid">
                            <div class="skill-form-field">
                                <label>名称</label>
                                <input type="text" id="mcp-f-name" value="${escapeHtml(name)}" placeholder="例如 everything" autocomplete="off">
                            </div>
                            <div class="skill-form-field">
                                <label>传输类型</label>
                                <select id="mcp-f-transport" onchange="mcpSyncTransportFields()">
                                    <option value="stdio" ${transport === 'stdio' ? 'selected' : ''}>stdio（本地命令）</option>
                                    <option value="http" ${transport === 'http' ? 'selected' : ''}>http / sse（URL）</option>
                                </select>
                            </div>
                            <div class="skill-form-field" id="mcp-f-row-command">
                                <label>命令</label>
                                <input type="text" id="mcp-f-command" value="${escapeHtml(cfg.command || '')}" placeholder="例如 npx" autocomplete="off">
                            </div>
                            <div class="skill-form-field" id="mcp-f-row-args">
                                <label>参数（每行一个）</label>
                                <textarea id="mcp-f-args" placeholder="-y&#10;@modelcontextprotocol/server-everything">${escapeHtml((cfg.args || []).join('\n'))}</textarea>
                            </div>
                            <div class="skill-form-field" id="mcp-f-row-url" style="display:none;">
                                <label>URL</label>
                                <input type="text" id="mcp-f-url" value="${escapeHtml(cfg.url || '')}" placeholder="https://example.com/mcp" autocomplete="off">
                            </div>
                            <div class="skill-form-field" id="mcp-f-row-headers" style="display:none;">
                                <label>请求头（每行 KEY=VALUE）</label>
                                <textarea id="mcp-f-headers" placeholder="Authorization=Bearer xxx">${escapeHtml(Object.entries(cfg.headers || {}).map(([k, v]) => `${k}=${v}`).join('\n'))}</textarea>
                            </div>
                            <div class="skill-form-field full">
                                <label>环境变量（每行 KEY=VALUE）</label>
                                <textarea id="mcp-f-env" placeholder="例如 API_KEY=xxx">${escapeHtml(Object.entries(cfg.env || {}).map(([k, v]) => `${k}=${v}`).join('\n'))}</textarea>
                            </div>
                            <div class="skill-form-field" style="flex-direction: row;">
                                <input type="checkbox" id="mcp-f-enabled" ${enabled ? 'checked' : ''} style="width:1rem;">
                                <label class="center-div" style="justify-content:flex-start;">启用（保存后自动连接）</label>
                            </div>
                        </div>
                    </div>
                    <div id="mcp-json-pane" style="display:none;">
                        <label style="font-size:0.75rem;color:var(--color-text-secondary);font-weight:600;display:block;margin-bottom:0.375rem;">配置（.mcp.json 官方格式，JSON Schema 校验）</label>
                        <div id="mcp-json-editor" style="width:100%;height:22rem;border:1px solid var(--color-border,#333);border-radius:var(--radius-md);overflow:hidden;"></div>
                    </div>
                    <div style="display:flex;align-items:center;gap:0.75rem;flex-wrap:wrap;margin-top:0.75rem;">
                        <button class="btn" id="mcp-f-test-btn" type="button">测试连接</button>
                        <span id="mcp-f-test-rtn" style="font-size:12px;color:var(--color-orange);display:none;"></span>
                    </div>
                    <div style="font-size:11px;color:var(--t3,#888);margin-top:0.5rem;">⚠ 配置将运行本地命令，请确认命令来源可信；配置写入 .mdgo/mcp.json</div>
                </div>`;
    mcpSyncTransportFields();
    const testBtn = document.getElementById('mcp-f-test-btn');
    if (testBtn) {
        testBtn.addEventListener('click', async (e) => {
            const btn = e.target;
            const rtn = document.getElementById('mcp-f-test-rtn');
            const parsed = mcpGetEditConfig(false);
            if (!parsed) return;
            btn.disabled = true; btn.textContent = '测试中...';
            if (rtn) rtn.style.display = 'none';
            try {
                const count = await window.__mdgoMcp.mcpTest(parsed.config);
                if (rtn) { rtn.textContent = `✓ 连接成功，发现 ${count} 个工具`; rtn.style.display = 'inline'; }
                showNotification('✓ MCP 连接成功', 'success');
            } catch (err) {
                if (rtn) { rtn.textContent = '✗ ' + (err && err.message ? err.message : err); rtn.style.display = 'inline'; }
                showNotification('✗ MCP 连接失败: ' + (err && err.message ? err.message : err), 'error');
            } finally {
                btn.disabled = false; btn.textContent = '测试连接';
            }
        });
    }
}

/** 传输类型联动：stdio 显示命令/参数，http/sse 显示 URL/请求头 */
function mcpSyncTransportFields() {
    const sel = document.getElementById('mcp-f-transport');
    if (!sel) return;
    const isStdio = sel.value === 'stdio';
    const rows = {
        command: document.getElementById('mcp-f-row-command'),
        args: document.getElementById('mcp-f-row-args'),
        url: document.getElementById('mcp-f-row-url'),
        headers: document.getElementById('mcp-f-row-headers'),
    };
    if (rows.command) rows.command.style.display = isStdio ? '' : 'none';
    if (rows.args) rows.args.style.display = isStdio ? '' : 'none';
    if (rows.url) rows.url.style.display = isStdio ? 'none' : '';
    if (rows.headers) rows.headers.style.display = isStdio ? 'none' : '';
}

/** 表单 → 配置对象（strict=true 时校验必填） */
function mcpCollectFormConfig(strict) {
    const name = (document.getElementById('mcp-f-name').value || '').trim();
    const transport = document.getElementById('mcp-f-transport').value;
    const enabled = document.getElementById('mcp-f-enabled').checked;
    const config = {
        command: '',
        args: [],
        env: mcpParseKeyValueLines(document.getElementById('mcp-f-env').value),
        headers: mcpParseKeyValueLines(document.getElementById('mcp-f-headers').value),
        url: null,
        enabled,
    };
    if (transport === 'stdio') {
        config.command = (document.getElementById('mcp-f-command').value || '').trim();
        config.args = document.getElementById('mcp-f-args').value.split('\n').map(s => s.trim()).filter(Boolean);
    } else {
        config.url = (document.getElementById('mcp-f-url').value || '').trim() || null;
    }
    if (strict) {
        if (!name) { showNotification('✗ 请填写服务器名称', 'warning'); return null; }
        if (transport === 'stdio' && !config.command) { showNotification('✗ 请填写命令（stdio 传输）', 'warning'); return null; }
        if (transport !== 'stdio' && !config.url) { showNotification('✗ 请填写 URL（http/sse 传输）', 'warning'); return null; }
    }
    return { name: name || 'everything', config };
}

/** 按行解析 KEY=VALUE 文本 */
function mcpParseKeyValueLines(text) {
    const out = {};
    (text || '').split('\n').map(s => s.trim()).filter(Boolean).forEach(line => {
        const idx = line.indexOf('=');
        if (idx > 0) out[line.slice(0, idx).trim()] = line.slice(idx + 1).trim();
    });
    return out;
}

/** 配置对象 → .mcp.json 格式（表单切 JSON 用） */
function mcpConfigToJson(config) {
    const s = {
        command: config.command || '',
        args: Array.isArray(config.args) ? config.args : [],
        env: config.env || {},
        enabled: config.enabled !== false,
    };
    if (config.url && config.url.trim()) {
        s.url = config.url;
        if (config.headers && Object.keys(config.headers).length) s.headers = config.headers;
        delete s.command;
        delete s.args;
    }
    return s;
}

/** 切换编辑模式（表单 ↔ JSON），双向同步 */
function mcpSetEditMode(mode) {
    if (mode === mcpEditMode) return;
    if (mode === 'form') {
        // JSON → 表单：解析成功才切换（失败留在 JSON 并已提示）
        const parsed = mcpParseEditJson();
        if (!parsed) return;
        mcpEditMode = 'form';
        mcpFillForm(parsed);
        mcpRefreshEditViews();
    } else {
        // 表单 → JSON：宽松收集（不做必填校验）
        const collected = mcpCollectFormConfig(false);
        if (!collected) return;
        mcpEditMode = 'json';
        mcpRefreshEditViews(); // 先显示 JSON 面板（Monaco 需可见容器才有尺寸）
        const text = JSON.stringify({ mcpServers: { [collected.name || 'everything']: mcpConfigToJson(collected.config) } }, null, 2);
        mcpEnsureJsonEditor(text);
    }
}

/** 刷新双模式视图（tab 高亮 + 面板显隐） */
function mcpRefreshEditViews() {
    const formBtn = document.getElementById('mcp-mode-form-btn');
    const jsonBtn = document.getElementById('mcp-mode-json-btn');
    const formPane = document.getElementById('mcp-form-pane');
    const jsonPane = document.getElementById('mcp-json-pane');
    if (formBtn) formBtn.classList.toggle('btn-primary', mcpEditMode === 'form');
    if (jsonBtn) jsonBtn.classList.toggle('btn-primary', mcpEditMode === 'json');
    if (formPane) formPane.style.display = mcpEditMode === 'form' ? '' : 'none';
    if (jsonPane) jsonPane.style.display = mcpEditMode === 'json' ? '' : 'none';
}

// ── JSON 模式 Monaco 编辑器生命周期 ──

/** 释放 Monaco 编辑器实例（防泄漏；退出编辑页 / 重新渲染前调用） */
function mcpDisposeJsonEditor() {
    if (mcpJsonEditor) {
        try {
            mcpJsonEditor.dispose();
        } catch (e) {
            console.warn('[MCP] dispose Monaco 编辑器失败:', e);
        }
        mcpJsonEditor = null;
    }
}

/** 读取 JSON 编辑器内容（Monaco；无实例返回空串） */
function mcpJsonGetValue() {
    return mcpJsonEditor ? mcpJsonEditor.getValue() : '';
}

/** 确保 JSON Monaco 编辑器存在并写入内容（首次创建，异步等 Monaco 就绪） */
function mcpEnsureJsonEditor(value) {
    if (mcpJsonEditor) {
        mcpJsonEditor.setValue(value);
        mcpJsonEditor.layout();
        return;
    }
    const container = document.getElementById('mcp-json-editor');
    if (!container) return;
    initMonacoEditor().then(() => {
        if (mcpJsonEditor || !document.getElementById('mcp-json-editor')) return;
        mcpJsonEditor = monaco.editor.create(container, {
            value: value,
            language: 'json',
            theme: 'vs-dark',
            automaticLayout: true,
            minimap: { enabled: false },
            fontSize: 13,
            wordWrap: 'on',
            scrollBeyondLastLine: false,
            lineNumbersMinChars: 3,
            fixedOverflowWidgets: true,
        });
    });
}

/** JSON → 表单回填 */
function mcpFillForm(parsed) {
    const nameInput = document.getElementById('mcp-f-name');
    if (nameInput) nameInput.value = parsed.name;
    const transportSel = document.getElementById('mcp-f-transport');
    if (transportSel) {
        transportSel.value = (parsed.config.url && parsed.config.url.trim()) ? 'http' : 'stdio';
        mcpSyncTransportFields();
    }
    const set = (id, val) => { const el = document.getElementById(id); if (el) el.value = val; };
    set('mcp-f-command', parsed.config.command || '');
    set('mcp-f-args', (parsed.config.args || []).join('\n'));
    set('mcp-f-url', parsed.config.url || '');
    set('mcp-f-env', Object.entries(parsed.config.env || {}).map(([k, v]) => `${k}=${v}`).join('\n'));
    set('mcp-f-headers', Object.entries(parsed.config.headers || {}).map(([k, v]) => `${k}=${v}`).join('\n'));
    const enabledEl = document.getElementById('mcp-f-enabled');
    if (enabledEl) enabledEl.checked = parsed.config.enabled !== false;
}

/** 按当前模式获取编辑配置（strict=true 时校验必填） */
function mcpGetEditConfig(strict) {
    if (mcpEditMode === 'json') {
        return mcpParseEditJson();
    }
    return mcpCollectFormConfig(strict);
}

/** 取消编辑：返回详情或空态 */
function mcpBackToDetail() {
    // 退出编辑页：释放 Monaco 编辑器实例
    mcpDisposeJsonEditor();
    mcpEditServer = null;
    const view = document.getElementById('mcp-edit-view');
    if (view) view.style.display = 'none';
    if (mcpCurrent) {
        mcpRenderDetail(mcpCurrent);
    } else {
        const detail = document.getElementById('mcp-detail-view');
        if (detail) detail.style.display = 'none';
    }
}

/** 保存服务器配置（按当前模式取配置 → 写 .mdgo/mcp.json + 刷新列表） */
async function mcpSaveEdit() {
    const parsed = mcpGetEditConfig(true);
    if (!parsed) return;
    try {
        await window.__mdgoMcp.mcpUpsert(mcpDirPath, parsed.name, parsed.config);
        // 保存成功退出编辑页：释放 Monaco 编辑器实例 + 隐藏编辑视图
        mcpDisposeJsonEditor();
        mcpEditServer = null;
        showNotification('✓ 服务器配置已保存', 'success');
        const editView = document.getElementById('mcp-edit-view');
        if (editView) editView.style.display = 'none';
        await mcpLoadList();
        mcpSelectServer(parsed.name);
    } catch (err) {
        showNotification('✗ 保存失败: ' + (err && err.message ? err.message : err), 'error');
    }
}

/**
 * 解析 JSON 编辑器内容（.mcp.json 官方格式）并执行 JSON Schema（draft-07）标准校验。
 * 校验通过返回 { name, config }；失败显示错误（JSON Pointer 定位）并返回 null。
 */
function mcpParseEditJson() {
    const jsonText = mcpJsonGetValue();
    let parsed;
    try {
        parsed = JSON.parse(jsonText);
    } catch (e) {
        showNotification('✗ JSON 格式错误: ' + e.message, 'warning');
        return null;
    }
    // JSON Schema 标准校验（MCP_CONFIG_JSON_SCHEMA，.mcp.json 格式）
    const errors = mcpValidateConfigJson(parsed);
    if (errors.length > 0) {
        const lines = errors.slice(0, 5).map(e => `[${e.pointer || '/'}] ${e.message}`);
        showNotification('✗ 配置不符合 JSON Schema：\n' + lines.join('\n'), 'warning');
        return null;
    }
    const servers = parsed.mcpServers;
    const name = String(Object.keys(servers)[0]).trim();
    const raw = servers[Object.keys(servers)[0]];
    const enabledEl = document.getElementById('mcp-f-enabled');
    const config = {
        command: raw.command || '',
        args: Array.isArray(raw.args) ? raw.args.map(String) : [],
        env: (raw.env && typeof raw.env === 'object' && !Array.isArray(raw.env))
            ? Object.fromEntries(Object.entries(raw.env).map(([k, v]) => [k, String(v)]))
            : {},
        headers: (raw.headers && typeof raw.headers === 'object' && !Array.isArray(raw.headers))
            ? Object.fromEntries(Object.entries(raw.headers).map(([k, v]) => [k, String(v)]))
            : {},
        enabled: typeof raw.enabled === 'boolean' ? raw.enabled : (enabledEl ? enabledEl.checked : true),
        url: (raw.url && typeof raw.url === 'string') ? raw.url : null,
    };
    return { name, config };
}

async function mcpConnect(name) {
    try {
        await window.__mdgoMcp.mcpConnect(mcpDirPath, name);
        showNotification('✓ 已连接', 'success');
    } catch (e) {
        showNotification('✗ 连接失败: ' + (e && e.message ? e.message : e), 'error');
    }
    await mcpLoadList();
}
async function mcpDisconnect(name) {
    try {
        await window.__mdgoMcp.mcpDisconnect(mcpDirPath, name);
        showNotification('已断开', 'info', 1000);
    } catch (e) {
        showNotification('✗ 断开失败: ' + (e && e.message ? e.message : e), 'error');
    }
    await mcpLoadList();
}
async function mcpRestart(name) {
    try {
        await window.__mdgoMcp.mcpRestart(mcpDirPath, name);
        showNotification('✓ 已重启', 'success');
    } catch (e) {
        showNotification('✗ 重启失败: ' + (e && e.message ? e.message : e), 'error');
    }
    await mcpLoadList();
}
async function mcpDeleteConfirm(name) {
    if (!window.confirm(`确定删除服务器「${name}」？`)) return;
    try {
        await window.__mdgoMcp.mcpDelete(mcpDirPath, name);
        showNotification('✓ 已删除', 'success');
        mcpCurrent = null;
        mcpLogsExpanded = false;
        mcpLogsData = [];
        const view = document.getElementById('mcp-detail-view');
        if (view) view.style.display = 'none';
    } catch (e) {
        showNotification('✗ 删除失败: ' + (e && e.message ? e.message : e), 'error');
    }
    await mcpLoadList();
}
function mcpHandleSearchInput() {
    const input = document.getElementById('mcp-search-input');
    mcpSearchTerm = input ? input.value : '';
    const clear = document.getElementById('mcp-search-clear');
    if (clear) clear.style.display = mcpSearchTerm ? 'inline-block' : 'none';
    mcpRenderList();
}
function mcpHandleSearchKeydown(e) {
    if (e.key === 'Enter') mcpRenderList();
}
function mcpClearSearch() {
    mcpSearchTerm = '';
    const input = document.getElementById('mcp-search-input');
    if (input) input.value = '';
    const clear = document.getElementById('mcp-search-clear');
    if (clear) clear.style.display = 'none';
    mcpRenderList();
}
function mcpStatusChange(status) {
    mcpStatusFilter = status;
    document.querySelectorAll('#mcp-status-chips .skill-chip').forEach(chip => {
        chip.classList.toggle('active', chip.dataset.scope === status);
    });
    mcpRenderList();
}

/**
 * 清理 MCP 模块残留（界面切换离开时由主页面 cleanupData 调用）：
 * 停止详情轮询、释放 Monaco 编辑器实例、重置编辑/详情状态并清空动态 DOM，
 * 避免定时器空转、Monaco 实例泄漏与下次进入时残留旧内容。
 */
function mcpCleanup() {
    mcpStopPolling();
    mcpDisposeJsonEditor();
    mcpEditServer = null;
    mcpCurrent = null;
    mcpSearchTerm = '';
    mcpStatusFilter = '';
    mcpToolSearchTerm = '';
    mcpLogsExpanded = false;
    mcpLogsData = [];
    // 清空动态 DOM（下次进入 openMcpManager 时重新渲染）
    const listEl = document.getElementById('mcp-list');
    if (listEl) listEl.innerHTML = '';
    const detailEl = document.getElementById('mcp-detail-view');
    if (detailEl) { detailEl.innerHTML = ''; detailEl.style.display = 'none'; }
    const editEl = document.getElementById('mcp-edit-view');
    if (editEl) { editEl.innerHTML = ''; editEl.style.display = 'none'; }
    // 搜索框与状态筛选 chips 复位
    const searchInput = document.getElementById('mcp-search-input');
    if (searchInput) searchInput.value = '';
    const clearBtn = document.getElementById('mcp-search-clear');
    if (clearBtn) clearBtn.style.display = 'none';
    document.querySelectorAll('#mcp-status-chips .skill-chip').forEach(chip => {
        chip.classList.toggle('active', chip.dataset.scope === '');
    });
}
