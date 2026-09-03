/**
 * ===== Agent 管理模块（模块化标杆 · css_js/modules/agent.js） =====
 *
 * 【职责】Agent（RAG 模式）独立业务逻辑：
 *   1. 工具调用轨迹可视化（agent:tool_call / agent:tool_result 事件 → 卡片渲染）
 *   2. RAG 检索参数设置（openRagSettings / saveRagSettings）
 *   3. RAG 查询（sendRagQuery：流式增量 / 状态 / 来源 / 阶段耗时 trace）
 *   4. AgentInit：模式按钮显示、RAG 配置加载、Tauri 事件监听（watcher/approval/plan）
 *   5. Trace 阶段面板（renderTracePanel，按 request_id 渲染阶段耗时）
 * 【入口】dashboard「Agent」→ aiChat → chatMode='rag' → sendChatMessage() 调用 sendRagQuery()；
 *         AgentInit() 由主脚本启动流程延迟调用（定义在本模块，主脚本之后加载）。
 * 【对外暴露】AgentInit / sendRagQuery / openRagSettings / closeRagSettings / saveRagSettings /
 *            handleAgentToolEvent / expandToolHistory / ensureToolTrace / renderToolTraceFromRecords / renderTracePanel
 * 【留在 index.html 的公共代码】（本模块不包含、仅运行时依赖）
 *   - callAIAPI / callAnthropicAPI：小助手（ai-fab-tooltip）单次 API 请求（非流式）
 *   - AGENT_LIMITS 常量：被公共代码（模型管理 / callAIAPI / LOCAL_LLM_*）广泛引用
 *   - chat 核心：sendChatMessage / sendLlmQuery / 会话管理 / 消息渲染 / 流式状态
 *   - setChatMode：模式切换（含 rag 分支，与会话切换耦合）
 * 【运行时依赖的全局服务/状态】（来自 index.html 主脚本，加载顺序：主脚本 → 本模块）
 *   - isTauriVisit / showNotification / showConfirmModalAsync / Notify.sticky
 *   - getRootHandle / getDirPath / rememberLastChatSession / saveChatMessageWithRetry
 *   - renderChatMarkdown / addChatMessage / renderChatSources / updateStreamingStatus / getMessageAssistant
 *   - chat 流式状态：chatStreaming / chatAbortController / _chatStreamingDiv / _chatStreamingSources / _streamingToolCalls 等
 * 【SOLID 说明】
 *   - S 单一职责：本文件只负责 Agent/RAG 模式的展示与交互，不含小助手单次 API 请求逻辑。
 *   - O 开闭原则：新增 Agent 能力优先扩展 Tauri 适配层事件（agent、rag、plan 系列事件），模块主体保持稳定。
 *   - D 依赖倒置：只依赖上述稳定全局服务接口，不依赖任何具体模块内部实现。
 */

// ── 工具调用轨迹可视化 ──
// 渲染 Agent 工具调用卡片（🔧 工具名 + 参数 + 状态），挂在流式消息 div 上
function ensureStreamingAssistantDiv() {
    if (_chatStreamingDiv) return _chatStreamingDiv;
    removeChatTyping();
    const container = document.getElementById('chat-messages');
    const div = getMessageAssistant();
    container.appendChild(div);
    _chatStreamingDiv = div;
    let _rafPending = false;
    div._scroll = () => {
        if (!_rafPending && window._chatAutoScroll !== false) {
            _rafPending = true;
            requestAnimationFrame(() => {
                _rafPending = false;
                if (window._chatAutoScroll !== false) {
                    container.scrollTop = container.scrollHeight;
                }
            });
        }
    };
    return div;
}
// ── Agent 过程时间线（三合一：thinking + 工具 + 阶段 trace） ──
// 将 llm:thinking / rag:thinking、agent:tool_call / agent:tool_result、
// trace:event 三类事件合并为一条按时间排序的过程时间线，挂在助手消息 div 上。
// 默认折叠为摘要条（步数 / 思考时长 / 工具数 / 总耗时），点开展开完整时间线；
// thinking 条目默认折叠成一行摘要，点击展开全文。

/** 单条消息 div 的时间线状态（request_id 作用域） */
function ensureTimelineState(div, requestId) {
    if (!div) return null;
    if (!div._timeline) {
        div._timeline = {
            requestId: requestId || '',
            entries: [],          // { type, ts, ... }
            thinkingBuf: '',      // 当前 thinking 增量累积（未落条目）
            thinkingStart: 0,
            toolOpen: {},         // seq -> call 事件（等 result 配对）
            stageOpen: {},        // stage -> start 事件（等 end 配对）
            rafPending: false,
            dirty: false,
            _seq: 0,
        };
    }
    return div._timeline;
}

/** 追加一条时间线条目并调度重渲染（rAF 节流） */
function timelinePush(div, entry) {
    const tl = div && div._timeline;
    if (!tl) return;
    // 内部顺序号：仅用于条目排序展示；后端 seq 单独存 backendSeq（供 result 配对），
    // 避免内部号覆盖后端号导致 tool result 无法配对（原 bug）。
    entry.seq = tl._seq++;
    entry.ts = entry.ts || Date.now();
    tl.entries.push(entry);
    tl.dirty = true;
    if (!tl.rafPending) {
        tl.rafPending = true;
        requestAnimationFrame(() => {
            tl.rafPending = false;
            if (tl.dirty) {
                tl.dirty = false;
                renderProcessTimeline(div);
            }
        });
    }
    if (div._scroll) div._scroll();
}

/** 时间线 DOM：外层 <details> 折叠摘要条 + 展开的条目列表 */
function ensureProcessTimeline(div) {
    if (!div) return null;
    if (!div._timelineEl) {
        const wrap = document.createElement('details');
        wrap.className = 'process-timeline';
        wrap.dataset.open = '0';
        const summary = document.createElement('summary');
        summary.className = 'process-timeline-summary';
        summary.innerHTML = '<span class="pt-summary-text">思考过程时间线</span><span class="pt-summary-stats"></span>';
        const list = document.createElement('div');
        list.className = 'process-timeline-list';
        wrap.appendChild(summary);
        wrap.appendChild(list);
        // 插入到正文前（与旧 tool-trace 同位置）
        div.insertBefore(wrap, div._body);
        div._timelineEl = wrap;
        div._timelineList = list;
        // 点击摘要条展开/收起
        summary.addEventListener('click', () => {
            const open = wrap.open;
            wrap.dataset.open = open ? '1' : '0';
        });
    }
    return div._timelineEl;
}

/** 重渲染整个时间线（从 entries 重建 DOM，简单可靠；条目数有限） */
function renderProcessTimeline(div) {
    const tl = div._timeline;
    const wrap = div._timelineEl;
    if (!tl || !wrap) return;
    ensureProcessTimeline(div);
    const list = div._timelineList;
    const entries = tl.entries.slice().sort((a, b) => a.seq - b.seq);

    // 统计（折叠摘要条）
    const toolCount = entries.filter(e => e.type === 'tool').length;
    const thinkMs = entries.filter(e => e.type === 'thinking').reduce((s, e) => s + (e.duration_ms || 0), 0);
    const totalStart = entries.length ? entries[0].ts : 0;
    const totalEnd = entries.length ? entries[entries.length - 1].ts : 0;
    const totalMs = totalEnd > totalStart ? totalEnd - totalStart : 0;
    const errCount = entries.filter(e => (e.type === 'tool' && e.ok === false) || (e.type === 'stage' && e.status === 'error')).length;
    const stats = `${entries.length} steps` + (thinkMs ? ` · think ${(thinkMs / 1000).toFixed(1)}s` : '')
        + ` · tool ${toolCount}` + (totalMs ? ` · total ${(totalMs / 1000).toFixed(1)}s` : '')
        + (errCount ? ` · ${errCount} failed` : '');
    const sum = wrap.querySelector('.pt-summary-stats');
    if (sum) sum.textContent = stats;

    list.innerHTML = entries.map(entry => renderTimelineEntry(entry)).join('');
    // 事件委托：thinking 点击展开全文、tool 点击展开 detail（每次重建后重新绑定到 list 一次即可，
    // 用 onlistener 属性避免重复绑定累积）
    if (!list.dataset.bound) {
        list.dataset.bound = '1';
        list.addEventListener('click', (ev) => {
            const thinkingEl = ev.target.closest('.pt-thinking');
            if (thinkingEl) {
                // 展开状态持久到条目（entry._open），rAF 重建时保持
                const idx = Array.prototype.indexOf.call(list.children, thinkingEl);
                const tl = div._timeline;
                if (tl && idx >= 0) {
                    const sorted = tl.entries.slice().sort((a, b) => a.seq - b.seq);
                    const entry = sorted[idx];
                    if (entry && entry.type === 'thinking') {
                        entry._open = !entry._open;
                        tl.dirty = true;
                        renderProcessTimeline(div);
                    }
                }
                return;
            }
            const toolEl = ev.target.closest('.pt-tool');
            if (toolEl) {
                const d = toolEl.querySelector('.tool-detail');
                if (d) d.style.display = d.style.display === 'none' ? 'block' : 'none';
            }
        });
    }
}

