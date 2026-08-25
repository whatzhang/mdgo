/**
 * ===== 日程模块（css_js/modules/schedule.js） =====
 * 【职责】日程功能的前端展示层：ScheduleService 门面（IPC 调 Rust 引擎）+ 日历渲染 + 提醒 UI。
 * 【数据逻辑】全部在 Rust core::schedule（CRUD 校验 / Cron 展开 / 冲突 / 提醒判定与调度 / 农历节假日），
 *            前端只做展示；到点提醒由 Rust 调度器经 schedule:reminder 事件推送，本模块监听弹窗。
 * 【依赖的全局服务】（来自 main.html 主脚本，加载顺序：主脚本 → 本模块）
 *   - window.__TAURI__ / currentRootPath / showNotification / TimerManager / switchToView
 *   - Lunar（lunar.min.js 全局）/ HOLIDAY_API / paySound / escapeHtml / markedMd
 */
        function _saveLunarCache() {
            try {
                const payload = { _expire: Date.now() + CACHE_EXPIRE_DAYS * 86400000, data: _lunarCache };
                safeLocalStorageSet(LUNAR_CACHE_KEY, JSON.stringify(payload));
            } catch (e) { /* ignore quota */
            }
        }
        // 仅在缓存有变更时保存，在渲染/导航完成后调用
        function _flushLunarCache() {
            if (_lunarCacheDirty) {
                _lunarCacheDirty = false;
                _saveLunarCache();
            }
        }

        // localStorage → 农历缓存
        function _loadLunarCache() {
            try {
                const raw = safeLocalStorageGet(LUNAR_CACHE_KEY);
                if (!raw) return false;
                const payload = JSON.parse(raw);
                if (!payload || payload._expire < Date.now()) {
                    localStorage.removeItem(LUNAR_CACHE_KEY);
                    return false;
                }
                const data = payload.data || {};
                // 验证缓存有效性：至少有一条记录的 lunarDay 非空，否则判定为脏数据丢弃
                const keys = Object.keys(data);
                if (keys.length > 0) {
                    const first = data[keys[0]];
                    if (!first.lunarDay) {
                        console.warn('⚠ 农历缓存数据无效（lunarDay 为空），已丢弃');
                        localStorage.removeItem(LUNAR_CACHE_KEY);
                        return false;
                    }
                }
                _lunarCache = data;
                return true;
            } catch (e) {
                return false;
            }
        }

        // 节假日缓存 → localStorage
        function _saveHolidayCache(year, data) {
            try {
                const payload = { _year: year, _expire: Date.now() + CACHE_EXPIRE_DAYS * 86400000, holiday: data };
                safeLocalStorageSet(HOLIDAY_CACHE_KEY, JSON.stringify(payload));
            } catch (e) { /* ignore quota */
            }
        }

        // localStorage → 节假日缓存
        function _loadHolidayCache(year) {
            try {
                const raw = safeLocalStorageGet(HOLIDAY_CACHE_KEY);
                if (!raw) return false;
                const payload = JSON.parse(raw);
                if (!payload || payload._expire < Date.now() || payload._year !== year) {
                    localStorage.removeItem(HOLIDAY_CACHE_KEY);
                    return false;
                }
                _holidayCache = payload.holiday || {};
                _holidayCache._year = year;
                return true;
            } catch (e) {
                return false;
            }
        }

        // 加载某年节假日（优先走缓存，没有再调 API）
        async function _ensureHolidayData(year) {
            if (_holidayCache && _holidayCache._year === year) return;
            if (_loadHolidayCache(year)) return;
            if (_holidayLoading) {
                await _holidayLoading;
                return;
            }
            _holidayLoading = (async () => {
                try {
                    const resp = await fetch(HOLIDAY_API + year);
                    const result = await resp.json();
                    if (result.code === 0) {
                        // API 返回的 key 是 MM-DD 格式，转为 YYYY-MM-DD 格式存储（与 _getDayLunarInfo 查找一致）
                        const apiData = result.holiday || {};
                        const normalized = {};
                        for (const mmdd of Object.keys(apiData)) {
                            const fullKey = `${year}-${mmdd}`;
                            normalized[fullKey] = apiData[mmdd];
                        }
                        _holidayCache = normalized;
                        _holidayCache._year = year;
                        _saveHolidayCache(year, normalized);
                    }
                } catch (e) {
                    console.warn('节假日数据加载失败:', e);
                } finally {
                    _holidayLoading = false;
                }
            })();
            await _holidayLoading;
        }

        /** 获取某天的农历信息 + 节假日 */
        function _getDayLunarInfo(year, month, day) {
            const key = `${year}-${String(month).padStart(2, '0')}-${String(day).padStart(2, '0')}`;
            if (_lunarCache[key]) return _lunarCache[key];

            const info = { lunarDay: '', lunarMonth: '', festival: '', holiday: null, isHoliday: false, isWeekend: false };
            // 先获取农历基础数据
            try {
                if (typeof Lunar !== 'undefined') {
                    const lunar = Lunar.fromDate(new Date(year, month - 1, day));
                    info.lunarDay = lunar.getDayInChinese();
                    if (info.lunarDay === '初一') {
                        info.lunarMonth = (lunar.isLeapMonth() ? '闰' : '') + lunar.getMonthInChinese() + '月';
                    }
                    info._lunarFestivals = [...lunar.getFestivals(), ...lunar.getOtherFestivals()];
                }
            } catch (e) { /* ignore */
            }

            // 节假日（优先 API 数据，若无则按周末判断）
            const d = new Date(year, month - 1, day);
            const dow = d.getDay();
            const isWeekend = (dow === 0 || dow === 6);
            info.isWeekend = isWeekend;

            if (_holidayCache && _holidayCache[key]) {
                const h = _holidayCache[key];
                info.holiday = h;
                info.isHoliday = h.holiday === true;
                // API 有数据时，农历节日不要覆盖（避免端午节假期每天都显示"端午节"）
            } else {
                info.isHoliday = isWeekend;
                // 仅当 API 无数据时，使用农历节日
                if (info._lunarFestivals && info._lunarFestivals.length > 0) {
                    info.festival = info._lunarFestivals[0];
                }
            }

            _lunarCache[key] = info;
            _lunarCacheDirty = true;
            return info;
        }

        /** 格式化农历显示文本（仅农历数据，不含 API 节假日名） */
        function _formatLunarLabel(info) {
            if (info.festival) return info.festival;
            if (info.lunarMonth) return info.lunarMonth;  // 初一显示"正月"等
            return info.lunarDay;                          // 初二、初三...
        }

        /** 获取节假日名称 */
        function _getHolidayName(info) {
            if (!info.holiday || info.holiday.holiday !== true) return '';
            return info.holiday.name || '';
        }

        /** 判断某天是否为休息日（周末或法定假日，排除调休上班日） */
        function _isRestDay(info) {
            if (info.holiday) return info.holiday.holiday === true;
            return info.isWeekend;
        }

        /* ==================== 日历应用逻辑 ==================== */
        // ====== ScheduleService：日程数据门面（Tauri 模式经 IPC 调 Rust 引擎，前端只做展示） ======
        // 全部数据逻辑（CRUD 校验 / Cron 展开 / 冲突检测 / 提醒计算 / 农历节假日）在 Rust core::schedule，
        // 前端经本门面取数与提交，内存 todoEvents 仅作渲染镜像（Rust 存储为唯一事实源）。
        const ScheduleService = {
            _invoke(name, args) {
                return window.__TAURI__.core.invoke(name, args);
            },
            list: (dirPath) => window.__TAURI__.core.invoke('schedule_list', { dirPath }),
            get: (dirPath, id) => window.__TAURI__.core.invoke('schedule_get', { dirPath, id }),
            add: (dirPath, input) => window.__TAURI__.core.invoke('schedule_add', { dirPath, input }),
            update: (dirPath, id, input) => window.__TAURI__.core.invoke('schedule_update', { dirPath, id, input }),
            remove: (dirPath, id) => window.__TAURI__.core.invoke('schedule_remove', { dirPath, id }),
            eventsOnDate: (dirPath, date) => window.__TAURI__.core.invoke('schedule_events_on_date', { dirPath, date }),
            conflicts: (dirPath, start, end, ignoreId) => window.__TAURI__.core.invoke('schedule_conflicts', { dirPath, start, end, ignoreId }),
            lunar: (dirPath, date) => window.__TAURI__.core.invoke('schedule_lunar', { dirPath, date }),
            nextAvailable: (dirPath, durationMinutes, startAfter, skipRestDays) => window.__TAURI__.core.invoke('schedule_next_available', { dirPath, durationMinutes, startAfter, skipRestDays }),
            setActiveDir: (dirPath) => window.__TAURI__.core.invoke('schedule_set_active_dir', { dirPath }),
            clearActiveDir: () => window.__TAURI__.core.invoke('schedule_clear_active_dir'),
        };

        async function openCalendar() {
            await switchToView(calendarContainer);
            // 尝试从 localStorage 加载农历缓存
            _loadLunarCache();
            // 异步加载当年节假日数据（优先 localStorage）
            const year = new Date().getFullYear();
            _ensureHolidayData(year).then(() => {
                if (todoCurrentDate.getFullYear() !== year) return;
                todoRenderAll();
                _flushLunarCache(); // 初始渲染完成后持久化农历缓存
            });
            todoInit();
            const timeDisplay = calendarContainer.querySelector('#todo-display-time');
            if (timeDisplay) {
                TimerManager.set('todo-display-time', () => {
                    // 空闲时不更新时间，减少 CPU 占用
                    if (!isIdle()) {
                        todoUpdateModalTime(timeDisplay);
                    }
                }, 1000);
            }
        }

        // 状态管理
        let todoCurrentDate = new Date();
        let todoCurrentView = 'month';
        let todoSelectedDate = new Date();
        let todoEditingEvent = null;
        let todoSelectedColor = 'blue';
        let todoMiniCalendarDate = new Date();
        todoMiniCalendarDate.setDate(1); // 迷你日历初始化为当月第一天
        let todoEventSource = null; // SSE 连接实例
        let _todoEventsCache = null; // 按日期排序的事件缓存
        let todoLastCheckTime = 0; // 上次检查时间（用于去重）
        const _remindedEventIds = new Set(); // 已提醒的事件 ID（防止重复弹窗）

        /** 颜色配置映射表 - 统一管理所有颜色相关样式 */
        const TODO_COLORS = {
            blue: { primary: '#3370FF', light: '#E8F0FE', weekBg: '#3370FF', monthBg: '#E8F0FE', monthText: '#3370FF' },
            purple: { primary: '#7B61FF', light: '#F0EBFF', weekBg: '#7B61FF', monthBg: '#F0EBFF', monthText: '#7B61FF' },
            green: { primary: '#10B981', light: '#D1FAE5', weekBg: '#10B981', monthBg: '#D1FAE5', monthText: '#059669' },
            orange: { primary: '#F59E0B', light: '#FFEDD5', weekBg: '#F59E0B', monthBg: '#FFEDD5', monthText: '#EA580C' },
            red: { primary: '#EF4444', light: '#FEE2E2', weekBg: '#EF4444', monthBg: '#FEE2E2', monthText: '#DC2626' }
        };

        // 日程事件数据（从本地 JSON 文件加载，默认为空）
        let todoEvents = [];

        /**
         * 解析 Cron 表达式的分钟和小时部分
         * 支持: * , - /step 格式
         * @param {string} field - Cron 字段（如 *, *\/5, 1,2,3, 1-5）
         * @param {number} min - 最小值
         * @param {number} max - 最大值
         * @returns {number[]} 匹配的值列表
         */
        function _parseCronField(field, min, max) {
            const result = new Set();
            const parts = field.split(',');
            for (const part of parts) {
                const trimmed = part.trim();
                if (trimmed === '*') {
                    for (let i = min; i <= max; i++) result.add(i);
                } else if (trimmed.startsWith('*/')) {
                    const step = parseInt(trimmed.slice(2), 10);
                    if (step > 0) for (let i = min; i <= max; i += step) result.add(i);
                } else if (trimmed.includes('-')) {
                    const [s, e] = trimmed.split('-').map(Number);
                    for (let i = Math.max(min, s); i <= Math.min(max, e); i++) result.add(i);
                } else {
                    const n = parseInt(trimmed, 10);
                    if (!isNaN(n) && n >= min && n <= max) result.add(n);
                }
            }
            return [...result].sort((a, b) => a - b);
        }

        // 展开 cron 日程到指定日期，生成虚拟事件
        function _expandCronEventsForDate(todoE, date) {
            if (!todoE.cron) return [];

            const p = todoE._cronParsed;
            const startDate = todoE.start instanceof Date ? todoE.start : new Date(todoE.start);
            const endDate = todoE.end instanceof Date ? todoE.end : new Date(todoE.end);

            // 只处理当天在区间内的日期
            const dayStart = new Date(date);
            dayStart.setHours(0, 0, 0, 0);
            const dayEnd = new Date(date);
            dayEnd.setHours(23, 59, 59, 999);
            if (dayEnd < startDate || dayStart > endDate) return [];

            // 使用缓存的 parsed 字段做 O(1) 日月匹配，避免 _parseCronField 重复解析
            if (p) {
                const targetMonth = date.getMonth() + 1;
                const targetDay = date.getDate();
                const targetDow = date.getDay();
                if (!p.allMon && !p._months.has(targetMonth)) return [];
                if (!p.allDay && !p._days.has(targetDay)) return [];
                if (!p.allDow && !p._dows.has(targetDow) && !(targetDow === 0 && p._dows.has(7))) return [];
            } else {
                // 无缓存时回退到原始解析方式
                const cronParts = todoE.cron.trim().split(/\s+/);
                if (cronParts.length < 5) return [];
                const [minField, hourField, domField, monthField, dowField] = cronParts;
                const targetMonth = date.getMonth() + 1;
                if (!_parseCronField(monthField, 1, 12).includes(targetMonth)) return [];
                const targetDay = date.getDate();
                if (!_parseCronField(domField, 1, 31).includes(targetDay)) return [];
                const targetDow = date.getDay();
                // 周字段统一 0-7（0 与 7 均为周日），与 Rust cron crate / _findNextCronTime 一致
                const dowHits = _parseCronField(dowField, 0, 7);
                if (!dowHits.includes(targetDow) && !(targetDow === 0 && dowHits.includes(7))) return [];
            }

            // 当天的实际区间（取 start~end 和 当天的交集）
            const rangeStart = dayStart > startDate ? dayStart : startDate;
            const rangeEnd = dayEnd < endDate ? dayEnd : endDate;

            let hours, minutes;
            if (p) {
                hours = [...p._hours];
                minutes = [...p._mins];
            } else {
                const cronParts = todoE.cron.trim().split(/\s+/);
                hours = _parseCronField(cronParts[1], 0, 23);
                minutes = _parseCronField(cronParts[0], 0, 59);
            }

            const events = [];
            const baseTime = new Date(date);
            for (let hi = 0; hi < hours.length; hi++) {
                const h = hours[hi];
                for (let mi = 0; mi < minutes.length; mi++) {
                    const m = minutes[mi];
                    const eventTime = new Date(baseTime);
                    eventTime.setHours(h, m, 0, 0);
                    if (eventTime >= rangeStart && eventTime <= rangeEnd) {
                        events.push({ ...todoE, _isCronVirtual: true, _cronTime: eventTime });
                    }
                }
            }
            return events;
        }

        // 按日期缓存 getEventsForDate 结果，todoEvents 变更或切换月份时失效
        let _eventsByDateCache = null;
        let _eventsByDateCacheKey = '';
        let _eventsCacheVersion = 0;            // 递增计数器，todoEvents 变更时 +1

        /** 判断是否为"稍后提醒"临时事件（界面一律不显示）：
         *  - 会话内：`_isSnoozed` 标记（新版前端定时器实现不落库，仅兼容旧流程）
         *  - 存量/任何来源：标题 `[稍后提醒] ` 前缀（旧版本曾落库产生，重启后标记丢失仍能识别）
         */
        function _isSnoozeReminderEvent(todoE) {
            return !!todoE && (todoE._isSnoozed === true || (typeof todoE.title === 'string' && todoE.title.startsWith('[稍后提醒]')));
        }

        /**
         * 获取指定日期的所有事件（含 Cron 展开的虚拟事件）
         * @param {Date} date
         * @returns {Array} 排序后的事件列表
         */
        function getEventsForDate(date) {
            // 使用缓存：以版本号为 key（_eventsCacheVersion 在 todoEvents 变更时递增）
            const verKey = `${_eventsCacheVersion}`;
            const dateKey = date.toDateString();
            if (_eventsByDateCache && _eventsByDateCacheKey === verKey && _eventsByDateCache.has(dateKey)) {
                return _eventsByDateCache.get(dateKey);
            }
            // 普通事件（非 cron 事件），按 start 日期匹配，排除稍后提醒临时事件
            const normalEvents = todoEvents.filter(todoE => !_isSnoozeReminderEvent(todoE) && !todoE.cron && todoIsSameDay(todoE.start, date));
            // Cron 事件展开为虚拟事件，排除稍后提醒临时事件
            const cronEvents = todoEvents
                .filter(todoE => !_isSnoozeReminderEvent(todoE) && todoE.cron)
                .flatMap(todoE => _expandCronEventsForDate(todoE, date));

            const result = [...normalEvents, ...cronEvents].sort((a, b) => {
                const aTime = a._cronTime || a.start;
                const bTime = b._cronTime || b.start;
                return aTime - bTime;
            });
            // 存入缓存
            if (!_eventsByDateCache || _eventsByDateCacheKey !== verKey) {
                _eventsByDateCache = new Map();
                _eventsByDateCacheKey = verKey;
            }
            _eventsByDateCache.set(dateKey, result);
            return result;
        }
        /** 使事件缓存失效，在 todoEvents 变更后调用 */
        function _invalidateEventsCache() {
            _eventsByDateCache = null;
            _eventsByDateCacheKey = '';
            _eventsCacheVersion++;                  // 递增版本号，让所有缓存失效
        }
        function validateScheduleTime(start, end) {
            var now = new Date();
            if (end <= start) return '结束时间必须晚于开始时间';
            if (end < now) return '结束时间不能早于当前时间';
            return null;
        }
        function generateEventId() {
            return `${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
        }
        function todo$(id) {
            return document.getElementById(id);
        }
        // 初始化
        let _todoEarlyInitialized = false;
        let _todoModalClickBound = false;

        async function todoInit() {
            // 如果已通过 todoEarlyInit 初始化过，只需确保 SSE 连接和渲染
            if (!_todoEarlyInitialized) {
                await todoLoadFromFile();
            }
            todoRenderAll();
            // 系统级定时器：页面隐藏时继续运行，确保时间线实时更新
            TimerManager.set('todoTimeline', todoRenderCurrentTimeLine, 60000, true);
            // 点击遮罩关闭（只绑定一次）
            if (!_todoModalClickBound) {
                const modal = todo$('todo-modal');
                if (modal) {
                    modal.addEventListener('click', (todoEv) => {
                        if (todoEv.target === modal) todoCloseModal();
                    });
                    _todoModalClickBound = true;
                }
            }
            todoBindCronPresets();
            if (!_todoEarlyInitialized) {
                todoStartLocalScheduler();
                _todoEarlyInitialized = true;
            }
        }

        /** 清理日历 UI 定时器，并复位日程初始化状态（目录切换时调用）：
         *  1. 清 UI 定时器；
         *  2. 通知 Rust 调度器清除 active_dir（旧目录提醒停止触发）；
         *  3. 复位 _todoEarlyInitialized，使切换后的 initAll → todoEarlyInit
         *     重新加载新目录日程并 setActiveDir(新目录)。
         */
        function todoDestroy() {
            if (TimerManager.has('todoTimeline')) {
                TimerManager.clear('todoTimeline');
            }
            TimerManager.clear('todo-display-time');
            TimerManager.clear('todoModalTime');
            todoStopLocalScheduler();
            _todoEarlyInitialized = false;
        }

        function todoRenderAll() {
            todoUpdateHeader();
            todoRenderMiniCalendar();
            // 只渲染当前可见视图，减少不必要的 DOM 操作
            if (todoCurrentView === 'month') todoRenderMonthView();
            else if (todoCurrentView === 'week') todoRenderWeekView();
            else todoRenderDayView();
            if (_todoScheduleTab === 'today') todoRenderTodayEvents();
            else todoRenderHistoryEvents();
            todoRenderCurrentTimeLine();
        }

        // 更新顶部日期显示
        function todoUpdateHeader() {
            const year = todoCurrentDate.getFullYear();
            const month = todoCurrentDate.getMonth() + 1;
            const el = todo$('todo-currentDate');
            if (el) el.textContent = `${year}年${month}月`;
        }

        function _monOffset(date) {
            // 返回周一=0, 周二=1, ..., 周日=6
            return (date.getDay() + 6) % 7;
        }

        // 渲染迷你日历（使用文档片段优化性能）
        function todoRenderMiniCalendar() {
            const year = todoMiniCalendarDate.getFullYear();
            const month = todoMiniCalendarDate.getMonth();
            const monthEl = todo$('todo-miniCalendarMonth');
            if (monthEl) monthEl.textContent = `${year}年${month + 1}月`;
            const grid = todo$('todo-miniCalendar');
            if (!grid) return;
            grid.innerHTML = '';
            const fragment = document.createDocumentFragment();
            const firstDay = new Date(year, month, 1);
            const lastDay = new Date(year, month + 1, 0);
            const startPadding = _monOffset(firstDay);
            const daysInMonth = lastDay.getDate();
            const today = new Date();
            // 周标签行
            const dayLabels = ['周一', '周二', '周三', '周四', '周五', '周六', '周日'];
            dayLabels.forEach(d => {
                const el = document.createElement('div');
                el.className = 'todo-day-label';
                el.textContent = d;
                fragment.appendChild(el);
            });
            // 上个月填充
            const prevMonthDays = new Date(year, month, 0).getDate();
            for (let i = startPadding - 1; i >= 0; i--) {
                const el = document.createElement('div');
                el.className = 'todo-mini-day todo-other-month';
                el.textContent = prevMonthDays - i;
                fragment.appendChild(el);
            }
            // 本月天数
            for (let day = 1; day <= daysInMonth; day++) {
                const date = new Date(year, month, day);
                const el = document.createElement('div');
                el.className = 'todo-mini-day';
                // 农历/节假日
                const lunarInfo = _getDayLunarInfo(year, month + 1, day);
                const lunarLabel = _formatLunarLabel(lunarInfo);
                const holidayName = _getHolidayName(lunarInfo);
                const isRest = _isRestDay(lunarInfo);
                const isWorkday = lunarInfo.holiday && lunarInfo.holiday.holiday === false;
                const dow = date.getDay(); // 0=周日, 6=周六
                const isWeekend = (dow === 0 || dow === 6);
                // 决定显示标签：节日名（仅非周末法定假日）或其他用农历
                const displayLabel = (holidayName && !isWeekend) ? holidayName : lunarLabel;
                const showHolidayName = displayLabel === holidayName;  // 是否正在显示节日名
                if (isRest) el.classList.add('todo-rest-day');
                if (isWeekend) el.classList.add('todo-weekend');
                if (isWorkday) el.classList.add('todo-work-day');
                if (todoIsSameDay(date, today)) el.classList.add('todo-today');
                if (todoIsSameDay(date, todoSelectedDate)) el.classList.add('todo-selected');
                if (todoHasEvent(date)) el.classList.add('todo-has-event');
                // 迷你日历标签 + 标记
                let lunarHtml = '';
                if (displayLabel) {
                    // 休息日且未显示节日名时→(休)；调休上班日→(班)；节日名直接显示不加后缀
                    if (isRest) {
                        lunarHtml = `<span class="todo-mini-lunar">${displayLabel}<span class="todo-rest-tag">休</span></span>`;
                    } else if (isWorkday) {
                        lunarHtml = `<span class="todo-mini-lunar">${displayLabel}<span class="todo-work-tag">班</span></span>`;
                    } else {
                        lunarHtml = `<span class="todo-mini-lunar">${displayLabel}</span>`;
                    }
                }
                el.innerHTML = `<span class="todo-mini-day-number">${day}</span>${lunarHtml}`;
                el.onclick = () => {
                    todoSelectedDate = date;
                    todoCurrentDate = new Date(date);
                    todoRenderAll();
                };
                fragment.appendChild(el);
            }
            // 下个月填充
            const totalCells = startPadding + daysInMonth;
            const remaining = (7 - (totalCells % 7)) % 7;
            for (let day = 1; day <= remaining; day++) {
                const el = document.createElement('div');
                el.className = 'todo-mini-day todo-other-month';
                el.textContent = day;
                fragment.appendChild(el);
            }
            grid.appendChild(fragment);
        }

        function todoHasEvent(date) {
            // 轻量检查：先查普通事件，再查 cron 事件，命中即短路（排除稍后提醒临时事件）
            const hasNormal = todoEvents.some(todoE => !_isSnoozeReminderEvent(todoE) && !todoE.cron && todoIsSameDay(todoE.start, date));
            if (hasNormal) return true;
            // cron 事件使用缓存的解析字段快速判断，避免重复调用 _expandCronEventsForDate
            return todoEvents.some(todoE => {
                if (_isSnoozeReminderEvent(todoE) || !todoE.cron) return false;
                // 尝试用已缓存的 cronParsed 做快速日月匹配
                const p = todoE._cronParsed;
                if (p) {
                    const m = date.getMonth() + 1;
                    const d = date.getDate();
                    const w = date.getDay();
                    if (!p.allMon && !p._months.has(m)) return false;
                    if (!p.allDay && !p._days.has(d)) return false;
                    if (!p.allDow && !p._dows.has(w) && !(w === 0 && p._dows.has(7))) return false;
                    return true; // 日月星期都匹配 → 有事件
                }
                // 无缓存时回退到展开方式
                const expanded = _expandCronEventsForDate(todoE, date);
                return expanded.length > 0;
            });
        }

        // 渲染月视图（使用文档片段优化，动态计算行数）
        function todoRenderMonthView() {
            const year = todoCurrentDate.getFullYear();
            const month = todoCurrentDate.getMonth();
            const firstDay = new Date(year, month, 1);
            const lastDay = new Date(year, month + 1, 0);
            const startPadding = _monOffset(firstDay);
            const daysInMonth = lastDay.getDate();
            const grid = todo$('todo-monthGrid');
            if (!grid) return;
            grid.innerHTML = '';
            const fragment = document.createDocumentFragment();
            const prevMonthDays = new Date(year, month, 0).getDate();
            const today = new Date();
            // 上个月填充
            for (let i = startPadding - 1; i >= 0; i--) {
                const el = document.createElement('div');
                el.className = 'todo-month-day todo-other-month';
                el.innerHTML = `<span class="todo-day-number">${prevMonthDays - i}</span>`;
                fragment.appendChild(el);
            }
            // 本月天数
            for (let day = 1; day <= daysInMonth; day++) {
                const date = new Date(year, month, day);
                const el = document.createElement('div');
                el.className = 'todo-month-day';
                // 农历/节假日
                const lunarInfo = _getDayLunarInfo(year, month + 1, day);
                const lunarLabel = _formatLunarLabel(lunarInfo);
                const holidayName = _getHolidayName(lunarInfo);
                const isRest = _isRestDay(lunarInfo);
                const isWorkday = lunarInfo.holiday && lunarInfo.holiday.holiday === false;
                const dow = date.getDay();
                const isWeekend = (dow === 0 || dow === 6);
                // 决定显示标签：节日名（仅非周末法定假日）或其他用农历
                const displayLabel = (holidayName && !isWeekend) ? holidayName : lunarLabel;
                const showHolidayName = displayLabel === holidayName;
                // ✅ 休息日（法定假日/周末）→ 淡蓝色背景
                if (isRest) el.classList.add('todo-rest-day');
                // ✅ 周末
                if (isWeekend) el.classList.add('todo-weekend');
                // ✅ 调休上班日 → 橙色（班）标记
                if (isWorkday) el.classList.add('todo-work-day');
                if (todoIsSameDay(date, today)) el.classList.add('todo-today');
                // 构建标签
                let suffixHtml = '';
                if (displayLabel) {
                    // 休息日且未显示节日名时→(休)；调休上班日→(班)；节日名直接显示不加后缀
                    if (isRest && !showHolidayName) {
                        suffixHtml = `<span class="todo-lunar-label"><span>${displayLabel}</span><span class="todo-rest-tag">休</span></span>`;
                    } else if (isWorkday) {
                        suffixHtml = `<span class="todo-lunar-label"><span>${displayLabel}</span><span class="todo-work-tag">班</span></span>`;
                    } else if (showHolidayName) {
                        suffixHtml = `<span class="todo-lunar-label">${displayLabel}</span>`;
                    } else {
                        suffixHtml = `<span class="todo-lunar-label">${displayLabel}</span>`;
                    }
                }
                let html = `<span class="todo-day-number">${day}</span>${suffixHtml}<div class="todo-month-div">`;
                const dayEvents = getEventsForDate(date);
                dayEvents.forEach(todoE => {
                    const isCronVirtual = todoE._isCronVirtual;
                    const isReminder = todoE.event_type === 'reminder';
                    const eventTime = isCronVirtual ? todoFormatTime(todoE._cronTime) : '';
                    const cronClass = isCronVirtual ? ' todo-cron-event' : '';
                    const reminderClass = isReminder ? ' todo-reminder-event' : '';
                    const onclick = ` onclick="event.stopPropagation(); todoEditEvent('${todoE.id}')"`;
                    html += `<div class="todo-month-event todo-${todoE.color || 'blue'}${cronClass}${reminderClass}"${onclick}>${isCronVirtual ? eventTime + ' ' : ''}${escapeHtml(todoE.title)}</div>`;
                });
                html += `</div><div class="todo-add-event-hint">+</div>`;
                el.innerHTML = html;
                el.onclick = () => todoOpenModal(date);
                fragment.appendChild(el);
            }
            // 下个月填充
            const totalCells = startPadding + daysInMonth;
            const remaining = (7 - (totalCells % 7)) % 7;
            for (let day = 1; day <= remaining; day++) {
                const el = document.createElement('div');
                el.className = 'todo-month-day todo-other-month';
                el.innerHTML = `<span class="todo-day-number">${day}</span>`;
                fragment.appendChild(el);
            }
            grid.appendChild(fragment);
            // 动态计算实际需要的行数并设置 grid-template-rows
            const totalSlots = startPadding + daysInMonth + remaining;
            const actualRows = totalSlots / 7;
            grid.style.gridTemplateRows = `repeat(${actualRows}, 1fr)`;
        }

        /** 创建24小时时间列（周/日视图共用） */
        function todoRenderTimeColumn(container) {
            if (!container) return;
            const fragment = document.createDocumentFragment();
            for (let h = 0; h < 24; h++) {
                const timeSlot = document.createElement('div');
                timeSlot.className = 'todo-time-slot';
                const timeLabel = document.createElement('div');
                timeLabel.className = 'todo-time-label';
                timeLabel.textContent = `${h}:00`;
                timeSlot.appendChild(timeLabel);
                fragment.appendChild(timeSlot);
            }
            container.appendChild(fragment);
        }

        /** 创建单个日期列的24小时时间槽 */
        function todoCreateDayColumnSlots(container) {
            if (!container) return;
            const fragment = document.createDocumentFragment();
            for (let h = 0; h < 24; h++) {
                const slot = document.createElement('div');
                slot.className = 'todo-time-slot';
                fragment.appendChild(slot);
            }
            container.appendChild(fragment);
        }

        const TODO_WEEKDAY_NAMES = ['周一', '周二', '周三', '周四', '周五', '周六', '周日'];

        // 渲染周视图
        function todoRenderWeekView() {
            const startOfWeek = new Date(todoCurrentDate);
            // 周一为一周开始
            startOfWeek.setDate(todoCurrentDate.getDate() - ((todoCurrentDate.getDay() + 6) % 7));
            const header = todo$('todo-weekHeader');
            const timeColumn = todo$('todo-weekTimeColumn');
            const columns = todo$('todo-weekColumns');
            if (!header || !timeColumn || !columns) return;
            header.innerHTML = '<div class="todo-time-column"></div>';
            columns.innerHTML = '';
            timeColumn.innerHTML = '';
            todoRenderTimeColumn(timeColumn);
            const _todayForWeek = new Date();
            for (let i = 0; i < 7; i++) {
                const date = new Date(startOfWeek);
                date.setDate(startOfWeek.getDate() + i);
                const isToday = todoIsSameDay(date, _todayForWeek);
                const headerCol = document.createElement('div');
                headerCol.className = 'todo-day-column-header';
                headerCol.innerHTML = `
                    <div class="todo-day-name">${TODO_WEEKDAY_NAMES[i]}</div>
                    <div class="todo-day-number-header ${isToday ? 'todo-today' : ''}">${date.getDate()}</div>
                `;
                header.appendChild(headerCol);
                const col = document.createElement('div');
                col.className = 'todo-day-column';
                todoCreateDayColumnSlots(col);
                // 渲染事件（含重叠自动侧边布局）
                const dayEvents = getEventsForDate(date);
                _layoutOverlappingEvents(dayEvents).forEach(todoE => {
                    col.appendChild(todoCreateWeekEvent(todoE, dayEvents));
                });
                col.onclick = (todoEv) => {
                    if (todoEv.target === col || todoEv.target.classList.contains('todo-time-slot')) {
                        const rect = col.getBoundingClientRect();
                        const y = todoEv.clientY - rect.top + col.scrollTop;
                        const hour = Math.floor(y / 60);
                        const clickedDate = new Date(date);
                        clickedDate.setHours(hour, 0, 0, 0);
                        todoOpenModal(clickedDate);
                    }
                };
                columns.appendChild(col);
            }
        }

        // 渲染日视图
        function todoRenderDayView() {
            const header = todo$('todo-dayViewHeader');
            const timeColumn = todo$('todo-dayTimeColumn');
            const slots = todo$('todo-dayTimeSlots');
            if (!header || !timeColumn || !slots) return;
            const isToday = todoIsSameDay(todoCurrentDate, new Date());
            header.innerHTML = `
                <div class="todo-day-column-header">
                    <div class="todo-day-name">${TODO_WEEKDAY_NAMES[(todoCurrentDate.getDay() + 6) % 7]}</div>
                    <div class="todo-day-number-header ${isToday ? 'todo-today' : ''}">${todoCurrentDate.getDate()}</div>
                </div>
            `;
            timeColumn.innerHTML = '';
            slots.innerHTML = '';
            todoRenderTimeColumn(timeColumn);
            todoCreateDayColumnSlots(slots);
            // 渲染事件（含重叠自动侧边布局）
            const dayEvents = getEventsForDate(todoCurrentDate);
            _layoutOverlappingEvents(dayEvents).forEach(todoE => {
                slots.appendChild(todoCreateWeekEvent(todoE, dayEvents));
            });
            slots.onclick = (todoEv) => {
                if (todoEv.target === slots || todoEv.target.classList.contains('todo-time-slot')) {
                    const rect = slots.getBoundingClientRect();
                    const y = todoEv.clientY - rect.top + slots.scrollTop;
                    const hour = Math.floor(y / 60);
                    const clickedDate = new Date(todoCurrentDate);
                    clickedDate.setHours(hour, 0, 0, 0);
                    todoOpenModal(clickedDate);
                }
            };
        }
        /**
         * 重叠事件侧边布局算法
         * 将同时段的事件水平分割，为每个事件分配 left/width，避免相互遮挡
         * @param {Array} events - 某一天已排序的事件列表
         * @returns {Array} 添加了 _layoutLeft, _layoutWidth, _layoutTotal 的 events
         */
        function _layoutOverlappingEvents(events) {
            if (!events.length) return events;
            // 为每个事件计算时间区间（分钟）
            const periods = events.map(todoE => {
                const isCronVirtual = todoE._isCronVirtual;
                const start = isCronVirtual ? todoE._cronTime : todoE.start;
                const end = isCronVirtual ? new Date(start.getTime() + 30 * 60 * 1000) : todoE.end;
                const startMin = start.getHours() * 60 + start.getMinutes();
                let endMin = end.getHours() * 60 + end.getMinutes();
                // 整天事件（00:00-00:00）视为 0-1440
                if (endMin === 0 && end > start) endMin = 1440;
                return { startMin, endMin, event: todoE };
            });
            // 检测重叠并分组
            const groups = [];
            for (const p of periods) {
                let placed = false;
                for (const group of groups) {
                    // 检查与组内任意事件是否有重叠
                    const overlaps = group.some(gp => p.startMin < gp.endMin && p.endMin > gp.startMin);
                    if (overlaps) {
                        group.push(p);
                        placed = true;
                        break;
                    }
                }
                if (!placed) {
                    groups.push([p]);
                }
            }
            // 为每个组内的事件分配水平位置
            for (const group of groups) {
                if (group.length === 1) {
                    group[0].event._layoutLeft = 0;
                    group[0].event._layoutWidth = 1;
                    group[0].event._layoutTotal = 1;
                } else {
                    const count = group.length;
                    // 同一组内再按 startMin 排序，依次分配列
                    group.sort((a, b) => a.startMin - b.startMin || b.endMin - a.endMin);
                    const colWidth = 1 / count;
                    group.forEach((p, idx) => {
                        p.event._layoutLeft = idx * colWidth;
                        p.event._layoutWidth = colWidth;
                        p.event._layoutTotal = count;
                    });
                }
            }
            return events;
        }
        // 创建周事件元素
        function todoCreateWeekEvent(todoE, allDayEvents) {
            const isCronVirtual = todoE._isCronVirtual;
            const eventTime = isCronVirtual ? todoE._cronTime : todoE.start;
            const eventEnd = isCronVirtual ? new Date(eventTime.getTime() + 30 * 60 * 1000) : todoE.end;
            const startHour = eventTime.getHours() + eventTime.getMinutes() / 60;
            const duration = (eventEnd - eventTime) / (1000 * 60 * 60);
            const el = document.createElement('div');
            const isReminder = todoE.event_type === 'reminder';
            el.className = `todo-week-event todo-${todoE.color}${isCronVirtual ? ' todo-cron-event' : ''}${isReminder ? ' todo-reminder-event' : ''}`;
            el.style.top = `${startHour * 60}px`;
            el.style.height = `${Math.max(duration * 60, 20)}px`;
            // 重叠布局 - 并排显示时左右各留 4px 间隙
            if (todoE._layoutWidth !== undefined && todoE._layoutWidth < 1) {
                const gapPx = 4;
                const colIdx = Math.round(todoE._layoutLeft / todoE._layoutWidth);
                const colCount = todoE._layoutTotal;
                el.style.right = 'auto';
                el.style.left = `${todoE._layoutLeft * 100}%`;
                el.style.width = `${todoE._layoutWidth * 100}%`;
                // 用透明 border 做间隙，background-clip 让背景色不进入 border 区域
                el.style.boxSizing = 'border-box';
                el.style.backgroundClip = 'padding-box';
                el.style.borderLeft = `${gapPx / 2}px solid transparent`;
                el.style.borderRight = `${gapPx / 2}px solid transparent`;
                el.style.zIndex = colCount - colIdx;
            }
            if (isCronVirtual) {
                el.innerHTML = `
                    <div style="font-size:10px;opacity:0.8;">🔄 ${todoFormatTime(eventTime)}</div>
                    <div style="font-size:11px;">${escapeHtml(todoE.title)}</div>
                `;
                el.title = `Cron: ${todoE.cron}`;
                el.style.cursor = 'pointer';
                el.onclick = (todoEv) => {
                    todoEv.stopPropagation();
                    todoEditEvent(todoE.id);
                };
            } else if (isReminder) {
                // 单点提醒：时间胶囊（最小高度），点击打开提醒编辑
                el.innerHTML = `
                    <div class="todo-event-time-small">🔔 ${todoFormatTime(eventTime)}</div>
                    <div>${escapeHtml(todoE.title)}</div>
                `;
                el.title = (todoE.desc ? '备注: ' + todoE.desc : '单点提醒');
                el.style.cursor = 'pointer';
                el.onclick = (todoEv) => {
                    todoEv.stopPropagation();
                    todoEditEvent(todoE.id);
                };
            } else {
                el.innerHTML = `
                    <div class="todo-event-time-small">${todoFormatTime(todoE.start)} - ${todoFormatTime(todoE.end)}</div>
                    <div>${escapeHtml(todoE.title)}</div>
                `;
                el.draggable = true;
                el.ondragstart = (todoEv) => {
                    todoEv.dataTransfer.setData('eventId', todoE.id);
                    el.classList.add('todo-dragging');
                };
                el.ondragend = () => el.classList.remove('todo-dragging');
                el.onclick = (todoEv) => {
                    todoEv.stopPropagation();
                    todoEditEvent(todoE.id);
                };
            }
            return el;
        }

        // 渲染今日事件侧边栏
        let _todoScheduleTab = 'today'; // today / history
        // 切换日程标签
        function todoSwitchScheduleTab(tab) {
            _todoScheduleTab = tab;
            document.querySelectorAll('.todo-schedule-tab').forEach(function (t) {
                t.classList.toggle('todo-schedule-tab-active', t.dataset.tab === tab);
            });
            document.getElementById('todo-todayEvents').style.display = tab === 'today' ? '' : 'none';
            document.getElementById('todo-historyEvents').style.display = tab === 'history' ? '' : 'none';
            if (tab === 'today') todoRenderTodayEvents();
            else todoRenderHistoryEvents();
        }
        // 渲染今日事件
        function todoRenderTodayEvents() {
            const container = todo$('todo-todayEvents');
            if (!container) return;
            const today = new Date();
            const now = Date.now();
            const todayEvents = getEventsForDate(today);
            // 过滤：仅显示当前时间之后的日程
            const upcoming = [];
            todayEvents.forEach(function (todoE) {
                const raw = todoE._isCronVirtual ? todoE._cronTime : todoE.start;
                const eventTime = raw instanceof Date ? raw : new Date(raw);
                if (!isNaN(eventTime) && eventTime >= now) {
                    upcoming.push(todoE);
                }
            });
            // 更新数量（写在按钮文本内）
            const countEl = todo$('todo-todayCount');
            if (upcoming.length === 0) {
                container.innerHTML = '<div class="todo-empty-state">今日暂无待办日程</div>';
                return;
            }
            let html = '';
            for (let i = 0; i < upcoming.length; i++) {
                const todoE = upcoming[i];
                const isCronVirtual = todoE._isCronVirtual;
                const eventTime = isCronVirtual ? todoFormatTime(todoE._cronTime) : (todoE.start instanceof Date ? todoE.start : new Date(todoE.start));
                const endDate = isCronVirtual ? null : (todoE.end instanceof Date ? todoE.end : new Date(todoE.end));
                let timeStr;
                if (isCronVirtual) {
                    timeStr = '🔄 ' + eventTime;
                } else if (eventTime instanceof Date && !isNaN(eventTime) && endDate instanceof Date && !isNaN(endDate)) {
                    timeStr = todoFormatTime(eventTime) + ' - ' + todoFormatTime(endDate);
                } else {
                    timeStr = '';
                }
                const cronClass = isCronVirtual ? ' todo-cron-sidebar' : '';
                html += '<div class="todo-event-item todo-' + (todoE.color || 'blue') + cronClass + '" onclick="todoEditEvent(\'' + todoE.id + '\')">' +
                    '<div class="todo-event-time">' + timeStr + '</div>' +
                    '<div class="todo-event-title">' + escapeHtml(todoE.title) + '</div>' +
                    '</div>';
            }
            container.innerHTML = html;
            if (countEl) countEl.innerText = upcoming.length;
        }

        function todoRenderHistoryEvents() {
            const list = todo$('todo-historyList');
            if (!list) return;
            const startStr = todo$('todo-historyStart').value;
            const endStr = todo$('todo-historyEnd').value;
            const searchKeyword = todo$('todo-historySearch').value.trim().toLowerCase();

            const startFilter = startStr ? new Date(startStr + 'T00:00:00') : null;
            const endFilter = endStr ? new Date(endStr + 'T23:59:59') : null;

            const now = Date.now();
            const historyMap = new Map();
            todoEvents.forEach(function (todoE) {
                if (_isSnoozeReminderEvent(todoE)) return; // 稍后提醒临时事件不显示在历史中
                if (todoE.cron) {
                    const cronStart = todoE.start instanceof Date ? todoE.start : new Date(todoE.start);
                    const cronEnd = todoE.end instanceof Date ? todoE.end : new Date(todoE.end);
                    const cursor = new Date(cronStart);
                    cursor.setHours(0, 0, 0, 0);
                    const scanEnd = new Date(Math.min(cronEnd.getTime(), now));
                    // 安全上限：最多扫描 365 天，防止跨多年的 cron 事件导致卡死
                    const MAX_SCAN_DAYS = 365;
                    let daysScanned = 0;
                    while (cursor <= scanEnd && daysScanned < MAX_SCAN_DAYS) {
                        const expanded = _expandCronEventsForDate(todoE, cursor);
                        expanded.forEach(function (ev) {
                            const et = ev._cronTime.getTime();
                            if (et < now) {
                                if (searchKeyword && !ev.title.toLowerCase().includes(searchKeyword)) return;
                                if ((!startFilter || et >= startFilter.getTime()) && (!endFilter || et <= endFilter.getTime())) {
                                    historyMap.set(ev._cronTime.getTime() + '-' + ev.id, ev);
                                }
                            }
                        });
                        cursor.setDate(cursor.getDate() + 1);
                        daysScanned++;
                    }
                } else {
                    const et = todoE.start instanceof Date ? todoE.start.getTime() : new Date(todoE.start).getTime();
                    if (et < now) {
                        if (searchKeyword && !todoE.title.toLowerCase().includes(searchKeyword)) return;
                        if ((!startFilter || et >= startFilter.getTime()) && (!endFilter || et <= endFilter.getTime())) {
                            historyMap.set(et + '-' + todoE.id, todoE);
                        }
                    }
                }
            });
            // 排序历史事件
            const sorted = Array.from(historyMap.values()).sort(function (a, b) {
                const aT = a._cronTime || a.start;
                const bT = b._cronTime || b.start;
                return bT - aT; // 倒序，最新的在前
            });
            const countEl = todo$('todo-historyCount');
            if (countEl) countEl.textContent = sorted.length;

            if (sorted.length === 0) {
                list.innerHTML = '<div class="todo-empty-state">暂无历史日程</div>';
                return;
            }
            // 渲染历史事件
            list.innerHTML = sorted.map(function (todoE) {
                const isCronVirtual = todoE._isCronVirtual;
                const eventTime = isCronVirtual ? todoE._cronTime : (todoE.start instanceof Date ? todoE.start : new Date(todoE.start));
                const endDate = isCronVirtual ? null : (todoE.end instanceof Date ? todoE.end : new Date(todoE.end));
                const dateStr = eventTime instanceof Date ? eventTime.toLocaleString('zh-CN', {
                    month: '2-digit',
                    day: '2-digit',
                    hour: '2-digit',
                    minute: '2-digit'
                }) : '';
                const timeStr = isCronVirtual
                    ? '\uD83D\uDD04 ' + dateStr
                    : (endDate
                        ? dateStr + ' - ' + (endDate instanceof Date ? endDate.toLocaleString('zh-CN', {
                            hour: '2-digit',
                            minute: '2-digit'
                        }) : '')
                        : dateStr);
                const cronClass = isCronVirtual ? ' todo-cron-sidebar' : '';
                return '<div class="todo-event-item todo-' + (todoE.color || 'blue') + cronClass + '" onclick="todoEditEvent(\'' + todoE.id + '\')">' +
                    '<div class="todo-event-time">' + timeStr + '</div>' +
                    '<div class="todo-event-title">' + escapeHtml(todoE.title) + '</div>' +
                    '</div>';
            }).join('');
        }
        // 当前时间线
        function todoRenderCurrentTimeLine() {
            const now = new Date();
            const currentHour = now.getHours() + now.getMinutes() / 60;
            // 清除旧的时间线
            document.querySelectorAll('.todo-current-time-line').forEach(el => el.remove());
            if (todoCurrentView === 'week') {
                // 基于当前真实时间计算所在周（周一为一周开始）
                const realWeekStart = new Date(now);
                realWeekStart.setDate(now.getDate() - ((now.getDay() + 6) % 7));
                realWeekStart.setHours(0, 0, 0, 0);
                const startOfWeek = new Date(todoCurrentDate);
                startOfWeek.setDate(todoCurrentDate.getDate() - ((todoCurrentDate.getDay() + 6) % 7));
                startOfWeek.setHours(0, 0, 0, 0);
                // 只有当当前周与视图周匹配时才显示时间线
                if (realWeekStart.getTime() === startOfWeek.getTime()) {
                    const columns = todo$('todo-weekColumns');
                    if (columns) {
                        const dayDiff = (now.getDay() + 6) % 7; // 周一=0, ..., 周日=6
                        if (columns.children[dayDiff]) {
                            const line = document.createElement('div');
                            line.className = 'todo-current-time-line';
                            line.style.top = `${currentHour * 60}px`;
                            columns.children[dayDiff].appendChild(line);
                        }
                    }
                }
            } else if (todoCurrentView === 'day' && todoIsSameDay(todoCurrentDate, now)) {
                const slots = todo$('todo-dayTimeSlots');
                if (slots) {
                    const line = document.createElement('div');
                    line.className = 'todo-current-time-line';
                    line.style.top = `${currentHour * 60}px`;
                    slots.appendChild(line);
                }
            }
        }
        // 工具函数
        function todoIsSameDay(d1, d2) {
            return d1.getFullYear() === d2.getFullYear() &&
                d1.getMonth() === d2.getMonth() &&
                d1.getDate() === d2.getDate();
        }
        // 格式化时间
        function todoFormatTime(date) {
            return `${String(date.getHours()).padStart(2, '0')}:${String(date.getMinutes()).padStart(2, '0')}`;
        }
        // 格式化日期时间（本地时间）   
        function todoFormatDateTimeLocal(date) {
            const year = date.getFullYear();
            const month = String(date.getMonth() + 1).padStart(2, '0');
            const day = String(date.getDate()).padStart(2, '0');
            const hours = String(date.getHours()).padStart(2, '0');
            const minutes = String(date.getMinutes()).padStart(2, '0');
            return `${year}-${month}-${day}T${hours}:${minutes}`;
        }
        // 视图切换
        function todoSwitchView(view, todoEv) {
            todoCurrentView = view;
            document.querySelectorAll('.todo-view-btn').forEach(btn => btn.classList.remove('todo-active'));
            if (todoEv && todoEv.target) todoEv.target.classList.add('todo-active');
            const monthView = todo$('todo-monthView');
            const weekView = todo$('todo-weekView');
            const dayView = todo$('todo-dayView');
            if (monthView) monthView.style.display = view === 'month' ? 'flex' : 'none';
            if (weekView) weekView.style.display = view === 'week' ? 'flex' : 'none';
            if (dayView) dayView.style.display = view === 'day' ? 'flex' : 'none';
            todoRenderAll();
        }
        // 日期导航
        function todoChangeDate(delta) {
            if (todoCurrentView === 'month') {
                const newMonth = todoCurrentDate.getMonth() + delta;
                const dd = new Date(todoCurrentDate);
                dd.setDate(1);
                dd.setMonth(newMonth);
                todoCurrentDate = dd;
            } else if (todoCurrentView === 'week') {
                todoCurrentDate.setDate(todoCurrentDate.getDate() + delta * 7);
            } else {
                todoCurrentDate.setDate(todoCurrentDate.getDate() + delta);
            }
            todoRenderAll();
            // 异步加载切换后年份的节假日数据
            _ensureHolidayData(todoCurrentDate.getFullYear());
        }
        // 迷你日历月份切换
        function todoChangeMiniMonth(delta) {
            const newMonth = todoMiniCalendarDate.getMonth() + delta;
            todoMiniCalendarDate.setDate(1); // 先设为1号避免跨月溢出
            todoMiniCalendarDate.setMonth(newMonth);
            // 同步主日历到迷你日历选中的月份
            todoCurrentDate = new Date(todoMiniCalendarDate);
            todoRenderAll();
        }
        // 跳转到今天
        function todoGoToToday() {
            todoCurrentDate = new Date();
            todoSelectedDate = new Date();
            todoMiniCalendarDate = new Date();
            todoRenderAll();
            _ensureHolidayData(todoCurrentDate.getFullYear());
        }
        function todoUpdateDay(el) {
            const now = new Date();
            const year = now.getFullYear();
            const month = now.getMonth() + 1;
            const day = now.getDate();
            const weekDays = ['日', '一', '二', '三', '四', '五', '六'];
            const weekDay = weekDays[now.getDay()];
            el.textContent = `${year}年${month}月${day}日 周${weekDay}`;
        }
        // 实时时间更新
        function todoUpdateModalTime(el) {
            const now = new Date();
            const year = now.getFullYear();
            const month = now.getMonth() + 1;
            const day = now.getDate();
            const hours = String(now.getHours()).padStart(2, '0');
            const minutes = String(now.getMinutes()).padStart(2, '0');
            const seconds = String(now.getSeconds()).padStart(2, '0');
            const weekDays = ['日', '一', '二', '三', '四', '五', '六'];
            const weekDay = weekDays[now.getDay()];
            el.textContent = `${year}年${month}月${day}日 周${weekDay} ${hours}:${minutes}:${seconds}`;
        }
        //解析 cron 表达式为中文说明
        function todoParseCron(cronExpr) {
            if (!cronExpr || !cronExpr.trim()) return '';
            if (typeof cronstrue === 'undefined') return cronExpr;
            try {
                return cronstrue.toString(cronExpr, { locale: 'zh_CN', use24HourTimeFormat: true });
            } catch (e) {
                try {
                    return cronstrue.toString(cronExpr, { use24HourTimeFormat: true });
                } catch (e2) {
                    return '⚠️ 无效的 Cron 表达式';
                }
            }
        }
        //更新 cron 预览
        function todoUpdateCronPreview() {
            const input = todo$('todo-eventCron');
            const preview = todo$('todo-cron-preview');
            if (!input || !preview) return;
            const val = input.value.trim();
            if (!val) {
                preview.textContent = '';
                return;
            }
            preview.textContent = '📋 ' + todoParseCron(val);
        }
        // 绑定预设按钮事件
        function todoBindCronPresets() {
            const presets = document.querySelectorAll('.todo-cron-btn');
            presets.forEach(btn => {
                btn.onclick = (e) => {
                    e.stopPropagation();
                    const input = todo$('todo-eventCron');
                    if (!input) return;
                    input.value = btn.dataset.cron;
                    todoUpdateCronPreview();
                };
            });
            // 输入框实时解析
            const input = todo$('todo-eventCron');
            if (input) {
                input.oninput = todoUpdateCronPreview;
            }
        }
        // 弹窗操作
        function todoOpenModal(date = null) {
            todoEditingEvent = null;
            const titleEl = todo$('todo-modalTitle');
            const deleteBtn = todo$('todo-deleteBtn');
            const eventTitle = todo$('todo-eventTitle');
            const eventDesc = todo$('todo-eventDesc');
            const eventStart = todo$('todo-eventStart');
            const eventEnd = todo$('todo-eventEnd');
            const modal = todo$('todo-modal');
            const timeDisplay = todo$('todo-modalTitle-time');
            const cronInput = todo$('todo-eventCron');
            if (!modal || !titleEl || !deleteBtn || !eventTitle || !eventDesc || !eventStart || !eventEnd) return;
            titleEl.textContent = '新建日程';
            deleteBtn.style.display = 'none';
            eventTitle.value = '';
            eventDesc.value = '';
            if (cronInput) cronInput.value = '';
            todoUpdateCronPreview();
            // 新建日程：日期跟随点击/默认今天，时分始终为当前时间（用户要求），结束默认当天 23:59:59
            const now = new Date();
            const start = date ? new Date(date) : new Date();
            start.setHours(now.getHours(), now.getMinutes(), 0, 0);
            const end = new Date(start);
            end.setHours(23, 59, 59, 999);
            eventStart.value = todoFormatDateTimeLocal(start);
            eventEnd.value = todoFormatDateTimeLocal(end);
            // 弹窗实时时间显示
            if (timeDisplay) {
                todoUpdateModalTime(timeDisplay);
                // 每秒更新时间
                TimerManager.set('todoModalTime', () => todoUpdateModalTime(timeDisplay), 1000);
            }
            const defaultColor = document.querySelector('.todo-color-option[data-color="orange"]');
            if (defaultColor) todoSelectColor(defaultColor);
            // 新建日程：提醒默认开启、提前 0 分钟
            const notifyCb = todo$('todo-eventNotify');
            if (notifyCb) notifyCb.checked = true;
            const notifyBefore = todo$('todo-eventNotifyBefore');
            if (notifyBefore) notifyBefore.value = '0';
            modal.classList.add('todo-active');
        }
        // 编辑事件
        function todoEditEvent(id) {
            const todoE = todoEvents.find(todoEv => todoEv.id === id);
            if (!todoE) return;
            // 提醒（闹钟式单点事件）走独立提醒模态框
            if (todoE.event_type === 'reminder') {
                todoOpenReminderModal(todoE);
                return;
            }
            const titleEl = todo$('todo-modalTitle');
            const deleteBtn = todo$('todo-deleteBtn');
            const eventTitle = todo$('todo-eventTitle');
            const eventDesc = todo$('todo-eventDesc');
            const eventStart = todo$('todo-eventStart');
            const eventEnd = todo$('todo-eventEnd');
            const modal = todo$('todo-modal');
            const timeDisplay = todo$('todo-modalTitle-time');
            const cronInput = todo$('todo-eventCron');
            if (!modal || !titleEl || !deleteBtn || !eventTitle || !eventDesc || !eventStart || !eventEnd) return;
            todoEditingEvent = todoE;
            titleEl.textContent = '编辑日程';
            deleteBtn.style.display = 'block';
            eventTitle.value = todoE.title;
            eventDesc.value = todoE.desc || '';
            eventStart.value = todoFormatDateTimeLocal(todoE.start);
            eventEnd.value = todoFormatDateTimeLocal(todoE.end);
            // cron 表达式
            if (cronInput) {
                cronInput.value = todoE.cron || '';
                todoUpdateCronPreview();
            }
            // 弹窗实时时间显示
            if (timeDisplay) {
                todoUpdateModalTime(timeDisplay);
                TimerManager.set('todoModalTime', () => todoUpdateModalTime(timeDisplay), 1000);
            }
            const colorEl = document.querySelector(`.todo-color-option[data-color="${todoE.color}"]`);
            if (colorEl) todoSelectColor(colorEl);
            // 编辑日程：回填提醒开关与提前量
            const notifyCb = todo$('todo-eventNotify');
            if (notifyCb) notifyCb.checked = todoE.notify !== false;
            const notifyBefore = todo$('todo-eventNotifyBefore');
            if (notifyBefore) notifyBefore.value = todoE.notify_before || 0;
            modal.classList.add('todo-active');
        }
        // 关闭弹窗
        function todoCloseModal() {
            const modal = todo$('todo-modal');
            if (modal) modal.classList.remove('todo-active');
            todoEditingEvent = null;
            // 关闭弹窗时清除实时时间定时器
            TimerManager.clear('todoModalTime');
        }
        // 选择颜色
        function todoSelectColor(el) {
            if (!el) return;
            document.querySelectorAll('#todo-modal .todo-color-option').forEach(c => c.classList.remove('todo-selected'));
            el.classList.add('todo-selected');
            todoSelectedColor = el.dataset.color;
        }
        // ── 独立提醒（闹钟式：标题/时间/颜色/备注；内部为 event_type=reminder 单点事件）──
        let todoReminderEditing = null;
        let todoReminderSelectedColor = 'blue';

        function todoReminderSelectColor(el) {
            if (!el) return;
            document.querySelectorAll('#todo-reminderModal .todo-color-option').forEach(c => c.classList.remove('todo-selected'));
            el.classList.add('todo-selected');
            todoReminderSelectedColor = el.dataset.color;
        }

        function todoOpenReminderModal(event = null) {
            const modal = todo$('todo-reminderModal');
            const titleEl = todo$('todo-reminderModalTitle');
            const inputTitle = todo$('todo-reminderTitle');
            const inputTime = todo$('todo-reminderTime');
            const inputNote = todo$('todo-reminderNote');
            const deleteBtn = todo$('todo-reminderDeleteBtn');
            if (!modal || !inputTitle || !inputTime || !inputNote) return;
            todoReminderEditing = event;
            const colorEls = document.querySelectorAll('#todo-reminderModal .todo-color-option');
            if (event) {
                titleEl.textContent = '编辑提醒';
                inputTitle.value = event.title || '';
                const t = event.start instanceof Date ? event.start : new Date(event.start);
                inputTime.value = todoFormatDateTimeLocal(t);
                inputNote.value = event.desc || '';
                todoReminderSelectedColor = event.color || 'blue';
                if (deleteBtn) deleteBtn.style.display = '';
            } else {
                titleEl.textContent = '新建提醒';
                inputTitle.value = '';
                const now = new Date();
                // 默认下一个 5 分钟整点
                now.setMinutes(now.getMinutes() + (5 - (now.getMinutes() % 5)) % 5, 0, 0);
                inputTime.value = todoFormatDateTimeLocal(now);
                inputNote.value = '';
                todoReminderSelectedColor = 'blue';
                if (deleteBtn) deleteBtn.style.display = 'none';
            }
            colorEls.forEach(c => c.classList.toggle('todo-selected', c.dataset.color === todoReminderSelectedColor));
            modal.classList.add('todo-active');
        }

        function todoCloseReminderModal() {
            const modal = todo$('todo-reminderModal');
            if (modal) modal.classList.remove('todo-active');
            todoReminderEditing = null;
        }

        async function todoSaveReminder() {
            const inputTitle = todo$('todo-reminderTitle');
            const inputTime = todo$('todo-reminderTime');
            const inputNote = todo$('todo-reminderNote');
            if (!inputTitle || !inputTime || !inputNote) return;
            const title = inputTitle.value.trim();
            const timeVal = inputTime.value;
            if (!title) {
                showNotification('请输入提醒标题', 'warning');
                return;
            }
            if (!timeVal) {
                showNotification('请选择提醒时间', 'warning');
                return;
            }
            const t = new Date(timeVal);
            const input = {
                title,
                start: _dateToLocalISO(t),
                end: _dateToLocalISO(t),
                color: todoReminderSelectedColor,
                desc: inputNote.value.trim(),
                event_type: 'reminder',
                notify: true,
            };
            try {
                if (todoReminderEditing) {
                    const updated = await ScheduleService.update(currentRootPath, todoReminderEditing.id, input);
                    const idx = todoEvents.findIndex(e => e.id === updated.id);
                    if (idx >= 0) todoEvents[idx] = { ...updated, start: new Date(updated.start), end: new Date(updated.end) };
                } else {
                    const res = await ScheduleService.add(currentRootPath, input);
                    todoEvents.push({ ...res.event, start: new Date(res.event.start), end: new Date(res.event.end) });
                }
            } catch (e) {
                showNotification('保存提醒失败: ' + e, 'error');
                return;
            }
            todoCloseReminderModal();
            _invalidateEventsCache();
            todoRenderAll();
            todoUpdateLocalScheduler();
        }

        async function todoDeleteReminder() {
            if (!todoReminderEditing) return;
            const id = todoReminderEditing.id;
            try {
                await ScheduleService.remove(currentRootPath, id);
                todoEvents = todoEvents.filter(e => e.id !== id);
            } catch (e) {
                showNotification('删除提醒失败: ' + e, 'error');
                return;
            }
            todoCloseReminderModal();
            _invalidateEventsCache();
            todoRenderAll();
            todoUpdateLocalScheduler();
        }
        async function todoSaveEvent() {
            const eventTitle = todo$('todo-eventTitle');
            const eventDesc = todo$('todo-eventDesc');
            const eventStart = todo$('todo-eventStart');
            const eventEnd = todo$('todo-eventEnd');
            const cronInput = todo$('todo-eventCron');
            if (!eventTitle || !eventDesc || !eventStart || !eventEnd) return;
            const title = eventTitle.value.trim();
            const start = new Date(eventStart.value);
            const end = new Date(eventEnd.value);
            const desc = eventDesc.value.trim();
            const cron = cronInput ? cronInput.value.trim() : '';
            if (!title) {
                showNotification('请输入日程标题', 'warning');
                return;
            }
            // 校验 Cron 表达式（前端即时提示；最终校验由 Rust 引擎执行）
            if (cron && typeof cronstrue !== 'undefined') {
                try {
                    cronstrue.toString(cron, { locale: 'zh_CN', use24HourTimeFormat: true });
                } catch (e) {
                    try {
                        cronstrue.toString(cron, { use24HourTimeFormat: true });
                    } catch (e2) {
                        showNotification('⚠️ Cron 表达式无效，请检查格式', 'warning');
                        return;
                    }
                }
            }
            const input = {
                title,
                start: _dateToLocalISO(start),
                end: _dateToLocalISO(end),
                color: todoSelectedColor,
                desc,
                cron,
                notify: (todo$('todo-eventNotify') || {}).checked !== false,
                notify_before: parseInt((todo$('todo-eventNotifyBefore') || {}).value, 10) || 0,
            };
            try {
                if (todoEditingEvent) {
                    const updated = await ScheduleService.update(currentRootPath, todoEditingEvent.id, input);
                    const idx = todoEvents.findIndex(e => e.id === updated.id);
                    if (idx >= 0) todoEvents[idx] = { ...updated, start: new Date(updated.start), end: new Date(updated.end) };
                } else {
                    const res = await ScheduleService.add(currentRootPath, input);
                    todoEvents.push({ ...res.event, start: new Date(res.event.start), end: new Date(res.event.end) });
                    if (res.conflicts && res.conflicts.length > 0) {
                        showNotification(`⚠ 与 ${res.conflicts.length} 个现有日程时间冲突`, 'warning');
                    }
                }
            } catch (e) {
                showNotification('保存日程失败: ' + e, 'error');
                return;
            }
            todoCloseModal();
            _invalidateEventsCache();
            todoRenderAll();
            todoUpdateLocalScheduler();
        }
        // 删除当前编辑的事件   
        async function todoDeleteEvent() {
            if (!todoEditingEvent) return;
            const deletedId = todoEditingEvent.id;
            const deletedTitle = todoEditingEvent.title;
            try {
                await ScheduleService.remove(currentRootPath, deletedId);
                // 级联清理关联的"稍后提醒"事件：先删后端持久化记录（重启后不残留孤儿提醒），再清内存镜像
                if (deletedTitle) {
                    try {
                        const all = await ScheduleService.list(currentRootPath);
                        const snoozePrefix = `[稍后提醒] ${deletedTitle}`;
                        for (const s of all) {
                            if (s.title === snoozePrefix) {
                                await ScheduleService.remove(currentRootPath, s.id);
                            }
                        }
                    } catch (e) {
                        console.warn('清理稍后提醒失败:', e);
                    }
                }
                todoEvents = todoEvents.filter(todoE => {
                    if (todoE.id === deletedId) return false;
                    if (deletedTitle && todoE.title === `[稍后提醒] ${deletedTitle}`) return false;
                    return true;
                });
            } catch (e) {
                showNotification('删除日程失败: ' + e, 'error');
                return;
            }
            _invalidateEventsCache();
            todoCloseModal();
            todoRenderAll();
            todoUpdateLocalScheduler();
        }
        function _dateToLocalISO(d) {
            // 复用 todoFormatDateTimeLocal，增加 Date 类型校验
            return d instanceof Date ? todoFormatDateTimeLocal(d) : d;
        }
        // 日程数据已由 Rust 引擎（schedule_add/update/remove）持久化到 .mdgo/index_schedule.json；
        // 本函数保留调用点（防抖），Tauri 模式下改为从 Rust 刷新内存镜像，不再前端直写文件。
        let _syncTimer = null;
        async function todoSyncToFile() {
            if (_syncTimer) clearTimeout(_syncTimer);
            _syncTimer = setTimeout(async () => {
                _syncTimer = null;
                try {
                    const list = await ScheduleService.list(currentRootPath);
                    if (Array.isArray(list)) {
                        todoEvents = list.map(e => ({ ...e, start: new Date(e.start), end: new Date(e.end) }));
                        _invalidateEventsCache();
                        todoRenderAll();
                        todoUpdateLocalScheduler();
                    }
                } catch (e) {
                    console.error('刷新日程失败:', e);
                }
            }, 500);
        }
        // 从 Rust 引擎加载日程数据（schedule_list 读取 .mdgo/index_schedule.json）
        async function todoLoadFromFile() {
            try {
                const list = await ScheduleService.list(currentRootPath);
                if (Array.isArray(list)) {
                    todoEvents = list.map(e => ({ ...e, start: new Date(e.start), end: new Date(e.end) }));
                }
            } catch (e) {
                console.error('从 Rust 加载日程失败:', e);
            }
        }
                // 本地调度器检查函数：仅清理已过期的提醒记录（到点判定已由 Rust 调度器完成）
        function todoCheckScheduledEvents() {
            const now = new Date();
            for (const id of _remindedEventIds) {
                const ev = todoEvents.find(e => e.id === id);
                if (!ev || (ev.end && new Date(ev.end).getTime() + 300000 < now.getTime())) {
                    _remindedEventIds.delete(id);
                }
            }
        }
        // 启动提醒：激活当前目录（Rust 后台调度器按分钟判定），前端监听 schedule:reminder 事件弹窗
        let _reminderListenerBound = false;
        function todoStartLocalScheduler() {
            if (!window.__TAURI__) return;
            ScheduleService.setActiveDir(currentRootPath).catch(e => console.error('激活日程提醒失败', e));
            if (!_reminderListenerBound) {
                _reminderListenerBound = true;
                window.__TAURI__.event.listen('schedule:reminder', (ev) => {
                    const payload = (ev && ev.payload) || {};
                    // 目录归属校验：只处理当前知识库目录的提醒，避免多目录/切换竞态下错弹
                    if (payload.dir_path && payload.dir_path !== currentRootPath) return;
                    const events = payload.events || [];
                    for (const event of events) {
                        if (_remindedEventIds.has(event.id)) continue;
                        _remindedEventIds.add(event.id);
                        todoShowReminder({
                            id: event.id, title: event.title,
                            start: event.start, end: event.end,
                            desc: event.desc || '', color: event.color || '',
                            cron: event.cron || '', cron_trigger: !!event.cron,
                            event_type: event.event_type || '',
                            _snoozeCount: 0
                        });
                    }
                    todoCheckScheduledEvents();
                }).catch(e => console.error('监听日程提醒失败', e));
                // AI 工具 / 其他入口直写 DB 后经此事件通知前端全量刷新（UI 与 Rust 存储保持同步）
                window.__TAURI__.event.listen('schedule:changed', () => _reloadEventsFromRust())
                    .catch(e => console.error('监听日程变更失败', e));
            }
        }
        // 数据变更后全量重拉（防抖：批量新增/删除时合并为一次刷新）
        let _reloadEventsTimer = null;
        function _reloadEventsFromRust() {
            if (_reloadEventsTimer) clearTimeout(_reloadEventsTimer);
            _reloadEventsTimer = setTimeout(async () => {
                _reloadEventsTimer = null;
                try {
                    await todoLoadFromFile();
                    _invalidateEventsCache();
                    if (_todoEarlyInitialized) {
                        todoRenderAll();
                        todoUpdateLocalScheduler();
                    }
                } catch (e) {
                    console.error('日程数据变更刷新失败:', e);
                }
            }, 150);
        }
        // 停止提醒（原 TimerManager 轮询已移除，改为通知 Rust 调度器停止）
        function todoStopLocalScheduler() {
            if (window.__TAURI__) {
                ScheduleService.clearActiveDir().catch(e => console.error('停止日程提醒失败', e));
            }
        }
        // 更新调度器（重启）
        function todoUpdateLocalScheduler() {
            todoStartLocalScheduler();
        }        // 显示日程提醒弹窗
        function todoShowReminder(data) {
            // 判断是否为 Cron 重复提醒
            const isCron = data.cron_trigger === true || data.cron_trigger === 'true';
            // 先显示轻量通知
            const prefix = isCron ? '🔄 ' : '⏰ ';
            showNotification(`${prefix}日程提醒: ${data.title}`, 'success');
            // 移除旧的提醒弹窗，只保留最新一个
            document.querySelectorAll('.todo-reminder-overlay').forEach(el => el.remove());
            // 显示详细弹窗
            const overlay = document.createElement('div');
            overlay.className = 'todo-reminder-overlay';
            function todoFormatTime(date) {
                const month = String(date.getMonth() + 1).padStart(2, '0');
                const day = String(date.getDate()).padStart(2, '0');
                const hours = String(date.getHours()).padStart(2, '0');
                const minutes = String(date.getMinutes()).padStart(2, '0');
                return `${month}-${day} ${hours}:${minutes}`;
            }
            function todoFormatTime2(date) {
                const hours = String(date.getHours()).padStart(2, '0');
                const minutes = String(date.getMinutes()).padStart(2, '0');
                return `${hours}:${minutes}`;
            }
            const startTime = data.start ? todoFormatTime(new Date(data.start)) : '';
            const endTime = data.end ? todoFormatTime(new Date(data.end)) : '';
            // 单点提醒（闹钟）只显示一次时间；日程显示 起~止
            const isReminder = data.event_type === 'reminder';
            const timeStr = isReminder ? startTime : (startTime && endTime ? `${startTime} - ${endTime}` : '');
            const cronHint = isCron && data.cron ? `${escapeHtml(data.cron)}&nbsp;&nbsp;&nbsp;${todoParseCron(data.cron)}` : '';
            overlay.innerHTML = `
                <div class="todo-reminder-modal todo-${data.color || 'blue'}">
                    <div class="todo-reminder-header">
                        <span class="todo-reminder-title">🔔&nbsp;${escapeHtml(data.title)}</span>
                        <span class="todo-reminder-title-time">${todoFormatTime2(new Date())}</span>
                    </div>
                    <div class="todo-reminder-body">
                        ${timeStr ? `<div class="todo-reminder-time"><span>${escapeHtml(timeStr)}</span><span>${cronHint}</span></div>` : ''}
                        ${data.desc ? `<div class="todo-reminder-desc">${escapeHtml(data.desc)}</div>` : ''}
                    </div>
                    <div class="todo-reminder-footer">
                        <button class="btn btn-danger" onclick="this.closest('.todo-reminder-overlay').remove()">关闭</button>
                        ${isCron ? '' : (data._snoozeCount >= 2
                    ? `<button class="btn btn-primary" onclick="todoConfirmReminder(this)">确认</button>`
                    : `<button class="btn " onclick="todoSnoozeReminder(this, '${data.id}')">稍后提醒(5分钟)</button>`)}
                    </div>
                </div>
            `;
            document.body.appendChild(overlay);
            //声音提醒
            paySound();
        }
        // 稍后提醒：5 分钟后再次弹窗（纯前端临时定时器，不落库、不显示在日历/日程列表中）。
        // 旧实现会创建 "[稍后提醒] xxx" 单点事件并落库——重启后 _isSnoozed 内存标记丢失，
        // 该临时事件会以正常日程形态出现在界面中（用户不可见预期）。改为 TimerManager 定时
        // 到点直接重新弹窗：界面零残留、无孤儿事件堆积；应用运行期间（含页面隐藏）可靠触发。
        function todoSnoozeReminder(btn, eventId) {
            const overlay = btn.closest('.todo-reminder-overlay');
            if (overlay) overlay.remove();
            const event = todoEvents.find(e => e.id === eventId);
            if (!event) return;
            // 计数记回原事件（内存态）：连续"稍后提醒"递增，第 3 次弹窗起显示"确认"按钮
            event._snoozeCount = (event._snoozeCount || 0) + 1;
            const nextCount = event._snoozeCount;
            // 一次性定时器（TimerManager.setTimeout，触发后自动清理）；key 含事件 id 与时间戳避免同名覆盖
            const timerKey = `snooze-${eventId}-${Date.now()}`;
            TimerManager.setTimeout(timerKey, () => {
                todoShowReminder({
                    id: event.id, title: event.title,
                    start: event.start, end: event.end,
                    desc: event.desc || '', color: event.color || '',
                    cron: event.cron || '', cron_trigger: !!event.cron,
                    event_type: event.event_type || '',
                    _snoozeCount: nextCount
                });
            }, 5 * 60 * 1000);
            showNotification(`已设为 5 分钟后再次提醒：${event.title}`, 'info');
        }
        // 确认（最后提醒的按钮）：关闭弹窗，不创建新事件
        function todoConfirmReminder(btn) {
            const overlay = btn.closest('.todo-reminder-overlay');
            if (overlay) overlay.remove();
        }

        /**
         * 页面加载后提前初始化日程后台服务（不依赖进入日历页面）
         * - 从文件加载日程数据
         * - 启动本地定时任务调度器
         */
        async function todoEarlyInit() {
            if (_todoEarlyInitialized) return;
            await todoLoadFromFile();
            todoStartLocalScheduler();
            _todoEarlyInitialized = true;
        }
        function _formatShortDateTime(date) {
            const m = String(date.getMonth() + 1).padStart(2, '0');
            const d = String(date.getDate()).padStart(2, '0');
            const hh = String(date.getHours()).padStart(2, '0');
            const mm = String(date.getMinutes()).padStart(2, '0');
            return `${m}/${d} ${hh}:${mm}`;
        }
        async function updateTimeHeader() {
            if (window.__tauriInitPromise) {
                await window.__tauriInitPromise;
            }
            if (!isTauriVisit() || !navigator.userAgent.includes('Mac')) return;
            const timeDisplay = document.getElementById('header-time');
            if (timeDisplay) {
                TimerManager.set('header-time', () => {
                    if (!isIdle()) {
                        todoUpdateModalTime(timeDisplay);
                    }
                }, 1000, true);
            }
        }
        async function updateTimeAll() {
            if (window.__tauriInitPromise) {
                await window.__tauriInitPromise;
            }
            if (!(isTauriVisit() && navigator.userAgent.includes('Mac'))) return;
            updateHeaderUpcoming();
        }

        // ---- 重要节日白名单（用于右上角预告） ----
        const _IMP_FESTIVALS = new Set([
            '元旦', '春节', '清明节', '劳动节', '端午节', '中秋节', '国庆节',
            '元宵节', '七夕节', '重阳节', '除夕', '腊八节'
        ]);
        let _nextFestivalCache = { days: Infinity, name: '' };
        let _festivalCacheDate = '';

        // 查找 cron 定时事件的下一次触发时间（高性能版）
        // 通过解析 cron 字段 + 逐级跳过（月→日→时→分），避免逐日展开创建大量虚拟事件
        function _findNextCronTime(todoE, afterMs) {
            if (!todoE.cron) return Infinity;
            // 1) 解析 cron 字段并缓存到事件对象上（仅首次解析）
            let p = todoE._cronParsed;
            if (!p) {
                const parts = todoE.cron.trim().split(/\s+/);
                if (parts.length !== 5) return Infinity;
                p = todoE._cronParsed = {
                    months: new Set(_parseCronField(parts[3], 1, 12)),
                    days: new Set(_parseCronField(parts[2], 1, 31)),
                    dows: new Set(_parseCronField(parts[4], 0, 7)), // [0,7] 兼容周日 0/7
                    hours: new Set(_parseCronField(parts[1], 0, 23)),
                    mins: new Set(_parseCronField(parts[0], 0, 59)),
                };
                p.allMon = (p.months.size === 12);
                p.allDay = (p.days.size === 31);
                p.allDow = (p.dows.size >= 7); // [0,7] 范围共 8 个值，>=7 即可覆盖全部
                p.allHr = (p.hours.size === 24);
                p.allMin = (p.mins.size === 60);
                // 将常用 Set 转为预检布尔值，避免反复 .has() 调用（常数级优化）
                p._months = p.months; p._days = p.days; p._dows = p.dows;
                p._hours = p.hours; p._mins = p.mins;
            }
            const startMs = (todoE.start instanceof Date ? todoE.start : new Date(todoE.start)).getTime();
            const endMs = (todoE.end instanceof Date ? todoE.end : new Date(todoE.end)).getTime();
            let cursor = new Date(Math.max(afterMs + 60000, startMs)); // 至少从下一分钟开始
            cursor.setSeconds(0, 0);
            const limitMs = Math.min(startMs + 31622400000, endMs); // 上限 ≤ 1 年且 ≤ end
            const incDay = () => { cursor.setDate(cursor.getDate() + 1); cursor.setHours(0, 0, 0, 0); };
            const incMonth = () => { cursor.setMonth(cursor.getMonth() + 1); cursor.setDate(1); cursor.setHours(0, 0, 0, 0); };
            while (cursor.getTime() <= limitMs) {
                // ---- 月 ----
                if (!p.allMon && !p._months.has(cursor.getMonth() + 1)) { incMonth(); continue; }
                // ---- 日（月日 + 星期 均需匹配） ----
                if ((!p.allDay && !p._days.has(cursor.getDate())) ||
                    (!p.allDow && !p._dows.has(cursor.getDay()) && !(cursor.getDay() === 0 && p._dows.has(7)))) { incDay(); continue; }
                // ---- 时 ----
                const ch = cursor.getHours();
                if (!p.allHr) {
                    // 找下一个匹配的小时
                    let found = false;
                    for (let h = ch; h <= 23; h++) { if (p._hours.has(h)) { if (h !== ch) cursor.setHours(h, 0, 0, 0); found = true; break; } }
                    if (!found) { incDay(); continue; }
                }
                // ---- 分 ----
                const cm = cursor.getMinutes();
                if (!p.allMin) {
                    let found = false;
                    for (let m = cm; m <= 59; m++) { if (p._mins.has(m)) { if (m !== cm) cursor.setMinutes(m, 0, 0); found = true; break; } }
                    if (!found) { cursor.setHours(cursor.getHours() + 1, 0, 0, 0); continue; }
                }
                // ---- 全部匹配 → 命中 ----
                const t = cursor.getTime();
                if (t > afterMs && t >= startMs && t <= endMs) return t;
                // 当前时间已匹配，前进到下一分钟继续扫描
                cursor.setMinutes(cursor.getMinutes() + 1);
            }
            return Infinity;
        }

        function updateHeaderUpcoming() {
            const el = document.getElementById('upcomingEvent');
            const fel = document.getElementById('upcomingFestival');
            if (!el || !fel) return;
            const now = Date.now();
            // 1) 查找下一个日程事件（含一次性和 cron 定时事件）
            let nextEv = null, nextEvStart = Infinity, nextEvTime = null;
            if (todoEvents && todoEvents.length > 0) {
                for (let i = 0; i < todoEvents.length; i++) {
                    const ev = todoEvents[i];
                    if (_isSnoozeReminderEvent(ev)) continue; // 稍后提醒不参与头部预告
                    let t = Infinity;
                    if (ev.cron) {
                        t = _findNextCronTime(ev, now);
                    } else if (ev.start && typeof ev.start.getTime === 'function') {
                        t = ev.start.getTime();
                    }
                    if (t > now && t < nextEvStart) {
                        nextEvStart = t;
                        nextEv = ev;
                        nextEvTime = new Date(t);
                    }
                }
            }
            el.textContent = nextEv ? `${_formatShortDateTime(nextEvTime)} ${nextEv.title}` : '';
            // 2) 查找下一个重要节日（每日缓存）
            const today = new Date();
            const cacheKey = `${today.getFullYear()}-${today.getMonth()}-${today.getDate()}`;
            let festival = _nextFestivalCache;
            if (_festivalCacheDate !== cacheKey) {
                festival = { days: Infinity, name: '' };
                for (let i = 1; i <= 365; i++) {
                    const d = new Date(today);
                    d.setDate(today.getDate() + i);
                    const info = _getDayLunarInfo(d.getFullYear(), d.getMonth() + 1, d.getDate());
                    let name = '';
                    if (info.festival && _IMP_FESTIVALS.has(info.festival)) {
                        name = info.festival;
                    } else if (info.holiday && info.holiday.holiday === true && _IMP_FESTIVALS.has(info.holiday.name)) {
                        name = info.holiday.name;
                    }
                    if (name) { festival = { days: i, name }; break; }
                }
                _nextFestivalCache = festival;
                _festivalCacheDate = cacheKey;
            }
            if (festival.name) {
                fel.textContent = festival.days <= 1
                    ? (festival.days === 0 ? `今天${festival.name}` : `明天${festival.name}`)
                    : `${festival.name}: ${festival.days}天后`;
            } else {
                fel.textContent = '';
            }
        }
