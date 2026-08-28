/**
 * ===== Prompt 管理模块（css_js/modules/prompt.js） =====
 *
 * 【职责】Prompt 模板管理面板：列表 / 搜索 / 作用域筛选 / 详情 / 新建 / 编辑 / 删除。
 * 【入口】主页面视图路由 type='prompt' → 调用全局函数 openPromptManager() 打开面板。
 * 【对外暴露】prompt 前缀全局函数，供面板 HTML 内联 onclick 调用。
 *
 * 三层体系（与 Skill 一致）：
 * - system：resources/prompt/*.md（只读，打包后走资源目录）
 * - global：用户数据目录 prompts.db（跨项目共享）
 * - project：{dir}/.mdgo/mdgo.db 的 prompts 表（随项目走）
 *
 * 【依赖的全局服务】（加载顺序：主脚本 → 本模块）
 *   - window.__mdgoPrompt  后端适配层（Tauri 命令封装）
 *   - isTauriVisit() / switchToView() / showNotification() / showConfirmModal()
 *   - getRootHandle()/getDirPath() / escapeHtml()
 */
// ====== Prompt 管理（prompt 前缀隔离） ======
const PROMPT_SCOPE_NAMES = { system: '系统', global: '全局', project: '项目' };

let promptDirPath = '';          // 当前根目录路径（打开面板时刷新）
let promptAllList = [];          // 后端返回的全量 prompt（三层合并）
let promptFilteredList = [];     // 前端筛选后的列表
let promptCurrent = null;        // 当前选中 prompt（完整对象）
let promptScopeFilter = '';      // 作用域筛选：'' | system | global | project
let promptSearchTerm = '';       // 搜索关键词
let promptEditMode = 'create';   // create | edit
let promptEditKey = null;        // 编辑目标 {scope, id}
let promptChangedSubscribed = false; // prompt:changed 事件只订阅一次
let promptContentEditor = null;     // Prompt 内容 Monaco 编辑器实例（编辑视图生命周期内有效）

function promptGetDirPath() {
    const handle = getRootHandle();
    return handle ? getDirPath(handle) : '';
}

function promptFormatTime(ms) {
    if (!ms) return '-';
    try {
        const d = new Date(ms);
        const p = (n) => String(n).padStart(2, '0');
        return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
    } catch {
        return String(ms);
    }
}

function promptScopeName(scope) {
    return PROMPT_SCOPE_NAMES[scope] || scope;
}

/** 打开 Prompt 管理面板（Tauri 专用） */
async function openPromptManager() {
    if (!isTauriVisit() || !window.__mdgoPrompt) {
        showNotification('Prompt 管理仅在桌面版（Tauri）可用', 'error');
        return;
    }
    const dirPath = promptGetDirPath();
    if (!dirPath) {
        showNotification('请先打开根目录', 'error');
        return;
    }
    await switchToView(document.getElementById('prompt-container'), 'flex');
    promptDirPath = dirPath;
    promptSubscribeChanged();
    await promptLoadList();
}

/** 订阅变更事件（只订阅一次；写操作后自动刷新列表） */
async function promptSubscribeChanged() {
    if (promptChangedSubscribed || !window.__mdgoPrompt) return;
    promptChangedSubscribed = true;
    try {
        await window.__mdgoPrompt.promptOnChanged(() => {
            if (promptDirPath) promptLoadList();
        });
    } catch (e) {
        console.warn('[PromptPanel] prompt:changed 订阅失败:', e);
    }
}

/** 从后端拉取全量 prompt 并渲染 */
async function promptLoadList() {
    try {
        promptAllList = await window.__mdgoPrompt.promptList(promptDirPath, null);
    } catch (e) {
        showNotification('加载 Prompt 失败: ' + e, 'error');
        return;
    }
    // 保留选中态
    if (promptCurrent) {
        const cur = promptAllList.find(p => p.scope === promptCurrent.scope && p.id === promptCurrent.id);
        if (cur) {
            promptCurrent = cur;
            promptRenderList();
            return;
        }
    }
    promptRenderList();
    promptCurrent = null;
    promptShowEmpty();
}

function promptShowEmpty() {
    const first = promptAllList[0];
    if (first) {
        promptSelectPrompt(first.scope, first.id);
    }
}