/** 渲染单条时间线条目（emoji 全部用英文单词替代） */
function renderTimelineEntry(entry) {
    switch (entry.type) {
        case 'thinking': {
            // 展开状态持久在条目上（entry._open），rAF 重建时保持用户选择
            const open = !!entry._open;
            const short = entry.content && entry.content.length > 80 ? entry.content.slice(0, 80) + '…' : (entry.content || '');
            // 结构：一行（think 标签 + preview 单行摘要）→ 点击展开 full body（完整文本换行显示）
            return `<div class="pt-entry pt-thinking${open ? ' open' : ''}" data-kind="thinking">
                <div class="pt-thinking-row">
                    <span class="pt-dot">think</span>
                    <span class="pt-thinking-preview"${open ? ' style="display:none;"' : ''}>${traceEscapeHtml(short)}</span>
                    <span class="pt-time">${entry.duration_ms ? (entry.duration_ms / 1000).toFixed(1) + 's' : ''}</span>
                </div>
                <div class="pt-thinking-full"${open ? '' : ' style="display:none;"'}>${traceEscapeHtml(entry.content || '')}</div>
            </div>`;
        }
        case 'tool': {
            const cls = entry.ok === null || entry.ok === undefined ? 'running' : (entry.ok ? 'ok' : 'fail');
            const statusText = entry.ok === null || entry.ok === undefined ? 'running…' : (entry.ok ? 'done' : 'failed');
            const skillBadge = entry.skill_id
                ? `<span class="tool-skill-badge" title="技能触发: ${escapeHtml(entry.skill_id)}">${escapeHtml(entry.skill_id.split(':').pop() || entry.skill_id)}</span>`
                : '';
            const dur = entry.duration_ms ? `<span class="pt-time">${(entry.duration_ms / 1000).toFixed(1)}s</span>` : '';
            return `<div class="pt-entry pt-tool tool-card ${cls}" data-backend-seq="${entry.backendSeq || ''}">
                <span class="pt-dot">tool</span>
                <span class="tool-name">${escapeHtml(entry.tool || '')}</span>${skillBadge}
                <span class="tool-args">${escapeHtml(String(entry.args_preview || '').slice(0, 80))}</span>
                <span class="tool-status">${statusText}</span>${dur}
                ${entry.summary ? `<div class="tool-detail" style="display:none;">${escapeHtml(String(entry.summary))}</div>` : ''}
            </div>`;
        }
        case 'stage': {
            const status = entry.status || '';
            const icon = status === 'start' ? 'start' : status === 'ok' ? 'ok' : status === 'error' ? 'error' : status === 'cancelled' ? 'cancelled' : status === 'denied' ? 'denied' : 'stage';
            const dur = status !== 'start' && entry.duration_ms ? `${(entry.duration_ms / 1000).toFixed(1)}s` : '';
            return `<div class="pt-entry pt-stage">
                <span class="pt-dot">${icon}</span>
                <span class="pt-stage-name">${traceEscapeHtml(entry.stage || '')}${entry.detail ? ' — ' + traceEscapeHtml(entry.detail) : ''}</span>
                <span class="pt-time">${dur}</span>
            </div>`;
        }
        default:
            return '';
    }
}

/** 把未落条的 thinking 缓冲固化为正式条目（移除 _streaming 标记并补耗时）。返回是否落条 */
function flushThinkingBuffer(div) {
    const tl = div && div._timeline;
    if (!tl || !tl.thinkingBuf) return false;
    const now = Date.now();
    const duration = tl.thinkingStart ? now - tl.thinkingStart : 0;
    // 若已存在流式进行中的 thinking 条目，直接补全它（避免重复条目）
    let updated = false;
    for (let i = tl.entries.length - 1; i >= 0; i--) {
        const e = tl.entries[i];
        if (e.type === 'thinking' && e._streaming) {
            e.content = tl.thinkingBuf;
            e.duration_ms = duration;
            delete e._streaming;
            updated = true;
            break;
        }
    }
    if (!updated) {
        timelinePush(div, {
            type: 'thinking',
            content: tl.thinkingBuf,
            duration_ms: duration,
        });
    }
    tl.thinkingBuf = '';
    tl.thinkingStart = 0;
    return true;
}

/** thinking 增量累积：到达时追加到缓冲，间隔 >1.2s 或总量超限时落成一条 */
function timelineThinking(div, content) {
    const tl = div && div._timeline;
    if (!tl) return;
    const now = Date.now();
    if (tl.thinkingBuf && now - tl.thinkingStart > 1200) {
        // 落一条已累积的 thinking
        flushThinkingBuffer(div);
        tl.thinkingStart = now;
    }
    if (!tl.thinkingBuf) tl.thinkingStart = now;
    tl.thinkingBuf += content;
    // 长思考防爆：超 4000 字符强制落条
    if (tl.thinkingBuf.length > 4000) {
        flushThinkingBuffer(div);
        tl.thinkingStart = now;
    }
    // 流式可见性：rAF 重渲染时若缓冲非空，把缓冲内容挂到「进行中」thinking 条目上，
    // 让用户实时看到正在思考的内容（而非等 1.2s 落条才显示）。
    tl.dirty = true;
    if (!tl.rafPending) {
        tl.rafPending = true;
        requestAnimationFrame(() => {
            tl.rafPending = false;
            // 若存在未落条的缓冲，渲染为最新一条 thinking 的实时内容
            if (tl.thinkingBuf) {
                let found = false;
                for (let i = tl.entries.length - 1; i >= 0; i--) {
                    const e = tl.entries[i];
                    if (e.type === 'thinking' && e._streaming) {
                        e.content = tl.thinkingBuf;
                        found = true;
                        break;
                    }
                }
                if (!found) {
                    timelinePush(div, {
                        type: 'thinking',
                        content: tl.thinkingBuf,
                        duration_ms: 0,
                        _streaming: true, // 标记：流式进行中的 thinking，落条时移除该标记
                    });
                }
            }
            if (tl.dirty) {
                tl.dirty = false;
                renderProcessTimeline(div);
            }
        });
    }
}

/** 阶段事件：start 开一条、end 配对更新耗时/状态 */
function timelineStage(div, ev) {
    const tl = div && div._timeline;
    if (!tl) return;
    if (ev.status === 'start') {
        tl.stageOpen[ev.stage] = { ts: Date.now(), detail: ev.detail || '' };
        timelinePush(div, { type: 'stage', stage: ev.stage, status: 'start', detail: ev.detail || '', duration_ms: 0 });
    } else {
        timelinePush(div, {
            type: 'stage',
            stage: ev.stage,
            status: ev.status,
            detail: ev.detail || '',
            duration_ms: ev.duration_ms || 0,
        });
    }
}

/** 工具事件：call 开卡片（running）、result 按后端 seq 精确配对更新 */
function timelineTool(div, p) {
    const tl = div && div._timeline;
    if (!tl) return;
    if (p.kind === 'call') {
        const backendSeq = Number(p.seq);
        tl.toolOpen[String(backendSeq)] = {
            tool: String(p.tool || ''),
            backendSeq,
            ts: Date.now(),
        };
        timelinePush(div, {
            type: 'tool',
            backendSeq, // 后端 seq：result 按此精确配对（不再用内部号）
            tool: String(p.tool || ''),
            args_preview: String(p.args_preview || ''),
            skill_id: p.skill_id || null,
            ok: null,
            summary: '',
            duration_ms: 0,
        });
    } else {
        // result：按后端 call_seq 精确配对（替代原内部号/工具名 fallback——同名工具并行时也会错配）
        const backendSeq = Number(p.call_seq);
        const open = tl.toolOpen[String(backendSeq)];
        let matched = false;
        for (let i = tl.entries.length - 1; i >= 0; i--) {
            const e = tl.entries[i];
            if (e.type === 'tool' && e.ok === null && e.backendSeq === backendSeq) {
                e.ok = !!p.ok;
                e.summary = String(p.summary || '');
                e.duration_ms = open ? Date.now() - open.ts : 0;
                matched = true;
                break;
            }
        }
        // 兜底：后端 call_seq 未匹配到（事件乱序/丢失）时，按同工具最近未决条目更新
        if (!matched) {
            for (let i = tl.entries.length - 1; i >= 0; i--) {
                const e = tl.entries[i];
                if (e.type === 'tool' && e.ok === null && e.tool === String(p.tool || '')) {
                    e.ok = !!p.ok;
                    e.summary = String(p.summary || '');
                    e.duration_ms = 0;
                    break;
                }
            }
        }
        if (open) delete tl.toolOpen[String(backendSeq)];
        tl.dirty = true;
        if (!tl.rafPending) {
            tl.rafPending = true;
            requestAnimationFrame(() => {
                tl.rafPending = false;
                if (tl.dirty) {
                    tl.dirty = false;
                    renderProcessTimeline(div);
                }
            });
        }
        if (div._scroll) div._scroll();
    }
}

