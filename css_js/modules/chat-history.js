/**
 * ===== 聊天历史工具配对语义（P1-1 单一来源） =====
 *
 * 【职责】「assistant(tool_calls) + 紧随其后的 tool 结果」配对/分组规则的唯一前端来源。
 *        此前该语义在 main.html（trimChatHistory）与 agent.js（expandToolHistory）
 *        各有一份近似实现，改一处漏一处（孤儿 tool 消息 / 历史失真）。
 *        本模块收敛两份实现；main.html / agent.js 中的原函数改为薄包装委托。
 *
 * 【对外暴露】window.chatHistory = { groupToolUnits, expandToolHistory, trimChatHistory }
 *
 * 【运行时依赖的全局服务】（来自 main.html 主脚本，未定义时用内置兜底）
 *   - estimateTokenCount(text)   —— CJK 感知的 token 估算
 *   - LOCAL_LLM_CONTEXT_LENGTH   —— 模型上下文窗口（token）
 *   - HISTORY_BUDGET_RATIO       —— 历史预算比例（默认 0.6）
 */
(function () {
    'use strict';

    // 兜底估算（main.html 的 estimateTokenCount 未定义时用，口径一致：CJK 1.5 字符/token、其余 4）
    function defaultEstimateTokenCount(text) {
        if (!text) return 0;
        let chars = 0, cjk = 0;
        for (let i = 0; i < text.length; i++) {
            const code = text.charCodeAt(i);
            chars++;
            if (code >= 0x4E00 && code <= 0x9FFF) cjk++;
        }
        return Math.ceil(cjk / 1.5 + (chars - cjk) / 4);
    }
    function estimateTokenCount(text) {
        return (typeof window.estimateTokenCount === 'function')
            ? window.estimateTokenCount(text)
            : defaultEstimateTokenCount(text);
    }

    /**
     * 按「工具调用单元」分组消息：
     * - assistant（带 tool_calls）与其紧随其后的连续 role==='tool' 结果同组
     * - 其余消息各自成组
     * - 孤儿 tool 结果消息自成组（防御，不静默丢弃）
     * @param {Array} messages 消息数组（含 toolCalls / tool_call_id 字段）
     * @returns {Array<{role:string, group:Array}>} 单元数组（保持原顺序）
     */
    function groupToolUnits(messages) {
        const units = [];
        for (const m of messages) {
            if (m.role === 'tool') {
                const last = units[units.length - 1];
                if (last && last.role === 'assistant') {
                    last.group.push(m);
                } else {
                    units.push({ role: 'orphan-tool', group: [m] }); // 防御：孤儿 tool 自成组
                }
            } else if (m.role === 'assistant' && Array.isArray(m.tool_calls) && m.tool_calls.length > 0) {
                units.push({ role: 'assistant', group: [m] });
            } else {
                units.push({ role: 'other', group: [m] });
            }
        }
        return units;
    }

    /**
     * 将内存中的对话消息展开为发送给后端的协议消息数组（工具历史回流）：
     * assistant 消息的工具调用轨迹（toolCalls 记录数组）还原为
     *   {role:'assistant', tool_calls:[{id,name,arguments}]}
     * 并紧随其后的 {role:'tool', tool_call_id, content:结果} 结果消息，
     * 使模型在后续轮次能看到自己此前调用过哪些工具、拿到什么结果。
     * 老数据（无 call_id 或 result 的记录）降级为普通文本消息，行为不变。
     */
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

    /** 工具消息单元的 token 估算：assistant(tool_calls) + 其后续连续 tool 结果 */
    function unitTokens(unit) {
        let t = 0;
        for (const m of unit.group) {
            t += 4 + estimateTokenCount(m.content);
            if (Array.isArray(m.tool_calls)) {
                for (const tc of m.tool_calls) {
                    const argsText = typeof tc.arguments === 'string'
                        ? tc.arguments
                        : (tc.arguments ? JSON.stringify(tc.arguments) : '{}');
                    t += estimateTokenCount(argsText);
                }
            }
        }
        return t;
    }

    /**
     * 裁剪对话历史，同时满足：
     *   历史消息总 token 不超过 context 预算（留给 system/RAG context）
     *   - 始终保留最后一条 user 消息（当前输入）
     *   - 工具消息成对裁剪：assistant（带 tool_calls）与其后续连续 tool 结果同组，
     *     避免产生孤儿 tool 消息（后端 OpenAI 协议要求 tool_call 与结果同侧）
     *
     * @param {Array} messages  - 对话消息数组
     * @param {number} [budgetRatio] - 可选的历史预算比例，覆盖 HISTORY_BUDGET_RATIO
     *                                  RAG 场景建议传 0.4，纯对话传 0.6
     * @returns {Array} 裁剪后的消息数组
     */
    function trimChatHistory(messages, budgetRatio) {
        if (!messages || messages.length <= 2) return messages; // 只有 1 轮或刚输入，无需裁剪
        // 全局词法绑定（main.html 顶层 let/const，跨脚本可见）；typeof 兜底防缺失
        const ratio = (budgetRatio !== undefined)
            ? budgetRatio
            : (typeof HISTORY_BUDGET_RATIO !== 'undefined' ? HISTORY_BUDGET_RATIO : 0.6);
        const ctxLen = (typeof LOCAL_LLM_CONTEXT_LENGTH !== 'undefined' && LOCAL_LLM_CONTEXT_LENGTH > 0)
            ? LOCAL_LLM_CONTEXT_LENGTH
            : 8000;
        const budget = Math.floor(ctxLen * ratio);
        // 按「工具调用单元」分组（review 修复：避免单条裁剪切散 tool_call 与 tool 结果）
        const units = groupToolUnits(messages);
        let totalTokens = 0;
        for (const u of units) totalTokens += unitTokens(u);
        if (totalTokens <= budget) return messages;
        // 从旧到新按组保留（最新优先），组内 token 含 tool_calls arguments
        const keptLast = units[units.length - 1];
        const trimmedUnits = [keptLast];
        let keptTokens = unitTokens(keptLast);
        let droppedCount = 0;
        for (let i = units.length - 2; i >= 0; i--) {
            const uTokens = unitTokens(units[i]);
            if (keptTokens + uTokens > budget) {
                droppedCount++;
                continue;
            }
            trimmedUnits.unshift(units[i]);
            keptTokens += uTokens;
        }
        if (droppedCount > 0) {
            console.warn('[chat] 对话 token 超预算(' + totalTokens + '>' + budget + ')，丢弃了' + droppedCount + '条旧消息');
        }
        const out = [];
        for (const u of trimmedUnits) {
            for (const m of u.group) out.push(m);
        }
        return out;
    }

    window.chatHistory = { groupToolUnits, expandToolHistory, trimChatHistory };
})();
