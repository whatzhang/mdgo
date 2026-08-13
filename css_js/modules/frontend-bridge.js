/**
 * ===== 前端通信桥（模块标杆 · css_js/modules/frontend-bridge.js） =====
 *
 * 【职责】FrontendBridge：Rust 工具闭包 → WebSocket 双向通道 → 前端业务 handler 的协议层。
 *        高可用设计：自动重连 + 不依赖就绪门控（超时由 Rust 侧兜底）。
 * 【协议】请求 {type:"request", request_id, tool, action, args} → 本桥路由到对应 handler →
 *        回传 {type:"result", request_id, ok, message}。
 * 【扩展】新增业务只需 FrontendBridge.register(tool, handler)，无需改动协议层。
 *        当前注册业务：pomodoro（番茄钟 status/start/autoBreak/autoFocus/stop）、raw-photography（RAW 照片 parse）。
 * 【入口】主脚本启动流程延迟调用 startBridgeTauri() → FrontendBridge.start()（仅 Tauri 模式）。
 * 【对外暴露】全局 const FrontendBridge：register / start。
 */

// ====== 前端通信桥（FrontendBridge）：WebSocket 双向通道，替代 Tauri 事件/命令 ======
// 依赖主页面全局：isTauriVisit / PomodoroService / pomodoroRefreshUI / pomodoroSyncModeUI
// 协议：Rust 工具闭包 → WebSocket {type:"request", ...} → 本桥路由到对应业务 handler →
// 回传 WebSocket {type:"result", ...}。新增业务只需 register 一个 handler，无需改动协议层。
//
// 高可用设计：自动重连 + 不依赖就绪门控，超时是 Rust 侧唯一安全网。
const FrontendBridge = {
    _handlers: {},      // tool → (action, args) => string | Promise<string>
    _ws: null,
    _started: false,
    _reconnectTimer: null,
    _reconnectDelay: 500,

    // 注册业务处理器（如番茄钟 pomodoro）
    register(tool, handler) {
        this._handlers[tool] = handler;
    },

    // 启动桥（仅 Tauri 模式；本地 HTML 模式无后端命令，直接跳过）
    start() {
        if (this._started) return;
        this._started = true;
        if (!isTauriVisit()) return;
        // 获取 WebSocket 端口 → 建立连接
        this._getPortAndConnect();
        console.log('FrontendBridge 启动');
    },

    _getPortAndConnect() {
        if (!window.__TAURI__) {
            setTimeout(() => this._getPortAndConnect(), 500);
            return;
        }
        window.__TAURI__.core.invoke('get_bridge_port')
            .then(port => {
                console.log('FrontendBridge WebSocket 端口:', port);
                this._connect(port);
            })
            .catch(() => {
                // 服务端尚未就绪，延迟重试
                setTimeout(() => this._getPortAndConnect(), 500);
            });
    },

    _connect(port) {
        if (this._ws) {
            try { this._ws.close(); } catch (_) { }
            this._ws = null;
        }
        const url = `ws://127.0.0.1:${port}`;
        console.log('FrontendBridge 连接 WebSocket:', url);
        const ws = new WebSocket(url);
        ws.onopen = () => {
            console.log('FrontendBridge WebSocket 已连接');
            // 上报就绪（best-effort，仅用于观测）
            ws.send(JSON.stringify({ type: 'ready', ready: true }));
        };
        ws.onmessage = (event) => {
            try {
                const msg = JSON.parse(event.data);
                if (msg.type === 'request') {
                    this._dispatch(msg);
                }
            } catch (e) {
                console.warn('FrontendBridge 无法解析消息:', event.data);
            }
        };
        ws.onclose = () => {
            console.log('FrontendBridge WebSocket 断开，将重连');
            this._ws = null;
            this._scheduleReconnect(port);
        };
        ws.onerror = (e) => {
            console.error('FrontendBridge WebSocket 错误:', e);
        };
        this._ws = ws;
    },

    _scheduleReconnect(port) {
        if (this._reconnectTimer) return;
        this._reconnectDelay = Math.min(this._reconnectDelay * 2, 5000);
        this._reconnectTimer = setTimeout(() => {
            this._reconnectTimer = null;
            this._connect(port);
        }, this._reconnectDelay);
    },

    // 分发请求：查找 handler → 执行（同步或异步）→ 回传结果
    _dispatch({ request_id, tool, action, args }) {
        console.log('FrontendBridge 收到请求:', { request_id, tool, action, args });
        const reply = (ok, message) => {
            if (!this._ws || this._ws.readyState !== WebSocket.OPEN) return;
            this._ws.send(JSON.stringify({
                type: 'result',
                request_id,
                ok,
                message: message == null ? 'ok' : String(message)
            }));
        };
        const handler = this._handlers[tool];
        if (!handler) {
            reply(false, `未注册的业务处理器: ${tool}`);
            return;
        }
        let result;
        try {
            result = handler(action, args || {});
        } catch (err) {
            reply(false, String((err && err.message) || err));
            return;
        }
        if (result && typeof result.then === 'function') {
            result.then(
                msg => reply(true, msg == null ? 'ok' : String(msg)),
                err => reply(false, String((err && err.message) || err))
            );
        } else {
            reply(true, result == null ? 'ok' : String(result));
        }
    }
};

