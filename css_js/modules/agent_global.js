// ===== Agent 全局后台任务观察者 + 全局交互弹窗（应用级，不随视图销毁） =====
// Phase 3：重业务（任务状态快照 / 审批挂起 / 计划挂起）已由后端承担
// （AppState.agent_tasks + approval_pending/plan_pending），本模块只做：
//   1. 全局任务状态条（任意界面可见：运行中任务 + 停止按钮）
//   2. 全局交互弹窗（approval/plan 确认，跨界面弹出，响应「权限弹框在其他界面正常弹出」）
//   3. 后台任务完成通知（用户不在 Agent 页面时 sticky 提醒）
// 遵循 SOLID：SRP（观察/弹窗/通知各自独立函数）、OCP（新增事件类型只需在
// OBSERVED_CHANNELS 增加一行）、DIP（仅依赖后端 agent_task_list/kb_cancel_task 窄接口）。

(function () {
    if (typeof window.__agentGlobalInited !== 'undefined') return;
    window.__agentGlobalInited = true;

    let _taskBarTimer = null;

    // 驱动状态条刷新的事件通道（新增事件类型只需在此追加）
    const OBSERVED_CHANNELS = [
        'rag:status', 'rag:delta', 'rag:done', 'rag:error',
        'llm:delta', 'llm:done', 'llm:error',
        'trace:event', 'agent:tool_call', 'agent:tool_result',
    ];

    // ── 全局任务状态条渲染（数据源：后端 agent_task_list） ──
    async function refreshAgentTaskBar() {
        if (!isTauriVisit() || !window.__TAURI__?.core?.invoke) return;
        const container = document.getElementById('agent-global-task-bar');
        if (!container) return;
        // 问题1修复：用户在 Agent/chat 页面时不显示全局状态条——
        // 页面本身已有流式渲染、停止按钮与 typing 状态；仅切换到其他页面时才显示。
        const chatEl = document.getElementById('chat-container');
        if (chatEl && chatEl.style.display !== 'none') {
            container.style.display = 'none';
            container.innerHTML = '';
            return;
        }
        let tasks;
        try {
            tasks = await window.__TAURI__.core.invoke('agent_task_list');
        } catch (e) {
            return;
        }
        if (!Array.isArray(tasks)) return;
        const running = tasks.filter(t => t.status === 'running');
        if (running.length === 0) {
            container.style.display = 'none';
            container.innerHTML = '';
            return;
        }
        container.style.display = 'flex';
        const items = running.map(t => {
            const modeLabel = t.mode === 'chat' ? '对话' : 'Agent';
            const statusText = t.status_message || '运行中…';
            const contentPreview = (t.content || '').trim() ? String(t.content).slice(-60) : '';
            return `<div class="agent-global-task-item" title="${escapeHtml(contentPreview || statusText)}">
                <span class="agent-global-task-label">⚡ ${escapeHtml(modeLabel)}</span>
                <span class="agent-global-task-status">${escapeHtml(statusText)}</span>
                <button class="agent-global-task-stop" data-rid="${escapeHtml(t.request_id)}">停止</button>
            </div>`;
        }).join('');
        container.innerHTML = items;
    }

    // 防抖刷新（事件驱动，避免高频事件频繁 invoke）
    function scheduleTaskBarRefresh() {
        if (_taskBarTimer) clearTimeout(_taskBarTimer);
        _taskBarTimer = setTimeout(refreshAgentTaskBar, 300);
    }

    // ── 后台任务完成通知（用户不在 Agent 页面时） ──
    function notifyIfAway(channel) {
        const chatContainer = document.getElementById('chat-container');
        if (chatContainer && chatContainer.style.display !== 'none') return; // 页面可见无需通知
        if (typeof Notify === 'undefined' || !Notify.sticky) return;
        const failed = channel.indexOf('error') >= 0;
        Notify.sticky(failed ? '✗ Agent 后台任务失败，请到 Agent 页面查看详情' : '✓ Agent 后台任务已完成，切回 Agent 页面可查看结果');
    }

    // ── 初始化：任务观察者（应用级监听 + 全局状态条） ──
    async function initAgentTaskObserver() {
        if (!isTauriVisit() || !window.__TAURI__?.event) return;
        const { listen } = window.__TAURI__.event;
        for (const ch of OBSERVED_CHANNELS) {
            try {
                await listen(ch, () => scheduleTaskBarRefresh());
            } catch (e) {
                console.warn('[agent-global] 注册事件监听失败:', ch, e);
            }
        }
        try { await listen('rag:done', () => notifyIfAway('rag:done')); } catch (e) { /* 忽略 */ }
        try { await listen('rag:error', () => notifyIfAway('rag:error')); } catch (e) { /* 忽略 */ }
        try { await listen('llm:done', () => notifyIfAway('llm:done')); } catch (e) { /* 忽略 */ }
        try { await listen('llm:error', () => notifyIfAway('llm:error')); } catch (e) { /* 忽略 */ }
        // 停止按钮：事件委托（任意界面点击生效）
        document.addEventListener('click', (e) => {
            const btn = e.target.closest('.agent-global-task-stop');
            if (!btn) return;
            const rid = btn.dataset.rid;
            if (rid) {
                window.__TAURI__.core.invoke('kb_cancel_task', { requestId: rid }).catch(() => { });
                scheduleTaskBarRefresh();
            }
        });
        // 首次渲染
        scheduleTaskBarRefresh();
        // 启动后周期性兜底刷新（覆盖事件丢失场景，30s 一次，成本低）
        setInterval(scheduleTaskBarRefresh, 30000);
    }

    // ── 初始化：全局交互弹窗（approval / plan，跨界面弹出） ──
    // 原注册于 agent.js AgentInit（页面级），Phase 3 拆出为应用级，独立于 Agent 页面。
    async function initAgentGlobalDialogs() {
        if (!isTauriVisit() || !window.__TAURI__?.event) return;
        const { listen } = window.__TAURI__.event;
        // AI 工具审批请求（编辑/删除文件确认）：系统弹窗 → IPC 回传
        try {
            await listen('approval:request', async (e) => {
                const { request_id, tool, summary, detail } = (e.payload || {});
                if (!request_id) return;
                const opName = tool === 'delete' ? '删除文件' : tool === 'edit' ? '修改文件' : '执行操作';
                const message = `${summary || ''}${detail ? '\n\n' + detail : ''}`;
                let approved = false;
                try {
                    approved = await showConfirmModalAsync(`AI 请求${opName}`, message, { blockOutsideClick: true, blockEsc: true });
                } catch (err) {
                    console.warn('[approval] 确认框异常:', err);
                }
                try {
                    await window.__TAURI__.core.invoke('approval_respond', {
                        requestId: request_id,
                        approved,
                        reason: approved ? null : '用户拒绝了此操作',
                    });
                } catch (err) {
                    console.warn('[approval] 回传审批结果失败:', err);
                }
            });
        } catch (e) {
            console.warn('注册 approval 审批监听失败:', e);
        }
        // AI 任务计划确认请求（plan:request）：展示计划，用户批准/拒绝经 IPC 回传
        try {
            await listen('plan:request', async (e) => {
                const { plan_id, plan } = (e.payload || {});
                if (!plan_id) return;
                const p = plan || {};
                const goal = p.goal ? '目标：' + p.goal : '';
                const steps = Array.isArray(p.steps) ? p.steps.map((s, i) => (i + 1) + '. ' + s).join('\n') : '(无步骤)';
                const acceptance = Array.isArray(p.acceptance) && p.acceptance.length ? '\n\n验收标准：\n' + p.acceptance.map(a => '· ' + a).join('\n') : '';
                const touchpoints = Array.isArray(p.touchpoints) && p.touchpoints.length ? '\n\n涉及范围：\n' + p.touchpoints.map(t => '· ' + t).join('\n') : '';
                const risks = Array.isArray(p.risks) && p.risks.length ? '\n\n风险注意：\n' + p.risks.map(r => '· ' + r).join('\n') : '';
                const nonGoals = Array.isArray(p.non_goals) && p.non_goals.length ? '\n\n非目标（明确不做）：\n' + p.non_goals.map(n => '· ' + n).join('\n') : '';
                const rollback = Array.isArray(p.rollback) && p.rollback.length ? '\n\n失败回滚：\n' + p.rollback.map(r => '· ' + r).join('\n') : '';
                const message = goal + '\n\n步骤：\n' + steps + acceptance + touchpoints + risks + nonGoals + rollback;
                let approved = false;
                try {
                    approved = await showConfirmModalAsync('AI 任务计划，请确认是否执行', message, { blockOutsideClick: true, blockEsc: true, modalStyle: 'max-width: 40%;width: 40%;', modelBodyStyle: 'height: 32rem;overflow-y: auto;' });
                } catch (err) {
                    console.warn('[plan] 计划确认框异常:', err);
                }
                try {
                    await window.__TAURI__.core.invoke('plan_respond', {
                        planId: plan_id,
                        approved,
                        reason: approved ? null : '用户未批准此计划',
                    });
                } catch (err) {
                    console.warn('[plan] 回传计划确认结果失败:', err);
                }
            });
        } catch (e) {
            console.warn('注册 plan 计划确认监听失败:', e);
        }
        // 计划未获批准事件（超时/通道异常，非用户主动拒绝）：sticky 常驻提醒
        try {
            await listen('plan:rejected', (e) => {
                const payload = (e && e.payload) || {};
                const reason = payload.reason ? '原因：' + payload.reason : '';
                if (typeof Notify !== 'undefined' && Notify.sticky) {
                    Notify.sticky('AI 任务计划未获批准，任务已中止。\n' + reason);
                }
            });
        } catch (e) {
            console.warn('注册 plan:rejected 监听失败:', e);
        }
    }

    window.initAgentGlobalDialogs = initAgentGlobalDialogs;
    window.initAgentTaskObserver = initAgentTaskObserver;
})();
