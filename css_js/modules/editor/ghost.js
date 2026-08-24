/**
 * ===== 幽灵文本续写模块（css_js/modules/editor/ghost.js） =====
 *
 * 【职责】P1-4 智能续写（Ghost Text）：
 *   - 光标处按 Ctrl+J（或手动触发）→ 基于光标前上下文调用 AI 续写；
 *   - 续写内容以 Monaco inline suggestion（幽灵文本）显示：Tab 接受 / Esc 拒绝 /
 *     再次 Ctrl+J 重新生成（Monaco 0.50 内置交互）。
 * 【入口】main.html createMonacoEditor 末尾调用 initGhostText(editor)（幂等）
 * 【依赖】Monaco 0.36+（registerInlineCompletionsProvider / InlineCompletionTriggerKind）；
 *         运行时主脚本全局：callAIAPI / showNotification
 * 【对外暴露】window.initGhostText / window.MdgoGhost
 */
(function () {
    'use strict';

    const GHOST_CONTEXT_CHARS = 4000; // 续写上下文取光标前字符上限
    const GHOST_PROMPT = '你是 Markdown 写作助手。请根据给定上下文自然续写 2-4 句，保持语气与风格一致，'
        + '直接输出续写内容（不要重复已有内容、不要解释、不要输出 Markdown 围栏）。\n\n上下文：\n';

    let lastRequestTs = 0;

    /** 手动触发续写（Ctrl+J） */
    function requestContinuation(editor) {
        if (!editor || !editor.getModel()) return;
        // 防止 800ms 内重复触发
        const now = Date.now();
        if (now - lastRequestTs < 800) return;
        lastRequestTs = now;
        try {
            editor.trigger('mdgo-ghost', 'editor.action.inlineSuggest.trigger', {});
        } catch (e) {
            console.error('[mdgo] 续写触发失败:', e);
        }
    }

    // H1 修复：InlineCompletionsProvider 是语言级全局注册，必须只注册一次。
    // 原来放在 initGhostText（per-editor）里，打开多个编辑器会重复注册多个 provider，
    // Ctrl+J 触发时多个 provider 都调用 callAIAPI 造成重复请求。
    let providerRegistered = false;
    function ensureProviderRegistered(monaco) {
        if (providerRegistered || !monaco) return;
        providerRegistered = true;
        monaco.languages.registerInlineCompletionsProvider('markdown', {
            provideInlineCompletions: async (model, position, context) => {
                // 仅响应手动触发（Ctrl+J），避免自动请求消耗 LLM
                const kind = monaco.languages.InlineCompletionTriggerKind;
                if (!context || context.triggerKind !== kind.Invoke) {
                    return { items: [] };
                }
                if (!model) return { items: [] };
                // 取光标前上下文（限制长度）
                const prefix = model.getValueInRange({
                    startLineNumber: 1, startColumn: 1,
                    endLineNumber: position.lineNumber, endColumn: position.column
                });
                const contextText = prefix.slice(-GHOST_CONTEXT_CHARS);
                if (!contextText.trim()) return { items: [] };
                try {
                    const result = await window.callAIAPI(GHOST_PROMPT + contextText, '');
                    const text = String(result || '').trim();
                    if (!text) return { items: [] };
                    return {
                        items: [{
                            insertText: text,
                            range: {
                                startLineNumber: position.lineNumber,
                                startColumn: position.column,
                                endLineNumber: position.lineNumber,
                                endColumn: position.column
                            }
                        }]
                    };
                } catch (e) {
                    console.error('[mdgo] 续写失败:', e);
                    if (typeof window.showNotification === 'function') {
                        window.showNotification('✗ 续写失败: ' + (e && e.message ? e.message : e), 'error');
                    }
                    return { items: [] };
                }
            }
        });
    }

    window.initGhostText = function (editor) {
        if (!editor || editor._mdgoGhostBound) return;
        editor._mdgoGhostBound = true;
        const monaco = window.monaco;
        if (!monaco) return;

        // 启用 inline suggestions 渲染（幽灵文本）
        try { editor.updateOptions({ inlineSuggest: { enabled: true, showToolbar: 'onHover' } }); } catch (e) { }

        // provider 只注册一次（语言级全局）
        ensureProviderRegistered(monaco);

        // Ctrl/Cmd+J 触发续写（Tab=接受、Esc=拒绝 由 Monaco 内置处理）
        editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyJ, () => requestContinuation(editor));

        console.log('[mdgo] 幽灵文本续写已就绪（Ctrl+J 触发 / Tab 接受 / Esc 拒绝）');
    };

    window.MdgoGhost = {
        request: requestContinuation
    };
})();
