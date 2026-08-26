/**
 * AI 总结面板数据层（用户行为分析 + 知识库分析）
 *
 * 职责（SOLID：单一职责——本模块只负责「采集数据 → 拼文本 → 组织分类」，
 * 模型推理在 Rust 后端 `kb_ai_summary`，复用 LLMClient 重试链）：
 * 1. 每个采集函数 = 一个分类，返回该分类的纯文本数据；
 * 2. `getSummary()` 把各分类组装为 `{ key, label, instruction, text }[]`，
 *    调用 `kb_ai_summary` 逐类让 LLM 总结（含建议）；
 * 3. 返回结构化结果 `{ behavior: Section[], knowledge: Section[] }`，
 *    供 dashboard 左右手风琴面板渲染。
 *
 * 依赖（main.html / 适配层全局能力，须在 main.html 内联脚本与
 * bookmark/schedule/git 适配层之后加载）：
 * - loadViewData / getScanFileList / getAssetsData / readMarkers /
 *   videoGetHistory / renderDashStats / scanDashboardData / _fmtDuration /
 *   isEmpty / currentRootPath / INDEX_DELETED_FILE / INDEX_HISTORY_FILE
 * - window.__mdgoBookmark / window.GitRustAdapter / ScheduleService
 * - Tauri 命令：kb_ai_summary / stats_ai_usage / ai_history_stats /
 *   chat_session_list / skill_metrics / kb_status / kb_dashboard_stats /
 *   kb_embedding_info / kb_skill / kb_mcp / bookmark_stat / bookmark_list /
 *   schedule_list / git_log
 */
