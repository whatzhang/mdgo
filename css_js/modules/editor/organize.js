/**
 * ===== 智能整理模块（css_js/modules/editor/organize.js） =====
 *
 * 【职责】P2-5 智能整理命令（footer「整理」下拉菜单）：
 *   1. 生成目录（TOC）：解析当前文档标题层级 → 插入文档顶部（frontmatter 后）
 *   2. 标签建议：现有 frontmatter tags + 高频词 → 建议并写入 frontmatter
 *   3. 重复笔记检测：_scanFileList 中 basename 相同/相似（编辑距离≤2）→ 弹窗
 *   4. 周报/月报：聚合本周/本月日期命名日记 → AI 生成报告（弹窗，可插入新文档）
 *   5. 归档已完成任务：扫描 - [x] → 汇总区块追加到文档末尾
 * 【入口】main.html enterEditMode 调用 initOrganizeButton()
 * 【依赖】运行时主脚本全局：MdgoDocument / currentRootPath / _scanFileList /
 *         showNotification / callAIAPI / openFileFromPath；Tauri invoke read_file
 * 【对外暴露】window.initOrganizeButton / window.MdgoOrganize
 */
(function () {
    'use strict';

    // ===== 工具 =====
    function getDoc() {
        return (window.MdgoDocument && window.MdgoDocument.getValue) ? window.MdgoDocument.getValue() : '';
    }

    function notify(msg, type) {
        if (typeof window.showNotification === 'function') window.showNotification(msg, type, 2500);
        else console.log('[mdgo]', msg);
    }

    function levenshtein(a, b) {
        const m = a.length, n = b.length;
        if (m === 0) return n;
        if (n === 0) return m;
        let prev = new Array(n + 1), cur = new Array(n + 1);
        for (let j = 0; j <= n; j++) prev[j] = j;
        for (let i = 1; i <= m; i++) {
            cur[0] = i;
            for (let j = 1; j <= n; j++) {
                const cost = a[i - 1] === b[j - 1] ? 0 : 1;
                cur[j] = Math.min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + cost);
            }
            [prev, cur] = [cur, prev];
        }
        return prev[n];
    }

    function showListPanel(title, items, onPick) {
        const overlay = document.createElement('div');
        overlay.className = 'mdgo-semantic-overlay';
        overlay.innerHTML = `
            <div class="mdgo-semantic-panel" style="align-items:flex-start;justify-content:flex-start;border-radius:0 10px 10px 0;">
                <div class="mdgo-semantic-head">
                    <span>${title}（${items.length}）</span>
                    <button type="button" class="mdgo-semantic-close">✕</button>
                </div>
                <div class="mdgo-semantic-body">
                    ${items.length === 0 ? '<div class="mdgo-semantic-empty">无结果</div>' :
                        // 修复：label 转义，防文件名含 <>& 造成 XSS
                        items.map((it, i) => `<div class="mdgo-semantic-item" data-i="${i}">${escapeHtml2(String(it.label))}</div>`).join('')}
                </div>
            </div>`;
        document.body.appendChild(overlay);
        const close = () => overlay.remove();
        overlay.querySelector('.mdgo-semantic-close').addEventListener('click', close);
        overlay.addEventListener('click', (e) => { if (e.target === overlay) close(); });
        overlay.querySelectorAll('.mdgo-semantic-item').forEach(el => {
            el.addEventListener('click', () => {
                const item = items[Number(el.dataset.i)];
                close();
                if (item && onPick) onPick(item);
            });
        });
    }

    function stripFence(text) {
        // 移除代码块与行内代码，避免标题/标签误提取
        return String(text).replace(/```[\s\S]*?```/g, '').replace(/`[^`]*`/g, '');
    }

    // ===== 1. 生成目录 =====
    function generateToc() {
        const text = getDoc();
        if (!text.trim()) { notify('文档为空', 'warning'); return; }
        const lines = stripFence(text).split('\n');
        const toc = [];
        let inFm = false, fmEnd = -1;
        for (let i = 0; i < lines.length; i++) {
            const l = lines[i];
            if (i === 0 && l.trim() === '---') { inFm = true; continue; }
            if (inFm && l.trim() === '---') { inFm = false; fmEnd = i; continue; }
            if (inFm) continue;
            const m = l.match(/^(#{1,6})\s+(.+)$/);
            if (m) {
                const level = m[1].length;
                const title = m[2].trim().replace(/[`*_]/g, '');
                toc.push({ level, title });
            }
        }
        if (toc.length === 0) { notify('未发现标题，无法生成目录', 'warning'); return; }
        // 修复：重复执行不插入第二份目录
        if (new RegExp('## 📑 目录').test(text)) { notify('目录已存在，请删除后再生成', 'warning'); return; }
        // 目录 Markdown（# 前缀与文档标题层级错开：H1 → 目录内一级）
        const mdToc = toc.map(t => `${'  '.repeat(t.level - 1)}- ${t.title}`).join('\n');
        const d = window.MdgoDocument;
        // 插入位置：frontmatter 之后（fmEnd 行后）或文档开头。
        // 修复：无 frontmatter 时 (1,1) 插入不带前导 \n（避免文档首行空行）；
        // 有 frontmatter 时在 --- 后的空行前插
        let insert;
        let line;
        if (fmEnd >= 0) {
            insert = `## 📑 目录\n\n${mdToc}\n\n`;
            line = fmEnd + 2;
        } else {
            insert = `## 📑 目录\n\n${mdToc}\n`;
            line = 1;
        }
        const pos = { lineNumber: line, column: 1 };
        const ok = d.insertAt(pos, insert);
        notify(ok ? '✓ 目录已生成（Ctrl+Z 可撤销）' : '目录插入失败', ok ? 'success' : 'error');
        return insert;
    }

    // ===== 2. 标签建议 =====
    function suggestTags() {
        const text = getDoc();
        const existing = new Set();
        const fm = text.match(/^---\n([\s\S]*?)\n---/);
        if (fm) {
            const b = fm[1].match(/tags:\s*\[([^\]]+)\]/);
            if (b) b[1].split(',').map(t => t.trim()).filter(Boolean).forEach(t => existing.add(t));
            const h = fm[1].match(/tags:\s*(#\S+(?:\s*,\s*#\S+)*)/);
            if (h) h[1].split(',').map(t => t.trim().replace(/^#/, '')).filter(Boolean).forEach(t => existing.add(t));
        }
        const body = stripFence(text);
        const re = /(^|\s)#([\w\u4e00-\u9fa5\-_\/]+)/g;
        let m;
        const inline = [];
        while ((m = re.exec(body))) {
            const lineStart = body.lastIndexOf('\n', m.index);
            const lineEnd = body.indexOf('\n', m.index);
            const line = body.substring(lineStart + 1, lineEnd < 0 ? body.length : lineEnd);
            if (/^#{1,6}\s/.test(line.trim())) continue;
            inline.push(m[2]);
        }
        // 高频词建议（2 字以上中文词/英文词，排除停用词）
        const stop = new Set(['的', '了', '是', '在', '和', '与', '及', '等', '一个', '我们', '你们', '他们', '这个', '那个', '进行', '通过', 'the', 'and', 'for', 'with', 'that', 'this']);
        const words = (body.match(/[\u4e00-\u9fa5]{2,4}|[A-Za-z]{4,}/g) || [])
            .filter(w => !stop.has(w.toLowerCase()));
        const freq = {};
        words.forEach(w => { const k = w.toLowerCase(); freq[k] = (freq[k] || 0) + 1; });
        const top = Object.entries(freq).sort((a, b) => b[1] - a[1]).slice(0, 8).map(e => e[0]);
        const candidates = [...existing, ...inline, ...top];
        const unique = [...new Set(candidates)].slice(0, 20);
        showListPanel('🏷 标签建议（点击写入 frontmatter）', unique.map(t => ({ label: '#' + t, value: t })), (item) => {
            applyTags([...existing, ...inline, item.value]);
        });
    }

    function applyTags(tags) {
        const list = [...new Set(tags)].filter(Boolean);
        const text = getDoc();
        const d = window.MdgoDocument;
        const tagsLine = `tags: [${list.join(', ')}]\n`;
        // 修复：容忍 BOM 与前导空行的 frontmatter（原 /^---\n/ 不识别会创建重复 frontmatter）
        const fmHead = /^\uFEFF?---\s*\n/.test(text) || /^\s*\n---\s*\n/.test(text);
        if (fmHead) {
            // 定位真正的 frontmatter 起始行（跳过 BOM/前导空行）
            const lines = text.split('\n');
            let fmStart = -1;
            for (let i = 0; i < lines.length; i++) {
                if (lines[i].trim() === '---') { fmStart = i; break; }
            }
            if (fmStart >= 0) {
                let idx = -1;
                for (let i = fmStart + 1; i < lines.length; i++) {
                    if (lines[i].trim() === '---') break;
                    if (/^tags:\s*/.test(lines[i])) { idx = i; break; }
                }
                if (idx >= 0) {
                    const range = { startLineNumber: idx + 1, startColumn: 1, endLineNumber: idx + 1, endColumn: lines[idx].length + 1 };
                    const ok = d.replace(range, tagsLine.trimEnd());
                    notify(ok ? '✓ 标签已更新' : '标签更新失败', ok ? 'success' : 'error');
                } else {
                    const pos = { lineNumber: fmStart + 2, column: 1 };
                    const ok = d.insertAt(pos, tagsLine);
                    notify(ok ? '✓ 标签已写入 frontmatter' : '标签写入失败', ok ? 'success' : 'error');
                }
            } else {
                notify('frontmatter 解析失败', 'warning');
            }
        } else {
            const fmBlock = '---\n' + tagsLine + '---\n\n';
            const pos = { lineNumber: 1, column: 1 };
            const ok = d.insertAt(pos, fmBlock);
            notify(ok ? '✓ 已创建 frontmatter 并写入标签' : '标签写入失败', ok ? 'success' : 'error');
        }
    }

    // ===== 3. 重复笔记检测 =====
    function dupScan() {
        const list = _scanFileList;
        if (!Array.isArray(list)) { notify('文件列表未就绪', 'warning'); return; }
        const base = new Map(); // basename(去扩展名) → paths
        for (const f of list) {
            if (!f || !f.name) continue;
            const stem = f.name.replace(/\.[^.]+$/, '').toLowerCase();
            if (!base.has(stem)) base.set(stem, []);
            base.get(stem).push(f.path || f.name);
        }
        const dupes = [];
        for (const [stem, paths] of base) {
            if (paths.length > 1) dupes.push({ label: `📄 ${stem}（${paths.length} 个：${paths.join('、')}）`, paths });
        }
        // 相似名（编辑距离 ≤2 且非完全相同 basename）
        const stems = [...base.keys()];
        for (let i = 0; i < stems.length; i++) {
            for (let j = i + 1; j < stems.length; j++) {
                if (levenshtein(stems[i], stems[j]) <= 2 && stems[i] !== stems[j]) {
                    dupes.push({ label: `🔀 ${stems[i]} ≈ ${stems[j]}（${base.get(stems[i]).join('、')} / ${base.get(stems[j]).join('、')}）`, paths: [...base.get(stems[i]), ...base.get(stems[j])] });
                }
            }
        }
        const unique = dupes.slice(0, 30);
        showListPanel('🔍 重复/相似笔记检测', unique, (item) => {
            if (item.paths && item.paths[0] && typeof window.openFileFromPath === 'function') {
                window.openFileFromPath(item.paths[0]);
            }
        });
    }

    // ===== 4. 周报/月报 =====
    async function generateReport(period) {
        const list = _scanFileList;
        if (!Array.isArray(list)) { notify('文件列表未就绪', 'warning'); return; }
        const now = new Date();
        const pad = (n) => String(n).padStart(2, '0');
        let start, end;
        if (period === 'week') {
            const day = (now.getDay() + 6) % 7; // 周一为一周开始
            start = new Date(now.getFullYear(), now.getMonth(), now.getDate() - day);
            end = new Date(start.getFullYear(), start.getMonth(), start.getDate() + 6);
        } else {
            start = new Date(now.getFullYear(), now.getMonth(), 1);
            end = new Date(now.getFullYear(), now.getMonth() + 1, 0);
        }
        const y = (d) => d.getFullYear();
        const m = (d) => pad(d.getMonth() + 1);
        const dd = (d) => pad(d.getDate());
        const dateRe = /^(\d{4})-(\d{2})-(\d{2})\.md$/i;
        const notes = [];
        for (const f of list) {
            const name = f && f.name ? f.name : '';
            const mch = name.match(dateRe);
            if (!mch) continue;
            const t = new Date(Number(mch[1]), Number(mch[2]) - 1, Number(mch[3]));
            if (t >= start && t <= end) {
                notes.push({ path: f.path || name, name });
            }
        }
        if (notes.length === 0) {
            notify(period === 'week' ? '本周无日期命名笔记（如 2026-08-24.md）' : '本月无日期命名笔记', 'warning');
            return;
        }
        notify('正在聚合 ' + notes.length + ' 篇日记生成报告...', 'info');
        const invoke = window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke;
        let contents = '';
        for (const n of notes.slice(0, 15)) {
            try {
                if (invoke) {
                    // 修复：_scanFileList.path 是相对知识库根路径，read_file 按进程 cwd 解析
                    // 找不到，需拼接 currentRootPath 得到绝对路径
                    const abs = (currentRootPath ? currentRootPath.replace(/[\\/]+$/, '') + '/' : '') + n.path;
                    contents += `\n\n## ${n.name}\n${await invoke('read_file', { path: abs })}`;
                } else {
                    contents += `\n\n## ${n.name}\n（非 Tauri 环境无法读取）`;
                }
            } catch (e) { /* 跳过读取失败 */ }
        }
        const periodLabel = period === 'week' ? `本周（${y(start)}-${m(start)}-${dd(start)} ~ ${y(end)}-${m(end)}-${dd(end)}）` : `本月（${y(start)}-${m(start)}）`;
        const prompt = `请根据以下日记内容，生成一份${periodLabel}工作周报：1) 完成事项 2) 进行中 3) 遇到的问题 4) 下周计划。输出 Markdown 结构。\n\n日记内容：\n${contents.slice(0, 15000)}`;
        try {
            const result = await window.callAIAPI(prompt, '');
            showListPanel(`📅 ${periodLabel}报告`, [{ label: '预览并复制结果', value: result }], (item) => {
                if (typeof window.copyToClipboard === 'function') window.copyToClipboard(item.value);
                notify('✓ 报告已复制到剪贴板', 'success');
            });
        } catch (e) {
            notify('报告生成失败: ' + (e && e.message ? e.message : e), 'error');
        }
    }

    // ===== 5. 归档已完成任务 =====
    function archiveDone() {
        const text = stripFence(getDoc());
        const lines = text.split('\n');
        const done = [];
        const todo = [];
        for (const l of lines) {
            const m = l.match(/^\s*[-*]\s*\[([ xX])\]\s*(.+)$/);
            if (m) {
                const item = m[2].trim();
                if (/[xX]/.test(m[1])) done.push(item);
                else todo.push(item);
            }
        }
        if (done.length === 0) { notify('未发现已完成任务（- [x]）', 'warning'); return; }
        const block = `\n## ✅ 已完成归档（${new Date().toISOString().slice(0, 10)}）\n\n${done.map(t => `- [x] ~~${t}~~`).join('\n')}\n`;
        const d = window.MdgoDocument;
        const total = text.split('\n').length;
        const ok = d.insertAt({ lineNumber: total + 1, column: 1 }, block);
        notify(ok ? `✓ 已归档 ${done.length} 个完成任务（${todo.length} 个未完成保留）` : '归档失败', ok ? 'success' : 'error');
    }

    // ===== 入口：footer「整理」菜单 =====
    window.initOrganizeButton = function () {
        const footer = document.getElementById('editor-footer');
        if (!footer || footer.querySelector('.mdgo-organize-toggle')) return;
        const btn = document.createElement('button');
        btn.type = 'button';
        btn.className = 'mdgo-export-btn mdgo-organize-toggle';
        btn.textContent = '整理';
        btn.title = '智能整理（目录/标签/去重/周报/归档）';
        const menu = document.createElement('div');
        menu.className = 'mdgo-organize-menu';
        menu.style.display = 'none';
        const items = [
            { label: '📑 生成目录', fn: () => generateToc() },
            { label: '🏷 标签建议', fn: () => suggestTags() },
            { label: '🔍 重复笔记检测', fn: () => dupScan() },
            { label: '📅 本周周报', fn: () => generateReport('week') },
            { label: '📅 本月月报', fn: () => generateReport('month') },
            { label: '✅ 归档已完成任务', fn: () => archiveDone() }
        ];
        menu.innerHTML = items.map(it => `<div class="mdgo-organize-item">${it.label}</div>`).join('');
        menu.querySelectorAll('.mdgo-organize-item').forEach((el, i) => {
            el.addEventListener('click', () => { menu.style.display = 'none'; items[i].fn(); });
        });
        btn.addEventListener('click', (e) => {
            e.stopPropagation();
            menu.style.display = menu.style.display === 'none' ? 'block' : 'none';
        });
        // 修复：document 级"点击关菜单"监听器只注册一次（模块级标志），
        // 否则反复进出编辑模式（footer 重建）会累积匿名监听器泄漏
        if (!docClickBound) {
            docClickBound = true;
            document.addEventListener('click', closeOrganizeMenus, true);
        }
        menu.addEventListener('click', (e) => e.stopPropagation());
        const right = footer.querySelector('.status-right');
        if (right) { right.appendChild(btn); right.appendChild(menu); }
    };

    // 关闭所有整理菜单（document 捕获阶段）
    let docClickBound = false;
    function closeOrganizeMenus() {
        document.querySelectorAll('.mdgo-organize-menu').forEach(m => { m.style.display = 'none'; });
    }

    window.MdgoOrganize = {
        generateToc, suggestTags, dupScan, generateReport, archiveDone
    };
})();
