/**
 * ===== 内联 AI 模块（css_js/modules/editor/ai-inline.js） =====
 *
 * 【职责】P1-3 内联 AI 全链路：选区 AI 动作**就地应用**（替换/插入/转表格/转列表/转代码），
 *   结果入 undo 栈可撤销。动作清单 MdgoInlineAI.ACTIONS 与主脚本 AI_SELECTION_ACTIONS
 *   合并渲染到选区工具条；run() 经 MdgoDocument 操作当前编辑器。
 * 【依赖】运行时主脚本全局：callAIAPI / currentEditor / markdownSelectionState /
 *         showNotification；window.MdgoDocument（core.js，须先加载）
 * 【对外暴露】window.MdgoInlineAI（ACTIONS / run / applyResult / getSelectionText）
 */
(function () {
    'use strict';

    const ACTIONS = [
        { id: 'inline-continue', name: '继续写', applyMode: 'insert-below', stream: true,
            prompt: '你是写作助手。请顺着所选文本的语境自然续写 2-4 句，保持语气、风格与术语一致，直接输出续写内容，不要解释。' },
        { id: 'inline-expand', name: '扩写', applyMode: 'replace',
            prompt: '请扩写所选文本：补充细节、论据与例子，保持原意与风格，直接输出扩写结果。' },
        { id: 'inline-shorten', name: '缩写', applyMode: 'replace',
            prompt: '请缩写所选文本：保留核心信息，去除冗余，输出比原文更精炼的版本，直接输出。' },
        { id: 'inline-polish', name: '润色', applyMode: 'replace',
            prompt: '请润色所选文本：优化用词与句式，使其更通顺专业，不改变原意，直接输出润色结果。' },
        { id: 'inline-tone-professional', name: '语气·专业', applyMode: 'replace',
            prompt: '请将所选文本改写为专业正式语气，保持原意与术语，直接输出。' },
        { id: 'inline-tone-casual', name: '语气·轻松', applyMode: 'replace',
            prompt: '请将所选文本改写为轻松口语化语气，保持原意，直接输出。' },
        { id: 'inline-tone-academic', name: '语气·学术', applyMode: 'replace',
            prompt: '请将所选文本改写为学术语气（严谨、客观、书面化），保持原意，直接输出。' },
        { id: 'inline-tone-concise', name: '语气·简洁', applyMode: 'replace',
            prompt: '请将所选文本改写为简洁干练的风格，去掉冗余修饰，保持原意，直接输出。' },
        { id: 'inline-translate-any', name: '翻译', applyMode: 'replace',
            prompt: '请将所选文本翻译成用户期望的语言（默认：中文↔英文互译），保留格式与专有名词，直接输出译文。' },
        { id: 'inline-extract-todos', name: '提取待办', applyMode: 'insert-below',
            prompt: '请从所选文本中提取所有可执行的待办事项，输出为 Markdown 任务列表（每行 "- [ ] 事项"），不要输出其他内容。' },
        { id: 'inline-to-table', name: '转表格', applyMode: 'to-table',
            prompt: '请将所选内容整理为 Markdown 表格（首行表头，| 分隔），直接输出表格，不要其他内容。' },
        { id: 'inline-to-list', name: '转列表', applyMode: 'replace',
            prompt: '请将所选内容整理为 Markdown 列表（- 或 1.），直接输出列表。' },
        { id: 'inline-to-code', name: '转代码块', applyMode: 'to-code',
            prompt: '请将所选内容整理为带围栏的代码块（``` 语言 开始，``` 结束），直接输出代码块。' },
        { id: 'inline-summary', name: '摘要', applyMode: 'insert-below',
            prompt: '请对所选文本生成简洁摘要（3-5 条要点或一段话），保留关键数据，直接输出摘要。' },
        { id: 'inline-points', name: '要点', applyMode: 'insert-below',
            prompt: '请提取所选文本的核心要点，输出为 Markdown 列表，每点一行，直接输出。' },
        { id: 'inline-gen-mermaid', name: '生成 Mermaid', applyMode: 'insert-below',
            prompt: '请根据所选内容生成语法正确的 Mermaid 图代码（flowchart/sequenceDiagram 等，选择最合适的类型），只输出 ```mermaid 围栏代码块，不要解释。' },
        { id: 'inline-gen-table', name: '生成表格', applyMode: 'insert-below',
            prompt: '请根据所选内容生成一张结构化的 Markdown 表格（表头 + 数据行），直接输出表格。' }
    ];

    function getAction(id) {
        return ACTIONS.find(a => a.id === id) || null;
    }

    /** 当前选区文本（编辑器选区优先；否则预览选区 markdownSelectionState.text） */
    function getSelectionText() {
        const d = window.MdgoDocument;
        if (d && d.editable) {
            const sel = d.getSelection();
            if (sel) return sel.text;
        }
        return (markdownSelectionState && markdownSelectionState.text) || '';
    }

    /** 确定应用位置：编辑器选区 → 文本匹配兜底 → 光标 */
    function resolveRange(selectionText) {
        const d = window.MdgoDocument;
        if (!d || !d.editable) return { range: null, insertPos: null };
        const sel = d.getSelection();
        if (sel) return { range: sel.range, insertPos: null };
        if (selectionText) {
            const found = d.findFirst(selectionText);
            if (found) return { range: found.range, insertPos: null };
        }
        // 无选区：插入到光标
        const pos = currentEditor.getPosition();
        return { range: null, insertPos: pos };
    }

    /** 校验结果形态并就地应用（applyMode 见 ACTIONS 定义） */
    function applyResult(mode, result, range, insertPos) {
        const d = window.MdgoDocument;
        if (!d) return false;
        const text = String(result || '').trim();
        if (!text) { showNotif('AI 返回为空', 'warning'); return false; }
        switch (mode) {
            case 'replace':
                if (range) return d.replace(range, text);
                return d.insertAt(insertPos, text);
            case 'insert-below': {
                const at = range ? { lineNumber: range.endLineNumber, column: range.endColumn } : insertPos;
                const lead = (range && range.endColumn > 1) ? '\n\n' : '';
                return d.insertAt(at, lead + text);
            }
            case 'to-table': {
                // 仅接受含 | 的表格形态，否则降级为原样替换
                if (!/^\s*\|/.test(text) && !/^\s*\|?[^|]+\|/.test(text)) {
                    showNotif('AI 输出非表格格式，已按原文替换', 'warning');
                    return range ? d.replace(range, text) : d.insertAt(insertPos, text);
                }
                return range ? d.replace(range, '\n' + text + '\n') : d.insertAt(insertPos, '\n' + text + '\n');
            }
            case 'to-code': {
                const code = /^```/.test(text) ? text : '```\n' + text + '\n```';
                return range ? d.replace(range, code) : d.insertAt(insertPos, '\n' + code + '\n');
            }
            default:
                return range ? d.replace(range, text) : d.insertAt(insertPos, text);
        }
    }

    /** 执行内联 AI 动作（就地应用，可撤销） */
    async function run(id, selectionText) {
        const action = getAction(id);
        if (!action) return false;
        const d = window.MdgoDocument;
        if (!d || !d.editable) {
            showNotif('内联 AI 需要在编辑模式下使用（请先进入编辑模式）', 'warning');
            return false;
        }
        const text = selectionText || getSelectionText();
        if (!text) { showNotif('请先选中文本', 'warning'); return false; }
        // 修复：记录 AI 请求前的文档版本，await 期间用户继续输入时 range 会过期
        const model = currentEditor && currentEditor.getModel ? currentEditor.getModel() : null;
        const versionBefore = model ? model.getVersionId() : -1;
        const { range, insertPos } = resolveRange(text);
        const notif = showNotif(`正在${action.name}...`, 'info', 0);
        try {
            const result = await window.callAIAPI(action.prompt + '\n\n所选文本为：\n' + text, '');
            // await 后重新校验：若文档已变更（用户输入/撤销），旧 range 不再可靠——
            // 重新解析；解析不到且原 range 来自旧版本则放弃并提示重试，避免改错位置
            const modelNow = currentEditor && currentEditor.getModel ? currentEditor.getModel() : null;
            let effRange = range, effPos = insertPos;
            if (modelNow && modelNow.getVersionId() !== versionBefore) {
                const re = resolveRange(text);
                effRange = re.range;
                effPos = re.insertPos;
                if (!effRange && !effPos) {
                    showNotif('文档已变更，请重新选中文本再试', 'warning');
                    return false;
                }
            }
            applyResult(action.applyMode, result, effRange, effPos);
            showNotif(`✓ ${action.name}完成（已就地应用，Ctrl+Z 可撤销）`, 'success', 2500);
            return true;
        } catch (e) {
            console.error('[mdgo] 内联 AI 失败:', e);
            showNotif('✗ ' + action.name + '失败: ' + (e && e.message ? e.message : e), 'error');
            return false;
        } finally {
            if (notif && notif.dismiss) notif.dismiss();
        }
    }

    function showNotif(msg, type, dur) {
        if (typeof window.showNotification === 'function') {
            return window.showNotification(msg, type, dur);
        }
        console.log('[mdgo]', msg);
        return null;
    }

    window.MdgoInlineAI = {
        ACTIONS,
        getAction,
        getSelectionText,
        run,
        applyResult
    };
})();
