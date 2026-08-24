/**
 * ===== 块编辑模块（css_js/modules/editor/block.js） =====
 *
 * 【职责】P2-1 Notion 式块编辑（TipTap / ProseMirror）：
 *   - 块类型：标题/段落/列表/任务列表/表格/代码块/引用/图片/分割线（StarterKit +
 *     TaskList + Table + Image + DragHandle）
 *   - 块拖拽排序（DragHandle）、块操作菜单
 *   - Markdown ⇄ 块双向：进入块模式 = marked(GFM) → TipTap HTML；
 *     保存/切回源码 = TipTap HTML → turndown → Markdown（Markdown 为事实源，
 *     切回时以序列化结果为准，尽力保真）
 *   - 双容器叠加：#mdgo-editor-monaco（源码，默认显示）+ #mdgo-editor-block（隐藏叠加），
 *     与 Monaco 源码互切（wysiwyg.js 已删除，monaco-host 由此模块自行创建）
 * 【入口】main.html enterEditMode 调用 initBlockToggle(editorInner)
 * 【依赖】懒加载：css_js/cdn/tiptap/tiptap.bundle.js（window.TipTap）、
 *         css_js/cdn/tiptap/turndown.js（window.TurndownService）；
 *         运行时主脚本全局：currentEditor / showNotification；marked（head 已加载）
 * 【对外暴露】window.initBlockToggle / window.toggleBlockMode / window.MdgoBlock
 */