/** 工具调用轨迹可视化（兼容旧入口：新建消息时初始化 timeline 容器） */
function ensureToolTrace(div) {
    if (!div) return null;
    // 旧 tool-trace 容器已废弃，统一走 process-timeline
    if (!div._timeline) ensureTimelineState(div, '');
    return ensureProcessTimeline(div);
}
// 历史消息回放：根据持久化的工具调用记录重建轨迹卡片（兼容旧入口；新路径走 timeline）
function renderToolTraceFromRecords(div, records) {
    const trace = document.createElement('div');
    trace.className = 'tool-trace';
    trace.innerHTML = records.map(tc => {
        const cls = tc.ok === null || tc.ok === undefined ? 'running' : (tc.ok ? 'ok' : 'fail');
        const statusText = tc.ok === null || tc.ok === undefined ? 'running…' : (tc.ok ? 'done' : 'failed');
        const skillId = tc.skill_id ? String(tc.skill_id) : '';
        const skillBadge = skillId
            ? `<span class="tool-skill-badge" title="技能触发: ${escapeHtml(skillId)}">${escapeHtml(skillId.split(':').pop() || skillId)}</span>`
            : '';
        return `<div class="tool-card ${cls}"><span class="tool-name">${escapeHtml(String(tc.tool || ''))}</span>${skillBadge}<span class="tool-args">${escapeHtml(String(tc.args_preview || '').slice(0, 80))}</span><span class="tool-status" title="${escapeHtml(String(tc.summary || ''))}">${statusText}</span></div>`;
    }).join('');
    div.insertBefore(trace, div._body);
}
// 统一处理 agent:tool_call / agent:tool_result（聊天与 RAG 两个事件块共用）
function handleAgentToolEvent(e, rid) {
    const p = e.payload;
    if (!p || p.request_id !== rid || p.tool === undefined) return;
    const div = ensureStreamingAssistantDiv();
    ensureTimelineState(div, rid);
    ensureProcessTimeline(div);
    // 写入统一时间线
    timelineTool(div, p);
    // 保留持久化记录（供历史回放 expandToolHistory / renderToolTraceFromRecords）
    if (p.kind === 'call') {
        _streamingToolCalls.push({
            tool: String(p.tool),
            call_id: String(p.call_id || ''),
            args_preview: String(p.args_preview || ''),
            args: String(p.arguments || ''),
            skill_id: p.skill_id || null,
            seq: Number(p.seq),
            ok: null,
            summary: '',
            result: '',
        });
    } else {
        const rec = _streamingToolCalls.find(r => r.seq === Number(p.call_seq));
        if (rec) {
            rec.ok = !!p.ok;
            rec.summary = String(p.summary || '');
            if (typeof p.result === 'string' && p.result) {
                rec.result = p.result;
            }
        }
    }
    if (div._scroll) div._scroll();
}

// 将内存中的对话消息展开为发送给后端的协议消息数组（工具历史回流）：
// assistant 消息的工具调用轨迹（toolCalls 记录数组）还原为
//   {role:'assistant', tool_calls:[{id,name,arguments}]}
// 并紧随其后的 {role:'tool', tool_call_id, content:结果} 结果消息，
// 使模型在后续轮次能看到自己此前调用过哪些工具、拿到什么结果。
// 老数据（无 call_id 或 result 的记录）降级为普通文本消息，行为不变。
// P1-1：实现已收敛到 css_js/modules/chat-history.js（window.chatHistory），
// 本函数保留为薄包装以兼容既有调用点。
function expandToolHistory(chatMsgs) {
    return window.chatHistory.expandToolHistory(chatMsgs);
}

// ─── RAG 检索参数设置 ───
let ragSettings = null; // 延迟初始化，从后端加载
// P0-1：embedding 模型窗口（token），用于约束 chunk_size 上限。
// 本地模式回退 AGENT_LIMITS.ragDefaults.maxPositionEmbeddings；Tauri 下由 kb_embedding_info 下发覆盖。
let ragEmbedWindow = (typeof AGENT_LIMITS !== 'undefined' && AGENT_LIMITS.ragDefaults && AGENT_LIMITS.ragDefaults.maxPositionEmbeddings) || 512;
// M29：embedding 窗口信息是否有效（kb_embedding_info 成功且非 0）；false 时跳过前端 maxChunk 拒绝分支，
// 交由后端 kb_update_indexer_config 返回权威错误（避免 info 失败时按 512 兜底误拒 >512 的合法值）。
let ragEmbedWindowValid = false;
let ragSettingsTippy = null;
let kbWatcherTimer = null; // 知识库 watcher 事件防抖定时器（模块级，便于 agentCleanup 清除）
let fileWrittenTimer = null; // Agent 写文件事件防抖定时器（模块级，便于 agentCleanup 清除）
async function openRagSettings() {
    const overlay = document.getElementById('rag-settings-overlay');
    if (!overlay) return;
    // L36：先显示 overlay 再异步拉取配置（kb_embedding_info 可能触发 ONNX 初始化，避免阻塞首次打开）
    overlay.style.display = 'flex';
    // 从后端加载当前配置
    try {
        if (window.__TAURI__?.core?.invoke) {
            const cfg = await window.__TAURI__.core.invoke('kb_get_indexer_config');
            const rd = AGENT_LIMITS.ragDefaults; // 本地模式/后端缺省时的统一默认值（集中配置）
            ragSettings = ragSettings || {};
            document.getElementById('rag-setting-topk').value = cfg.top_k ?? rd.topK;
            document.getElementById('rag-setting-min-score').value = cfg.min_score ?? rd.minScore;
            document.getElementById('rag-setting-chunk-size').value = cfg.chunk_size ?? rd.chunkSize;
            document.getElementById('rag-setting-chunk-overlap').value = cfg.chunk_overlap ?? rd.chunkOverlap;
            document.getElementById('rag-setting-fusion-alpha').value = cfg.fusion_alpha ?? rd.fusionAlpha;
            document.getElementById('rag-setting-max-docs').value = cfg.max_context_docs ?? rd.maxContextDocs;
            document.getElementById('rag-setting-max-chunks').value = cfg.max_chunks_per_doc ?? rd.maxChunksPerDoc;
            document.getElementById('rag-setting-candidate-k').value = cfg.candidate_k ?? rd.candidateK;
            document.getElementById('rag-setting-rrf-k').value = cfg.rrf_k ?? rd.rrfK;
            document.getElementById('rag-setting-vec-min-score').value = cfg.vec_min_score ?? rd.vecMinScore;
            document.getElementById('rag-setting-rerank-min-score').value = cfg.rerank_min_score ?? rd.rerankMinScore;
            document.getElementById('rag-setting-bm25-msm').value = cfg.bm25_msm_ratio ?? rd.bm25MsmRatio;
            document.getElementById('rag-setting-reranker-enabled').checked = cfg.reranker_enabled ?? rd.rerankerEnabled;
            // M23：证据校验开关（对应后端 kb_update_indexer_config 的 evidence_check_enabled）
            const evidenceChk = document.getElementById('rag-setting-evidence-check');
            if (evidenceChk) evidenceChk.checked = cfg.evidence_check_enabled ?? false;
            // P0-1/M29：拉取 embedding 模型窗口 → 展示提示并约束 chunk_size 输入范围。
            // 仅当 info 成功且窗口非 0 时启用前端硬上限校验（ragEmbedWindowValid=true）；
            // info 失败/模型未就绪时跳过前端 maxChunk 拒绝分支，交给后端 kb_update_indexer_config 返回权威错误。
            let embedInfoOk = false;
            try {
                const info = await window.__TAURI__.core.invoke('kb_embedding_info');
                const win = info?.max_position_embeddings;
                if (typeof win === 'number' && win > 0) {
                    ragEmbedWindow = win;
                    ragEmbedWindowValid = true;
                    embedInfoOk = true;
                } else {
                    ragEmbedWindow = 512; // 模型未就绪/无窗口信息：不启用前端硬上限
                    ragEmbedWindowValid = false;
                }
            } catch (e) {
                ragEmbedWindow = 512; // 拉取失败：不启用前端硬上限（交给后端权威校验）
                ragEmbedWindowValid = false;
            }
            const chunkSizeInput = document.getElementById('rag-setting-chunk-size');
            const windowHint = document.getElementById('rag-setting-window-hint');
            if (embedInfoOk) {
                const maxChunk = Math.max(64, ragEmbedWindow - 8); // 窗口 - special tokens 预留
                if (chunkSizeInput) {
                    chunkSizeInput.min = 64;
                    chunkSizeInput.max = maxChunk;
                }
                if (windowHint) {
                    windowHint.textContent = `embedding 模型窗口: ${ragEmbedWindow} token（分块大小上限 ${maxChunk}）`;
                }
            } else if (windowHint) {
                windowHint.textContent = '未获取到 embedding 模型窗口，分块大小上限以后端校验为准';
            }
        } else {
            // L36：非 Tauri（本地模式）直接用默认窗口写入 hint 与输入框 max
            const win = (AGENT_LIMITS && AGENT_LIMITS.ragDefaults && AGENT_LIMITS.ragDefaults.maxPositionEmbeddings) || 512;
            ragEmbedWindow = win;
            ragEmbedWindowValid = true;
            const maxChunk = Math.max(64, win - 8); // 窗口 - special tokens 预留
            const chunkSizeInput = document.getElementById('rag-setting-chunk-size');
            if (chunkSizeInput) {
                chunkSizeInput.min = 64;
                chunkSizeInput.max = maxChunk;
            }
            const windowHint = document.getElementById('rag-setting-window-hint');
            if (windowHint) {
                windowHint.textContent = `embedding 模型窗口: ${win} token（分块大小上限 ${maxChunk}）`;
            }
        }
    } catch (e) {
        console.warn('[rag-settings] 加载配置失败:', e);
    }
    // 初始化 ⓘ 提示
    if (!ragSettingsTippy) {
        ragSettingsTippy = tippy(overlay.querySelectorAll('.setting-help'), {
            theme: 'custom',
            content: (ref) => ref.getAttribute('data-tippy-content'),
        });
    }
}

