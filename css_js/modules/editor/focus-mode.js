/**
 * ===== 专注模式模块（css_js/modules/editor/focus-mode.js） =====
 *
 * 【职责】P0-3 专注模式（Monaco）：
 *   1. 打字机滚动（Ctrl+Shift+T 切换）：光标行垂直居中（revealLineInCenter）
 *   2. 聚焦段落（Ctrl+Shift+P 切换）：当前段落（空行分隔）高亮，段落外行淡出（dim）
 * 【入口】main.html createMonacoEditor 末尾统一调用 initFocusMode(editor)
 *         （本文件在 <body> 底部加载，先于任何编辑器创建）
 * 【依赖】window.monaco（调用方保证 Monaco 已就绪）；主脚本全局 showNotification
 * 【对外暴露】window.initFocusMode —— 幂等（editor._mdgoFocusBound）
 */
(function () {
    'use strict';

    // 超过此行数不做全行 dim（Monaco decoration 逐行开销），仅高亮当前段落
    const FOCUS_MAX_LINES = 6000;

    window.initFocusMode = function (editor) {
        if (!editor || editor._mdgoFocusBound) return;
        editor._mdgoFocusBound = true;

        let typewriter = false;
        let focusParagraph = false;
        let focusDecos = [];
        let lastFocusLine = 0;

        function applyTypewriter() {
            const pos = editor.getPosition();
            if (pos && typewriter) editor.revealLineInCenter(pos.lineNumber);
        }

        function applyFocusParagraph() {
            const model = editor.getModel();
            if (!model) return;
            if (!focusParagraph) {
                focusDecos = editor.deltaDecorations(focusDecos, []);
                return;
            }
            const pos = editor.getPosition();
            const line = pos ? pos.lineNumber : 1;
            const total = model.getLineCount();
            // 段落范围（空行分隔，包含光标行）
            let start = line, end = line;
            while (start > 1 && model.getLineContent(start - 1).trim() !== '') start--;
            while (end < total && model.getLineContent(end + 1).trim() !== '') end++;

            const R = window.monaco && window.monaco.Range;
            if (!R) return;
            const decos = [];
            if (total <= FOCUS_MAX_LINES) {
                for (let i = 1; i <= total; i++) {
                    const inPara = i >= start && i <= end;
                    decos.push({
                        range: new R(i, 1, i, 1),
                        options: { className: inPara ? 'mdgo-focus-line' : 'mdgo-focus-dim' }
                    });
                }
            } else {
                // 大文件降级：仅高亮当前段落，不做全行 dim
                for (let i = start; i <= end; i++) {
                    decos.push({ range: new R(i, 1, i, 1), options: { className: 'mdgo-focus-line' } });
                }
            }
            focusDecos = editor.deltaDecorations(focusDecos, decos);
        }

        editor.onDidChangeCursorPosition(() => {
            if (typewriter) applyTypewriter();
            if (focusParagraph) {
                const line = editor.getPosition() ? editor.getPosition().lineNumber : 0;
                if (line !== lastFocusLine) { lastFocusLine = line; applyFocusParagraph(); }
            }
        });
        editor.onDidChangeModelContent(() => {
            if (focusParagraph) applyFocusParagraph();
        });

        // Ctrl+Shift+T：打字机滚动；Ctrl+Alt+P：聚焦段落
        // （修复：原 Ctrl+Shift+P 是 Monaco standalone 命令面板默认快捷键，
        //  占用会导致编辑器内命令面板失效，改用无冲突的 Ctrl+Alt+P）
        const mods = (typeof window.monaco !== 'undefined')
            ? (window.monaco.KeyMod.CtrlCmd | window.monaco.KeyMod.Shift) : 0;
        const altMods = (typeof window.monaco !== 'undefined')
            ? (window.monaco.KeyMod.CtrlCmd | window.monaco.KeyMod.Alt) : 0;
        editor.addCommand(mods | (window.monaco && window.monaco.KeyCode.KeyT), () => {
            typewriter = !typewriter;
            if (typewriter) applyTypewriter();
            if (typeof showNotification === 'function') {
                showNotification(typewriter ? '✍ 打字机模式：开（光标行居中）' : '打字机模式：关', 'info', 1500);
            }
        });
        editor.addCommand(altMods | (window.monaco && window.monaco.KeyCode.KeyP), () => {
            focusParagraph = !focusParagraph;
            if (focusParagraph) {
                lastFocusLine = 0;
                applyFocusParagraph();
            } else {
                focusDecos = editor.deltaDecorations(focusDecos, []);
            }
            if (typeof showNotification === 'function') {
                showNotification(focusParagraph ? '🔍 聚焦段落：开（段落外淡出）' : '聚焦段落：关', 'info', 1500);
            }
        });
    };
})();