/** 渲染左侧列表（作用域 + 关键词双重过滤） */
function promptRenderList() {
    const kw = promptSearchTerm.trim().toLowerCase();
    promptFilteredList = promptAllList.filter(p => {
        if (promptScopeFilter && p.scope !== promptScopeFilter) return false;
        if (!kw) return true;
        const searchText = (p.id + ' ' + p.name + ' ' + p.prompt).toLowerCase();
        return searchText.includes(kw);
    });
    const listEl = document.getElementById('prompt-list');
    if (!listEl) return;
    if (!promptFilteredList.length) {
        listEl.innerHTML = `<div class="skill-empty-list">${promptAllList.length ? '没有匹配的 Prompt' : '暂无 Prompt，点击下方按钮新建'}</div>`;
        return;
    }
    listEl.innerHTML = promptFilteredList.map(p => `
                <div class="skill-list-item ${promptCurrent && promptCurrent.scope === p.scope && promptCurrent.id === p.id ? 'active' : ''}"
                    onclick="promptSelectPrompt('${p.scope}','${p.id}')">
                    <div class="skill-item-avatar">
                        <div class="skill-icon-box ${p.scope}">
                            <svg class="skill-icon-svg ${p.scope}" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 10h.01M12 10h.01M16 10h.01M9 16H5a2 2 0 01-2-2V6a2 2 0 012-2h14a2 2 0 012 2v8a2 2 0 01-2 2h-5l-5 5v-5z"></path>
                            </svg>
                        </div>
                    </div>
                    <div class="skill-item-info">
                        <div class="skill-item-name">${escapeHtml(p.name)}</div>
                        <div class="skill-item-meta"><span class="skill-item-tag ${p.scope}">${promptScopeName(p.scope)}</span><span>${escapeHtml(p.id)}</span></div>
                    </div>
                </div>`).join('');
}

/** 选中 prompt 并渲染详情 */
function promptSelectPrompt(scope, id) {
    const item = promptAllList.find(p => p.scope === scope && p.id === id);
    if (!item) return;
    promptCurrent = item;
    promptRenderList();
    promptRenderDetail(item);
}

function promptRenderDetail(p) {
    document.getElementById('prompt-edit-view').style.display = 'none';
    const view = document.getElementById('prompt-detail-view');
    view.style.display = 'block';

    const writable = p.scope !== 'system';
    const actions = writable
        ? `<button class="btn btn-sm btn-primary" onclick="promptEditPrompt('${p.scope}','${p.id}')">编辑</button>
           <button class="btn btn-sm btn-danger" onclick="promptDeletePrompt('${p.scope}','${p.id}')">删除</button>`
        : '';

    const metaItems = [
        ['作用域', promptScopeName(p.scope)],
        ['创建时间', promptFormatTime(p.created_at)],
        ['更新时间', promptFormatTime(p.updated_at)],
        ['ID', p.id],
    ].map(([label, value]) => `
                <span class="skill-meta-item"><span class="skill-meta-label">${label}</span><span class="skill-meta-value">${escapeHtml(String(value))}</span></span>`).join('');

    view.innerHTML = `
                <div class="skill-detail-wrap">
                    <div class="skill-detail-header">
                        <div class="skill-detail-title-box">
                            <div class="skill-detail-title">${escapeHtml(p.name)}
                                <span class="skill-badge scope-${p.scope}">${promptScopeName(p.scope)}</span>
                                ${writable ? '' : '<span class="skill-badge disabled">只读</span>'}
                            </div>
                            <div class="skill-detail-subtitle">ID: ${escapeHtml(p.id)}</div>
                        </div>
                        <div class="skill-detail-actions">${actions}</div>
                    </div>
                    <div class="skill-detail-meta">${metaItems}</div>
                    <div class="skill-detail-section">
                        <div class="skill-detail-section-title">Prompt 内容</div>
                        <div class="skill-detail-body"><div class="markdown-body" style="font-size: 0.8125rem;">${markedMd(p.prompt || '（空）')}</div></div>
                    </div>
                </div>`;
}

/** 新建（默认 project 作用域） */
function promptCreateNew() {
    const dirPath = promptGetDirPath();
    if (!dirPath) {
        showNotification('请先打开根目录', 'error');
        return;
    }
    promptEditMode = 'create';
    promptEditKey = null;
    promptRenderEdit(null);
}

