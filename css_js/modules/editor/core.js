/**
 * ===== 统一文档模型（css_js/modules/editor/core.js） =====
 *
 * 【职责】P1-2 EditorDocument 抽象：向 AI 层/导出/命令提供统一的
 *   "当前文档" 接口，屏蔽底层编辑器差异（Monaco 源码 / TipTap 块模式）。
 *   - 双容器方案下 Monaco 恒为 currentEditor，块模式编辑实时同步 Monaco，
 *     故本模型以 Monaco 为唯一事实源即可覆盖两种模式。
 * 【依赖】运行时主脚本全局：currentEditor / originalContent / previewFileText /
 *         saveFileOnly
 * 【对外暴露】window.MdgoDocument
 */
(function () {
    'use strict';

    const doc = {
        /** 当前文档全文（Monaco → 原始内容兜底） */
        getValue() {
            if (currentEditor && typeof currentEditor.getValue === 'function') {
                const v = currentEditor.getValue();
                // 修复：空串文档也是合法内容，不能按 falsy 回退到 originalContent
                if (v !== undefined && v !== null) return v;
            }
            return originalContent || previewFileText || '';
        },

        /** 全量替换文档（入 undo 栈） */
        setValue(text) {
            if (currentEditor && currentEditor.getModel) {
                const m = currentEditor.getModel();
                if (m) {
                    try { currentEditor.pushUndoStop(); } catch (e) { }
                    m.setValue(text);
                    return true;
                }
            }
            return false;
        },

        /**
         * 监听文档变更（块模式编辑已实时同步 Monaco，Monaco 变更即全模式变更源）。
         * @returns {Function} 取消监听
         */
        onChange(cb) {
            if (currentEditor && typeof currentEditor.onDidChangeModelContent === 'function') {
                return currentEditor.onDidChangeModelContent(() => {
                    try { cb(doc.getValue()); } catch (e) { console.error('[mdgo] onChange 回调异常:', e); }
                });
            }
            return () => { };
        },

        /** 当前编辑器选区（非空时返回 {range, text}，否则 null） */
        getSelection() {
            if (currentEditor && currentEditor.getSelection && currentEditor.getModel) {
                const sel = currentEditor.getSelection();
                if (sel && !sel.isEmpty()) {
                    const text = currentEditor.getModel().getValueInRange(sel);
                    return { range: sel, text };
                }
            }
            return null;
        },

        /** 在文档中查找文本第一次出现位置（供预览选区 → 编辑器定位） */
        findFirst(text) {
            if (!text || !currentEditor || !currentEditor.getModel) return null;
            const model = currentEditor.getModel();
            const full = model.getValue();
            const idx = full.indexOf(text);
            if (idx < 0) return null;
            const pos = model.getPositionAt(idx);
            const end = model.getPositionAt(idx + text.length);
            return { range: { startLineNumber: pos.lineNumber, startColumn: pos.column, endLineNumber: end.lineNumber, endColumn: end.column } };
        },

        /** 替换指定 range 为文本（入 undo 栈） */
        replace(range, text) {
            if (currentEditor && typeof currentEditor.executeEdits === 'function') {
                currentEditor.executeEdits('mdgo-inline-ai', [{ range, text }]);
                return true;
            }
            return false;
        },

        /** 在光标/指定位置插入文本（入 undo 栈） */
        insertAt(position, text) {
            if (currentEditor && typeof currentEditor.executeEdits === 'function') {
                const pos = position || (currentEditor.getPosition && currentEditor.getPosition());
                if (!pos) return false;
                currentEditor.executeEdits('mdgo-inline-ai', [{
                    range: { startLineNumber: pos.lineNumber, startColumn: pos.column, endLineNumber: pos.lineNumber, endColumn: pos.column },
                    text
                }]);
                return true;
            }
            return false;
        },

        /** 保存当前文档（统一入口） */
        save() {
            if (typeof window.saveFileOnly === 'function') return window.saveFileOnly();
            return Promise.reject(new Error('saveFileOnly 不可用'));
        },

        /** 当前是否处于编辑态（有活动编辑器） */
        get editable() {
            return !!(currentEditor && currentEditor.getModel);
        }
    };

    window.MdgoDocument = doc;
})();
