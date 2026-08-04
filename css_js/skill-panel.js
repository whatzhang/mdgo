// ====== 技能管理（Skill Manager，skill 前缀隔离） ======
// 依赖主页面全局：skillContainer / escapeHtml / showConfirmModal / showNotification
// getRootHandle / getDirPath / isTauriVisit / switchToView（加载顺序：本文件须在主页内联脚本之后）
// 工具白名单常量（与后端 core/skill.rs ALLOWED_TOOLS 一致）
const ALLOWED_SKILL_TOOLS = ['kb_search', 'code_lookup', 'read_file', 'list_files', 'git_status'];
const SKILL_SCOPE_NAMES = { system: '系统', global: '全局', project: '当前目录' };

let skillDirPath = '';          // 当前根目录路径（打开面板时刷新）
let skillAllList = [];          // 后端返回的全量技能（按 priority 排序）
let skillFilteredList = [];     // 前端筛选后的列表
let skillCurrent = null;        // 当前选中技能（完整对象，含 body）
let skillScopeFilter = '';      // 作用域筛选：'' | system | global | project
let skillSearchTerm = '';       // 搜索关键词
let skillEditMode = 'create';   // create | edit
let skillEditKey = null;        // 编辑目标 {scope, id}
let skillChangedSubscribed = false; // skill:changed 事件只订阅一次

function skillGetDirPath() {
    const handle = getRootHandle();
    return handle ? getDirPath(handle) : '';
}

function skillFormatTime(ms) {
    if (!ms) return '-';
    try {
        const d = new Date(ms);
        const p = (n) => String(n).padStart(2, '0');
        return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
    } catch {
        return String(ms);
    }
}

function skillScopeName(scope) {
    return SKILL_SCOPE_NAMES[scope] || scope;
}

/** 打开技能管理面板（Tauri 专用） */
async function openSkillManager() {
    if (!isTauriVisit() || !window.__mdgoSkill) {
        showNotification('技能管理仅在桌面版（Tauri）可用', 'error');
        return;
    }
    const dirPath = skillGetDirPath();
    if (!dirPath) {
        showNotification('请先打开根目录', 'error');
        return;
    }
    await switchToView(skillContainer, 'flex');
    skillDirPath = dirPath;
    skillSubscribeChanged();
    await skillLoadList();
}

/** 订阅注册表变更事件（只订阅一次；写操作/文件变更后自动刷新列表） */
async function skillSubscribeChanged() {
    if (skillChangedSubscribed || !window.__mdgoSkill) return;
    skillChangedSubscribed = true;
    try {
        await window.__mdgoSkill.skillOnChanged(() => {
            if (skillDirPath) skillLoadList();
        });
    } catch (e) {
        console.warn('[SkillPanel] skill:changed 订阅失败:', e);
    }
}

/** 从后端拉取全量技能并渲染 */
async function skillLoadList() {
    try {
        skillAllList = await window.__mdgoSkill.skillList(skillDirPath, null);
        skillAllList.sort((a, b) => (b.created_at || 0) - (a.created_at || 0));
    } catch (e) {
        showNotification('加载技能失败: ' + e, 'error');
        return;
    }
    // 保留选中态：列表刷新后若当前选中技能仍存在则沿用
    if (skillCurrent) {
        const cur = skillAllList.find(s => s.scope === skillCurrent.scope && s.id === skillCurrent.id);
        if (cur) {
            skillCurrent = cur;
            skillRenderList(); // 重新渲染以更新 active 状态
            return;
        }
    }
    skillRenderList();
    
    skillCurrent = null;
    skillShowEmpty();
}

function skillShowEmpty() {
    // 默认选中列表第一个；空列表时保持空白（由 skillRenderList 展示空态提示）
    const first = skillAllList[0];
    if (first) {
        skillSelectSkill(first.scope, first.id);
    }
}