/** 编辑已有 prompt（仅用户级） */
function promptEditPrompt(scope, id) {
    const item = promptAllList.find(p => p.scope === scope && p.id === id);
    if (!item) {
        showNotification('Prompt 不存在，请刷新列表', 'error');
        return;
    }
    if (item.scope === 'system') {
        showNotification('系统内置 Prompt 不可编辑', 'error');
        return;
    }
    promptEditMode = 'edit';
    promptEditKey = { scope, id };
    promptRenderEdit(item);
}

function promptRenderEdit(item) {
    document.getElementById('prompt-detail-view').style.display = 'none';
    const view = document.getElementById('prompt-edit-view');
    view.style.display = 'block';

    const isEdit = promptEditMode === 'edit';
    const scope = item ? item.scope : 'project';
    view.innerHTML = `
                <div class="skill-edit-wrap">
                    <div class="skill-edit-header">
                        <div class="skill-edit-title">${isEdit ? '编辑 Prompt' : '新建 Prompt'}</div>
                        <button class="btn" onclick="promptBackToDetail()">取消</button>
                        <button class="btn btn-primary" onclick="promptSavePrompt()">保存</button>
                    </div>
                    <div class="skill-form-grid">
                        <div class="skill-form-field">
                            <label>名称</label>
                            <input type="text" id="prompt-f-name" value="${escapeHtml(item ? item.name : '')}" placeholder="Prompt 名称" autocomplete="off">
                        </div>
                        <div class="skill-form-field">
                            <label>作用域</label>
                            <select id="prompt-f-scope" ${isEdit ? 'disabled' : ''}>
                                <option value="global" ${scope === 'global' ? 'selected' : ''}>全局（跨项目共享）</option>
                                <option value="project" ${scope === 'project' ? 'selected' : ''}>项目（当前目录）</option>
                            </select>
                        </div>
                        <div class="skill-form-field full">
                            <label>Prompt 内容</label>
                            <div id="prompt-f-content" class="skill-body-editor" style="height: 24rem; border: 1px solid var(--color-border); border-radius: var(--radius-md); overflow: hidden;"></div>
                        </div>
                    </div>
                </div>`;
    // Monaco 编辑器初始化（独立实例，不占用文件编辑的 currentEditor）
    promptInitContentEditor(item ? item.prompt || '' : '');
}

/** 初始化 Prompt 内容 Monaco 编辑器（创建前先销毁旧实例，防止泄漏） */
async function promptInitContentEditor(value) {
    promptDestroyContentEditor();
    const container = document.getElementById('prompt-f-content');
    if (!container) return;
    if (typeof initMonacoEditor !== 'function') return;
    try {
        await initMonacoEditor();
        const editor = createMonacoEditor(container, {
            value: value || '',
            language: 'markdown',
            wordWrap: 'on',
            minimap: { enabled: false },
            lineNumbers: 'off',   // Prompt 内容不显示行号
            skipCtrlS: true, // 嵌入编辑器：Ctrl+S 不保存文件（保存 Prompt 走页面按钮）
        });
        promptContentEditor = editor;
    } catch (e) {
        console.warn('[PromptPanel] 内容编辑器初始化失败:', e);
    }
}

/** 销毁 Prompt 内容编辑器实例（取消/保存/切换/清理时调用） */
function promptDestroyContentEditor() {
    if (promptContentEditor) {
        try { disposeEditor(promptContentEditor); } catch (e) { /* ignore */ }
        promptContentEditor = null;
    }
    const container = document.getElementById('prompt-f-content');
    if (container) container.innerHTML = '';
}

/** 读取 Prompt 内容（Monaco 实例优先，回退容器文本） */
function promptGetContentValue() {
    if (promptContentEditor && typeof promptContentEditor.getValue === 'function') {
        try {
            return promptContentEditor.getValue() || '';
        } catch (e) { /* editor 已销毁等异常，回退容器 */ }
    }
    const container = document.getElementById('prompt-f-content');
    return container && container.textContent ? container.textContent : '';
}

/** 返回详情视图 */
function promptBackToDetail() {
    promptDestroyContentEditor();
    if (promptCurrent) {
        promptRenderDetail(promptCurrent);
    } else {
        promptShowEmpty();
    }
}