function closeRagSettings() {
    const overlay = document.getElementById('rag-settings-overlay');
    if (overlay) overlay.style.display = 'none';
    // 销毁 tippy 实例，避免下次打开时重复绑定
    if (ragSettingsTippy) {
        ragSettingsTippy.forEach(t => t.destroy());
        ragSettingsTippy = null;
    }
}

// ── 网络搜索配置（web_search 工具：Tavily / Brave / Exa） ──

/** 渲染网络搜索设置 popup（与模型/think/权限弹层一致：向上弹出） */
function renderWebSearchPopup() {
    const popup = document.getElementById('chat-websearch-popup');
    if (!popup) return;
    popup.innerHTML = `<div class="chat-toolbar-popup-warp">
        <div class="chat-toolbar-popup-title">网络搜索（web_search）</div>
        <div class="websearch-row">
            <label>启用</label>
            <input type="checkbox" id="web-search-enabled" style="width:auto;margin:0;">
            <span class="setting-help" data-tippy-content="启用后 Agent 获得 web_search 工具，可搜索互联网获取最新信息（新闻/文档/技术动态）。需同时配置提供商与 API Key。">ⓘ</span>
        </div>
        <div class="websearch-row">
            <label>提供商</label>
            <select id="web-search-provider" onchange="refreshWebSearchKeyField()">
                <option value="">请选择</option>
                <option value="tavily">Tavily（免费 1000 次/月）</option>
                <option value="brave">Brave Search（免费 2000 次/月）</option>
                <option value="exa">Exa（搜索 + 语义）</option>
            </select>
        </div>
        <div class="websearch-row">
            <label>API Key</label>
            <input type="password" id="web-search-api-key" placeholder="粘贴 API Key" autocomplete="off">
            <button class="btn btn-sm btn-success" id="web-search-test-btn" onclick="testWebSearch(event)">测试</button>
        </div>
        <div class="websearch-row">
            <label>返回条数</label>
            <input type="number" id="web-search-max-results" min="1" max="10" step="1" value="5">
        </div>
        <div class="websearch-status" id="web-search-status">未配置</div>
        <div class="websearch-footer">
            <button class="btn btn-sm btn-primary" onclick="saveWebSearchSettingsFromPopup()">保存</button>
        </div>
    </div>`;
    // 初始化 tippy（如果可用）
    if (typeof tippy === 'function') {
        tippy(popup.querySelectorAll('.setting-help'), {
            theme: 'custom',
            content: (ref) => ref.getAttribute('data-tippy-content'),
        });
    }
}

/**
 * 开关网络搜索设置面板（向上弹出，与权限/think 弹层一致）
 */
function toggleWebSearchPicker(event) {
    if (event) event.stopPropagation();
    const popup = document.getElementById('chat-websearch-popup');
    if (!popup) return;
    const isOpening = popup.style.display === 'none';
    // 打开时关闭其它弹层
    if (isOpening) {
        ['chat-skill-popup', 'chat-prompt-popup', 'chat-approval-popup', 'chat-model-select-popup', 'chat-think-popup'].forEach(id => {
            const el = document.getElementById(id);
            if (el) el.style.display = 'none';
        });
        renderWebSearchPopup();
        loadWebSearchSettings();
    }
    popup.style.display = isOpening ? 'block' : 'none';
}

/** 保存 popup 中的网络搜索配置（保存后更新状态行） */
/** 保存 popup 中的网络搜索配置（保存成功后关闭弹层并更新状态） */
async function saveWebSearchSettingsFromPopup() {
    await saveWebSearchSettings();
    updateWebSearchStatusDot();
    // 保存成功后关闭弹层（与其它设置弹层行为一致）
    const popup = document.getElementById('chat-websearch-popup');
    if (popup) popup.style.display = 'none';
}

/** 更新工具栏图标激活态：仅当「启用 + 已选提供商 + 该提供商 API key 已配置」全部满足时
 *  才显示绿色（websearch-active）；任一缺失（含仅开关打开但无 provider/key）→ 灰色。
 *  等价后端 WebSearchConfig::is_ready()，无需真实网络请求。 */
async function updateWebSearchStatusDot() {
    const trigger = document.getElementById('chat-websearch-trigger');
    if (!trigger) return;
    try {
        if (!window.__TAURI__?.core?.invoke) return;
        const cfg = await window.__TAURI__.core.invoke('web_search_config_get');
        // 三条件全满足才算就绪：enabled + provider 已选 + 该 provider 的 key 已配置
        const pid = (cfg.provider || '').trim();
        const keyInfo = pid && cfg.keys ? cfg.keys[pid] : null;
        const ready = !!(cfg.enabled && pid && keyInfo && keyInfo.configured);
        trigger.classList.toggle('websearch-active', ready);
        if (!ready) trigger.classList.remove('websearch-active');
    } catch (e) {
        trigger.classList.remove('websearch-active');
    }
}

/** 当前 Web 搜索配置缓存（含各提供商 key 掩码，provider 切换时据此回显） */
let _webSearchCfg = null;

/** 更新 key 输入框：显示当前所选提供商的 key 状态（已配置=掩码，未配置=空） */
function refreshWebSearchKeyField() {
    const providerEl = document.getElementById('web-search-provider');
    const keyEl = document.getElementById('web-search-api-key');
    if (!providerEl || !keyEl) return;
    const pid = (providerEl.value || '').trim();
    const keyInfo = pid && _webSearchCfg && _webSearchCfg.keys ? _webSearchCfg.keys[pid] : null;
    // 已配置显示掩码（占位提示已保存）；未配置清空
    keyEl.value = keyInfo && keyInfo.configured ? (keyInfo.masked || '') : '';
    keyEl.placeholder = keyInfo && keyInfo.configured ? '已保存，留空或输入新 Key 覆盖' : '粘贴 API Key';
}