/** 渲染左侧列表（作用域 + 关键词双重过滤） */
function skillRenderList() {
    const kw = skillSearchTerm.trim().toLowerCase();
    skillFilteredList = skillAllList.filter(s => {
        if (skillScopeFilter && s.scope !== skillScopeFilter) return false;
        if (!kw) return true;
        // 搜索范围：id + name + description + 触发关键词
        const keywords = (s.trigger_rules && s.trigger_rules.keywords) || [];
        const searchText = (s.id + ' ' + s.name + ' ' + s.description + ' ' + keywords.join(' ')).toLowerCase();
        return searchText.includes(kw);
    });
    const listEl = document.getElementById('skill-list');
    if (!listEl) return;
    if (!skillFilteredList.length) {
        listEl.innerHTML = `<div class="skill-empty-list">${skillAllList.length ? '没有匹配的技能' : '暂无技能，点击下方按钮新建'}</div>`;
        return;
    }
    listEl.innerHTML = skillFilteredList.map(s => `
        <div class="skill-list-item ${skillCurrent && skillCurrent.scope === s.scope && skillCurrent.id === s.id ? 'active' : ''}"
             onclick="skillSelectSkill('${s.scope}','${s.id}')">
            <div class="skill-item-avatar">
                <div class="skill-icon-box ${s.scope}">
                    <svg class="skill-icon-svg ${s.scope}" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 10V3L4 14h7v7l9-11h-7z"></path>
                    </svg>
                </div>
            </div>
            <div class="skill-item-info">
                <div class="skill-item-name">${escapeHtml(s.name)}</div>
                <div class="skill-item-meta"><span class="skill-item-tag ${s.enabled ? 'enabled' : 'disabled'}" id="skill-tag-${s.id}">${s.enabled ? '已启用' : '已停用'}</span><span>${escapeHtml(s.id)}</span></div>
            </div>
        </div>`).join('');
}

/** 选中技能并渲染详情 */
async function skillSelectSkill(scope, id) {
    const cached = skillAllList.find(s => s.scope === scope && s.id === id);
    try {
        // 详情需要完整正文，从后端实时获取
        const skill = await window.__mdgoSkill.skillGet(skillDirPath, scope, id);
        skillCurrent = skill;
        if (cached) {
            Object.assign(cached, skill);
        } else {
            skillAllList.push(skill);
        }
    } catch (e) {
        showNotification('加载技能详情失败: ' + e, 'error');
        return;
    }
    skillRenderList();
    skillRenderDetail(skillCurrent);
}

function skillRenderDetail(skill) {
    document.getElementById('skill-edit-view').style.display = 'none';
    const view = document.getElementById('skill-detail-view');
    view.style.display = 'block';

    const writable = skill.scope !== 'system';
    const keywords = (skill.trigger_rules && skill.trigger_rules.keywords) || [];
    const tools = skill.tools || [];
    const actions = [
        writable ? `<button class="btn btn-sm btn-primary" onclick="skillEditSkill('${skill.scope}','${skill.id}')">编辑</button>` : '',
        writable ? `<button class="btn btn-sm btn-warning" onclick="skillToggleEnabled('${skill.scope}','${skill.id}',${!skill.enabled})">${skill.enabled ? '停用' : '启用'}</button>` : '',
        writable ? `<button class="btn btn-sm btn-danger" onclick="skillDeleteSkill('${skill.scope}','${skill.id}')">删除</button>` : '',
    ].join('');

    const metaItems = [
        ['优先级', String(skill.priority)],
        ['top_k', skill.top_k ?? '-'],
        ['min_score', skill.min_score ?? '-'],
        ['max_docs', skill.max_docs ?? '-'],
        ['max_chunks', skill.max_chunks_per_doc ?? '-'],
    ].map(([label, value]) => `
        <span class="skill-meta-item"><span class="skill-meta-label">${label}</span><span class="skill-meta-value">${escapeHtml(String(value))}</span></span>`).join('');

    view.innerHTML = `
        <div class="skill-detail-wrap">
            <div class="skill-detail-header">
                <div class="skill-detail-title-box">
                    <div class="skill-detail-title">${escapeHtml(skill.name)}
                        <span class="skill-badge scope-${skill.scope}">${skillScopeName(skill.scope)}</span>
                        <span class="skill-badge ${skill.enabled ? 'enabled' : 'disabled'}">${skill.enabled ? '已启用' : '已停用'}</span>
                    </div>
                    <div class="skill-detail-subtitle">ID: ${escapeHtml(skill.id)} · v${skill.version} · 更新于 ${skillFormatTime(skill.updated_at)}</div>
                </div>
                <div class="skill-detail-actions">${actions}</div>
            </div>
            <div class="skill-detail-meta">${metaItems}</div>
            <div class="skill-detail-section">
                <div class="skill-detail-section-title">描述</div>
                <div class="skill-detail-desc">${escapeHtml(skill.description) || '（无描述）'}</div>
            </div>
            ${keywords.length ? `<div class="skill-detail-section"><div class="skill-detail-section-title">触发关键词</div><div class="skill-tag-row">${keywords.map(k => `<span class="skill-tag-chip key">${escapeHtml(k)}</span>`).join('')}</div></div>` : ''}
            ${tools.length ? `<div class="skill-detail-section"><div class="skill-detail-section-title">可用工具</div><div class="skill-tag-row">${tools.map(t => `<span class="skill-tag-chip">${escapeHtml(t)}</span>`).join('')}</div></div>` : ''}
            <div class="skill-detail-section">
                <div class="skill-detail-section-title">指令正文</div>
                <div class="skill-detail-body"><div class="markdown-body" style="zoom: 0.8;">${markedMd(escapeHtml(skill.body) || '（空）')}</div></div>
            </div>
        </div>`;
}

