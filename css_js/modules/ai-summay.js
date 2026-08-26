(function () {
    'use strict';
    function getOperationRecordsInfo(day = 7) {
        const now = Date.now();
        const threshold = now - day * 24 * 60 * 60 * 1000;
        let rtn = '';
        try {
            const viewData = loadViewData();
            const records = viewData.sumList || [];
            const opRecords = viewData.viewList || [];
            //TODO
        } catch (e) {
            console.error('获取操作记录失败', e);
        }
        return rtn;
    }
    async function getDeleteFilesInfo(day = 7) {
        const now = Date.now();
        const threshold = now - day * 24 * 60 * 60 * 1000;
        let rtn = '';
        try {
            const data = await getAssetsData(INDEX_DELETED_FILE);
            let deletedIndex = data || [];
            if (!isEmpty(deletedIndex)) {
                //TODO

            }
        } catch (e) {
            console.error('获取删除文件记录失败', e);
        }
        return rtn;
    }
    async function getAddAndEditFilesInfo(day = 7) {
        const now = Date.now();
        const threshold = now - day * 24 * 60 * 60 * 1000;
        let rtn = '';
        try {
            const fileList = await getScanFileList();
            if (!isEmpty(fileList)) {
                //TODO

            }
        } catch (e) {
            console.error('获取新增文件记录失败', e);
        }
        return rtn;
    }
    async function getBookmarksInfo(day = 7) {
        let rtn = '';
        try {
            //获取最近7天新增书签记录
            const info = await window.__mdgoBookmark.getAddedAndEditedBookmarks(day);
            const addedBookmarks = info?.addedBookmarks || [];
            const editedBookmarks = info?.editedBookmarks || [];
            if (!isEmpty(addedBookmarks)) {
                rtn += `最近${day}天新增书签记录: ${addedBookmarks.map(item => `${item.title}(${item.url})`).join(', ')}\n`;
            } else {
                rtn += `最近${day}天新增书签记录: 无\n`;
            }
            if (!isEmpty(editedBookmarks)) {
                rtn += `最近${day}天修改书签记录: ${editedBookmarks.map(item => `${item.title}(${item.url})`).join(', ')}\n`;
            } else {
                rtn += `最近${day}天修改书签记录: 无\n`;
            }
        } catch (e) {
            console.error('获取书签数据失败', e);
        }
        return rtn;
    }

    async function getMarkersInfo(day = 7) {
        const now = Date.now();
        const threshold = now - day * 24 * 60 * 60 * 1000;
        let rtn = '';
        try {
            const data = await readMarkers();
            const markersMap = data ? new Map(Object.entries(data)) : new Map();
            let list = [...markersMap.values()];
            if (!isEmpty(list)) {
                list = list.filter(item => item.time >= threshold);
                rtn += `最近${day}天新增文件标记记录: ${list.filter(item => item.type === 'file').map(item => `${item.name}(${item.color})`).join(', ')}\n`;
                rtn += `最近${day}天新增文件夹标记记录: ${list.filter(item => item.type === 'dir').map(item => `${item.name}(${item.color})`).join(', ')}\n`;
            } else {
                rtn += `最近${day}天新增文件标记记录: 无\n`;
                rtn += `最近${day}天新增文件夹标记记录: 无\n`;
            }
        } catch (e) {
            console.error('获取文件标记数据失败', e);
        }
        return rtn;
    }
    async function getVideoRecordsInfo(day = 7) {
        let rtn = '';
        try {
            let videoRecords = await videoGetHistory();
            if (!isEmpty(videoRecords)) {
                videoRecords = videoRecords.filter(item => item.time >= threshold);
                rtn += `最近${day}天新增视屏记录: ${videoRecords.map(item => `${item.title}(url:${item.url},type:${item.type},path:${item.path})}`).join(', ')}\n`;
            } else {
                rtn += `最近${day}天新增视屏记录: 无\n`;
            }
        } catch (e) {
            console.error('获取视屏数据失败', e);
        }
        return rtn;
    }
    async function getGitRecordsInfo(day = 7) {
        let rtn = '';
        try {
            //git提交记录和每次提交的文件路径列表
            let gitRecords = await window.GitRustAdapter.getGitRecords(day);
            //TODO

        } catch (e) {
            console.error('获取git数据失败', e);
        }
        return rtn;
    }
    async function getCalendarAndReminderInfo(day = 7) {
        let rtn = '';
        try {
            //获取过去7天
            let calendarRecords = await ScheduleService.getCalendarRecords(day);
            //获取未来一个月的日程与提醒
            let futureCalendarRecords = await ScheduleService.getFutureOneMonthCalendarRecords();
            //TODO

        } catch (e) {
            console.error('获取日程数据失败', e);
        }
        return rtn;
    }

    async function getConversationRecordsInfo(day = 7) {
        let rtn = '';
        try {
            //获取最近7天对话记录, 查询对话及每个对话的用户消息记录发送内容，时间
            const sessions = await window.__TAURI__.core.invoke('query_chat_session_list', { dirPath: currentRootPath, day: day });
            //TODO

        } catch (e) {
            console.error('获取对话记录失败', e);
        }
        return rtn;
    }

    async function getLLMRecordsInfo(day = 7) {
        let rtn = '';
        try {
            //获取最近7天llm调用记录记录,包含llm名称,llm调用次数,成功率,token数量
            let records = await window.__TAURI__.core.invoke('query_llm_records', { dirPath: currentRootPath, day: day });
            //TODO

        } catch (e) {
            console.error('获取llm记录失败', e);
        }
        return rtn;
    }

    async function getSkillRecordsInfo(day = 7) {
        let rtn = '';
        try {
            //获取最近7天skill调用记录记录,包含skill名称,skill调用次数,成功率
            let records = await window.__TAURI__.core.invoke('query_skill_metrics', { dirPath: currentRootPath, day: day });
            //TODO

        } catch (e) {
            console.error('获取skill记录失败', e);
        }
        return rtn;
    }


    function getSummary(day = 7) {
        const rtn = {};
        //行为习惯分析
        //操作记录,文件浏览时长top20,最近七天使用时段活跃度
        getOperationRecordsInfo(day);

        //最近7天删除文件记录
        getDeleteFilesInfo(day);

        //最近7天新增、修改文件记录，区分mermaid(图表)、canvas（画布）、excalidraw（白板）、opml（大纲笔记）、mm（思维导图）文件
        getAddAndEditFilesInfo(day);

        //最近7天新增书签记录,包含书签名称,书签URL,书签创建时间
        getBookmarksInfo(day);

        //文件标记
        getMarkersInfo(day);

        //视屏记录,包含视屏名称,视屏URL,视屏创建时间
        getVideoRecordsInfo(day);

        //最近7天git记录,包含git操作类型,git操作时间,git操作文件路径
        getGitRecordsInfo(day);

        //过去7天与未来一个月日程与提醒
        getCalendarAndReminderInfo(day);



        
        //ai行为分析
        //最近7天对话记录，对话发送内容，时间，对话数量
        getConversationRecordsInfo(day);

        //llm使用记录，包含llm名称,llm调用次数,成功率,token数量
        getLLMRecordsInfo(day);

        //skill使用记录，包含skill名称,skill使用数量,skill成功率
        getSkillRecordsInfo(day);




        //知识库分析
        getKnowledgeBaseInfo();
        return rtn;
    }
    async function getKnowledgeBaseInfo() {
        let rtn = '';
        try {
            //占用空间,文件数量,文件夹数量，垃圾箱文件数量
            const data = await scanDashboardData(true);
            rtn += `# 知识库文档文件相关信息：\n\n
## 文件及磁盘占用信息
文件数量: ${data?.stats?.total_files || 0}\n
文件夹数量: ${data?.stats?.total_folders || 0}\n
总占用空间: ${data?.stats?.total_size_str || 0}\n\n`;
        } catch (e) {
            console.error('获取基本信息失败', e);
        } try {
            const { docExtMap, fileSizeRangeMap } = await renderDashStats();
            //尾缀数量统计
            const extDistribution = Object.entries(docExtMap).sort((a, b) => b[1] - a[1])
                .slice(0, 20)
                .map(([ext, count]) => `${ext}: ${count}`)
                .join(',');

            const fileSizeDistribution = Object.entries(fileSizeRangeMap)
                .map(([ext, count]) => `${ext}: ${count}`)
                .join(',');
            rtn += `## 文件尾缀分布（top20文件尾缀）\n
其中mmd(mermaid图表)、canvas（画布）、excalidraw（白板）、opml（大纲笔记）、mm（思维导图）文件\n
${extDistribution}\n\n
## 文件大小分布（top20文件大小范围）\n
${fileSizeDistribution}\n\n`;
        } catch (e) {
            console.error('获取文件分布失败', e);
        }
        try {
            const [kbStatus, chatStats, aiStats, scanData, embInfo, bmStats, skill, mcp] = await Promise.all([
                window.__TAURI__.core.invoke('kb_status', { dirPath: path }),
                window.__TAURI__.core.invoke('kb_chat_stats', { dirPath: path }).catch(() => null),
                window.__TAURI__.core.invoke('ai_history_stats', { dirPath: path }).catch(() => null),
                window.__TAURI__.core.invoke('kb_dashboard_stats', { dirPath: path }).catch(() => null),
                window.__TAURI__.core.invoke('kb_embedding_info').catch(() => null),
                window.__mdgoBookmark.bookmarkStat(path).catch(() => null),
                window.__TAURI__.core.invoke('kb_skill', { dirPath: path }).catch(() => null),
                window.__TAURI__.core.invoke('kb_mcp', { dirPath: path }).catch(() => null),
            ]);
            const unindexed = Math.max(0, _scanFileList.length - (kbStatus.file_count || 0));
            const avgIndex = kbStatus.file_count > 0 ? (kbStatus.vector_count / kbStatus.file_count).toFixed(1) : '--';
            rtn += `# 索引RAG相关信息：\n
被索引文件数: ${kbStatus?.fileCount || 0}\n
不支持索引文件数: ${unindexed}\n
chunk 数: ${kbStatus?.chunkCount || 0}\n
向量数: ${kbStatus?.vectorCount || 0}\n
索引占用空间: ${scanData?.storage_size || 0}\n\n
# 书签相关：\n
书签数: ${(bmStats && bmStats.total) ? bmStats.total : 0}\n
未解析书签数: ${(bmStats && bmStats.pending) ? bmStats.pending : 0}\n
已解析书签数: ${(bmStats && bmStats.ready) ? bmStats.ready : 0}\n
书签死链数: ${(bmStats && bmStats.dead) ? bmStats.dead : 0}\n\n
# Agent对话相关信息：\n
对话数: ${chatStats?.sessionCount || 0}\n
消息数: ${chatStats?.messageCount || 0}\n
AI 操作数: ${aiStats?.total_count || 0}\n\n
# 向量模型相关信息：\n
向量模型名称: ${embInfo?.model_name || '未知'}\n
向量模型维度: ${embInfo?.dimension || 0}\n
平均向量/文件: ${avgIndex}\n
模型状态: ${embInfo?.status || '未知'}\n\n
# Skill相关信息：\n
skill总数: ${skill?.total || 0}\n
系统skill数: ${skill?.system_count || 0}\n
全局skill数: ${skill?.global_count || 0}\n
项目skill数: ${skill?.project_count || 0}\n
已启用skill数: ${skill?.enabled_count || 0}\n
已禁用skill数: ${skill?.disabled_count || 0}\n
skill都有：${skill?.skills?.map(skill => `${skill.id}:(${skill.name})`).join(',') || '无'}\n\n
# MCP相关信息：\n
MCP总数: ${mcp?.total || 0}\n\n
已连接MCP数: ${mcp?.connected_count || 0}\n
已断开MCP数: ${mcp?.disconnected_count || 0}\n
失败MCP数: ${mcp?.failed_count || 0}\n\n
MCP都有：${mcp?.servers?.map(server => server.name).join(',') || '无'}\n\n
`;
        } catch (e) {
            console.error('rag相关信息获取失败', e);
        }
        return rtn;
    }

    window.aiSummay = { getSummary };
})();