/** 加载网络搜索配置到设置表单 */
async function loadWebSearchSettings() {
    const enabledEl = document.getElementById('web-search-enabled');
    const providerEl = document.getElementById('web-search-provider');
    const keyEl = document.getElementById('web-search-api-key');
    const maxEl = document.getElementById('web-search-max-results');
    const statusEl = document.getElementById('web-search-status');
    if (!enabledEl || !providerEl || !keyEl || !maxEl || !statusEl) return;
    try {
        if (!window.__TAURI__?.core?.invoke) return;
        const cfg = await window.__TAURI__.core.invoke('web_search_config_get');
        _webSearchCfg = cfg;
        enabledEl.checked = !!cfg.enabled;
        providerEl.value = cfg.provider || '';
        maxEl.value = cfg.max_results || 5;
        // 按当前提供商回显对应 key 状态（不串号）
        refreshWebSearchKeyField();
        statusEl.textContent = cfg.enabled && cfg.provider_label
            ? `已启用（${cfg.provider_label}，返回 ${cfg.max_results || 5} 条）`
            : '未配置';
    } catch (e) {
        console.warn('[web-search] 加载配置失败:', e);
        statusEl.textContent = '加载失败';
    }
}

/** 测试网络搜索连接（用当前表单值发起一次真实搜索） */
async function testWebSearch(event) {
    if (event) event.stopPropagation();
    const btn = document.getElementById('web-search-test-btn');
    const statusEl = document.getElementById('web-search-status');
    if (!btn || !statusEl) return;
    const enabledEl = document.getElementById('web-search-enabled');
    const providerEl = document.getElementById('web-search-provider');
    const keyEl = document.getElementById('web-search-api-key');
    const maxEl = document.getElementById('web-search-max-results');
    try {
        if (!window.__TAURI__?.core?.invoke) {
            statusEl.textContent = '仅 Tauri 模式可用';
            return;
        }
        const provider = (providerEl.value || '').trim();
        const key = (keyEl.value || '').trim();
        if (!enabledEl.checked || !provider) {
            statusEl.textContent = '请先启用并选择提供商';
            return;
        }
        // 掩码 key 直接传（后端回退该提供商已保存值）；空 key 且未配置过则提示
        if (!key) {
            const keyInfo = _webSearchCfg && _webSearchCfg.keys ? _webSearchCfg.keys[provider] : null;
            if (!keyInfo || !keyInfo.configured) {
                statusEl.textContent = '请先填写该提供商的 API Key';
                return;
            }
        }
        btn.disabled = true;
        btn.textContent = '测试中…';
        statusEl.textContent = '正在发起搜索…';
        const result = await window.__TAURI__.core.invoke('web_search_test', {
            provider,
            apiKey: key || null,
            maxResults: parseInt(maxEl.value, 10) || 5,
            query: 'mdgo 知识库',
        });
        statusEl.textContent = `✓ ${result.provider} 返回 ${result.count} 条（首条: ${(result.first_title || '无标题').slice(0, 30)}）`;
    } catch (e) {
        console.warn('[web-search] 测试连接失败:', e);
        statusEl.textContent = '✗ 连接失败: ' + ((e && e.message) || e);
    } finally {
        if (btn) {
            btn.disabled = false;
            btn.textContent = '测试';
        }
    }
}

/** 保存网络搜索配置（只写当前所选提供商的 key，不影响其它提供商） */
async function saveWebSearchSettings() {
    const enabledEl = document.getElementById('web-search-enabled');
    const providerEl = document.getElementById('web-search-provider');
    const keyEl = document.getElementById('web-search-api-key');
    const maxEl = document.getElementById('web-search-max-results');
    const statusEl = document.getElementById('web-search-status');
    if (!enabledEl || !providerEl || !keyEl || !maxEl || !statusEl) return;
    try {
        if (!window.__TAURI__?.core?.invoke) return;
        const provider = (providerEl.value || '').trim();
        let apiKey = keyEl.value || null;
        // 掩码 = 未修改 → 传掩码让后端保留原值；空 = 若当前提供商已配置过，保留原值；
        // 显式清空需区分：前端空值传 null 时后端保留原值——用户想清除 key 需输入空格？
        // 简化：空值但已配置 → 传掩码占位（保留）；输入明文 → 覆盖。
        if (apiKey === '' || apiKey === null) {
            const keyInfo = _webSearchCfg && _webSearchCfg.keys && provider ? _webSearchCfg.keys[provider] : null;
            apiKey = keyInfo && keyInfo.configured ? (keyInfo.masked || '') : '';
        }
        const result = await window.__TAURI__.core.invoke('web_search_config_set', {
            enabled: enabledEl.checked,
            provider: provider || null,
            apiKey: apiKey || null,
            maxResults: parseInt(maxEl.value, 10) || 5,
        });
        _webSearchCfg = result;
        // 回显保存后的掩码 key（当前提供商）
        refreshWebSearchKeyField();
        statusEl.textContent = result.enabled && result.provider_label
            ? `已启用（${result.provider_label}，返回 ${result.max_results || 5} 条）`
            : '未配置';
        updateWebSearchStatusDot();
    } catch (e) {
        console.warn('[web-search] 保存配置失败:', e);
        showNotification('保存网络搜索配置失败: ' + ((e && e.message) || e), 'error');
    }
}

async function saveRagSettings() {
    const topK = parseInt(document.getElementById('rag-setting-topk').value) || 10;
    const minScore = parseFloat(document.getElementById('rag-setting-min-score').value) || 0.3;
    // L33：parseInt 结果为 0（分块重叠为 0 即不重叠，是合法语义）不得被默认值吞掉，仅 NaN 回退默认
    const chunkSizeRaw = parseInt(document.getElementById('rag-setting-chunk-size').value, 10);
    const chunkSize = Number.isFinite(chunkSizeRaw) ? chunkSizeRaw : 448;
    const chunkOverlapRaw = parseInt(document.getElementById('rag-setting-chunk-overlap').value, 10);
    const chunkOverlap = Number.isFinite(chunkOverlapRaw) ? chunkOverlapRaw : 56;
    // P0-1：分块参数校验（与后端 kb_update_indexer_config 同规则，拒绝非法值——
    // chunk 超模型窗口会被静默截断，必须显式拒绝）
    const maxChunk = Math.max(64, ragEmbedWindow - 8);
    // M29：仅当 embedding 窗口信息有效（ragEmbedWindowValid）时执行 maxChunk 拒绝；
    // info 拉取失败/未就绪时跳过前端上限校验，交给后端 kb_update_indexer_config 返回权威错误。
    if (chunkSize < 64 || (ragEmbedWindowValid && chunkSize > maxChunk)) {
        const boundMsg = ragEmbedWindowValid
            ? `分块大小需在 [64, ${maxChunk}]（token）之间（embedding 模型窗口 ${ragEmbedWindow}）`
            : '分块大小不能小于 64（token）';
        showNotification(boundMsg, 'error');
        return;
    }
    if (chunkOverlap >= Math.floor(chunkSize / 2)) {
        showNotification(`分块重叠需小于分块大小的一半（当前分块大小 ${chunkSize}）`, 'error');
        return;
    }
    const fusionAlpha = parseFloat(document.getElementById('rag-setting-fusion-alpha').value) || 0.6;
    const maxContextDocs = parseInt(document.getElementById('rag-setting-max-docs').value) || 4;
    const maxChunksPerDoc = parseInt(document.getElementById('rag-setting-max-chunks').value) || 3;
    const candidateK = parseInt(document.getElementById('rag-setting-candidate-k').value) || 100;
    const rrfK = parseInt(document.getElementById('rag-setting-rrf-k').value) || 60;
    const vecMinScore = parseFloat(document.getElementById('rag-setting-vec-min-score').value) || 0.35;
    const rerankMinScore = parseFloat(document.getElementById('rag-setting-rerank-min-score').value) || 0.2;
    const bm25MsmRatio = parseFloat(document.getElementById('rag-setting-bm25-msm').value) || 0.6;
    const rerankerEnabled = document.getElementById('rag-setting-reranker-enabled').checked;
    // M23：证据校验开关（后端 kb_update_indexer_config 的 evidence_check_enabled）
    const evidenceCheckEnabled = !!document.getElementById('rag-setting-evidence-check')?.checked;
    // 更新本地状态
    ragSettings = { topK, minScore, chunkSize, chunkOverlap, fusionAlpha, maxContextDocs, maxChunksPerDoc, candidateK, rrfK, vecMinScore, rerankMinScore, bm25MsmRatio, rerankerEnabled, evidenceCheckEnabled };
    // 持久化到后端
    try {
        if (window.__TAURI__?.core?.invoke) {
            await window.__TAURI__.core.invoke('kb_update_indexer_config', {
                chunkSize,
                chunkOverlap,
                topK,
                minScore,
                fusionAlpha,
                maxContextDocs,
                maxChunksPerDoc,
                candidateK,
                rrfK,
                vecMinScore,
                rerankMinScore,
                bm25MsmRatio,
                rerankerEnabled,
                evidenceCheckEnabled,
            });
            showNotification('RAG 参数已保存', 'success');
        }
    } catch (e) {
        console.error('[rag-settings] 保存配置失败:', e);
        showNotification('保存 RAG 参数失败: ' + (e.message || e), 'error');
    }
    // 关闭面板
    closeRagSettings();
}