/** 新建（默认 project 作用域） */
function skillCreateNew() {
    const dirPath = skillGetDirPath();
    if (!dirPath) {
        showNotification('请先打开根目录', 'error');
        return;
    }
    skillEditMode = 'create';
    skillEditKey = null;
    skillRenderEdit(null);
}

/** 编辑已有技能（仅用户级） */
function skillEditSkill(scope, id) {
    const skill = skillAllList.find(s => s.scope === scope && s.id === id);
    if (!skill) {
        showNotification('技能不存在，请刷新列表', 'error');
        return;
    }
    if (skill.scope === 'system') {
        showNotification('系统内置技能不可编辑', 'error');
        return;
    }
    skillEditMode = 'edit';
    skillEditKey = { scope, id };
    skillRenderEdit(skill);
}

function skillRenderEdit(skill) {
    document.getElementById('skill-detail-view').style.display = 'none';
    const view = document.getElementById('skill-edit-view');
    view.style.display = 'block';

    const isEdit = skillEditMode === 'edit';
    const scope = skill ? skill.scope : 'global';
    const enabled = skill ? skill.enabled !== false : true;
    const keywords = (skill && skill.trigger_rules && skill.trigger_rules.keywords) || [];
    const tools = (skill && skill.tools) || [];
    const priority = skill ? skill.priority : 50;
    const topK = skill && skill.top_k != null ? skill.top_k : '';
    const minScore = skill && skill.min_score != null ? skill.min_score : '';
    const maxDocs = skill && skill.max_docs != null ? skill.max_docs : '';
    const maxChunksPerDoc = skill && skill.max_chunks_per_doc != null ? skill.max_chunks_per_doc : '';
    view.innerHTML = `
        <div class="skill-edit-wrap">
            <div class="skill-edit-header">
                <div class="skill-edit-title">${isEdit ? '编辑技能' : '新建技能'}</div>
                <button class="btn" onclick="skillBackToDetail()">取消</button>
                <button class="btn btn-primary" onclick="skillSaveSkill()">保存</button>
            </div>
            <div class="skill-form-grid">
                <div class="skill-form-field">
                    <label>ID（小写字母/数字/连字符）</label>
                    <input type="text" id="skill-f-id" value="${escapeHtml(skill ? skill.id : '')}" ${isEdit ? 'readonly' : ''} placeholder="my-skill" autocomplete="off">
                </div>
                <div class="skill-form-field">
                    <label>名称</label>
                    <input type="text" id="skill-f-name" value="${escapeHtml(skill ? skill.name : '')}" placeholder="技能名称" autocomplete="off">
                </div>
                <div class="skill-form-field full">
                    <label>触发关键词（逗号分隔）</label>
                    <input type="text" id="skill-f-keywords" value="${escapeHtml(keywords.join(', '))}" placeholder="总结, 摘要" autocomplete="off">
                </div>
                <div class="skill-form-field full" style="height: auto;">
                    <label>描述</label>
                    <textarea id="skill-f-description" placeholder="一句话描述技能用途">${escapeHtml(skill ? skill.description : '')}</textarea>
                </div>
                <div class="skill-form-field">
                    <label>优先级（0-100）</label>
                    <input type="number" id="skill-f-priority" value="${priority}" min="0" max="100">
                </div>
                <div class="skill-form-field">
                    <label>作用域</label>
                    <select id="skill-f-scope" ${isEdit ? 'disabled' : ''}> 
                        <option value="global" ${scope === 'global' ? 'selected' : ''}>全局（跨项目共享）</option>
                        <option value="project" ${scope === 'project' ? 'selected' : ''}>项目（当前目录 .mdgo/skills）</option>
                    </select>
                </div>
                <div class="skill-form-field">
                    <label>top_k（检索返回数）</label>
                    <input type="number" id="skill-f-top-k" value="${topK}" min="1" placeholder="留空使用默认">
                </div>
                <div class="skill-form-field">
                    <label>min_score（最低相似度）</label>
                    <input type="number" id="skill-f-min-score" value="${minScore}" min="0" max="1" step="0.01" placeholder="留空使用默认">
                </div>
                <div class="skill-form-field">
                    <label>max_docs（最大文档数）</label>
                    <input type="number" id="skill-f-max-docs" value="${maxDocs}" min="1" placeholder="留空使用默认">
                </div>
                <div class="skill-form-field">
                    <label>max_chunks_per_doc</label>
                    <input type="number" id="skill-f-max-chunks" value="${maxChunksPerDoc}" min="1" placeholder="留空使用默认">
                </div>
                <div class="skill-form-field full">
                    <label>可用工具</label>
                    <div class="skill-form-tools" id="skill-f-tools">
                        ${ALLOWED_SKILL_TOOLS.map(t => `<label class="skill-form-tool"><input type="checkbox" value="${t}" ${tools.includes(t) ? 'checked' : ''}> ${t}</label>`).join('')}
                    </div>
                </div>
                <div class="skill-form-field full">
                    <label>启用状态</label>
                    <div class="skill-toggle-row">
                        <div>
                            <div class="skill-toggle-label">${enabled ? '已启用' : '已停用'}</div>
                            <div class="skill-toggle-hint">停用后该技能不会被触发执行</div>
                        </div>
                        <label class="ai-result-sw-lark">
                            <input type="checkbox" id="skill-f-enabled" ${enabled ? 'checked' : ''} onchange="skillUpdateEnabledLabel()">
                            <span class="slider"></span>
                        </label>
                    </div>
                </div>
                <div class="skill-form-field full">
                    <label>指令正文（Markdown）</label>
                    <textarea id="skill-f-body" oninput="skillValidateForm()" placeholder="# 技能指令&#10;当你收到任务时...">${escapeHtml(skill ? skill.body : '')}</textarea>
                </div>
            </div>
        </div>`;
}