(function () {
    'use strict';

    const DAY_MS = 24 * 60 * 60 * 1000;

    // ─── 工具 ───
    function invoke(name, args) {
        if (!window.__TAURI__ || !window.__TAURI__.core) return Promise.reject(new Error('非 Tauri 环境'));
        return window.__TAURI__.core.invoke(name, args);
    }

    function fmtDuration(ms) {
        if (typeof _fmtDuration === 'function') return _fmtDuration(ms);
        const totalSec = Math.floor((ms || 0) / 1000);
        if (totalSec < 60) return totalSec + ' 秒';
        const min = Math.floor(totalSec / 60);
        if (min < 60) return min + ' 分 ' + (totalSec % 60) + ' 秒';
        const h = Math.floor(min / 60);
        return h + ' 时 ' + (min % 60) + ' 分';
    }

    function fmtDateTime(ts) {
        if (!ts) return '';
        const d = new Date(ts);
        return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')} ${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
    }

    function truncate(s, n = 60) {
        if (!s) return '';
        return s.length > n ? s.slice(0, n) + '…' : s;
    }

    function joinText(parts) {
        return parts.filter(Boolean).join('\n');
    }

    // ─── 分类指令（每类一个，指示模型如何总结/给建议） ───

    const INSTRUCTIONS = {
        operation: '你是一个用户操作行为分析引擎。基于以下操作记录数据，总结：1.高频操作与高频访问文件；2.最近活跃时段；3.用户工作流模式与整理倾向；4.给出1-2条可执行建议。只基于给定数据，禁止臆造。输出 Markdown，先结论后建议。',
        deleted: '你是一个文件生命周期分析引擎。基于以下删除记录数据，总结：1.高频删除类型/目录；2.短生命周期文件现象；3.用户清理行为模式；4.给出1-2条建议（如垃圾箱清理/归档）。只基于给定数据，禁止臆造。输出 Markdown。',
        files: '你是一个知识库文件统计引擎。基于以下文件统计数据（新增/编辑/僵尸/臃肿目录/类型分布），总结：1.知识库新鲜度与活跃度；2.沉睡知识与臃肿目录风险；3.给出1-3条整理建议。只基于给定数据，禁止臆造。输出 Markdown。',
        markers: '你是一个知识价值分析引擎。基于以下文件标记数据，总结：1.高频标记文件与标签；2.核心关注主题；3.给出建议。只基于给定数据，禁止臆造。输出 Markdown。',
        bookmarks: '你是一个书签利用率分析引擎。基于以下书签数据（总数/死链/近7天新增），总结：1.书签使用习惯；2.死链与未利用风险；3.给出建议（如转存笔记/清理）。只基于给定数据，禁止臆造。输出 Markdown。',
        videos: '你是一个视频记录分析引擎。基于以下视频历史数据，总结使用习惯并给出建议。只基于给定数据，禁止臆造。输出 Markdown。',
        git: '你是一个版本管理行为分析引擎。基于以下 git 提交数据，总结提交频率与工作节奏，给出建议。只基于给定数据，禁止臆造。输出 Markdown。',
        calendar: '你是一个日程分析引擎。基于以下日程数据，总结时间安排特征并给出建议。只基于给定数据，禁止臆造。输出 Markdown。',
        chat: '你是一个对话行为分析引擎。基于以下对话会话数据，总结对话活跃度与使用模式，给出建议。只基于给定数据，禁止臆造。输出 Markdown。',
        aiUsage: '你是一个 AI 使用行为分析引擎。基于以下 AI 用量数据（调用次数/token/消息/会话），总结 AI 使用强度与趋势，给出建议。只基于给定数据，禁止臆造。输出 Markdown。',
        skillMetrics: '你是一个 Skill 使用分析引擎。基于以下 skill 执行指标，总结最常用技能与成功率，给出建议。只基于给定数据，禁止臆造。输出 Markdown。',
        kbBase: '你是一个知识库规模分析引擎。基于以下文件/文件夹/占用空间数据，总结知识库规模特征，给出建议。只基于给定数据，禁止臆造。输出 Markdown。',
        kbDist: '你是一个知识库内容分布分析引擎。基于以下类型/大小分布数据，总结知识构成特征，给出建议。只基于给定数据，禁止臆造。输出 Markdown。',
        kbEco: '你是一个知识库生态分析引擎。基于以下 RAG 索引/Skill/MCP/对话/向量模型数据，总结知识库数字化程度，给出建议。只基于给定数据，禁止臆造。输出 Markdown。',
    };

    // ─── 行为分类采集（每个函数 = 一个分类，返回文本） ───

    /** 1. 操作记录：浏览时长 top + 操作次数（localStorage 视图数据） */
    function collectOperation(day = 7) {
        try {
            const viewData = loadViewData();
            const sumList = viewData.sumList || [];
            const viewList = viewData.viewList || [];
            const top = sumList
                .filter(r => (r.durationMs || 0) > 0)
                .sort((a, b) => (b.durationMs || 0) - (a.durationMs || 0))
                .slice(0, 5);
            const totalDuration = sumList.reduce((s, r) => s + (r.durationMs || 0), 0);
            const lines = [];
            lines.push(`浏览文件数: ${sumList.length}，操作记录数: ${viewList.length}，总浏览时长: ${fmtDuration(totalDuration)}`);
            if (top.length) {
                lines.push('浏览时长 Top5:');
                top.forEach((t, i) => lines.push(`${i + 1}. ${t.name || '未知'} - ${fmtDuration(t.durationMs)}${t.path ? ' (路径: ' + t.path + ')' : ''}`));
            } else {
                lines.push('浏览时长 Top5: 暂无数据');
            }
            return lines.join('\n');
        } catch (e) {
            console.warn('[aiSummay] 操作记录采集失败', e);
            return '操作记录数据获取失败';
        }
    }

    /** 2. 删除文件（index_deleted.json，value 含 deletedAt） */
    async function collectDeleted(day = 7) {
        try {
            const res = await getAssetsData(INDEX_DELETED_FILE);
            const map = res && res.data ? res.data : {};
            const threshold = Date.now() - day * DAY_MS;
            const entries = Object.entries(map);
            const recent = entries.filter(([, v]) => v && (v.deletedAt || 0) >= threshold);
            const lines = [];
            lines.push(`近${day}天删除文件数: ${recent.length}，历史累计删除: ${entries.length}`);
            if (recent.length) {
                lines.push('近7天删除文件:');
                recent.slice(0, 10).forEach(([name, v]) => lines.push(`- ${name} (删除时间: ${fmtDateTime(v.deletedAt)})`));
            }
            return lines.join('\n');
        } catch (e) {
            console.warn('[aiSummay] 删除记录采集失败', e);
            return '删除记录数据获取失败';
        }
    }

    /** 3. 文件统计：新增/编辑/僵尸/臃肿目录/类型分布 */
    function collectFiles(day = 7) {
        try {
            const files = getScanFileList();
            if (!files || !files.length) return '文件列表为空';
            const threshold = Date.now() - day * DAY_MS;
            const now = Date.now();
            let added = 0, edited = 0, zombie = 0;
            const dirCount = {};
            const extCount = {};
            for (const f of files) {
                const ctime = (f.ctime || 0) * 1000;
                const mtime = (f.mtime || 0) * 1000;
                if (ctime >= threshold) added++;
                if (mtime >= threshold) edited++;
                if (now - mtime > 180 * DAY_MS) zombie++;
                const sep = f.path && f.path.includes('\\') ? '\\' : '/';
                const dir = f.path ? f.path.substring(0, f.path.lastIndexOf(sep)) : '/';
                dirCount[dir] = (dirCount[dir] || 0) + 1;
                if (f.ext) extCount[f.ext] = (extCount[f.ext] || 0) + 1;
            }
            const bulkyDirs = Object.entries(dirCount)
                .filter(([, c]) => c > 200)
                .sort((a, b) => b[1] - a[1])
                .slice(0, 3)
                .map(([dir, c]) => `${dir}(${c}个)`);
            const extTop = Object.entries(extCount).sort((a, b) => b[1] - a[1]).slice(0, 8);
            const lines = [];
            lines.push(`文件总数: ${files.length}，近${day}天新增: ${added}，编辑: ${edited}，180天未修改: ${zombie}`);
            if (bulkyDirs.length) lines.push(`臃肿目录(>200文件): ${bulkyDirs.join('、')}`);
            if (extTop.length) lines.push(`类型分布 Top8: ${extTop.map(([e, c]) => `${e}:${c}`).join('、')}`);
            return lines.join('\n');
        } catch (e) {
            console.warn('[aiSummay] 文件统计失败', e);
            return '文件统计获取失败';
        }
    }

    /** 4. 文件标记（近 N 天） */
    async function collectMarkers(day = 7) {
        try {
            const data = await readMarkers();
            const list = data ? Object.values(data) : [];
            const threshold = Date.now() - day * DAY_MS;
            const recent = list.filter(m => m && (m.time || 0) >= threshold);
            const lines = [];
            lines.push(`文件标记总数: ${list.length}，近${day}天新增: ${recent.length}`);
            if (recent.length) {
                lines.push('近7天标记: ' + recent.slice(0, 8).map(m => `${m.name}(${m.color})`).join('、'));
            }
            return lines.join('\n');
        } catch (e) {
            console.warn('[aiSummay] 标记采集失败', e);
            return '标记数据获取失败';
        }
    }

    /** 5. 书签（统计 + 近 N 天新增，added_at 毫秒） */
    async function collectBookmarks(day = 7) {
        try {
            const [stat, listRes] = await Promise.all([
                window.__mdgoBookmark.bookmarkStat(currentRootPath).catch(() => null),
                window.__mdgoBookmark.bookmarkList(currentRootPath, { limit: 200 }).catch(() => null),
            ]);
            const threshold = Date.now() - day * DAY_MS;
            let recent = [];
            if (Array.isArray(listRes)) {
                recent = listRes
                    .filter(b => b && b.added_at && b.added_at >= threshold)
                    .slice(0, 10)
                    .map(b => `${b.title || b.url}(${b.url})`);
            }
            const lines = [];
            lines.push(`书签总数: ${(stat && stat.total) || 0}，死链: ${(stat && stat.dead) || 0}，待处理: ${(stat && stat.pending) || 0}，近${day}天新增: ${recent.length}`);
            if (recent.length) lines.push('近7天新增书签: ' + recent.join('、'));
            return lines.join('\n');
        } catch (e) {
            console.warn('[aiSummay] 书签采集失败', e);
            return '书签数据获取失败';
        }
    }

    /** 6. 视频历史（localStorage） */
    function collectVideos(day = 7) {
        try {
            const records = videoGetHistory() || [];
            const threshold = Date.now() - day * DAY_MS;
            const recent = records.filter(v => v && (v.time || 0) >= threshold);
            const lines = [];
            lines.push(`视频记录总数: ${records.length}，近${day}天新增: ${recent.length}`);
            if (recent.length) {
                lines.push('近7天视频: ' + recent.slice(0, 5).map(v => `${v.title || v.url}(${v.type || '未知类型'})`).join('、'));
            }
            return lines.join('\n');
        } catch (e) {
            console.warn('[aiSummay] 视频采集失败', e);
            return '视频记录获取失败';
        }
    }

    /** 7. Git 提交（git_log，author.timestamp 秒） */
    async function collectGit(day = 7) {
        try {
            const commits = await invoke('git_log', { dir: window.gitRepoDir || currentRootPath, depth: 100, filepath: null });
            const list = Array.isArray(commits) ? commits : [];
            const threshold = Date.now() - day * DAY_MS;
            const recent = list.filter(c => {
                const ts = (c && c.commit && c.commit.author && c.commit.author.timestamp) ? c.commit.author.timestamp * 1000 : 0;
                return ts >= threshold;
            });
            return `近${day}天 git 提交数: ${recent.length}，历史累计: ${list.length}`;
        } catch (e) {
            console.warn('[aiSummay] git 采集失败', e);
            return 'git 数据获取失败';
        }
    }

    /** 8. 日程（schedule_list，start 为 YYYY-MM-DDTHH:MM） */
    async function collectCalendar() {
        try {
            const events = await invoke('schedule_list', { dirPath: currentRootPath });
            const list = Array.isArray(events) ? events : [];
            const now = Date.now();
            const in30d = list.filter(e => {
                const t = e && e.start ? new Date(e.start.replace('T', ' ')).getTime() : NaN;
                return t >= now && t <= now + 30 * DAY_MS;
            });
            const lines = [];
            lines.push(`日程总数: ${list.length}，未来30天: ${in30d.length}`);
            if (in30d.length) {
                lines.push('未来日程: ' + in30d.slice(0, 5).map(e => `${e.title}(${e.start})`).join('、'));
            }
            return lines.join('\n');
        } catch (e) {
            console.warn('[aiSummay] 日程采集失败', e);
            return '日程数据获取失败';
        }
    }

    /** 9. 对话会话（chat_session_list，updated_at 毫秒） */
    async function collectChat(day = 7) {
        try {
            const sessions = await invoke('chat_session_list', { dirPath: currentRootPath });
            const list = Array.isArray(sessions) ? sessions : [];
            const threshold = Date.now() - day * DAY_MS;
            const recent = list.filter(s => s && (s.updated_at || 0) >= threshold);
            const messages = recent.reduce((s, x) => s + (x.message_count || 0), 0);
            return `对话会话总数: ${list.length}，近${day}天活跃: ${recent.length}，近${day}天消息: ${messages}`;
        } catch (e) {
            console.warn('[aiSummay] 对话采集失败', e);
            return '对话数据获取失败';
        }
    }

    /** 10. AI 用量（stats_ai_usage，后端 30s 缓存） */
    async function collectAiUsage(day = 7) {
        try {
            const stats = await invoke('stats_ai_usage', { dirPath: currentRootPath, days: day });
            if (!stats) return 'AI 用量数据为空';
            const s = stats.summary || {};
            const lines = [];
            lines.push(`近${day}天 AI 调用: ${s.ai_calls || 0} 次，Token: ${s.tokens || 0}，对话消息: ${s.messages || 0}，会话: ${s.sessions || 0}`);
            const daily = stats.daily || [];
            if (daily.length) {
                const last5 = daily.slice(-5);
                lines.push('最近5天 AI 调用: ' + last5.map(d => `${d.date}: ${d.ai_calls}次`).join('、'));
            }
            return lines.join('\n');
        } catch (e) {
            console.warn('[aiSummay] AI 用量采集失败', e);
            return 'AI 用量数据获取失败';
        }
    }

    /** 11. Skill 执行指标（skill_metrics，since 毫秒） */
    async function collectSkillMetrics(day = 7) {
        try {
            const since = Date.now() - day * DAY_MS;
            const metrics = await invoke('skill_metrics', { dirPath: currentRootPath, skillId: null, since });
            if (!metrics) return 'Skill 指标数据为空';
            const lines = [];
            lines.push(`近${day}天 Skill 执行: ${metrics.total_executions || 0} 次，成功: ${metrics.total_successes || 0}，失败: ${metrics.total_failures || 0}，成功率: ${Math.round((metrics.global_success_rate || 0) * 100)}%`);
            const skills = (metrics.skills || []).slice(0, 5);
            if (skills.length) {
                lines.push('Top Skills: ' + skills.map(s => `${s.skill_id}(${s.total_calls}次)`).join('、'));
            }
            return lines.join('\n');
        } catch (e) {
            console.warn('[aiSummay] skill 指标采集失败', e);
            return 'Skill 指标获取失败';
        }
    }

    // ─── 知识库分类采集 ───

    /** 12. 知识库基础（scanDashboardData） */
    async function collectKbBase() {
        try {
            const data = await scanDashboardData(true);
            const st = (data && data.stats) || {};
            return `文件数量: ${st.total_files || 0}，文件夹数量: ${st.total_folders || 0}，总占用空间: ${st.total_size_str || '--'}`;
        } catch (e) {
            console.warn('[aiSummay] 知识库基础统计失败', e);
            return '知识库基础统计获取失败';
        }
    }

    /** 13. 分布统计（renderDashStats） */
    async function collectKbDist() {
        try {
            const d = await renderDashStats();
            if (!d) return '分布数据为空';
            const lines = [];
            const extTop = Object.entries(d.docExtMap || {}).sort((a, b) => b[1] - a[1]).slice(0, 10);
            if (extTop.length) lines.push(`文档类型分布 Top10: ${extTop.map(([e, c]) => `${e}:${c}`).join('、')}`);
            const sizeRange = Object.entries(d.fileSizeRangeMap || {});
            if (sizeRange.length) lines.push(`大小分布: ${sizeRange.map(([r, c]) => `${r}:${c}`).join('、')}`);
            lines.push(`今日编辑: ${d.todayEdit || 0}，删除: ${d.deletedCount || 0}`);
            return lines.join('\n');
        } catch (e) {
            console.warn('[aiSummay] 分布统计失败', e);
            return '分布统计获取失败';
        }
    }

    /** 14. RAG + 生态（kb_status / kb_dashboard_stats / kb_embedding_info / kb_chat_stats / kb_skill / kb_mcp） */
    async function collectKbEco() {
        try {
            const [kbStatus, dash, embInfo, chatStats, skill, mcp] = await Promise.all([
                invoke('kb_status', { dirPath: currentRootPath }).catch(() => null),
                invoke('kb_dashboard_stats', { dirPath: currentRootPath }).catch(() => null),
                invoke('kb_embedding_info').catch(() => null),
                invoke('kb_chat_stats', { dirPath: currentRootPath }).catch(() => null),
                invoke('kb_skill', { dirPath: currentRootPath }).catch(() => null),
                invoke('kb_mcp', { dirPath: currentRootPath }).catch(() => null),
            ]);
            const lines = [];
            if (kbStatus) {
                lines.push(`RAG 索引: 文件 ${kbStatus.fileCount || 0}，chunk ${kbStatus.chunkCount || 0}，向量 ${kbStatus.vectorCount || 0}，状态 ${kbStatus.status || 'unknown'}`);
            }
            if (dash) lines.push(`索引占用空间: ${dash.storage_size || '--'}`);
            if (embInfo) lines.push(`向量模型: ${embInfo.model_name || '未知'}(${embInfo.dimension || 0}维, ${embInfo.status || '未知'})`);
            if (chatStats) lines.push(`对话: 会话 ${chatStats.sessionCount || 0}，消息 ${chatStats.messageCount || 0}`);
            if (skill) {
                lines.push(`Skill: 共 ${skill.total || 0}(系统 ${skill.system_count || 0}/全局 ${skill.global_count || 0}/项目 ${skill.project_count || 0}，启用 ${skill.enabled_count || 0})`);
                if (skill.skills && skill.skills.length) {
                    lines.push('Skill 列表: ' + skill.skills.slice(0, 10).map(s => `${s.name}(${s.id})`).join('、'));
                }
            }
            if (mcp) {
                lines.push(`MCP: 共 ${mcp.total || 0}(连接 ${mcp.connected_count || 0}/断开 ${mcp.disconnected_count || 0}/失败 ${mcp.failed_count || 0})`);
                if (mcp.servers && mcp.servers.length) {
                    lines.push('MCP 列表: ' + mcp.servers.map(s => `${s.name}(${s.server_type})`).join('、'));
                }
            }
            return lines.join('\n') || 'RAG 生态数据为空';
        } catch (e) {
            console.warn('[aiSummay] RAG 生态采集失败', e);
            return 'RAG 生态数据获取失败';
        }
    }

    // ─── 汇总入口 ───

    /**
     * 采集全部数据 → 组装分类 → 调 Rust 后端逐类 LLM 总结
     * @param {number} day 行为统计窗口（天），默认 7
     * @returns {Promise<{behavior: Array, knowledge: Array}>}
     *  每项: { key, label, ok, result }
     */
    async function getSummary(day = 7) {
        // 并行采集（互不依赖，单项失败降级为错误文本）
        const [deleted, markers, bookmarks, videos, git, calendar, chat, aiUsage, skillMetrics, kbBase, kbDist, kbEco] =
            await Promise.all([
                collectDeleted(day),
                collectMarkers(day),
                collectBookmarks(day),
                collectVideos(day),
                collectGit(day),
                collectCalendar(),
                collectChat(day),
                collectAiUsage(day),
                collectSkillMetrics(day),
                collectKbBase(),
                collectKbDist(),
                collectKbEco(),
            ]);
        const operation = collectOperation(day);
        const files = collectFiles(day);

        // 行为分类（左卡片）
        const behaviorSections = [
            { key: 'operation', label: '操作记录', instruction: INSTRUCTIONS.operation, text: operation },
            { key: 'deleted', label: '删除记录', instruction: INSTRUCTIONS.deleted, text: deleted },
            { key: 'files', label: '文件增改', instruction: INSTRUCTIONS.files, text: files },
            { key: 'markers', label: '文件标记', instruction: INSTRUCTIONS.markers, text: markers },
            { key: 'bookmarks', label: '书签', instruction: INSTRUCTIONS.bookmarks, text: bookmarks },
            { key: 'videos', label: '视频', instruction: INSTRUCTIONS.videos, text: videos },
            { key: 'git', label: 'Git', instruction: INSTRUCTIONS.git, text: git },
            { key: 'calendar', label: '日程', instruction: INSTRUCTIONS.calendar, text: calendar },
            { key: 'chat', label: '对话', instruction: INSTRUCTIONS.chat, text: chat },
            { key: 'aiUsage', label: 'AI 用量', instruction: INSTRUCTIONS.aiUsage, text: aiUsage },
            { key: 'skillMetrics', label: 'Skill 使用', instruction: INSTRUCTIONS.skillMetrics, text: skillMetrics },
        ];

        // 知识库分类（右卡片）
        const knowledgeSections = [
            { key: 'kbBase', label: '文件与磁盘', instruction: INSTRUCTIONS.kbBase, text: kbBase },
            { key: 'kbDist', label: '内容分布', instruction: INSTRUCTIONS.kbDist, text: kbDist },
            { key: 'kbEco', label: 'RAG 与生态', instruction: INSTRUCTIONS.kbEco, text: kbEco },
        ];

        // 逐类调用 Rust 后端 LLM 总结（后端串行、单类超时/失败降级）
        // 失败时保留错误信息（不静默吞掉），便于面板展示与排查
        const [behavior, knowledge] = await Promise.all([
            invoke('kb_ai_summary', { sections: behaviorSections })
                .catch(e => { console.warn('[aiSummay] 行为总结调用失败:', e); return null; }),
            invoke('kb_ai_summary', { sections: knowledgeSections })
                .catch(e => { console.warn('[aiSummay] 知识库总结调用失败:', e); return null; }),
        ]);

        const fill = (sections, results) => sections.map(s => {
            const r = (results || []).find(x => x.key === s.key);
            return {
                key: s.key,
                label: s.label,
                ok: r ? r.ok : false,
                result: r ? r.result : '总结生成失败（后端调用未返回结果）',
                text: s.text,
            };
        });

        return {
            behavior: fill(behaviorSections, behavior),
            knowledge: fill(knowledgeSections, knowledge),
        };
    }

    window.aiSummay = { getSummary, fmtDuration };
})();