async function sendRagQuery(text) {
    const dirHandle = getRootHandle();
    const path = dirHandle ? getDirPath(dirHandle) : null;
    if (!path) {
        console.warn('[rag] sendRagQuery: no directory selected');
        removeChatTyping();
        showNotification('❌ 未选择知识库目录', 'error');
        return;
    }

    console.debug('[rag] sendRagQuery ENTRY text_len=' + text.length + ' dir=' + path);
    chatStreaming = true;
    updateChatSendButton();
    chatAbortController = new AbortController();
    _chatStreamingDiv = null;
    _chatStreamingSources = null;
    _chatStreamingStatusMsg = '';
    _chatStreamingFullContent = '';
    _streamingToolCalls = [];
    showChatTyping();

    const requestId = crypto.randomUUID();
    // Phase 2：快照会话 ID——视图切换后 cleanupChatState 会清空全局 currentSessionId，
    // 但后台任务完成（rag:done）时仍需按原会话落库，故固化到局部变量。
    const ragSessionId = currentSessionId;
    let ragDone = false;
    let partialSaved = false; // 防止错误/断联路径重复保存半截消息
    _partialAutoSaved = false; // 新一轮请求重置自动保存防重
    console.debug('[rag] requestId=' + requestId);

    // 取消时通知后端
    const abortHandler = () => {
        console.debug('[rag] abort triggered requestId=' + requestId);
        window.__TAURI__.core.invoke('kb_cancel_task', { requestId }).catch(() => { });
    };
    chatAbortController.signal.addEventListener('abort', abortHandler);

    // 监听 Tauri 事件
    // 流式渲染节流（与 llm 路径一致）：rAF 合并 delta、流式轻量渲染（跳过 DOMPurify），
    // rag:done 用完整 renderChatMarkdown（含 sanitize）覆盖最终版
    let _streamRaf = 0;
    const _cancelStreamRender = () => {
        if (_streamRaf) {
            cancelAnimationFrame(_streamRaf);
            _streamRaf = 0;
        }
    };
    const _scheduleStreamRender = () => {
        if (_streamRaf) return;
        _streamRaf = requestAnimationFrame(() => {
            _streamRaf = 0;
            const div = _chatStreamingDiv;
            if (div && div._body) {
                div._body.innerHTML = `<div class="markdown-body" style="zoom: 1;background: transparent;">${renderChatMarkdownStream(_chatStreamingFullContent)}</div>`;
            }
            if (div && window._chatAutoScroll !== false) {
                const container = document.getElementById('chat-messages');
                if (container) container.scrollTop = container.scrollHeight;
            }
        });
    };
    let unlisteners;
    try {
        unlisteners = await Promise.all([
            window.__TAURI__.event.listen('rag:delta', (e) => {
                if (e.payload.request_id !== requestId) return;
                const delta = e.payload.content;
                if (!delta) return;
                _chatStreamingFullContent += delta;
                if (!_chatStreamingDiv) {
                    console.debug('[rag] FIRST delta received len=' + delta.length);
                    removeChatTyping();
                    const container = document.getElementById('chat-messages');
                    const div = getMessageAssistant();
                    container.appendChild(div);
                    _chatStreamingDiv = div;
                }
                if (_chatStreamingDiv) {
                    _chatStreamingDiv._rawContent = _chatStreamingFullContent;
                }
                // rAF 节流渲染 + 滚动（合并到同一帧）
                _scheduleStreamRender();
            }),
            window.__TAURI__.event.listen('trace:event', (e) => {
                const payload = e.payload ? e.payload : {};
                const tid = payload.request_id;
                const tev = payload.events;
                if (!tid || tid !== requestId) return;
                if (!Array.isArray(tev)) return;
                // 阶段事件写入统一时间线（保留 __chatTraceMap 兼容旧面板）
                const div = _chatStreamingDiv;
                if (div && div._timeline) {
                    tev.forEach(ev => timelineStage(div, ev));
                } else {
                    const existing = window.__chatTraceMap[tid] ? window.__chatTraceMap[tid] : [];
                    window.__chatTraceMap[tid] = existing.concat(tev);
                }
            }),
            window.__TAURI__.event.listen('rag:thinking', (e) => {
                if (e.payload.request_id !== requestId) return;
                const content = e.payload.content;
                if (!content) return;
                const div = ensureStreamingAssistantDiv();
                ensureTimelineState(div, requestId);
                ensureProcessTimeline(div);
                timelineThinking(div, content);
            }),
            window.__TAURI__.event.listen('rag:status', (e) => {
                if (e.payload.request_id !== requestId) return;
                _chatStreamingStatusMsg = e.payload.message;
                console.debug('[rag] status event stage=' + e.payload.stage + ' msg=' + e.payload.message);
                if (_chatStreamingDiv) {
                    updateStreamingStatus(_chatStreamingDiv, _chatStreamingStatusMsg);
                }
            }),
            window.__TAURI__.event.listen('rag:done', async (e) => {
                if (e.payload.request_id !== requestId) return;
                console.debug('[rag] done event requestId=' + requestId + ' content_len=' + (e.payload.content || '').length + ' sources=' + (e.payload.sources || []).length + ' tokens_in=' + e.payload.prompt_tokens + ' tokens_out=' + e.payload.completion_tokens + ' cached_in=' + e.payload.cached_input_tokens);
                ragDone = true;
                _chatStreamingStatusMsg = '';
                // 立即快照本次响应所需数据为局部变量：
                // finally 会清空 _chatStreamingDiv / _streamingToolCalls / _chatStreamingFullContent，
                // 必须先固化，保证落库与 UI 后处理不受竞态影响。
                // 取消未执行的流式渲染帧（done 后用完整渲染覆盖，避免轻量版回写）
                _cancelStreamRender();
                const fullContent = e.payload.content;
                const sources = e.payload.sources || [];
                const promptTokens = e.payload.prompt_tokens || 0;
                const completionTokens = e.payload.completion_tokens || 0;
                const cachedInputTokens = e.payload.cached_input_tokens || 0;
                const cacheCreationInputTokens = e.payload.cache_creation_input_tokens || 0;
                const toolCallsSnapshot = _streamingToolCalls.slice();
                let streamingDiv = _chatStreamingDiv;
                // 降级路径均依赖此变量，不赋值会导致引用在保存/断联时丢失
                _chatStreamingSources = sources;
                if (streamingDiv) {
                    const st = streamingDiv.querySelector('.chat-message-header-status');
                    if (st) st.remove();
                }
                // 时间线已实时渲染（thinking/tool/stage 均即时写入），done 时收尾：
                // 1) 把未落条的 thinking 缓冲固化 2) tool 状态兜底（result 丢失闭环）3) 清理兼容 map
                if (streamingDiv && streamingDiv._timeline) {
                    const tl = streamingDiv._timeline;
                    flushThinkingBuffer(streamingDiv);
                    // 兜底：请求结束仍未配对 result 的工具条目标记为 interrupted
                    let dirty = false;
                    tl.entries.forEach(e => {
                        if (e.type === 'tool' && (e.ok === null || e.ok === undefined)) {
                            e.ok = false;
                            e.summary = e.summary || '请求中断，结果未返回';
                            dirty = true;
                        }
                    });
                    if (dirty) tl.dirty = true;
                    renderProcessTimeline(streamingDiv);
                }
                delete window.__chatTraceMap[requestId];
                // 内存态（当前会话立即可见，不依赖落库结果）
                if (fullContent) {
                    chatMessages.push({ role: 'assistant', content: fullContent, sources: sources, toolCalls: toolCallsSnapshot, created_at: Date.now() });
                    updateTurnCounter();
                }
                // ===== 落库与 UI 后处理并行（互不依赖；各自异常隔离，不互相阻断） =====
                const persistTask = (async () => {
                    if (!ragSessionId || !fullContent) return;
                    try {
                        const savedMsg = await saveChatMessageWithRetry({
                            dirPath: currentRootPath,
                            sessionId: ragSessionId,
                            role: 'assistant',
                            content: fullContent,
                            tokenCount: completionTokens,
                            toolCalls: JSON.stringify(toolCallsSnapshot),
                            thinking: getStreamingThinking(),
                        });
                        // 回填真实 message_id（删除本轮对话时按 id 精确匹配后端删除）
                        if (savedMsg && savedMsg.id) {
                            const lastMsg = chatMessages[chatMessages.length - 1];
                            if (lastMsg && lastMsg.role === 'assistant') lastMsg.id = savedMsg.id;
                            if (streamingDiv && !streamingDiv.dataset.messageId) {
                                streamingDiv.dataset.messageId = savedMsg.id;
                            }
                        }
                        // 保存引用来源
                        if (sources.length > 0 && savedMsg?.id) {
                            const sourceEntries = sources.map(s => ({
                                id: crypto.randomUUID(),
                                message_id: savedMsg.id,
                                doc_name: s.doc_name || '未知文档',
                                score: s.score || 0,
                                snippet: s.text || '',
                                path_json: s.path_json || '',
                            }));
                            await window.__TAURI__.core.invoke('chat_message_sources_save', {
                                dirPath: currentRootPath,
                                messageId: savedMsg.id,
                                sources: sourceEntries,
                            }).catch(e => console.warn('保存引用来源失败:', e));
                        }
                        // 记录到 AI 历史（使 AI 使用统计包含 RAG 对话）
                        if (isTauriVisit() && fullContent) {
                            window.addAIHistoryItemTauri({
                                dirPath: currentRootPath, // 显式传入路径
                                type: 'chat',
                                label: (chatMessages[0]?.content || '对话').slice(0, 50),
                                prompt: text,
                                result: fullContent,
                                fileName: '',
                                filePath: '',
                                tokenCount: completionTokens || 0,
                            }).catch(e => console.warn('记录 AI 历史失败:', e));
                        }
                        // 会话列表刷新一次（含重命名检查；传快照 ragSessionId 防视图切换漂移）
                        await refreshChatSessionListOnce(ragSessionId);
                    } catch (e) {
                        console.error('[rag] 保存助手消息失败（重试后仍失败）:', e);
                        showNotification('⚠ 回复未能保存到对话历史，请检查数据库', 'warning');
                    }
                })();
                const uiTask = (async () => {
                    try {
                        // 更新上下文使用率（模型上下文窗口占用率）
                        if (promptTokens > 0) {
                            updateContextUsage(promptTokens, LOCAL_LLM_CONTEXT_LENGTH || 10000);
                        }
                        // 更新缓存命中率（DSH 口径；provider 未上报缓存字段时显示占位）
                        updateCacheRate({
                            prompt_tokens: promptTokens,
                            cached_input_tokens: cachedInputTokens,
                            cache_creation_input_tokens: cacheCreationInputTokens,
                        }, 'rag');
                        // 流式期间为轻量渲染（未 sanitize），done 后用完整渲染覆盖（含 DOMPurify）
                        if (streamingDiv) {
                            streamingDiv._rawContent = fullContent;
                        }
                        if (streamingDiv && streamingDiv._body) {
                            streamingDiv._body.innerHTML = `<div class="markdown-body" style="zoom: 1;background: transparent;">${renderChatMarkdown(fullContent)}</div>`;
                        }
                        // 流式结束后对代码块进行语法高亮（复制按钮读取原始 Markdown）
                        if (streamingDiv && streamingDiv._body) {
                            await highlightChatCodeBlocks(streamingDiv._body);
                        }
                        removeChatTyping();
                        // 兜底：如果流式过程中未能创建 DOM（如 _statusEl 等异常导致 delta 处理中断），在此创建
                        if (!streamingDiv && fullContent) {
                            const container = document.getElementById('chat-messages');
                            const div = getMessageAssistant();
                            div._rawContent = fullContent;
                            div._body.innerHTML = `<div class="markdown-body" style="zoom: 1;background: transparent;">${renderChatMarkdown(fullContent)}</div>`;
                            container.appendChild(div);
                            streamingDiv = div;
                        }
                        // 追加引用来源 UI
                        if (sources.length > 0 && streamingDiv) {
                            streamingDiv._chatSources = sources;
                            renderChatSources(streamingDiv, sources);
                        }
                        // 生成完成后追加底部操作按钮（复制/保存，hover 显示）+ 消息时间，必须为最后一个子元素
                        if (streamingDiv) {
                            streamingDiv.appendChild(createMessageFooter('assistant', Date.now()));
                        }
                        // 流式结束后滚动到底部，确保最新消息和引用完整可见
                        if (window._chatAutoScroll !== false) {
                            const chatContainer = document.getElementById('chat-messages');
                            if (chatContainer) {
                                requestAnimationFrame(() => {
                                    if (window._chatAutoScroll !== false) {
                                        chatContainer.scrollTop = chatContainer.scrollHeight;
                                    }
                                });
                            }
                        }
                    } catch (e) {
                        console.error('[rag] done UI 后处理异常:', e);
                    }
                })();
                await Promise.allSettled([persistTask, uiTask]);
            }),
            window.__TAURI__.event.listen('rag:error', async (e) => {
                if (e.payload.request_id !== requestId) return;
                console.error('[rag] error event requestId=' + requestId + ' msg=' + e.payload.message);
                ragDone = true;
                _chatStreamingStatusMsg = '';
                if (_chatStreamingDiv) {
                    const st = _chatStreamingDiv.querySelector('.chat-message-header-status');
                    if (st) st.remove();
                }
                removeChatTyping();
                // 生成失败/断联：保留已生成的部分内容（落库 + 入内存），
                // 避免重新打开页面后回复的半截消息消失
                if (!partialSaved && _chatStreamingFullContent && _chatStreamingFullContent.trim()) {
                    partialSaved = true;
                    await savePartialAssistantMessage(ragSessionId);
                }
                showNotification('✗ ' + e.payload.message, 'error');
            }),
            window.__TAURI__.event.listen('agent:tool_call', (e) => handleAgentToolEvent(e, requestId)),
            window.__TAURI__.event.listen('agent:tool_result', (e) => handleAgentToolEvent(e, requestId)),
        ]);
    } catch (e) {
        // 事件监听注册失败：复位流式状态，避免 chatStreaming 卡死 → 停止按钮失效/会话无法停止
        console.error('[rag] 事件监听注册失败:', e);
        chatStreaming = false;
        updateChatSendButton();
        chatAbortController = null;
        _chatStreamingDiv = null;
        _chatStreamingSources = null;
        _chatStreamingStatusMsg = '';
        _chatStreamingFullContent = '';
        removeChatTyping();
        showNotification('✗ 无法建立流式通道: ' + (e.message || e), 'error');
        return;
    }

    try {
        const histMessages = trimChatHistory(expandToolHistory(chatMessages), 0.4);
        // 发送前使用裁剪后的消息估算 token，展示实际发送给 LLM 的上下文占比
        const estTokens = estimateMessagesTokens(histMessages);
        if (estTokens > 0) {
            updateContextUsage(estTokens, LOCAL_LLM_CONTEXT_LENGTH || 10000);
        }
        await window.__TAURI__.core.invoke('agent_query', {
            dirPath: path,
            query: text,
            messages: histMessages,
            requestId,
            topK: ragSettings?.topK ?? AGENT_LIMITS.ragDefaults.topK,
            sessionId: ragSessionId,
        });
    } catch (err) {
        console.error('[rag] invoke failed requestId=' + requestId + ' err=' + (err.message || err));
        if (err.name !== 'AbortError' && !ragDone) {
            // 断联/网络失败：保留已生成的部分内容，避免重新打开页面后丢失
            if (!partialSaved && _chatStreamingFullContent && _chatStreamingFullContent.trim()) {
                partialSaved = true;
                await savePartialAssistantMessage(ragSessionId);
            }
            showNotification('✗ RAG 查询失败: ' + (err.message || err), 'error');
        } else if (err.name === 'AbortError' && !ragDone) {
            partialSaved = true;
            await savePartialAssistantMessage(ragSessionId);
        }
    } finally {
        chatAbortController?.signal.removeEventListener('abort', abortHandler);
        unlisteners.forEach(u => u());
        // 清理未执行的流式渲染帧（请求被取消/失败时避免残留 rAF）
        _cancelStreamRender();
        // 兜底：请求结束但未正常 done、半截内容仍未保存时落库（覆盖事件/调用顺序竞态）
        if (!partialSaved && !ragDone && _chatStreamingFullContent && _chatStreamingFullContent.trim()) {
            partialSaved = true;
            await savePartialAssistantMessage(ragSessionId);
        }
        chatStreaming = false;
        updateChatSendButton();
        chatAbortController = null;
        _chatStreamingDiv = null;
        _chatStreamingSources = null;
        _chatStreamingFullContent = '';
        _streamingToolCalls = [];
        await rememberLastChatSession();
        console.debug('[rag] cleanup done requestId=' + requestId);
    }
}