/** 启停开关标签同步 */
function skillUpdateEnabledLabel() {
    const toggle = document.getElementById('skill-f-enabled');
    if (!toggle) return;
    const label = toggle.closest('.skill-toggle-row')?.querySelector('.skill-toggle-label');
    if (label) label.textContent = toggle.checked ? '已启用' : '已停用';
}

/** 返回详情视图（编辑中取消/返回） */
function skillBackToDetail() {
    if (skillCurrent) {
        skillRenderDetail(skillCurrent);
    } else {
        skillShowEmpty();
    }
}

/** 前端字段级校验（与后端 validate_skill 约定一致） */
function skillValidateForm() {
    const errors = [];
    const id = (document.getElementById('skill-f-id')?.value || '').trim();
    const description = (document.getElementById('skill-f-description')?.value || '').trim();
    const keywords = (document.getElementById('skill-f-keywords')?.value || '').trim().split(',');
    const body = (document.getElementById('skill-f-body')?.value || '').trim();
    const name = (document.getElementById('skill-f-name')?.value || '').trim();
    const priority = parseInt(document.getElementById('skill-f-priority')?.value || '50', 10);

    if (!id) errors.push('id 不能为空');
    else if (!/^[a-z0-9][a-z0-9-]*$/.test(id)) errors.push('id 只能包含小写字母/数字/连字符，且以字母或数字开头');
    if (!name) errors.push('name 不能为空');
    if (isNaN(priority) || priority < 0 || priority > 100) errors.push('priority 必须在 0~100 之间');
    if (!description) errors.push('description 不能为空');
    if (!keywords.length) errors.push('keywords 不能为空');
    if (!body) errors.push('body 不能为空');

    if (errors.length){
         showNotification('⚠ ' + errors.join('；'), 'warning');
         return false;
    }
    return true;
}