/** 保存（创建/更新） */
async function promptSavePrompt() {
    if (!window.__mdgoPrompt) {
        showNotification('Prompt 管理仅在桌面版（Tauri）可用', 'error');
        return;
    }
    const name = (document.getElementById('prompt-f-name')?.value || '').trim();
    const content = promptGetContentValue().trim();
    if (!name) { showNotification('请填写名称', 'error'); return; }
    if (!content) { showNotification('请填写 Prompt 内容', 'error'); return; }
    const scope = promptEditMode === 'edit'
        ? (promptEditKey ? promptEditKey.scope : 'project')
        : (document.getElementById('prompt-f-scope')?.value || 'project');
    try {
        if (promptEditMode === 'edit' && promptEditKey) {
            await window.__mdgoPrompt.promptUpdate(promptDirPath, scope, promptEditKey.id, name, content);
            showNotification('Prompt 已更新');
        } else {
            await window.__mdgoPrompt.promptCreate(promptDirPath, scope, name, content);
            showNotification('Prompt 已创建');
        }
        promptEditMode = 'create';
        promptEditKey = null;
        promptDestroyContentEditor();
        await promptLoadList();
        if (promptCurrent) promptRenderDetail(promptCurrent);
    } catch (e) {
        showNotification('保存失败: ' + e, 'error');
    }
}

/** 删除（确认后执行；system 只读拒绝） */
function promptDeletePrompt(scope, id) {
    const item = promptAllList.find(p => p.scope === scope && p.id === id);
    if (!item) return;
    if (scope === 'system') {
        showNotification('系统内置 Prompt 不可删除', 'warning');
        return;
    }
    showConfirmModal('删除 Prompt', `确定删除 Prompt「${item.name}」吗？删除后不可恢复。`, async (ok) => {
        if (!ok) return;
        try {
            await window.__mdgoPrompt.promptDelete(promptDirPath, scope, id);
            showNotification('Prompt 已删除');
            promptCurrent = null;
            await promptLoadList();
            promptShowEmpty();
        } catch (e) {
            showNotification('删除失败: ' + e, 'error');
        }
    });
}

/** 搜索框输入（实时过滤 + 清空按钮显隐） */
function promptHandleSearchInput() {
    const input = document.getElementById('prompt-search-input');
    const clearBtn = document.getElementById('prompt-search-clear');
    if (!input) return;
    promptSearchTerm = input.value;
    if (clearBtn) clearBtn.style.display = promptSearchTerm ? 'flex' : 'none';
    promptRenderList();
}

/** 搜索框回车 */
function promptHandleSearchKeydown(e) {
    if (e.key === 'Enter') {
        promptSearchTerm = document.getElementById('prompt-search-input').value;
        promptRenderList();
    }
}

/** 清空搜索 */
function promptClearSearch() {
    promptSearchTerm = '';
    const input = document.getElementById('prompt-search-input');
    if (input) input.value = '';
    const clearBtn = document.getElementById('prompt-search-clear');
    if (clearBtn) clearBtn.style.display = 'none';
    promptRenderList();
}

/** 作用域筛选切换（chips 高亮同步） */
function promptScopeChange(value) {
    promptScopeFilter = value || '';
    document.querySelectorAll('#prompt-scope-chips .skill-chip').forEach(c => {
        c.classList.toggle('active', (c.dataset.scope || '') === promptScopeFilter);
    });
    promptRenderList();
}

/**
 * 清理 Prompt 模块残留（界面切换离开时由主页面 cleanupData 调用）
 */
function promptCleanup() {
    promptSearchTerm = '';
    promptScopeFilter = '';
    promptCurrent = null;
    promptEditMode = 'create';
    promptEditKey = null;
    promptAllList = [];
    promptFilteredList = [];
    // 销毁内容编辑器实例（离开页面时释放）
    promptDestroyContentEditor();
    const listEl = document.getElementById('prompt-list');
    if (listEl) listEl.innerHTML = '';
    const detailEl = document.getElementById('prompt-detail-view');
    if (detailEl) { detailEl.innerHTML = ''; detailEl.style.display = 'none'; }
    const editEl = document.getElementById('prompt-edit-view');
    if (editEl) { editEl.innerHTML = ''; editEl.style.display = 'none'; }
    const searchInput = document.getElementById('prompt-search-input');
    if (searchInput) searchInput.value = '';
    const clearBtn = document.getElementById('prompt-search-clear');
    if (clearBtn) clearBtn.style.display = 'none';
    document.querySelectorAll('#prompt-scope-chips .skill-chip').forEach(c => {
        c.classList.toggle('active', (c.dataset.scope || '') === '');
    });
}
