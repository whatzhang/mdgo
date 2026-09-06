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
        // 产品要求：
        // 1) Agent/chat 页面本身有内联状态 → 右下角任务条不显示；
        // 2) 小助手停靠栏打开（对话任务进行中）→ 右下角任务条也不显示；
        // 3) 其他页面且存在运行任务 → 右下角显示后台任务状态。
        const chatEl = document.getElementById('chat-container');
        const onChat = !!(chatEl && chatEl.style.display !== 'none');
        if (onChat) {
            container.style.display = 'none';
            container.innerHTML = '';
            return;
        }
        const docQaOverlay = document.getElementById('doc-qa-overlay');
        if (docQaOverlay && docQaOverlay.classList.contains('doc-qa-open')) {
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

    // ── 后台任务完成通知（不在 Agent 页时提示；小助手停靠栏打开期间不弹） ──
    function notifyIfAway(channel) {
        const chatContainer = document.getElementById('chat-container');
        if (chatContainer && chatContainer.style.display !== 'none') return; // Agent 页可见无需通知
        const docQaOverlay = document.getElementById('doc-qa-overlay');
        if (docQaOverlay && docQaOverlay.classList.contains('doc-qa-open')) return; // 小助手对话中不提示
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
        // AI 澄清提问请求（ask_user_question 工具）：弹窗收集用户回答 → IPC 回传
        // 协议与 approval/plan 同构：question:request → 前端弹窗 → question_respond 回传
        try {
            await listen('question:request', (e) => {
                const { question_id, question, header, options } = (e.payload || {});
                if (!question_id || !question) return;
                showQuestionModalAsync({ question_id, question, header, options })
                    .then(answer => {
                        return window.__TAURI__.core.invoke('question_respond', {
                            questionId: question_id,
                            answer: answer || null,
                        });
                    })
                    .catch(err => {
                        console.warn('[question] 回传提问结果失败:', err);
                        // 回传失败兜底：仍尝试以 null 回传，避免挂起表残留
                        window.__TAURI__.core.invoke('question_respond', {
                            questionId: question_id,
                            answer: null,
                        }).catch(() => { });
                    });
            });
        } catch (e) {
            console.warn('注册 question:request 监听失败:', e);
        }
    }

    // ── 澄清提问弹窗（P1-4：ask_user_question 工具的前端通道） ──
    // 独立实现（不依赖主页面 confirm 弹窗）：问题 + 候选选项（可点选）+ 自由输入
    function showQuestionModalAsync({ question_id, question, header, options }) {
        return new Promise((resolve) => {
            const overlay = document.createElement('div');
            overlay.className = 'agent-question-overlay';
            overlay.style.cssText = 'position:fixed;inset:0;background:rgba(0,0,0,0.45);z-index:9999;display:flex;align-items:center;justify-content:center;';
            const box = document.createElement('div');
            box.style.cssText = 'background:#fff;border-radius:10px;padding:1.2rem 1.4rem;max-width:30rem;width:92%;max-height:80vh;overflow:auto;box-shadow:0 8px 30px rgba(0,0,0,0.25);font-family:system-ui,sans-serif;';
            const title = document.createElement('div');
            title.textContent = header || 'AI 需要确认';
            title.style.cssText = 'font-size:1rem;font-weight:600;color:#333;margin-bottom:0.6rem;';
            const text = document.createElement('div');
            text.textContent = question;
            text.style.cssText = 'font-size:0.9rem;color:#444;white-space:pre-wrap;line-height:1.5;margin-bottom:0.8rem;word-break:break-all;';
            box.appendChild(title);
            box.appendChild(text);

            const input = document.createElement('textarea');
            input.placeholder = '输入回答…';
            input.style.cssText = 'width:100%;box-sizing:border-box;min-height:4.5rem;border:1px solid #d0d0d0;border-radius:6px;padding:0.5rem;font-size:0.85rem;resize:vertical;';

            const btns = document.createElement('div');
            btns.style.cssText = 'display:flex;gap:0.5rem;justify-content:flex-end;margin-top:0.8rem;';
            const cancelBtn = document.createElement('button');
            cancelBtn.textContent = '取消';
            cancelBtn.style.cssText = 'padding:0.35rem 0.9rem;border-radius:6px;border:1px solid #ccc;background:#f5f5f5;color:#555;font-size:0.85rem;cursor:pointer;';
            const okBtn = document.createElement('button');
            okBtn.textContent = '提交回答';
            okBtn.style.cssText = 'padding:0.35rem 0.9rem;border-radius:6px;border:none;background:#2f6fed;color:#fff;font-size:0.85rem;cursor:pointer;';
            okBtn.disabled = true;
            okBtn.style.opacity = '0.5';

            const finish = (answer) => {
                overlay.remove();
                resolve(answer);
            };
            cancelBtn.onclick = () => finish(null);
            const pick = (val) => {
                input.value = val;
                okBtn.disabled = !val.trim();
                okBtn.style.opacity = okBtn.disabled ? '0.5' : '1';
            };
            okBtn.onclick = () => finish(input.value.trim() || null);
            input.addEventListener('input', () => {
                okBtn.disabled = !input.value.trim();
                okBtn.style.opacity = okBtn.disabled ? '0.5' : '1';
            });
            // 候选选项（点选即填入输入框，可再编辑）
            if (Array.isArray(options) && options.length) {
                const optBox = document.createElement('div');
                optBox.style.cssText = 'display:flex;flex-wrap:wrap;gap:0.4rem;margin-bottom:0.7rem;';
                for (const opt of options.slice(0, 6)) {
                    const b = document.createElement('button');
                    b.textContent = opt;
                    b.style.cssText = 'padding:0.3rem 0.7rem;border-radius:999px;border:1px solid #2f6fed;background:#eef4ff;color:#2f6fed;font-size:0.8rem;cursor:pointer;';
                    b.onclick = () => pick(opt);
                    optBox.appendChild(b);
                }
                box.appendChild(optBox);
            }
            box.appendChild(input);
            btns.appendChild(cancelBtn);
            btns.appendChild(okBtn);
            box.appendChild(btns);
            overlay.appendChild(box);
            document.body.appendChild(overlay);
            setTimeout(() => input.focus(), 50);
        });
    }

    window.initAgentGlobalDialogs = initAgentGlobalDialogs;
    window.initAgentTaskObserver = initAgentTaskObserver;
})();