/** 保存（创建/更新） */
async function skillSaveSkill() {
    if (!window.__mdgoSkill) {
        showNotification('技能管理仅在桌面版（Tauri）可用', 'error');
        return;
    }
    if (!skillValidateForm()) {
        return;
    }
    const id = (document.getElementById('skill-f-id')?.value || '').trim();
    const name = (document.getElementById('skill-f-name')?.value || '').trim();
    const description = (document.getElementById('skill-f-description')?.value || '').trim();
    const scope = skillEditMode === 'edit'
        ? (skillEditKey ? skillEditKey.scope : '')
        : (document.getElementById('skill-f-scope')?.value || 'project');
    const priority = parseInt(document.getElementById('skill-f-priority')?.value || '50', 10);
    const keywords = (document.getElementById('skill-f-keywords')?.value || '')
        .split(/[,，]/).map(k => k.trim()).filter(Boolean);
    const topKVal = (document.getElementById('skill-f-top-k')?.value || '').trim();
    const minScoreVal = (document.getElementById('skill-f-min-score')?.value || '').trim();
    const maxDocsVal = (document.getElementById('skill-f-max-docs')?.value || '').trim();
    const maxChunksVal = (document.getElementById('skill-f-max-chunks')?.value || '').trim();
    const tools = Array.from(document.querySelectorAll('#skill-f-tools input:checked')).map(i => i.value);
    const enabled = document.getElementById('skill-f-enabled')?.checked ?? true;
    const body = document.getElementById('skill-f-body')?.value || '';

    if (!id) { showNotification('请填写 ID', 'error'); return; }
    if (!name) { showNotification('请填写名称', 'error'); return; }
    if (!description) { showNotification('请填写描述', 'error'); return; }
    if (!keywords.length) { showNotification('请填写关键词', 'error'); return; }
    if (!body) { showNotification('请填写指令正文', 'error'); return; }

    const input = {
        name,
        description,
        priority,
        trigger_rules: {
            keywords,
            // 编辑时保留原技能的自定义阈值，创建时用默认 0.5（UI 未提供该字段，避免编辑丢值）
            similarity_threshold: (() => {
                if (skillEditMode === 'edit' && skillEditKey) {
                    const editing = skillAllList.find(s => s.scope === skillEditKey.scope && s.id === skillEditKey.id);
                    return editing && editing.trigger_rules ? editing.trigger_rules.similarity_threshold : 0.5;
                }
                return 0.5;
            })(),
        },
        tools,
        enabled,
        body,
    };
    // 可选检索参数：留空则不传（后端保留原值或 null）
    if (topKVal) input.top_k = parseInt(topKVal, 10);
    if (minScoreVal) input.min_score = parseFloat(minScoreVal);
    if (maxDocsVal) input.max_docs = parseInt(maxDocsVal, 10);
    if (maxChunksVal) input.max_chunks_per_doc = parseInt(maxChunksVal, 10);
    try {
        if (skillEditMode === 'edit' && skillEditKey) {
            await window.__mdgoSkill.skillUpdate(skillDirPath, scope, id, input);
            showNotification('技能已更新');
        } else {
            input.id = id;
            await window.__mdgoSkill.skillCreate(skillDirPath, scope, input);
            showNotification('技能已创建');
        }
        skillEditMode = 'create';
        skillEditKey = null;
        await skillLoadList();
        skillCurrent = skillAllList.find(s => s.scope === scope && s.id === id) || null;
        if (skillCurrent) skillRenderDetail(skillCurrent);
        else skillShowEmpty();
    } catch (e) {
        showNotification('保存失败: ' + e, 'error');
    }
}

/** 启停切换 */
async function skillToggleEnabled(scope, id, enabled) {
    if (!window.__mdgoSkill) return;
    try {
        const updated = await window.__mdgoSkill.skillSetEnabled(skillDirPath, scope, id, enabled);
        const idx = skillAllList.findIndex(s => s.scope === scope && s.id === id);
        if (idx >= 0) skillAllList[idx] = updated;
        skillCurrent = updated;
        skillRenderList();
        skillRenderDetail(updated);
        const tagEl = document.getElementById(`skill-tag-${id}`);
        if (tagEl) {
            if (enabled){
                tagEl.textContent = '已启用';
                tagEl.classList.remove('disabled');
                tagEl.classList.add('enabled');
            } else{
                tagEl.textContent = '已停用';
                tagEl.classList.remove('enabled');
                tagEl.classList.add('disabled');
            } 
        }
        showNotification(`技能已${enabled ? '启用' : '停用'}`);
    } catch (e) {
        showNotification('操作失败: ' + e, 'error');
    }
}