async function AgentInit() {
    // Tauri 模式下显示 RAG/Agent 模式按钮
    if (isTauriVisit()) {
        const ragBtn = document.getElementById('chat-mode-rag');
        if (ragBtn) ragBtn.style.display = '';

        // 预加载 RAG 检索参数配置
        try {
            if (window.__TAURI__?.core?.invoke) {
                const cfg = await window.__TAURI__.core.invoke('kb_get_indexer_config');
                const rd = AGENT_LIMITS.ragDefaults;
                ragSettings = {
                    topK: cfg.top_k ?? rd.topK,
                    minScore: cfg.min_score ?? rd.minScore,
                    chunkSize: cfg.chunk_size ?? rd.chunkSize,
                    chunkOverlap: cfg.chunk_overlap ?? rd.chunkOverlap,
                    fusionAlpha: cfg.fusion_alpha ?? rd.fusionAlpha,
                    maxContextDocs: cfg.max_context_docs ?? rd.maxContextDocs,
                    maxChunksPerDoc: cfg.max_chunks_per_doc ?? rd.maxChunksPerDoc,
                };
            }
        } catch (e) {
            console.warn('[rag] 加载配置失败:', e);
        }

        // 监听知识库文件变更事件，自动刷新状态（携带变更文件相对路径，前端 800ms 防抖合并）
        try {
            const { listen } = window.__TAURI__.event;
            const kbWatcherPaths = new Set();
            listen('kb-watcher-event', (e) => {
                const paths = Array.isArray(e && e.payload) ? e.payload : [];
                for (const p of paths) {
                    if (typeof p === 'string' && p) kbWatcherPaths.add(p);
                }
                if (kbWatcherTimer) clearTimeout(kbWatcherTimer);
                kbWatcherTimer = setTimeout(() => {
                    kbWatcherTimer = null;
                    const changedPaths = Array.from(kbWatcherPaths);
                    kbWatcherPaths.clear();
                    if (knowledgeContainer.style.display !== 'none') refreshKnowledgeStatus();
                }, 800);
            });
        } catch (e) {
            console.warn('注册 watcher 事件监听失败:', e);
        }
        // 监听 Agent 写文件事件（write / edit / multi_edit 成功路径发射）：
        // 增量同步左侧文件树并定位写入的文件（见 main.html handleAgentFileWritten）。
        // 关键：绝不触发全量扫描/重建——10 万级知识库下每次写入全量刷新
        // （walkdir 全扫 + 大 IPC 载荷 + DOM 重建）不可接受；新建走增量插入，
        // 编辑/覆盖已有文件零成本。
        try {
            const { listen } = window.__TAURI__.event;
            const writtenMap = new Map(); // rel_path → payload {created,size,mtime}
            listen('agent:file-written', (e) => {
                const rel = e && e.payload && e.payload.rel_path;
                if (typeof rel !== 'string' || !rel) return;
                writtenMap.set(rel, e.payload || {});
                if (fileWrittenTimer) clearTimeout(fileWrittenTimer);
                fileWrittenTimer = setTimeout(async () => {
                    fileWrittenTimer = null;
                    const entries = Array.from(writtenMap.entries());
                    writtenMap.clear();
                    try {
                        if (typeof handleAgentFileWritten === 'function') {
                            // 逐个增量处理（新建插入 / 编辑定位）；600ms 防抖已合并 burst，
                            // 最终定位停留在最后一个写入的文件
                            for (const [rel, payload] of entries) {
                                await handleAgentFileWritten(rel, payload);
                                recordOperation(basename(rel), rel, OPERATION_TYPES.CREATE);
                            }
                        } else {
                            // 兜底（主脚本函数未就绪）：全量刷新 + 定位
                            if (typeof refreshTree === 'function') await refreshTree(false);
                            const last = entries.length > 0 ? entries[entries.length - 1][0] : null;
                            if (last && typeof navigateToFile === 'function') {
                                await navigateToFile(last);
                            }
                        }
                    } catch (err) {
                        console.warn('[agent] 文件写入后同步文件树失败:', err);
                    }
                }, 600);
            });
        } catch (e) {
            console.warn('注册 agent:file-written 监听失败:', e);
        }
        // 监听 watcher 错误事件，通知用户
        try {
            const { listen } = window.__TAURI__.event;
            listen('watcher-error', (e) => {
                console.warn('[watcher] 错误:', e.payload);
                showNotification('文件监听错误: ' + String(e.payload), 'warning');
            });
        } catch (e) {
            console.warn('注册 watcher-error 监听失败:', e);
        }
        // 监听聊天会话索引进度反馈
        try {
            const { listen } = window.__TAURI__.event;
            listen('chat-index-error', (e) => {
                console.warn('[chat] 索引失败:', e.payload);
            });
        } catch (e) {
            console.warn('注册 chat-index 事件监听失败:', e);
        }
        // 应用启动时，root 目录就绪即自动启动 watcher（静默运行），并同步索引开关
        if (getRootHandle()) {
            setTimeout(startWatcherIfNeeded, 500);
            setTimeout(syncIndexingEnabled, 1000);
        }
    }
}

