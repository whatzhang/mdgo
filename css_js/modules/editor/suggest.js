/**
 * ===== 智能输入建议模块（css_js/modules/editor/suggest.js） =====
 *
 * 【职责】P0-1 智能输入（源码模式 · Monaco）：
 *   1. `/` 块菜单：标题/表格/代码块/Mermaid/LaTeX/待办/看板/日期/AI 块
 *   2. `@` 文件引用：扫 _scanFileList（相对路径条目）+ 目录
 *   3. `[[` 双链补全：.md 笔记标题模糊匹配（P2 升级为语义推荐）
 *   4. `# ` 标题语法提示
 * 【入口】main.html initMonacoEditor 的 require 回调中调用 initMarkdownSuggest()
 *         （Monaco 加载完成后注册；本文件在 <body> 底部加载，先于 DOMContentLoaded）
 * 【依赖】window.monaco（懒加载，调用方保证已就绪）；
 *         _scanFileList（主脚本全局，运行时读取，勿在模块加载期依赖）
 * 【对外暴露】window.initMarkdownSuggest —— 幂等，可重复调用
 */
(function () {
    'use strict';

    // ===== 块菜单（/ 触发） =====
    const BLOCK_MENU = [
        {
            label: '标题 1',
            detail: '# 一级标题',
            insertText: '# ${1:标题}$0',
            snippet: true
        },
        {
            label: '标题 2',
            detail: '## 二级标题',
            insertText: '## ${1:标题}$0',
            snippet: true
        },
        {
            label: '标题 3',
            detail: '### 三级标题',
            insertText: '### ${1:标题}$0',
            snippet: true
        },
        {
            label: '表格',
            detail: 'Markdown 表格',
            insertText: '| ${1:列1} | ${2:列2} |\n| --- | --- |\n| ${3:内容} | $0 |',
            snippet: true
        },
        {
            label: '代码块',
            detail: '``` 围栏代码块',
            insertText: '```${1:语言}\n$2\n```$0',
            snippet: true
        },
        {
            label: 'Mermaid 图',
            detail: '流程图/时序图/甘特图等',
            insertText: '```mermaid\nflowchart TD\n    ${1:A} --> ${2:B}\n```$0',
            snippet: true
        },
        {
            label: 'LaTeX 公式',
            detail: '$$ 行间公式 $$',
            insertText: '$$${1:公式}$$$0',
            snippet: true
        },
        {
            label: '待办',
            detail: '- [ ] 任务项',
            insertText: '- [ ] ${1:任务}$0',
            snippet: true
        },
        {
            label: '看板',
            detail: 'kanban 语法（列 + 卡片）',
            insertText: 'kanban\n    ${1:todo}:\n      - ${2:任务}\n    ${3:done}:\n$0',
            snippet: true
        },
        {
            label: '日期',
            detail: '插入当天日期',
            insertText: (() => {
                const d = new Date();
                const pad = (n) => String(n).padStart(2, '0');
                return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
            })()
        },
        {
            label: 'AI 块',
            detail: '动态 AI 块（摘要/待办/语义搜索/标签，P2 生效）',
            insertText: '```ai-block\n{\n  "type": "${1:summary}",\n  "refresh": "auto"\n}\n```$0',
            snippet: true
        }
    ];

    // ===== 数据源访问（运行时读取主脚本全局） =====
    function getFileList() {
        const list = _scanFileList;
        return Array.isArray(list) ? list : [];
    }

    function isMarkdownPath(p) {
        return typeof p === 'string' && /\.md$/i.test(p);
    }

    // ===== 工具：构造 Monaco suggestion =====
    function makeSuggestion(label, detail, insertText, kind, range, snippet) {
        const s = {
            label: String(label),
            kind: kind,
            detail: detail || '',
            insertText: String(insertText),
            range: range
        };
        if (snippet && window.monaco) {
            s.insertTextRules = window.monaco.languages.CompletionItemInsertTextRule.InsertAsSnippet;
        }
        return s;
    }

    function replaceRange(model, position, triggerStart) {
        // 覆盖 [triggerStart, 光标) 区间（含触发符与已输入查询词）
        return {
            startLineNumber: position.lineNumber,
            startColumn: triggerStart,
            endLineNumber: position.lineNumber,
            endColumn: position.column
        };
    }

    // ===== 触发判定与候选 =====
    async function provide(model, position) {
        const monaco = window.monaco;
        if (!monaco) return { suggestions: [] };
        const linePrefix = model.getValueInRange({
            startLineNumber: position.lineNumber,
            startColumn: 1,
            endLineNumber: position.lineNumber,
            endColumn: position.column
        });
        const SnippetKind = monaco.languages.CompletionItemKind.Snippet;
        const FileKind = monaco.languages.CompletionItemKind.File;
        const TextKind = monaco.languages.CompletionItemKind.Text;

        // ── / 块菜单（支持 /查询词 过滤；触发要求：行首/空白/'>' 后的 /，避免 URL/路径误触） ──
        const slash = linePrefix.match(/(^|[\s>])\/([\w\u4e00-\u9fa5]*)$/);
        if (slash) {
            const query = slash[2].toLowerCase();
            const startCol = position.column - 1 - slash[2].length; // 覆盖 / 与查询词
            const suggestions = BLOCK_MENU
                .filter(item => !query || item.label.toLowerCase().includes(query))
                .map(item => makeSuggestion(
                    item.label, item.detail, item.insertText, SnippetKind,
                    replaceRange(model, position, startCol), !!item.snippet
                ));
            return { suggestions };
        }

        // ── @ 文件引用（修复：要求 @ 前是空白/行首/左括号，避免命中邮箱、URL 中的 @） ──
        const at = linePrefix.match(/(^|[\s(])(@)([\w\-\u4e00-\u9fa5\/\.]*)$/);
        if (at) {
            const query = at[3].toLowerCase();
            const startCol = position.column - 1 - at[3].length; // 覆盖 @ 与查询词
            const files = getFileList()
                .filter(f => f && f.path && f.path.toLowerCase().includes(query))
                .slice(0, 30)
                .map(f => {
                    const name = f.name || f.path.split('/').pop() || f.path;
                    return makeSuggestion(
                        name,
                        f.path + (isMarkdownPath(f.path) ? ' 📄' : ''),
                        f.path, FileKind, replaceRange(model, position, startCol), false
                    );
                });
            return { suggestions: files };
        }

        // ── [[ 双链补全（未闭合；P2-4 合并语义候选） ──
        const wiki = linePrefix.match(/\[\[([^\[\]\n]*)$/);
        if (wiki) {
            const query = wiki[1].toLowerCase();
            const startCol = position.column - 2 - wiki[1].length; // 覆盖 [[ 与查询词
            const suggestions = [];
            // 1) 文件名/标题模糊匹配（本地即时）
            getFileList()
                .filter(f => isMarkdownPath(f.path) && (f.name || '').toLowerCase().includes(query))
                .slice(0, 12)
                .forEach(f => {
                    const name = f.name || f.path;
                    const alias = name.replace(/\.md$/i, '');
                    suggestions.push(makeSuggestion(
                        alias, f.path, `[[${alias}]]`, TextKind, replaceRange(model, position, startCol), false
                    ));
                });
            // 2) 语义推荐（P2-4）：kb_search_hybrid 候选合并，去重后置顶
            if (typeof window.MdgoSemantic === 'object' && window.MdgoSemantic.wikilinkCandidates) {
                try {
                    const sem = await window.MdgoSemantic.wikilinkCandidates(query || wiki[1], 6);
                    const existing = new Set(suggestions.map(s => s.label));
                    for (const c of sem.slice(0, 4)) {
                        if (!c.name || existing.has(c.name)) continue;
                        existing.add(c.name);
                        suggestions.unshift(makeSuggestion(
                            c.name + ' ✨',
                            '语义推荐 · ' + (c.path || ''),
                            `[[${c.name}]]`, TextKind, replaceRange(model, position, startCol), false
                        ));
                    }
                } catch (e) { /* 语义候选失败不影响本地匹配 */ }
            }
            return { suggestions };
        }

        // ── # 标题语法提示（# 后恰为空格） ──
        const hash = linePrefix.match(/^(#{1,6})\s+$/);
        if (hash) {
            const level = hash[1].length;
            const suggestions = BLOCK_MENU
                .filter(b => /^标题/.test(b.label))
                .slice(0, level > 3 ? 1 : 3)
                .map(b => makeSuggestion(
                    b.label, b.detail, b.insertText, SnippetKind,
                    replaceRange(model, position, position.column - level - 1), true
                ));
            return { suggestions };
        }

        return { suggestions: [] };
    }

    // ===== 注册入口（幂等） =====
    let registered = false;
    window.initMarkdownSuggest = function () {
        if (registered || !window.monaco) return;
        registered = true;
        window.monaco.languages.registerCompletionItemProvider('markdown', {
            triggerCharacters: ['/', '@', '[', '#'],
            provideCompletionItems: provide
        });
        console.log('[mdgo] Markdown 智能输入（/ @ [[ #）已注册');
    };
})();