/** 删除（确认后执行） */
function skillDeleteSkill(scope, id) {
    const skill = skillAllList.find(s => s.scope === scope && s.id === id);
    showConfirmModal('删除技能', `确定删除技能「${skill ? skill.name : id}」（${id}）吗？删除后不可恢复。`, async (ok) => {
        if (!ok) return;
        try {
            await window.__mdgoSkill.skillDelete(skillDirPath, scope, id);
            showNotification('技能已删除');
            skillCurrent = null;
            await skillLoadList();
            skillShowEmpty();
        } catch (e) {
            showNotification('删除失败: ' + e, 'error');
        }
    });
}

/** 搜索框输入（实时过滤 + 清空按钮显隐） */
function skillHandleSearchInput() {
    const input = document.getElementById('skill-search-input');
    const clearBtn = document.getElementById('skill-search-clear');
    if (!input) return;
    skillSearchTerm = input.value;
    if (clearBtn) clearBtn.style.display = skillSearchTerm ? 'flex' : 'none';
    skillRenderList();
}

/** 搜索框回车 */
function skillHandleSearchKeydown(e) {
    if (e.key === 'Enter') {
        skillSearchTerm = document.getElementById('skill-search-input').value;
        skillRenderList();
    }
}

/** 清空搜索 */
function skillClearSearch() {
    skillSearchTerm = '';
    const input = document.getElementById('skill-search-input');
    if (input) input.value = '';
    const clearBtn = document.getElementById('skill-search-clear');
    if (clearBtn) clearBtn.style.display = 'none';
    skillRenderList();
}

/** 作用域筛选切换（chips 高亮同步） */
function skillScopeChange(value) {
    skillScopeFilter = value || '';
    document.querySelectorAll('#skill-scope-chips .skill-chip').forEach(c => {
        c.classList.toggle('active', (c.dataset.scope || '') === skillScopeFilter);
    });
    skillRenderList();
}

/** 意图匹配测试（调试入口，对应后端 skill_match 命令） */
async function skillTestMatch() {
    if (!window.__mdgoSkill) {
        showNotification('技能管理仅在桌面版（Tauri）可用', 'error');
        return;
    }
   showInputModal('输入测试语句（按 L1 关键词 → L2 语义 → L3 兜底匹配）', '', async (q) => {
        try {
            const results = await window.__mdgoSkill.skillMatch(skillDirPath, q);
            document.getElementById('skill-edit-view').style.display = 'none';
            const view = document.getElementById('skill-detail-view');
            view.style.display = 'block';
            if (!results.length) {
                view.innerHTML = `<div class="skill-detail-wrap">
                    <div class="skill-detail-title">意图匹配测试 <span class="skill-badge">无技能命中</span></div>
                    <div class="skill-detail-section-title">语句</div>
                    <div class="skill-detail-desc">${escapeHtml(q)}</div>
                    <div class="skill-detail-desc" style="color:var(--t3);">未命中任何启用的技能，可尝试调整触发关键词或相似度阈值。</div>
                </div>`;
                return;
            }
            const rows = results.map(r => `
                <div class="skill-list-item" style="margin-bottom:0.375rem;cursor:default;padding:0.5rem 0;">
                    <div class="skill-item-info">
                        <div class="skill-item-name">${escapeHtml(r.skill.name)}
                            <span class="skill-badge scope-${r.skill.scope}">${skillScopeName(r.skill.scope)}</span>
                            <span class="skill-badge enabled">${r.level}</span>
                        </div>
                        <div class="skill-item-meta"><span>ID: ${escapeHtml(r.skill.id)}</span>&nbsp;&nbsp;&nbsp;<span>得分: ${typeof r.score === 'number' ? r.score.toFixed(3) : r.score}</span></div>
                    </div>
                </div>`).join('');
            view.innerHTML = `<div class="skill-detail-wrap">
                <div class="skill-detail-title">意图匹配测试 <span class="skill-badge">命中 ${results.length} 个</span></div>
                <div class="skill-detail-section-title">测试语句</div>
                <div class="skill-detail-desc">${escapeHtml(q)}</div>
                <div class="skill-detail-section-title">匹配结果</div>
                ${rows}
            </div>`;
        } catch (e) {
            showNotification('匹配失败: ' + e, 'error');
        }
    });
}
