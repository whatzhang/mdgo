/**
 * ===== 图谱主页面桥接（css_js/modules/graph-bridge.js） =====
 * 【职责】主页面（main.html）与 graph.html iframe 之间的唯一桥接点：
 *        打开图谱页、写入 dataset 上下文、转发 postMessage（打开文件/聚焦/刷新）。
 *        主页面侧最小侵入：仅需在 toggleFile 增加 'graphOS' 分支调用 window.graphBridge.open()。
 *
 * 【时序要点】switchToView() 内部会调用 destroyEverything() → cleanupCommonIframe()，
 *        后者会清空 common-iframe 的 src 并调用旧页面 destroyCommon()。
 *        因此必须：① 先写 dataset → ② switchToView（清空旧 src）→ ③ 再设 src 加载图谱页。
 *        每次进入都重新加载（与主页面其它视图「每次进入重新初始化」行为一致）。
 *
 * 【依赖的全局服务】（来自 main.html 主脚本，运行时注入，延迟取值）
 *   - common-iframe / iframe-common-container（通用 iframe 容器，MAIN_VIEW_CONTAINERS 成员）
 *   - switchToView(container, displayValue)
 *   - toggleFile(type, data)（打开文件复用现有链路）
 *
 * 【与 iframe 契约对齐】见 docs/graph-os-frontend-design.md §一/§五。
 */
(function () {
    'use strict';

    /** graph.html 相对主页面（vite root='..'，dist 根）的路径 */
    const GRAPH_PAGE_SRC = './css_js/graph/graph.html';

    let _messageBound = false;

    /** 延迟获取 iframe（main.html 主脚本先执行，本模块后加载） */
    function getIframe() {
        return document.getElementById('common-iframe');
    }
    function getContainer() {
        return document.getElementById('iframe-common-container');
    }

    /**
     * 打开图谱页（每次进入重新加载，与主页面其它视图行为一致）。
     * @param {string} dirPath 知识库目录（绝对路径）
     * @param {{ focusNodeId?: string }} [opts]
     */
    async function open(dirPath, opts = {}) {
        const container = getContainer();
        const iframe = getIframe();
        if (!container || !iframe) {
            console.warn('[graph-bridge] iframe 容器不存在，图谱无法打开');
            return;
        }

        // ① 先写上下文（graph.html 启动时从 frameElement.dataset 读取；switchToView 不清 dataset）
        iframe.dataset.dirPath = dirPath || '';
        iframe.dataset.focusNodeId = opts.focusNodeId || '';

        // ② 切换视图（内部 destroyEverything → cleanupCommonIframe 清空旧 src 并调用旧 destroyCommon）
        await switchToView(container, 'flex');

        // ③ 再设置 src 加载图谱页（避免被 ② 的清空逻辑覆盖）
        iframe.src = GRAPH_PAGE_SRC;

        bindMessages();
    }

    /**
     * 关闭图谱页（幂等）。通常由主页面 destroyEverything → cleanupCommonIframe()
     * 自动调用 iframe.contentWindow.destroyCommon()，本方法仅为显式入口。
     */
    async function close() {
        const iframe = getIframe();
        if (iframe && iframe.src) {
            try {
                if (iframe.contentWindow && iframe.contentWindow.destroyCommon) {
                    iframe.contentWindow.destroyCommon();
                }
            } catch (e) { /* 跨源或未加载忽略 */ }
        }
    }

    /** 主页面 → iframe：聚焦指定节点 */
    function focusNode(nodeId) {
        const iframe = getIframe();
        if (iframe && iframe.contentWindow) {
            try {
                iframe.contentWindow.postMessage({ type: 'graph:focus-request', payload: { nodeId } }, '*');
            } catch (e) { /* ignore */ }
        }
    }

    /** 主页面 → iframe：提示刷新图谱（watcher 事件后调用） */
    function refresh() {
        const iframe = getIframe();
        if (iframe && iframe.contentWindow) {
            try {
                iframe.contentWindow.postMessage({ type: 'graph:refresh', payload: {} }, '*');
            } catch (e) { /* ignore */ }
        }
    }

    /** 绑定一次 iframe → 主页面消息监听（幂等） */
    function bindMessages() {
        if (_messageBound) return;
        _messageBound = true;
        window.addEventListener('message', (event) => {
            const msg = event.data;
            if (!msg || typeof msg.type !== 'string') return;
            switch (msg.type) {
                case 'graph:ready':
                    // 图谱页就绪（可更新菜单态/状态栏；骨架阶段仅日志）
                    console.log('[graph-bridge] 图谱页就绪:', msg.payload);
                    break;
                case 'graph:open-node': {
                    // 用户点击文件节点 → 复用主页面打开文件链路
                    const path = msg.payload && msg.payload.path;
                    if (path && typeof window.openFileFromPath === 'function') {
                        window.openFileFromPath(path);
                    }
                    break;
                }
                default:
                    break;
            }
        });
    }

    // ─── 对外暴露（模块惯例：window 单例） ───
    window.graphBridge = {
        open,
        close,
        focusNode,
        refresh,
    };
    console.log('[graph-bridge] 图谱桥接就绪');
})();
