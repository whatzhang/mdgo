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
function ensureToolTrace(div) {
    if (!div) return null;
    if (!div._toolTrace) {
        const trace = document.createElement('div');
        trace.className = 'tool-trace';
        div.insertBefore(trace, div._body);
        div._toolTrace = trace;
    }
    return div._toolTrace;
}
// 历史消息回放：根据持久化的工具调用记录重建轨迹卡片
function renderToolTraceFromRecords(div, records) {
    const trace = document.createElement('div');
    trace.className = 'tool-trace';
    trace.innerHTML = records.map(tc => {
        const cls = tc.ok === null || tc.ok === undefined ? 'running' : (tc.ok ? 'ok' : 'fail');
        const statusText = tc.ok === null || tc.ok === undefined ? '执行中…' : (tc.ok ? '✓ 完成' : '✗ 失败');
        const skillId = tc.skill_id ? String(tc.skill_id) : '';
        const skillBadge = skillId
            ? `<span class="tool-skill-badge" title="技能触发: ${escapeHtml(skillId)}">⚡${escapeHtml(skillId.split(':').pop() || skillId)}</span>`
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
    const trace = ensureToolTrace(div);
    if (!trace) return;
    const safeTool = escapeHtml(String(p.tool));
    // 技能来源标签（如果有 skill_id）
    const skillBadge = p.skill_id ? `<span class="tool-skill-badge" title="技能触发: ${escapeHtml(p.skill_id)}">⚡${escapeHtml(p.skill_id.split(':').pop() || p.skill_id)}</span>` : '';
    // 以 kind 字段区分 call/result（call 事件后端序列化后含 call_seq:0，
    // 不能再用 call_seq === undefined 判定）
    if (p.kind === 'call') {
        // call 事件：新增卡片（执行中）
        const card = document.createElement('div');
        card.className = 'tool-card running';
        card.dataset.seq = String(p.seq);
        // P1-14：记录开始时间，供 result 事件计算耗时徽标
        card.dataset.startTs = String(Date.now());
        const args = escapeHtml(String(p.args_preview || '').slice(0, 80));
        card.innerHTML = `<span class="tool-name">${safeTool}</span>${skillBadge}<span class="tool-args">${args}</span><span class="tool-status">执行中…</span>`;
        trace.appendChild(card);
        // 记录到持久化列表（待 result 事件补全 ok/summary/result）
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
        // result 事件：按 call_seq 配对卡片并更新状态
        const card = trace.querySelector(`.tool-card[data-seq="${p.call_seq}"]`);
        if (card) {
            card.classList.remove('running');
            card.classList.add(p.ok ? 'ok' : 'fail');
            const st = card.querySelector('.tool-status');
            if (st) {
                st.textContent = p.ok ? '✓ 完成' : '✗ 失败';
                st.title = String(p.summary || '');
            }
            // P1-14：耗时徽标（call 事件记录的开始时间）
            const startTs = Number(card.dataset.startTs || 0);
            if (startTs > 0) {
                const cost = document.createElement('span');
                cost.className = 'tool-cost';
                cost.textContent = ((Date.now() - startTs) / 1000).toFixed(1) + 's';
                cost.style.cssText = 'margin-left:0.4rem;font-size:0.65rem;color:#999;';
                card.appendChild(cost);
            }
            // P1-14：点击卡片展开/收起完整结果摘要（可检视过程）
            const detail = document.createElement('div');
            detail.className = 'tool-detail';
            detail.style.cssText = 'display:none;margin-top:0.3rem;padding:0.3rem 0.5rem;background:rgba(0,0,0,0.04);border-radius:4px;font-size:0.72rem;color:#666;white-space:pre-wrap;word-break:break-all;max-height:12rem;overflow:auto;';
            detail.textContent = String(p.summary || '');
            card.appendChild(detail);
            // P2 输出结构化：按工具类型渲染增强卡片（git_diff 文件改动列表等）
            if (p.structured && typeof p.structured === 'object') {
                const sEl = document.createElement('div');
                sEl.className = 'tool-structured';
                sEl.style.cssText = 'margin-top:0.3rem;padding:0.3rem 0.5rem;background:rgba(0,0,0,0.03);border-radius:4px;font-size:0.72rem;color:#555;';
                if (p.tool === 'git_diff' && Array.isArray(p.structured.files)) {
                    const title = document.createElement('div');
                    title.textContent = `文件改动（${p.structured.files.length}）：`;
                    title.style.cssText = 'color:#666;margin-bottom:0.2rem;';
                    sEl.appendChild(title);
                    for (const f of p.structured.files) {
                        const row = document.createElement('div');
                        const fpath = String(f.path || '');
                        const adds = Number(f.additions || 0);
                        const dels = Number(f.deletions || 0);
                        row.innerHTML = `<span style="word-break:break-all;">${escapeHtml(fpath)}</span> <span style="color:#2e7d32;">+${adds}</span> <span style="color:#c62828;">-${dels}</span>`;
                        sEl.appendChild(row);
                    }
                    card.appendChild(sEl);
                }
                // 其他工具的结构化渲染可在此扩展（grep/ls 列表等）
            }
            card.style.cursor = 'pointer';
            card.addEventListener('click', (ev) => {
                ev.stopPropagation();
                const d = card.querySelector('.tool-detail');
                if (d) d.style.display = d.style.display === 'none' ? 'block' : 'none';
            });
        }
        // 配对持久化记录并补全结果状态（含完整结果文本，供历史回放）
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
function expandToolHistory(chatMsgs) {
    const out = [];
    for (const m of chatMsgs) {
        const tools = (Array.isArray(m.toolCalls) ? m.toolCalls : []).filter(tc => tc && tc.call_id && tc.tool);
        if (m.role === 'assistant' && tools.length > 0) {
            const protocolTools = tools.map(tc => ({
                id: String(tc.call_id),
                name: String(tc.tool),
                arguments: (typeof tc.args === 'string' && tc.args) ? tc.args : '{}',
            }));
            out.push({ id: m.id || null, role: 'assistant', content: m.content, tool_calls: protocolTools });
            for (const tc of tools) {
                if (typeof tc.result === 'string' && tc.result) {
                    out.push({ id: m.id || null, role: 'tool', content: tc.result, tool_call_id: String(tc.call_id) });
                }
            }
        } else {
            out.push({ id: m.id || null, role: m.role, content: m.content });
        }
    }
    return out;
}

// ─── RAG 检索参数设置 ───
let ragSettings = null; // 延迟初始化，从后端加载
let ragSettingsTippy = null;
let kbWatcherTimer = null; // 知识库 watcher 事件防抖定时器（模块级，便于 agentCleanup 清除）
async function openRagSettings() {
    const overlay = document.getElementById('rag-settings-overlay');
    if (!overlay) return;
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
        }
    } catch (e) {
        console.warn('[rag-settings] 加载配置失败:', e);
    }
    overlay.style.display = 'flex';
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

async function saveRagSettings() {
    const topK = parseInt(document.getElementById('rag-setting-topk').value) || 10;
    const minScore = parseFloat(document.getElementById('rag-setting-min-score').value) || 0.3;
    const chunkSize = parseInt(document.getElementById('rag-setting-chunk-size').value) || 448;
    const chunkOverlap = parseInt(document.getElementById('rag-setting-chunk-overlap').value) || 56;
    const fusionAlpha = parseFloat(document.getElementById('rag-setting-fusion-alpha').value) || 0.6;
    const maxContextDocs = parseInt(document.getElementById('rag-setting-max-docs').value) || 4;
    const maxChunksPerDoc = parseInt(document.getElementById('rag-setting-max-chunks').value) || 3;
    const candidateK = parseInt(document.getElementById('rag-setting-candidate-k').value) || 100;
    const rrfK = parseInt(document.getElementById('rag-setting-rrf-k').value) || 60;
    const vecMinScore = parseFloat(document.getElementById('rag-setting-vec-min-score').value) || 0.35;
    const rerankMinScore = parseFloat(document.getElementById('rag-setting-rerank-min-score').value) || 0.2;
    const bm25MsmRatio = parseFloat(document.getElementById('rag-setting-bm25-msm').value) || 0.6;
    const rerankerEnabled = document.getElementById('rag-setting-reranker-enabled').checked;
    // 更新本地状态
    ragSettings = { topK, minScore, chunkSize, chunkOverlap, fusionAlpha, maxContextDocs, maxChunksPerDoc, candidateK, rrfK, vecMinScore, rerankMinScore, bm25MsmRatio, rerankerEnabled };
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
    const unlisteners = await Promise.all([
        window.__TAURI__.event.listen('rag:delta', (e) => {
            if (e.payload.request_id !== requestId) return;
            const delta = e.payload.content;
            if (!_chatStreamingDiv) {
                console.debug('[rag] FIRST delta received len=' + delta.length);
            }
            _chatStreamingFullContent += delta;
            if (!_chatStreamingDiv) {
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
            }
            if (_chatStreamingDiv) {
                _chatStreamingDiv._rawContent = _chatStreamingFullContent;
            }
            if (_chatStreamingDiv && _chatStreamingDiv._body) {
                _chatStreamingDiv._body.innerHTML = `<div class="markdown-body" style="zoom: 1;background: transparent;">${renderChatMarkdown(_chatStreamingFullContent)}</div>`;
            }
            if (_chatStreamingDiv) _chatStreamingDiv._scroll();
        }),
        window.__TAURI__.event.listen('trace:event', (e) => {
            const payload = e.payload ? e.payload : {};
            const tid = payload.request_id;
            const tev = payload.events;
            if (!tid) return;
            if (!Array.isArray(tev)) return;
            const existing = window.__chatTraceMap[tid] ? window.__chatTraceMap[tid] : [];
            window.__chatTraceMap[tid] = existing.concat(tev);
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
            // 渲染阶段耗时面板（trace:event 按 request_id 收集）
            const traceEvents = window.__chatTraceMap[requestId] ? window.__chatTraceMap[requestId] : [];
            if (streamingDiv) {
                if (traceEvents.length) {
                    try {
                        const panel = document.createElement('div');
                        panel.innerHTML = renderTracePanel(traceEvents);
                        streamingDiv.appendChild(panel.firstChild);
                    } catch (err) {
                        console.warn('[trace] 渲染阶段面板失败:', err);
                    }
                }
            }
            delete window.__chatTraceMap[requestId];
            // 内存态（当前会话立即可见，不依赖落库结果）
            if (fullContent) {
                chatMessages.push({ role: 'assistant', content: fullContent, sources: sources, toolCalls: toolCallsSnapshot, created_at: Date.now() });
                updateTurnCounter();
            }
            // ===== 落库优先：先持久化再做 UI 后处理（UI 异常不得阻断保存） =====
            if (ragSessionId && fullContent) {
                try {
                    const savedMsg = await saveChatMessageWithRetry({
                        dirPath: currentRootPath,
                        sessionId: ragSessionId,
                        role: 'assistant',
                        content: fullContent,
                        tokenCount: completionTokens,
                        toolCalls: JSON.stringify(toolCallsSnapshot),
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
                    const sessions = await window.__TAURI__.core.invoke('chat_session_list', {
                        dirPath: currentRootPath,
                    });
                    chatSessions = sessions;
                    renderChatSessionList();
                } catch (e) {
                    console.error('[rag] 保存助手消息失败（重试后仍失败）:', e);
                    showNotification('⚠ 回复未能保存到对话历史，请检查数据库', 'warning');
                }
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
            // ===== UI 后处理（异常隔离，不影响持久化结果） =====
            try {
                // 更新上下文使用率（模型上下文窗口占用率）
                // 优先使用 API 返回的 prompt_tokens，不可用时保留上次的值
                if (promptTokens > 0) {
                    updateContextUsage(promptTokens, LOCAL_LLM_CONTEXT_LENGTH || 10000);
                }
                // 更新缓存命中率（DSH 口径；provider 未上报缓存字段时显示占位）
                updateCacheRate({
                    prompt_tokens: promptTokens,
                    cached_input_tokens: cachedInputTokens,
                    cache_creation_input_tokens: cacheCreationInputTokens,
                }, 'rag');
                // 流式结束后对代码块进行语法高亮（复制按钮读取原始 Markdown）
                if (streamingDiv) {
                    streamingDiv._rawContent = fullContent;
                }
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
            // 重命名"新对话"（容错：防止 sendChatMessage 中因竞态未能更新）
            if (ragSessionId) {
                window.__TAURI__.core.invoke('chat_session_list', {
                    dirPath: currentRootPath,
                }).then(sessions => {
                    const cur = sessions.find(s => s.id === ragSessionId);
                    if (cur && (cur.title === '新对话' || !cur.title)) {
                        const msg = chatMessages[0]?.content || '';
                        if (msg) {
                            window.__TAURI__.core.invoke('chat_session_rename', {
                                dirPath: currentRootPath,
                                id: ragSessionId,
                                title: msg,
                            }).then(() => {
                                cur.title = msg;
                                chatSessions = sessions;
                                renderChatSessionList();
                            }).catch(e => console.warn('重命名失败:', e));
                        }
                    }
                }).catch(e => console.warn('刷新列表失败:', e));
            }
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
    return '<details style="margin-top:8px;border:1px solid #333;border-radius:6px;padding:6px 8px;">'
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
}