(function () {
    'use strict';

    let blockEditor = null;   // TipTap Editor
    let blockHost = null;     // 块容器
    let monacoHost = null;
    let onBlockChange = null;

    // ===== 懒加载 TipTap + turndown =====
    let loading = null;
    function loadDeps() {
        if (window.TipTap && window.TurndownService) return Promise.resolve();
        if (loading) return loading;
        loading = new Promise((resolve, reject) => {
            const needTiptap = !window.TipTap;
            const needTurndown = !window.TurndownService;
            let remaining = (needTiptap ? 1 : 0) + (needTurndown ? 1 : 0);
            const done = () => { if (--remaining <= 0) resolve(); };
            const fail = (e) => { loading = null; reject(e); };
            if (needTiptap) {
                const s = document.createElement('script');
                s.src = 'css_js/cdn/tiptap/tiptap.bundle.js';
                s.onload = done; s.onerror = () => fail(new Error('TipTap bundle 加载失败'));
                document.head.appendChild(s);
            }
            if (needTurndown) {
                const s = document.createElement('script');
                s.src = 'css_js/cdn/tiptap/turndown.js';
                s.onload = done; s.onerror = () => fail(new Error('turndown 加载失败'));
                document.head.appendChild(s);
            }
            if (remaining === 0) resolve();
        });
        return loading;
    }

    // ===== Markdown → HTML（进入块模式） =====
    function mdToHtml(mdText) {
        try {
            return window.marked.parse(mdText || '');
        } catch (e) {
            console.error('[mdgo] md→html 失败:', e);
            return '<p></p>';
        }
    }

    // ===== HTML → Markdown（块 → 源码） =====
    function htmlToMd(html) {
        try {
            const td = new window.TurndownService({
                gfm: true,
                headingStyle: 'atx',
                codeBlockStyle: 'fenced',
                bulletListMarker: '-'
            });
            // 修复(S8)：TipTap 任务项输出 <li data-type="taskItem" data-checked>，
            // turndown 无内置规则会丢失 [x]/[ ] 状态，自定义规则保真
            td.addRule('mdgoTaskItem', {
                filter: (node) =>
                    node.nodeName === 'LI' &&
                    node.getAttribute && node.getAttribute('data-type') === 'taskItem',
                replacement: (content, node) => {
                    const checked = node.getAttribute('data-checked') === 'true';
                    return `- [${checked ? 'x' : ' '}] ${String(content || '').trim()}\n`;
                }
            });
            return td.turndown(html || '');
        } catch (e) {
            console.error('[mdgo] html→md 失败:', e);
            return '';
        }
    }

    function createBlockEditor(mdText) {
        const T = window.TipTap;
        blockEditor = new T.Editor({
            element: blockHost,
            extensions: [
                T.StarterKit,
                T.TaskList,
                T.TaskItem,
                T.Table.configure({ resizable: true }),
                T.TableRow,
                T.TableHeader,
                T.TableCell,
                T.Image,
                T.DragHandle
            ],
            content: mdToHtml(mdText),
            editorProps: {
                attributes: { class: 'mdgo-block-editor', spellcheck: 'false' }
            },
            onUpdate: ({ editor }) => {
                if (onBlockChange) onBlockChange(editor.getHTML());
                // H2 修复：块模式编辑实时同步 Monaco（保存链路读 currentEditor.getValue()，
                // 不同步会导致 Ctrl+S 保存旧内容丢编辑）；setValue 不 pushUndoStop，
                // 避免清空/污染 Monaco undo 栈（块编辑的撤销由 TipTap 自己管理）
                if (currentEditor && currentEditor.getModel) {
                    const m = currentEditor.getModel();
                    const md = htmlToMd(editor.getHTML());
                    if (m && m.getValue() !== md) {
                        try { m.setValue(md); } catch (e) { }
                    }
                }
            }
        });
        return blockEditor;
    }

    // ===== 切换入口（与 Monaco 源码互切） =====
    window.toggleBlockMode = async function () {
        if (!blockHost || !monacoHost) return;
        const isBlock = blockHost.style.display !== 'none';
        if (isBlock) {
            // 块 → 源码（Monaco）
            let md = '';
            if (blockEditor) {
                md = htmlToMd(blockEditor.getHTML());
                blockEditor.destroy();
                blockEditor = null;
            }
            blockHost.style.display = 'none';
            monacoHost.style.display = 'block';
            if (currentEditor && currentEditor.getModel()) {
                const model = currentEditor.getModel();
                // 切回源码：setValue 前不 pushUndoStop（避免制造空 undo 点；切换语义下
                // undo 从当前状态开始，块编辑期间的撤销由 TipTap 管理）
                if (md && model.getValue() !== md) {
                    model.setValue(md);
                }
                currentEditor.focus();
            }
            updateBlockBtn(false);
        } else {
            // 源码（Monaco）→ 块
            if (!currentEditor || !currentEditor.getModel()) {
                notify('请先进入编辑模式', 'warning');
                return;
            }
            const mdText = currentEditor.getValue();
            monacoHost.style.display = 'none';
            blockHost.style.display = 'block';
            try {
                await loadDeps();
                if (!window.TipTap || !window.TurndownService) {
                    notify('块编辑不可用（依赖加载失败）', 'error');
                    monacoHost.style.display = 'block';
                    blockHost.style.display = 'none';
                    return;
                }
                if (!blockEditor) createBlockEditor(mdText);
                else blockEditor.commands.setContent(mdToHtml(mdText), true);
                updateBlockBtn(true);
                blockEditor.commands.focus('start');
            } catch (e) {
                console.error('[mdgo] 块模式启动失败:', e);
                notify('块编辑启动失败: ' + (e && e.message ? e.message : e), 'error');
                monacoHost.style.display = 'block';
                blockHost.style.display = 'none';
            }
        }
    };

    function updateBlockBtn(isBlock) {
        const btn = document.querySelector('.mdgo-block-toggle');
        if (btn) {
            btn.textContent = isBlock ? '源码模式' : '块模式';
            btn.title = isBlock ? '切回 Monaco 源码编辑' : '切换到 Notion 式块编辑（TipTap）';
        }
    }

    function notify(msg, type) {
        if (typeof window.showNotification === 'function') window.showNotification(msg, type, 2000);
        else console.log('[mdgo]', msg);
    }

    /**
     * enterEditMode 调用：在 editorInner 内创建双容器（monaco-host 默认显示 +
     * block-host 隐藏叠加）并注入 footer 切换按钮。
     * （wysiwyg.js 已删除，monaco-host 由此模块自行创建）
     */
    window.initBlockToggle = function (editorInner) {
        if (!editorInner) return;
        if (!editorInner.querySelector('#mdgo-editor-block')) {
            // H3 修复：editorInner 重建（切换文件/重新进入编辑）时，旧 TipTap editor
            // 仍挂在已脱离 DOM 的 blockHost 上，必须先销毁，否则切换时编辑不生效
            if (blockEditor) {
                try { blockEditor.destroy(); } catch (e) { }
                blockEditor = null;
            }
            // 自行创建 monaco-host（若不存在；wysiwyg.js 已删除不再提供）
            monacoHost = editorInner.querySelector('#mdgo-editor-monaco');
            if (!monacoHost) {
                monacoHost = document.createElement('div');
                monacoHost.id = 'mdgo-editor-monaco';
                monacoHost.style.cssText = 'width:100%;height:100%;';
                editorInner.appendChild(monacoHost);
            }
            blockHost = document.createElement('div');
            blockHost.id = 'mdgo-editor-block';
            blockHost.style.cssText = 'width:100%;height:100%;display:none;overflow:auto;';
            editorInner.appendChild(blockHost);
            const footer = document.getElementById('editor-footer');
            if (footer && !footer.querySelector('.mdgo-block-toggle')) {
                const btn = document.createElement('button');
                btn.type = 'button';
                btn.className = 'mdgo-export-btn mdgo-block-toggle';
                btn.textContent = '块模式';
                btn.title = '切换到 Notion 式块编辑（TipTap）';
                btn.addEventListener('click', () => window.toggleBlockMode());
                const right = footer.querySelector('.status-right');
                if (right) right.prepend(btn);
            }
            updateBlockBtn(false);
        } else {
            blockHost = editorInner.querySelector('#mdgo-editor-block');
            monacoHost = editorInner.querySelector('#mdgo-editor-monaco');
        }
    };

    window.MdgoBlock = {
        get active() { return !!(blockEditor && blockHost && blockHost.style.display !== 'none'); },
        getValue() { return blockEditor ? htmlToMd(blockEditor.getHTML()) : null; },
        setOnChange(cb) { onBlockChange = cb; }
    };
})();