// ── 番茄钟业务 handler：调用前端番茄钟方法（PomodoroService），单任务语义在业务层内部保证 ──
FrontendBridge.register('pomodoro', (action, args) => {
    const st = PomodoroService.state;
    switch (action) {
        case 'status': {
            const running = st.isRunning;
            const remain = running
                ? Math.max(0, Math.ceil((st.endAt - Date.now()) / 1000))
                : st.timeLeft;
            const mm = Math.floor(remain / 60);
            const ss = remain % 60;
            const modeName = st.mode === 'short' ? '短休息' : st.mode === 'long' ? '长休息' : '专注';
            // 四态判定：进行中 / 已停止（完成或剩余归零）/ 已暂停 / 未开始
            const phase = running
                ? '进行中'
                : (st.timeLeft <= 0)
                    ? '已停止'
                    : (st.timeLeft < st.totalTime)
                        ? '已暂停'
                        : '未开始';
            return `${modeName}${phase}：剩余 ${String(mm).padStart(2, '0')}:${String(ss).padStart(2, '0')}，总时长 ${st.totalTime / 60} 分钟`;
        }
        case 'start': {
            const isBreak = args.mode === 'break';
            const minutes = Math.max(1, Math.min(180, parseInt(args.minutes, 10) || (isBreak ? 5 : 25)));
            const mode = isBreak ? 'short' : 'pomodoro';
            PomodoroService.startWith(minutes, mode);
            return `已开始 ${minutes} 分钟${isBreak ? '休息' : '专注'}`;
        }
        case 'autoBreak': {
            const s = PomodoroStore.loadSettings() || { pomodoro: 25, short: 5, long: 15, autoBreak: true, autoPomodoro: false };
            s.autoBreak = !!args.openEnable;
            PomodoroStore.saveSettings(s);
            const el = document.getElementById('pomoAutoBreak');
            if (el) el.classList.toggle('on', s.autoBreak);
            return `已${s.autoBreak ? '开启' : '关闭'}自动开始休息`;
        }
        case 'autoFocus': {
            const s = PomodoroStore.loadSettings() || { pomodoro: 25, short: 5, long: 15, autoBreak: true, autoPomodoro: false };
            s.autoPomodoro = !!args.openEnable;
            PomodoroStore.saveSettings(s);
            const el = document.getElementById('pomoAutoPomodoro');
            if (el) el.classList.toggle('on', s.autoPomodoro);
            return `已${s.autoPomodoro ? '开启' : '关闭'}自动开始专注`;
        }
        case 'stop': {
            if (!st.isRunning && st.timeLeft >= st.totalTime) return '当前没有进行中的计时';
            PomodoroService.reset();
            return '已停止番茄钟';
        }
        default:
            return `未知动作: ${action}`;
    }
});

// ── RAW 照片业务 handler：调用 mdgo.core.raw.parse，数据经文件路径传递 ──
// 协议：args {action: 'parse', path: 知识库内 RAW 文件相对路径} → 返回 Markdown 字符串
// （三大类「相机 · 镜头 / 拍摄参数 / 图像信息」中文参数名，值格式与页面 RAW 查看器一致，每类一行压缩 token）。
// 大文件（RAW 可达 200MB）不适合经 WebSocket JSON 传 base64，故 Rust 侧只传 path，
// 由前端 readFileAsBase64（Tauri 走 read_file_binary invoke）读取后本地解析。
// 工具名与 Rust 侧 build_raw_tool / SKILL.md 保持一致（raw-photography）。
FrontendBridge.register('raw-photography', async (action, args) => {
    const path = (args && args.path) || '';
    if (!path) return '解析失败：缺少参数 path（RAW 文件路径）';
    if (action !== 'parse') return `未知动作: ${action}`;
    try{
        const dataUrl = await readFileAsBase64(path);
        if (!dataUrl) return `解析失败：无法读取文件 ${path}`;
        // data URL → ArrayBuffer
        let bytes;
        try {
            const bin = atob(String(dataUrl).split(',')[1] || '');
            bytes = new Uint8Array(bin.length);
            for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
        } catch (e) {
            return '解析失败：base64 解码错误';
        }
        const parsed = mdgo.core.raw.parse(bytes.buffer);
        if (!parsed.ok) return '解析失败：' + (parsed.error || '无法解析 TIFF 结构');
        return parsed.markdown || '解析成功但无可用元数据';
    }catch(e){
        return '解析失败：' + (e.message || e);
    }
});