// ── Trace 阶段面板：按 request_id 渲染阶段耗时（trace:event）──
window.__chatTraceMap = {};
function traceEscapeHtml(s) {
    return String(s).replace(/[&<>"']/g, function (ch) {
        return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[ch];
    });
}
function renderTracePanel(events) {
    if (!events) return '';
    if (!events.length) return '';
    const rows = events.map(function (ev) {
        let icon = '▶';
        if (ev.status === 'ok') icon = '✅';
        else if (ev.status === 'error') icon = '❌';
        else if (ev.status === 'cancelled') icon = '⛔';
        else if (ev.status === 'denied') icon = '🚫';
        const dur = ev.status === 'start' ? '' : (ev.duration_ms + 'ms');
        const detail = ev.detail ? (' — ' + traceEscapeHtml(ev.detail)) : '';
        return '<div style="display:flex;justify-content:space-between;gap:8px;padding:2px 4px;font-size:12px;line-height:1.5;">'
            + '<span style="color:#888;">' + icon + ' ' + traceEscapeHtml(ev.stage) + detail + '</span>'
            + '<span style="color:#888;white-space:nowrap;">' + dur + '</span></div>';
    }).join('');
    return '<details style="margin-top:4px;">'
        + '<summary style="cursor:pointer;font-size:12px;color:#888;user-select:none;">⚙ 阶段耗时（trace）</summary>'
        + '<div style="margin-top:4px;">' + rows + '</div></details>';
}

/**
 * 清理 Agent 模块残留（界面切换离开时由主页面 cleanupData 调用）：
 * RAG 模式的聊天流式状态/会话由主页面 finalizeChatAndCleanup 统一清理，
 * 本函数只处理 Agent 自身持有的资源：RAG 设置弹窗与 tippy 实例。
 * AgentInit 注册的全局监听（kb-watcher/approval/plan 等）为应用级事件，
 * 不随视图切换销毁，保持常驻。
 */
function agentCleanup() {
    const overlay = document.getElementById('rag-settings-overlay');
    if (overlay && overlay.style.display !== 'none') {
        closeRagSettings();
    } else if (ragSettingsTippy) {
        ragSettingsTippy.forEach(t => t.destroy());
        ragSettingsTippy = null;
    }
    // 清除知识库 watcher 防抖定时器：切换视图后不再执行 pending 刷新回调
    if (kbWatcherTimer) {
        clearTimeout(kbWatcherTimer);
        kbWatcherTimer = null;
    }
    // 清除写文件事件防抖定时器（同上）
    if (fileWrittenTimer) {
        clearTimeout(fileWrittenTimer);
        fileWrittenTimer = null;
    }
}
