/**
 * ===== 小助手文档问答浮层（DocAgent · 宿主 A） =====
 *
 * 【职责】2/3 浮层「小助手」：打开当前文件 → 自动「关联当前文件」→ 对文档流式问答；
 *         会话以 type='doc' 落库、按「文件」维度本地聚合（localStorage 映射 文件↔sessionId）。
 * 【依赖】main.html 全局：isTauriVisit / showNotification / markedParse / parseObsidianToHTML /
 *         postProcessMarkdown / currentRootPath / currentActiveItem / currentFileName；
 *         Tauri 命令：doc_agent_query / doc_read_meta / kb_cancel_task /
 *         chat_session_create / chat_session_messages / chat_message_save。
 * 【事件】llm:delta / llm:thinking / llm:usage / llm:done / llm:error（与 chat 通道一致）。
 * 【限制】引用以 [§N] / (file:行-行) 随回答文本展示；行号点击定位在 citation 渲染层补全（P0/P1）。
 */
(function () {
    if (typeof window.__docQaInited !== 'undefined') return;
    window.__docQaInited = true;

    const MAP_KEY = 'mdgo_doc_sessions'; // { [dir+"\x01"+file]: { id, title, updatedAt } }
    const state = {
        open: false,
        streaming: false,
        requestId: null,
        sessionId: null,
        messages: [],       // [{role:'user'|'assistant', content}]
        file: '',
        dirPath: '',
        fileMeta: null,
        selText: '',
        selOffsets: null,   // {start,end} 文件字符偏移（选区优先问答用）
        depth: 'auto',
        writeMode: 'explore', // explore=先确认后写入；execute=直接写入
        remember: false,      // 记忆开关（注入长期记忆上下文）
        useBookmarks: false,  // P2-2 书签辅助参考
        template: 'default',  // 会话风格模板（P2-4：选择自动记住，下次打开沿用）
        compressNote: '',     // 本地历史压缩提示（T1-6b）
        scopeFiles: [],       // 会话级资料圈（T1-4b，≤3）
        layout: 'modal',      // modal(居中 2/3) | sidebar(右侧边栏)
        unlisteners: [],
    };
    // ── 布局：modal（居中浮层）⇄ docked（停靠主页面右侧、TOC 之后同图层） ──
    const LAYOUT_KEY = 'mdgo_doc_layout';
    try {
        const savedLayout = localStorage.getItem(LAYOUT_KEY);
        if (savedLayout === 'docked') state.layout = 'docked';
    } catch (e) { /* ignore */ }

    function setLayoutBtnUI() {
        const btn = $('doc-qa-layout-btn');
        if (!btn) return;
        const docked = state.layout === 'docked';
        btn.textContent = docked ? '窗口' : '停靠';
        btn.title = docked ? '切换为居中模态框' : '停靠到右侧（TOC 右侧）';
    }
    function placeDocQaNode() {
        const ov = overlay();
        if (!ov) return;
        const docked = state.layout === 'docked';
        ov.classList.toggle('doc-qa-docked', docked);
        const toc = document.getElementById('toc-container');
        if (docked) {
            if (toc && toc.parentNode && ov.parentNode !== toc.parentNode) {
                toc.insertAdjacentElement('afterend', ov);
            } else if (!toc && ov.parentNode !== document.body) {
                document.body.appendChild(ov);
            }
        } else if (ov.parentNode !== document.body) {
            document.body.appendChild(ov);
        }
    }
    function applyDocQaLayout() {
        placeDocQaNode();
        setLayoutBtnUI();
    }
    function docQaToggleLayout() {
        state.layout = state.layout === 'docked' ? 'modal' : 'docked';
        try {
            localStorage.setItem(LAYOUT_KEY, state.layout);
        } catch (e) { /* ignore */ }
        applyDocQaLayout();
        const input = $('doc-qa-input');
        if (input) input.focus();
    }

    // 小助手图标点击：停靠态下为 隐藏/显示 切换；模态态下为 开/关 切换
    function docQaFabClick() {
        if (state.layout === 'docked') {
            if (state.open) {
                closeDocQA();
                return;
            }
            applyDocQaLayout(); // 确保停靠列就位（TOC 右侧）
            openDocQA();
            return;
        }
        if (state.open) {
            closeDocQA();
            return;
        }
        openDocQA();
    }

    // 停靠态下离开文件视图（如 Dashboard）时隐藏停靠栏
    function docQaCloseIfDockedOpen() {
        if (state.open && state.layout === 'docked') closeDocQA();
    }
    function docQaFileSwitched() {
        if (!state.open || state.layout !== 'docked') return;
        if (!currentRootPath) {
            closeDocQA();
            return;
        }
        const rel = currentRelPath();
        if (!rel) {
            closeDocQA();
            return;
        }
        openDocQA(); // 内部：文件变化 → 重置消息并恢复该文件最近 doc 会话；文件未变则保持
    }
    const TEMPLATE_KEY = 'mdgo_doc_template';
    const DOC_TEMPLATES = {
        default: { text: '' },
        precise: { text: '必须逐条对照原文给出依据，引用标注 [§N] 与行号；不确定处明确说明“未在文中找到”，禁止推测。' },
        concise: { text: '回答尽量精炼：要点优先、少铺垫；能使用列表/表格就使用，不要重复提问内容。' },
        tutor: { text: '以通俗方式讲解文档内容，多用例子与类比；引申内容需明确标注为引申。' },
    };

    const $ = (id) => document.getElementById(id);
    const overlay = () => $('doc-qa-overlay');
    const messagesEl = () => $('doc-qa-messages');

    // ── 工具 ──
    function fileKey() {
        return state.dirPath + '\u0001' + state.file;
    }
    function loadMap() {
        try {
            const raw = localStorage.getItem(MAP_KEY);
            const obj = raw ? JSON.parse(raw) : {};
            return (obj && typeof obj === 'object') ? obj : {};
        } catch (e) {
            return {};
        }
    }
    function saveMap(obj) {
        try { localStorage.setItem(MAP_KEY, JSON.stringify(obj)); } catch (e) { /* ignore */ }
    }
    function estimateTokens(text) {
        let chars = 0, cjk = 0;
        for (const ch of String(text || '')) {
            chars++;
            if (ch >= '\u4e00' && ch <= '\u9fff') cjk++;
        }
        return Math.ceil(cjk / 1.5 + (chars - cjk) / 4);
    }
    function currentRelPath() {
        try {
            if (currentActiveItem && currentActiveItem.dataset && currentActiveItem.dataset.filePath) {
                return currentActiveItem.dataset.filePath;
            }
        } catch (e) { /* ignore */ }
        return typeof currentFileName === 'string' ? currentFileName : '';
    }
    const SEND_ICON_SVG = '<svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor"><path d="M2.01 21L23 12 2.01 3 2 10l15 2-15 2z" /></svg>';
    const STOP_ICON_SVG = '<svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor"><rect x="6" y="6" width="12" height="12" rx="1.5" /></svg>';

    // 发送/停止合一（与 Agent 页一致）：busy 时同一按钮变为“停止”
    function setBusy(busy) {
        const send = $('doc-qa-send');
        if (send) {
            send.disabled = false;
            send.innerHTML = busy ? STOP_ICON_SVG : SEND_ICON_SVG;
            send.title = busy ? '停止生成' : '发送消息';
            send.setAttribute('aria-label', busy ? '停止生成' : '发送');
            send.classList.toggle('doc-qa-streaming', busy);
        }
        state.streaming = busy;
    }
    function pushRender(role, content, streamingBody) {
        const box = messagesEl();
        if (!box) return null;
        const wrap = document.createElement('div');
        wrap.className = 'doc-qa-msg ' + role;
        if (role === 'user') {
            wrap.textContent = content;
            box.appendChild(wrap);
        } else {
            const body = document.createElement('div');
            body.className = 'markdown-body';
            wrap.appendChild(body);
            const foot = document.createElement('div');
            foot.className = 'doc-qa-msg-actions';
            for (const [act, label, tip] of [['copy', '复制', '复制回答全文'], ['preview', '预览', '预览结构化内容（Mermaid 等）'], ['insert', '插入', '插入到光标处（需编辑态）'], ['replace', '替换选区', '替换当前选区（需编辑态且有选区）'], ['note', '存为笔记', '保存为库内新笔记并回链当前文档']]) {
                const btn = document.createElement('button');
                btn.type = 'button';
                btn.className = 'doc-qa-msg-btn';
                btn.dataset.action = act;
                btn.textContent = label;
                btn.title = tip;
                foot.appendChild(btn);
            }
            wrap.appendChild(foot);
            box.appendChild(wrap);
            return body;
        }
        box.scrollTop = box.scrollHeight;
        return wrap;
    }
    async function renderAssistantBody(body, md, decorate) {
        if (!body) return;
        try {
            let src = md || '';
            if (decorate) src = decorateCitations(src);
            const html = await markedParse(parseObsidianToHTML(src));
            body.innerHTML = html;
            try { postProcessMarkdown(body); } catch (e) { /* 忽略后处理异常 */ }
            const box = messagesEl();
            if (box) box.scrollTop = box.scrollHeight;
        } catch (e) {
            body.textContent = md;
        }
    }

    // 把回答中的 [§N] 引用装饰成可点击元素（点击定位到原文章节）
    function decorateCitations(md) {
        return String(md || '').replace(/\[§(\d+)\]/g,
            '<span class="doc-qa-cite" data-sec="$1" title="点击定位到原文章节 §$1">[§$1]</span>');
    }

    async function jumpToSection(secId) {
        const meta = state.fileMeta;
        if (!meta || !Array.isArray(meta.sections)) {
            showNotification('暂无文档章节信息，无法定位', 'warning');
            return;
        }
        const sec = meta.sections.find((s) => s.id === Number(secId));
        if (!sec) {
            showNotification(`未找到章节 §${secId}`, 'warning');
            return;
        }
        const snippetLines = [sec.heading]
            .concat(sec.text.split('\n').filter((l) => l.trim()))
            .slice(0, 5);
        const snippet = snippetLines.join('\n');
        const isCurrent = currentFileName && String(currentFileName).toLowerCase() === String(state.file).split('/').pop().toLowerCase();
        try {
            if (isCurrent && typeof locateInOpenedFile === 'function') {
                const hit = await locateInOpenedFile({ doc_name: state.file, snippet });
                if (hit) return;
            }
            if (typeof openSourceAndLocate === 'function') {
                await openSourceAndLocate({ doc_name: state.file, snippet });
                return;
            }
        } catch (e) {
            console.warn('[doc-qa] 引用定位失败:', e);
        }
        showNotification(`定位 §${secId}（${sec.heading} · 第 ${sec.line_start}–${sec.line_end} 行）`, 'info');
    }
    // 选区字符偏移换算：markdown 选区是 DOM 文本，须映射回文件原始文本（char 计数）
    function rawContentOfActiveFile() {
        try {
            if (typeof currentEditor !== 'undefined' && currentEditor && typeof currentEditor.getValue === 'function') {
                return currentEditor.getValue() || '';
            }
        } catch (e) { /* ignore */ }
        try {
            return (typeof originalContent === 'string' ? originalContent : '') || '';
        } catch (e) {
            return '';
        }
    }
    function findCharOffsets(selText) {
        if (!selText) return null;
        const raw = rawContentOfActiveFile();
        if (!raw) return null;
        const byteIdx = raw.indexOf(selText);
        if (byteIdx < 0) {
            // 预览 DOM 可能对选区做了规范化，退化：按首个非空行搜索
            const firstLine = selText.split('\n').find((l) => l.trim());
            if (!firstLine) return null;
            const idx2 = raw.indexOf(firstLine.trim());
            if (idx2 < 0) return null;
            return { start: charCountBefore(raw, idx2), end: charCountBefore(raw, idx2 + firstLine.trim().length) };
        }
        return { start: charCountBefore(raw, byteIdx), end: charCountBefore(raw, byteIdx + selText.length) };
    }
    function charCountBefore(str, byteIdx) {
        return Array.from(str.slice(0, byteIdx)).length;
    }
    // @提及 / [[双链]] / @文件夹 解析：返回 {files, folders}（各 ≤3 / ≤2，去重且排除当前文件）
    async function collectExtraFiles(text) {
        const files = [];
        const folders = [];
        const seenFile = new Set([state.file]);
        const tryAdd = async (t) => {
            if (files.length >= 3 || !t || seenFile.has(t)) return;
            const cands = [t];
            if (!t.includes('/') && !/\.(md|txt)$/i.test(t)) {
                cands.push(t + '.md', t + '.txt');
            }
            for (const c of cands) {
                try {
                    await invoke('doc_read_meta', { dirPath: state.dirPath, relPath: c, budgetTokens: null });
                    seenFile.add(c);
                    files.push(c);
                    return;
                } catch (e) { /* 尝试下一个候选 */ }
            }
        };
        const folderSet = new Set();
        for (const m of String(text || '').matchAll(/@([\w.\-\/\u4e00-\u9fa5]+?)\/(?=\s|$)/g)) {
            const f = m[1].trim().replace(/\/+$/, '');
            if (f && !folderSet.has(f) && folders.length < 2) {
                folderSet.add(f);
                folders.push(f);
            }
        }
        // #标签：目录内 frontmatter 标签匹配（跨库标签检索归主 Agent 整库 RAG）
        for (const m of String(text || '').matchAll(/(?:^|\s)#([\w\u4e00-\u9fa5-]{1,24})/g)) {
            const tag = m[1];
            if (!tag) continue;
            try {
                const rels = await invoke('doc_tag_files', { dirPath: state.dirPath, filePath: state.file, tag });
                if (Array.isArray(rels)) {
                    for (const r of rels) {
                        if (files.length >= 3 || seenFile.has(r)) continue;
                        seenFile.add(r);
                        files.push(r);
                    }
                }
            } catch (e) { /* 忽略标签检索失败 */ }
            break; // 本轮只处理首个 #标签
        }
        const wiki = [...String(text || '').matchAll(/\[\[([^\[\]#|]+)/g)].map((m) => m[1].trim()).filter(Boolean);
        const at = [...String(text || '').matchAll(/@([\w.\-\/\u4e00-\u9fa5]+)/g)]
            .map((m) => m[1])
            .filter((s) => s.length >= 2 && !s.endsWith('/') && !folderSet.has(s));
        const explicit = [...wiki, ...at];
        for (const t of explicit) await tryAdd(t);
        if (explicit.length === 0 && files.length < 3) {
            // 无显式提及时，自动带入当前文档的 [[双链]] 邻居（补足剩余名额）
            const raw = rawContentOfActiveFile();
            if (raw) {
                const neighbors = [...raw.matchAll(/\[\[([^\[\]#|]+)/g)].map((m) => m[1].trim());
                for (const t of neighbors) {
                    if (files.length >= 3) break;
                    await tryAdd(t);
                }
            }
        }
        return { files, folders };
    }
    // 相关笔记提示（P1-7）：打开文件且尚无对话时，带出本文档的 [[双链]] 邻居，可一键带入
    async function renderRelatedChips() {
        const box = $('doc-qa-related');
        if (!box) return;
        box.innerHTML = '';
        const show = !state.selText && state.messages.length === 0;
        box.style.display = show ? '' : 'none';
        if (!show) return;
        const seen = new Set([state.file]);
        const targets = [];
        // 1) 服务端语义相关（词面重叠近似）Top3
        try {
            const rels = await invoke('doc_related', { dirPath: state.dirPath, filePath: state.file, limit: 3 });
            if (Array.isArray(rels)) {
                for (const r of rels) {
                    if (!r || !r.rel_path || seen.has(r.rel_path)) continue;
                    seen.add(r.rel_path);
                    targets.push({ name: r.rel_path, title: (r.score ? '相关度 ' + Math.round(r.score * 100) + '%' : '') });
                    if (targets.length >= 3) break;
                }
            }
        } catch (e) { /* 服务不可用时回退双链 */ }
        // 2) 回退/补充：本文档 [[双链]] 邻居
        if (targets.length === 0) {
            const raw = rawContentOfActiveFile();
            if (raw) {
                for (const m of raw.matchAll(/\[\[([^\[\]#|]+)/g)) {
                    const t = m[1].trim();
                    if (!t || seen.has(t)) continue;
                    seen.add(t);
                    targets.push({ name: t, title: '' });
                    if (targets.length >= 3) break;
                }
            }
        }
        if (targets.length === 0) {
            box.style.display = 'none';
            return;
        }
        const tip = document.createElement('span');
        tip.className = 'doc-qa-sel-tip';
        tip.textContent = '相关笔记：';
        box.appendChild(tip);
        for (const t of targets) {
            const b = document.createElement('button');
            b.type = 'button';
            b.className = 'doc-qa-sel-chip';
            b.textContent = t.name;
            b.title = t.title + '（点击把该笔记带入本轮问答）';
            b.onclick = () => docQaRelated(t.name);
            box.appendChild(b);
        }
    }
    function docQaRelated(target) {
        const input = $('doc-qa-input');
        if (!input) return;
        input.value = (input.value ? input.value.trim() + ' ' : '') + `[[${target}]]`;
        input.focus();
        const box = $('doc-qa-related');
        if (box) box.style.display = 'none';
    }

    // ── 会话级资料圈选（T1-4b） ──
    async function loadScopeCandidates() {
        const sel = $('doc-qa-scope-select');
        if (!sel) return;
        const prev = [...sel.selectedOptions].map((o) => o.value);
        sel.innerHTML = '';
        try {
            const files = await invoke('doc_dir_files', { dirPath: state.dirPath, filePath: state.file });
            if (Array.isArray(files)) {
                for (const f of files) {
                    if (!f || f === state.file) continue;
                    const o = document.createElement('option');
                    o.value = f;
                    o.textContent = f;
                    sel.appendChild(o);
                }
            }
        } catch (e) { /* 目录不可用 */ }
        for (const o of [...sel.options]) {
            if (prev.includes(o.value)) o.selected = true;
        }
        applyScopeSelection();
    }
    function applyScopeSelection() {
        const sel = $('doc-qa-scope-select');
        const picked = sel ? [...sel.selectedOptions].map((o) => o.value) : [];
        state.scopeFiles = picked.slice(0, 3);
        updateHint();
    }
    async function toggleDocQaScope() {
        const wrap = $('doc-qa-scope');
        if (!wrap) return;
        if (wrap.style.display !== 'none') {
            wrap.style.display = 'none';
            return;
        }
        wrap.style.display = '';
        const sel = $('doc-qa-scope-select');
        if (sel && sel.options.length === 0) await loadScopeCandidates();
    }
    function clearDocQaScope() {
        state.scopeFiles = [];
        const sel = $('doc-qa-scope-select');
        if (sel) {
            for (const o of [...sel.options]) o.selected = false;
        }
        updateHint();
    }

    // P2-2 相关书签（前端检索 → 仅文本注入）
    async function collectBookmarks(text) {
        if (!state.useBookmarks) return [];
        try {
            const res = await invoke('bookmark_search', { dirPath: state.dirPath, query: text, opts: { limit: 5 } });
            if (!Array.isArray(res)) return [];
            return res.slice(0, 3).map((b) => {
                const t = b && (b.title || b.name || b.category || '');
                const url = b && (b.url || b.link || '');
                const s = b && (b.summary || b.snippet || '');
                let line = String(t || '');
                if (url) line += (line ? ' | ' : '') + String(url);
                if (s) line += ' — ' + String(s).slice(0, 120);
                return line;
            }).filter(Boolean);
        } catch (e) {
            return [];
        }
    }
    function trimLocal(list) {
        if (list.length > 60) {
            state.compressNote = '已自动压缩较早消息（保留最近 ' + 60 + ' 条，服务端按需继续压缩历史）';
            return list.slice(list.length - 60);
        }
        return list;
    }

    // ── 消息写回动作：复制 / 插入光标处 / 替换选区 ──
    function contentOfWrap(node) {
        const w = node && node.closest ? node.closest('.doc-qa-msg.assistant') : null;
        return w && w.__docQa ? w.__docQa : '';
    }
    function setWrapContent(node, text) {
        const w = node && node.closest ? node.closest('.doc-qa-msg.assistant') : null;
        if (w) w.__docQa = text;
    }
    function currentFileMatchesDoc() {
        if (!currentFileName || !state.file) return false;
        return String(currentFileName).toLowerCase() === String(state.file).split('/').pop().toLowerCase();
    }
    function monacoApply(text, replaceSelection) {
        const ed = typeof currentEditor !== 'undefined' ? currentEditor : null;
        if (!ed || typeof ed.executeEdits !== 'function') return false;
        const model = ed.getModel ? ed.getModel() : null;
        if (!model) return false;
        if (replaceSelection) {
            const sel = ed.getSelection ? ed.getSelection() : null;
            if (!sel || sel.isEmpty()) return false;
            ed.executeEdits('doc-qa', [{ range: sel, text, forceMoveMarkers: true }]);
        } else {
            let lineNumber = 1;
            let column = 1;
            try {
                const pos = ed.getPosition();
                if (pos) { lineNumber = pos.lineNumber; column = pos.column; }
            } catch (e) { /* ignore */ }
            const range = {
                startLineNumber: lineNumber,
                startColumn: column,
                endLineNumber: lineNumber,
                endColumn: column,
            };
            ed.executeEdits('doc-qa', [{ range, text, forceMoveMarkers: true }]);
        }
        try { ed.focus(); } catch (e) { /* ignore */ }
        return true;
    }
    // 「存为笔记」：保存为库内新 Markdown，自动带来源回链（复用主程序的保存目录弹窗）
    function saveAnswerAsNote(content) {
        if (typeof showSaveModal !== 'function' || typeof saveAIResultToFolder !== 'function') {
            showNotification('当前环境不支持保存为笔记，请用“复制”后手动新建', 'warning');
            return;
        }
        const ts = new Date();
        const pad = (n) => String(n).padStart(2, '0');
        const stamp = `${ts.getFullYear()}-${pad(ts.getMonth() + 1)}-${pad(ts.getDate())}`;
        const base = state.file ? String(state.file).split('/').pop().replace(/\.(md|txt)$/i, '') : '文档';
        const backlink = state.file ? `> 由小助手基于「${state.file}」生成（${stamp}）\n\n` : '';
        const defaultName = `${base}-小助手笔记-${stamp}.md`;
        showSaveModal({
            title: '保存为 Markdown 笔记',
            getDefaultFileName: () => defaultName,
            getContent: () => backlink + (content || ''),
            onSave: saveAIResultToFolder,
        });
    }

    // 「存会话」：把整段会话导出为库内 Markdown（会话即资产；内容可被全库检索）
    function docQaExportConversation() {
        if (typeof showSaveModal !== 'function' || typeof saveAIResultToFolder !== 'function') {
            showNotification('当前环境不支持导出会话', 'warning');
            return;
        }
        if (state.messages.length === 0) {
            showNotification('当前没有可导出的对话内容', 'warning');
            return;
        }
        const ts = new Date();
        const pad = (n) => String(n).padStart(2, '0');
        const stamp = `${ts.getFullYear()}-${pad(ts.getMonth() + 1)}-${pad(ts.getDate())}-${pad(ts.getHours())}${pad(ts.getMinutes())}`;
        const base = state.file ? String(state.file).split('/').pop().replace(/\.(md|txt)$/i, '') : '文档';
        const header = `# 小助手会话：${state.file || '未关联'}（${stamp}）\n\n> 由小助手自动导出，可在知识库中检索。\n\n---\n\n`;
        let body = '';
        let lastRole = '';
        for (const m of state.messages) {
            const label = m.role === 'user' ? '**👤 提问**' : '**🤖 小助手**';
            const sep = m.role === lastRole ? '\n\n' : '\n\n';
            body += (body ? '\n\n---\n\n' : '') + `${label}\n\n${m.content}`;
            lastRole = m.role;
        }
        showSaveModal({
            title: '保存会话为 Markdown',
            getDefaultFileName: () => `${base}-小助手会话-${stamp}.md`,
            getContent: () => header + body + '\n',
            onSave: saveAIResultToFolder,
        });
    }

    // ── 结构化预览（围栏代码块：Mermaid 渲染，其它语言展示/插入） ──
    let _previewCode = '';
    let _previewLang = '';
    function extractFencedCode(content) {
        const m = String(content || '').match(/```([\w+-]*)\s*\n([\s\S]*?)```/);
        return m ? { lang: (m[1] || '').trim().toLowerCase(), code: m[2].trim() } : null;
    }
    async function openStructuredPreview(content) {
        const fence = extractFencedCode(content);
        const ov = $('doc-qa-preview-overlay');
        const box = $('doc-qa-preview-content');
        if (!ov || !box) return;
        if (!fence) {
            showNotification('该回答未包含可预览的围栏代码块', 'warning');
            return;
        }
        const { lang, code } = fence;
        _previewCode = code;
        _previewLang = lang;
        _writePending = null;
        resetPreviewUi();
        box.innerHTML = '';
        const pre = document.createElement('pre');
        pre.textContent = '```' + lang + '\n' + code + '\n```';
        box.appendChild(pre);
        const isDiagram = lang === 'mermaid' || lang === 'mmd';
        const title = $('doc-qa-preview-title');
        if (title) title.textContent = isDiagram ? '结构化预览（' + lang + '）' : '代码预览（' + (lang || 'text') + '）';
        ov.classList.add('doc-qa-preview-open');
        if (isDiagram) {
            try {
                if (typeof mermaid !== 'undefined' && typeof mermaid.render === 'function') {
                    const rendered = document.createElement('div');
                    rendered.className = 'mermaid';
                    box.appendChild(rendered);
                    const id = 'docqa-mmd-' + Math.random().toString(36).slice(2, 8);
                    const { svg } = await mermaid.render(id, code);
                    rendered.innerHTML = svg;
                } else {
                    const note = document.createElement('p');
                    note.style.cssText = 'font-size:12px;color:var(--color-text-secondary)';
                    note.textContent = '（未加载 Mermaid 渲染器，已展示源码，可插入后查看）';
                    box.appendChild(note);
                }
            } catch (e) {
                const err = document.createElement('p');
                err.style.cssText = 'color:#b00020;font-size:12px';
                err.textContent = '渲染失败：' + String((e && e.message) || e);
                box.appendChild(err);
            }
        }
    }
    function closeDocQaPreview() {
        _writePending = null;
        const ov = $('doc-qa-preview-overlay');
        if (ov) ov.classList.remove('doc-qa-preview-open');
        resetPreviewUi();
    }
    function resetPreviewUi() {
        const t = $('doc-qa-preview-title');
        if (t) t.textContent = '结构化预览';
        const btn = $('doc-qa-preview-insert');
        if (btn) btn.textContent = '插入到光标处';
    }
    function insertPreviewToCursor() {
        if (!_previewCode) {
            closeDocQaPreview();
            return;
        }
        const fenceText = '```' + (_previewLang || '') + '\n' + _previewCode + '\n```';
        const ok = monacoApply(fenceText, false);
        if (ok) {
            showNotification('✓ 已插入到光标处', 'success');
            closeDocQaPreview();
        } else {
            copyTextToClipboard(fenceText);
            showNotification('当前视图无编辑器，代码已复制', 'warning');
            closeDocQaPreview();
        }
    }

    // ── Explore 写回差异预览（T1-8a）：原文（或插入位置）↔ 拟写入 ──
    let _writePending = null; // {act, content}
    function getMonacoSelectionText() {
        const ed = typeof currentEditor !== 'undefined' ? currentEditor : null;
        if (!ed || typeof ed.getSelection !== 'function' || !ed.getModel) return '';
        try {
            const sel = ed.getSelection();
            if (!sel || sel.isEmpty()) return '';
            return ed.getModel().getValueInRange(sel) || '';
        } catch (e) {
            return '';
        }
    }
    function getMonacoCursorContext() {
        const ed = typeof currentEditor !== 'undefined' ? currentEditor : null;
        if (!ed || typeof ed.getPosition !== 'function' || !ed.getModel) return null;
        try {
            const pos = ed.getPosition();
            const text = String(ed.getModel().getLineContent(pos.lineNumber) || '').trim();
            return { line: pos.lineNumber, text: text.slice(0, 90) };
        } catch (e) {
            return null;
        }
    }
    function openWriteDiffPreview(act, node) {
        const content = contentOfWrap(node);
        const titleEl = $('doc-qa-preview-title');
        const btn = $('doc-qa-preview-insert');
        const box = $('doc-qa-preview-content');
        const ov = $('doc-qa-preview-overlay');
        if (!box || !ov) return;
        let html = '';
        if (act === 'replace') {
            const oldText = getMonacoSelectionText();
            html += '<div class="doc-qa-diff-title">原文（当前选区）</div><pre class="doc-qa-diff-old">'
                + escapeHtml(oldText || '（未读取到选区内容）') + '</pre>';
        } else {
            const ctx = getMonacoCursorContext();
            html += ctx
                ? '<div class="doc-qa-diff-title">插入位置（第 ' + ctx.line + ' 行）</div><pre class="doc-qa-diff-old">…' + escapeHtml(ctx.text || '') + '…</pre>'
                : '<div class="doc-qa-diff-title">插入位置</div><pre class="doc-qa-diff-old">（编辑器光标未知，将在当前光标处插入）</pre>';
        }
        html += '<div class="doc-qa-diff-title">' + (act === 'replace' ? '将替换为' : '将插入') + '</div><pre class="doc-qa-diff-new">'
            + escapeHtml(content) + '</pre>';
        box.innerHTML = html;
        if (titleEl) titleEl.textContent = '写回确认 · Explore';
        if (btn) btn.textContent = act === 'replace' ? '应用替换' : '应用插入';
        _writePending = { act, content };
        ov.classList.add('doc-qa-preview-open');
    }
    function performWritePending() {
        const p = _writePending;
        if (!p) return false;
        if (state.streaming) {
            showNotification('回答仍在生成，请先停止再应用写回', 'warning');
            return false;
        }
        const ok = monacoApply(p.content, p.act === 'replace');
        if (ok) {
            showNotification(p.act === 'replace' ? '✓ 已替换选区' : '✓ 已插入到光标处', 'success');
            closeDocQaPreview();
        } else {
            showNotification('当前视图无可用编辑器（请回到文件编辑态后再试）', 'warning');
            closeDocQaPreview();
        }
        return true;
    }
    function previewActionApply() {
        if (_writePending) {
            performWritePending();
        } else {
            insertPreviewToCursor();
        }
    }

    async function applyMsgAction(act, node) {
        const content = contentOfWrap(node);
        if (!content) {
            showNotification('暂无回答内容', 'warning');
            return;
        }
        if (act === 'copy') {
            copyTextToClipboard(content);
            showNotification('✓ 回答已复制到剪贴板', 'success');
            return;
        }
        if (act === 'note') {
            saveAnswerAsNote(content);
            return;
        }
        if (act === 'preview') {
            openStructuredPreview(content);
            return;
        }
        if (act === 'insert' || act === 'replace') {
            if (!currentFileMatchesDoc()) {
                showNotification('当前文件与问答所关联文件不一致，请先打开 ' + state.file + ' 后再插入/替换', 'warning');
                return;
            }
            if (act === 'replace') {
                const sel = typeof currentEditor !== 'undefined' && currentEditor && typeof currentEditor.getSelection === 'function'
                    ? currentEditor.getSelection() : null;
                if (!sel || sel.isEmpty()) {
                    showNotification('编辑器中没有选中区域，无法执行替换（可先用“插入”）', 'warning');
                    return;
                }
            }
            if (state.writeMode === 'explore') {
                openWriteDiffPreview(act, node);
                return;
            }
            if (state.streaming) {
                showNotification('回答仍在生成，请先停止再写回（Execute）', 'warning');
                return;
            }
            const ok = monacoApply(content, act === 'replace');
            if (ok) {
                showNotification(act === 'replace' ? '✓ 已替换选区' : '✓ 已插入到光标处', 'success');
            } else {
                showNotification('当前视图无可用编辑器（请在文件编辑态执行插入/替换，或使用“复制”）', 'warning');
            }
        }
    }

    // ── 会话与持久化 ──
    async function invoke(cmd, args) {
        return await window.__TAURI__.core.invoke(cmd, args);
    }
    async function ensureSession() {
        if (state.sessionId) return state.sessionId;
        const session = await invoke('chat_session_create', {
            dirPath: state.dirPath,
            title: state.file + ' · 小助手',
            type: 'doc',
        });
        state.sessionId = session.id;
        const map = loadMap();
        map[fileKey()] = { id: session.id, title: session.title || state.file, updatedAt: Date.now() };
        saveMap(map);
        try {
            await invoke('chat_session_set_file_key', { dirPath: state.dirPath, sessionId: session.id, fileKey: state.file });
        } catch (e) { /* 服务端可选能力，失败不阻断 */ }
        await refreshSessionPick();
        return state.sessionId;
    }
    async function touchSession() {
        const map = loadMap();
        const k = fileKey();
        if (map[k]) {
            map[k].updatedAt = Date.now();
            saveMap(map);
        }
        await refreshSessionPick();
    }
    async function persistMsg(role, content) {
        try {
            const sessionId = await ensureSession();
            await invoke('chat_message_save', {
                dirPath: state.dirPath,
                sessionId,
                role,
                content,
                tokenCount: estimateTokens(content),
                toolCalls: null,
                thinking: null,
            });
            await touchSession();
        } catch (e) {
            console.warn('[doc-qa] 消息落库失败:', e);
        }
    }
    async function refreshSessionPick() {
        const sel = $('doc-qa-session-pick');
        if (!sel) return;
        const cur = sel.value;
        sel.innerHTML = '<option value="">— 新对话 —</option>';
        let list = [];
        try {
            list = await invoke('chat_sessions_by_file', {
                dirPath: state.dirPath,
                sessionType: 'doc',
                fileKey: state.file,
            });
        } catch (e) {
            // 服务端暂不可用：回退本地 map
            const map = loadMap();
            const k = fileKey();
            if (map[k]) list = [map[k]];
        }
        for (const s of Array.isArray(list) ? list : []) {
            const opt = document.createElement('option');
            opt.value = s.id;
            const d = new Date(s.updated_at || s.created_at || Date.now());
            const pad = (n) => String(n).padStart(2, '0');
            opt.textContent = (s.title || '会话').slice(0, 40)
                + ' · ' + `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
            sel.appendChild(opt);
        }
        if (cur && [...sel.options].some((o) => o.value === cur)) {
            sel.value = cur;
        } else if (state.sessionId && [...sel.options].some((o) => o.value === state.sessionId)) {
            sel.value = state.sessionId;
        }
    }
    async function loadSession(sessionId) {
        const messages = await invoke('chat_session_messages', { dirPath: state.dirPath, sessionId });
        state.sessionId = sessionId;
        state.compressNote = '';
        state.messages = (Array.isArray(messages) ? messages : [])
            .filter((m) => (m.role === 'user' || m.role === 'assistant') && m.content)
            .map((m) => ({ role: m.role, content: m.content }));
        state.messages = trimLocal(state.messages);
        const box = messagesEl();
        if (box) box.innerHTML = '';
        for (const m of state.messages) {
            if (m.role === 'user') {
                pushRender('user', m.content);
            } else {
                const body = pushRender('assistant', m.content);
                if (body) setWrapContent(body, m.content);
                await renderAssistantBody(body, m.content, true);
            }
        }
        await updateHint();
        const map = loadMap();
        map[fileKey()] = { id: sessionId, title: state.file, updatedAt: Date.now() };
        saveMap(map);
        await refreshSessionPick();
    }

    // ── UI 打开/关闭 ──
    async function openDocQA(selectionText) {
        if (!window.__TAURI__ || !window.__TAURI__.core) {
            showNotification('小助手文档问答需要 Tauri 桌面模式', 'warning');
            return;
        }
        if (!currentRootPath) {
            showNotification('请先打开知识库目录', 'warning');
            return;
        }
        const rel = currentRelPath();
        if (!rel) {
            showNotification('请先打开一个 Markdown/文本文件', 'warning');
            return;
        }
        // Dashboard 等非文件页不显示小助手
        if (typeof dashboardContainer !== 'undefined' && dashboardContainer && dashboardContainer.style.display !== 'none') {
            showNotification('小助手仅在文件页可用（Dashboard 不显示）', 'warning');
            return;
        }
        // 固定「关联当前文件」：始终以当前打开文件作为上下文
        // 同一文件重开：保留上一次上下文；切文件：重置为「继续上次或新会话」
        const switchedFile = state.file !== rel || state.dirPath !== currentRootPath;
        state.file = rel;
        state.dirPath = currentRootPath;
        state.selText = (selectionText || '').trim();
        state.selOffsets = state.selText ? findCharOffsets(state.selText) : null;
        if (switchedFile) {
            state.scopeFiles = [];
            const wrap = $('doc-qa-scope');
            if (wrap) wrap.style.display = 'none';
            const sel = $('doc-qa-scope-select');
            if (sel) sel.innerHTML = '';
        }

        const ov = overlay();
        if (ov) {
            ov.classList.add('doc-qa-open');
            state.open = true;
            applyDocQaLayout();
        }
        if (switchedFile || !state.sessionId) {
            // 尝试恢复该文件的最近 doc 会话
            const map = loadMap();
            const last = map[fileKey()];
            if (last && last.id) {
                try {
                    await loadSession(last.id);
                } catch (e) {
                    state.sessionId = null;
                    state.messages = [];
                    if (messagesEl()) messagesEl().innerHTML = '';
                }
            } else {
                state.sessionId = null;
                state.messages = [];
                if (messagesEl()) messagesEl().innerHTML = '';
            }
        }
        await refreshFileMeta();
        await renderSelQuickChips();
        await renderRelatedChips();
        await updateHint();
        const input = $('doc-qa-input');
        if (input) setTimeout(() => input.focus(), 60);
    }

    function closeDocQA() {
        if (state.streaming) docQaStop();
        const ov = overlay();
        if (ov) ov.classList.remove('doc-qa-open');
        state.open = false;
        cleanupListeners();
    }

    async function cleanupListeners() {
        for (const u of state.unlisteners) {
            try { u(); } catch (e) { /* ignore */ }
        }
        state.unlisteners = [];
    }

    function listen(channel, cb) {
        return window.__TAURI__.event.listen(channel, (ev) => {
            const p = ev.payload || {};
            if (p.request_id && p.request_id !== state.requestId) return;
            cb(p);
        }).then((un) => state.unlisteners.push(un));
    }

    function docQaStop() {
        if (state.streaming && state.requestId) {
            invoke('kb_cancel_task', { requestId: state.requestId }).catch(() => { });
        }
    }

    // ── 主流程 ──
    async function runDocQuestion(text, extraFiles, extraFolders, extraBookmarks) {
        setBusy(true);
        const requestId = (crypto.randomUUID ? crypto.randomUUID() : 'doc-' + Date.now() + '-' + Math.random().toString(36).slice(2, 10));
        state.requestId = requestId;
        const streamingBody = pushRender('assistant', '', true);
        let buffer = '';
        let done = false;
        let errorMsg = '';

        const rafRender = (function () {
            let raf = null;
            return function (fn) {
                if (raf) return;
                raf = requestAnimationFrame(() => { raf = null; fn(); });
            };
        })();

        try {
            await Promise.all([
                listen('llm:delta', (p) => {
                    buffer += p.content || '';
                    rafRender(() => renderAssistantBody(streamingBody, buffer));
                }),
                listen('llm:done', async (p) => {
                    done = true;
                    if (p.content) buffer = p.content;
                    state.messages.push({ role: 'assistant', content: buffer });
                    state.messages = trimLocal(state.messages);
                    await renderAssistantBody(streamingBody, buffer, true);
                    setWrapContent(streamingBody, buffer);
                    await persistMsg('assistant', buffer);
                }),
                listen('llm:error', (p) => {
                    done = true;
                    errorMsg = (p && p.message) || 'LLM 请求失败';
                }),
            ]);
        } catch (e) {
            showNotification('无法监听事件通道: ' + (e.message || e), 'error');
            setBusy(false);
            return;
        }

        try {
            const sel = state.selOffsets;
            await invoke('doc_agent_query', {
                dirPath: state.dirPath,
                filePath: state.file,
                selectionStart: sel && sel.end > sel.start ? sel.start : null,
                selectionEnd: sel && sel.end > sel.start ? sel.end : null,
                extraFiles: extraFiles && extraFiles.length ? extraFiles : null,
                extraFolders: extraFolders && extraFolders.length ? extraFolders : null,
                messages: state.messages.map((m) => ({ role: m.role, content: m.content })),
                requestId,
                sessionId: state.sessionId || null,
                reasoning: state.depth === 'auto' ? null : state.depth,
                includeMemory: !!state.remember,
                systemTemplate: state.template && state.template !== 'default' && DOC_TEMPLATES[state.template]
                    ? DOC_TEMPLATES[state.template].text : null,
                extraBookmarks: extraBookmarks && extraBookmarks.length ? extraBookmarks : null,
            });
        } catch (err) {
            if (!done) {
                errorMsg = (err && err.message) || String(err);
                showNotification('✗ 文档问答失败: ' + errorMsg, 'error');
            }
        } finally {
            await new Promise((r) => setTimeout(r, 120));
            await cleanupListeners();
            setBusy(false);
            if (!done) {
                if (buffer.trim()) {
                    state.messages.push({ role: 'assistant', content: buffer });
                    state.messages = trimLocal(state.messages);
                    await renderAssistantBody(streamingBody, buffer, true);
                    setWrapContent(streamingBody, buffer);
                    await persistMsg('assistant', buffer);
                }
                if (errorMsg) showNotification('✗ ' + errorMsg, 'error');
            }
            state.requestId = null;
            await updateHint();
        }
    }

    async function docQaSend() {
        if (!state.open) return;
        // 生成中点击同一按钮 = 停止（与 Agent 页发送/停止合一语义一致）
        if (state.streaming) {
            docQaStop();
            return;
        }
        const input = $('doc-qa-input');
        if (!input) return;
        const text = input.value.trim();
        if (!text) return;
        input.value = '';
        const rel = $('doc-qa-related');
        if (rel) rel.style.display = 'none';
        state.messages.push({ role: 'user', content: text });
        pushRender('user', text);
        await persistMsg('user', text);
        await updateHint();
        const parsed = await collectExtraFiles(text);
        const merged = Array.isArray(state.scopeFiles) ? state.scopeFiles.slice(0, 3) : [];
        for (const f of parsed.files) {
            if (!merged.includes(f) && merged.length < 3) merged.push(f);
        }
        const bookmarks = await collectBookmarks(text);
        await runDocQuestion(text, merged, parsed.folders, bookmarks);
    }

    async function docQaNew() {
        if (state.streaming) docQaStop();
        state.sessionId = null;
        state.messages = [];
        state.compressNote = '';
        state.scopeFiles = [];
        const sc = $('doc-qa-scope');
        if (sc) sc.style.display = 'none';
        const scSel = $('doc-qa-scope-select');
        if (scSel) scSel.innerHTML = '';
        if (messagesEl()) messagesEl().innerHTML = '';
        const rel = $('doc-qa-related');
        if (rel) rel.style.display = 'none';
        await refreshSessionPick();
        updateHint();
        const input = $('doc-qa-input');
        if (input) input.focus();
    }

    async function refreshFileMeta() {
        const nameEl = $('doc-qa-file-name');
        const metaEl = $('doc-qa-file-meta');
        if (!state.file) {
            if (nameEl) nameEl.textContent = '（未打开文件）';
            if (metaEl) metaEl.textContent = '';
            return;
        }
        if (nameEl) nameEl.textContent = state.file;
        if (metaEl) metaEl.textContent = '读取中…';
        try {
            const res = await invoke('doc_read_meta', { dirPath: state.dirPath, relPath: state.file, budgetTokens: null });
            const meta = res && res.meta;
            state.fileMeta = meta || null;
            if (meta && metaEl) {
                const full = res.full_fits === true ? ' · 可整篇直读' : (res.full_fits === false ? ' · 超长按需引用' : '');
                metaEl.textContent = `${meta.total_lines} 行 / ${meta.total_chars} 字符 / ${meta.sections ? meta.sections.length : 0} 节${full}`;
            } else if (metaEl) {
                metaEl.textContent = '';
            }
        } catch (e) {
            if (metaEl) metaEl.textContent = '（读取元数据失败）';
        }
    }

    async function updateHint() {
        const hint = $('doc-qa-hint');
        if (!hint) return;
        if (state.file) {
            const selInfo = state.selText
                ? (state.selOffsets ? `（选区优先：已携带 ${state.selText.length} 字）` : `（已携带选区 ${state.selText.length} 字，未映射到行，按整篇）`)
                : '';
            hint.textContent = `始终关联当前文件：${state.file}${selInfo}${state.sessionId ? '' : ' · 首次发送将建立本文档会话'}`;
            if (state.scopeFiles && state.scopeFiles.length) hint.textContent += ` · 资料圈 ${state.scopeFiles.length}`;
            if (state.compressNote) hint.textContent += ' · ' + state.compressNote;
        } else {
            hint.textContent = '请先打开一个文件';
        }
    }

    // ── 快捷动作 ──
    const QUICK_PROMPTS = {
        summarize: '请对当前文档做一次结构化总结：先一句话概述，再按文档结构给出要点（引用请标注 [§N] 与行号）。',
        analyze: '请深度分析当前文档：核心观点与论证逻辑、结构与潜在问题、关键结论，最后给出可执行改进建议（引用标注 [§N] 与行号）。',
        reformat: '请评估当前文档的排版结构：标题层级、列表/段落组织、可读性问题，并给出需要改写的主要片段与修改建议（引用标注 [§N] 与行号，不要直接改写全文）。',
        outline: '请基于当前文档生成一份完整大纲：按文档结构整理为多级标题/要点清单（Markdown），保留关键章节与标题，不展开细节。',
        mindmap: '请基于当前文档要点生成一张 Mermaid mindmap 思维导图（只输出 ```mermaid 代码块，节点精炼）。',
        ask: '请基于当前文档回答：',
    };
    const SEL_QUICK_PROMPTS = {
        explain: '请用通俗易懂的语言解释上方选区内容的核心含义，并给出要点。',
        polish: '请对上方选区内容进行润色改写，使表达更清晰、流畅、专业（不改变原意）。',
        translate: '请将上方选区内容翻译为简体中文（若原文已是中文，则翻译成英文），保留 Markdown 结构。',
        todo: '请从上方选区内容中提取待办事项清单（若无待办，明确说明）。',
        summarizeSel: '请对上方选区内容做要点总结。',
    };
    const SEL_QUICK_ITEMS = [['explain', '解释'], ['polish', '润色'], ['translate', '翻译'], ['todo', '提取待办'], ['summarizeSel', '总结选区']];

    function renderSelQuickChips() {
        const box = $('doc-qa-sel-actions');
        if (!box) return;
        box.innerHTML = '';
        const canSel = !!(state.selText && state.selOffsets && state.selOffsets.end > state.selOffsets.start);
        box.style.display = canSel ? '' : 'none';
        if (!canSel) return;
        const tip = document.createElement('span');
        tip.className = 'doc-qa-sel-tip';
        tip.textContent = '选区（优先）：';
        box.appendChild(tip);
        for (const [kind, label] of SEL_QUICK_ITEMS) {
            const b = document.createElement('button');
            b.type = 'button';
            b.className = 'doc-qa-sel-chip';
            b.textContent = label;
            b.onclick = () => docQaSelQuick(kind);
            box.appendChild(b);
        }
    }
    function docQaSelQuick(kind) {
        if (!state.open || state.streaming) return;
        if (!state.selText || !state.selOffsets || state.selOffsets.end <= state.selOffsets.start) {
            showNotification('当前选区未能定位到文档，请重新选择或使用整篇提问', 'warning');
            return;
        }
        const prompt = SEL_QUICK_PROMPTS[kind] || '请基于上方选区内容回答：';
        const input = $('doc-qa-input');
        if (!input) return;
        input.value = prompt;
        docQaSend();
    }

    function docQaQuick(kind) {
        if (!state.open || state.streaming) return;
        const text = QUICK_PROMPTS[kind] || '';
        const input = $('doc-qa-input');
        if (!input) return;
        if (kind === 'ask') {
            input.value = text;
            input.focus();
            return;
        }
        input.value = text;
        docQaSend();
    }

    // ── 初始化绑定 ──
    document.addEventListener('keydown', (e) => {
        const tag = (e.target && e.target.tagName || '').toLowerCase();
        const typing = tag === 'input' || tag === 'textarea' || (e.target && e.target.isContentEditable);
        // 全局唤起（P2-3）：Alt+Q 打开/关闭小助手（输入态不拦截）
        if (e.altKey && !e.ctrlKey && !e.metaKey && (e.key === 'q' || e.key === 'Q')) {
            if (typing && !state.open) return;
            e.preventDefault();
            if (state.open) {
                closeDocQA();
            } else {
                openDocQA();
            }
            return;
        }
        if (!state.open) return;
        if (e.key === 'Escape') {
            closeDocQA();
            return;
        }
        if (e.key === 'Enter' && !e.shiftKey) {
            const t = e.target;
            if (t && t.id === 'doc-qa-input') {
                e.preventDefault();
                docQaSend();
            }
        }
    });
    const pick = $('doc-qa-session-pick');
    if (pick) {
        pick.addEventListener('change', async () => {
            const sid = pick.value;
            if (!state.open) return;
            if (state.streaming) docQaStop();
            if (!sid) {
                docQaNew();
                return;
            }
            try {
                await loadSession(sid);
            } catch (e) {
                showNotification('加载会话失败: ' + (e.message || e), 'error');
            }
        });
    }

    const mbox = messagesEl();
    if (mbox) {
        mbox.addEventListener('click', (e) => {
            const btn = e.target && e.target.closest ? e.target.closest('.doc-qa-msg-btn') : null;
            if (btn && btn.dataset && btn.dataset.action) {
                e.preventDefault();
                applyMsgAction(btn.dataset.action, btn);
                return;
            }
            const t = e.target && e.target.closest ? e.target.closest('.doc-qa-cite') : null;
            if (t && t.dataset && t.dataset.sec) {
                e.preventDefault();
                jumpToSection(t.dataset.sec);
            }
        });
    }
    const depthEl = $('doc-qa-depth');
    if (depthEl) {
        depthEl.value = state.depth || 'auto';
        depthEl.addEventListener('change', () => { state.depth = depthEl.value || 'auto'; });
    }
    const wmEl = $('doc-qa-write-mode');
    if (wmEl) {
        wmEl.value = state.writeMode || 'explore';
        wmEl.addEventListener('change', () => { state.writeMode = wmEl.value || 'explore'; });
    }
    const memEl = $('doc-qa-memory');
    if (memEl) {
        memEl.checked = !!state.remember;
        memEl.addEventListener('change', () => { state.remember = memEl.checked; });
    }
    const bmEl = $('doc-qa-bookmarks');
    if (bmEl) {
        bmEl.checked = !!state.useBookmarks;
        bmEl.addEventListener('change', () => { state.useBookmarks = bmEl.checked; });
    }
    const tplEl = $('doc-qa-template');
    if (tplEl) {
        try {
            const saved = localStorage.getItem(TEMPLATE_KEY);
            if (saved && DOC_TEMPLATES[saved]) state.template = saved;
        } catch (e) { /* ignore */ }
        tplEl.value = state.template || 'default';
        tplEl.addEventListener('change', () => {
            state.template = tplEl.value || 'default';
            try {
                if (state.template === 'default') localStorage.removeItem(TEMPLATE_KEY);
                else localStorage.setItem(TEMPLATE_KEY, state.template);
            } catch (e) { /* ignore */ }
        });
    }
    const scopeSel = $('doc-qa-scope-select');
    if (scopeSel) {
        scopeSel.addEventListener('change', () => applyScopeSelection());
    }

    window.openDocQA = openDocQA;
    window.closeDocQA = closeDocQA;
    window.docQaSend = docQaSend;
    window.docQaStop = docQaStop;
    window.docQaNew = docQaNew;
    window.docQaQuick = docQaQuick;
    window.docQaSelQuick = docQaSelQuick;
    window.docQaExportConversation = docQaExportConversation;
    window.closeDocQaPreview = closeDocQaPreview;
    window.insertPreviewToCursor = insertPreviewToCursor;
    window.previewActionApply = previewActionApply;
    window.docQaRelated = docQaRelated;
    window.toggleDocQaScope = toggleDocQaScope;
    window.clearDocQaScope = clearDocQaScope;
    window.docQaToggleLayout = docQaToggleLayout;
    window.docQaFabClick = docQaFabClick;
    window.docQaFileSwitched = docQaFileSwitched;
    window.docQaCloseIfDockedOpen = docQaCloseIfDockedOpen;
})();
