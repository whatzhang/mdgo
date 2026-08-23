/**
 * ===== 图谱应用组装（css_js/graph/graph-app.js） =====
 * 【职责】Composition Root：加载引擎 → 依赖注入组装 → 启动 → 暴露 destroyCommon。
 *        唯一组装点（S）；模块间全部经此处注入，互不直接 require。
 *
 * 【iframe 契约】本文件由 graph.html 以 <script type="module"> 加载：
 *   - 必须暴露 window.destroyCommon()：主页面 cleanupCommonIframe() 切换视图时调用
 *   - 上下文：读取 window.frameElement?.dataset（{ dirPath, focusNodeId }）
 *   - 通信：postMessage 协议（graph:ready / graph:focus-request / graph:refresh / graph:open-node）
 *
 * 【默认流程】（下一代 AI 知识图谱 PRD §66）：
 *   Loading → 读取 graph stats → 加载 clusters → 显示 Cluster（全局视图 L0）
 *   → 用户点击簇展开成员 → LOD 随缩放细化。
 *
 * 【引擎懒加载】动态 import() 单文件 ESM bundle（Sigma v3 + graphology + forceAtlas2）：
 *   - 路径：../cdn/sigma/sigma.bundle.js（随 css_js/** 分发；由 tauri/scripts/build-sigma.mjs 打包）
 *   - 缺失时降级：renderer 显示占位提示，其余模块（面板/桥）仍可用
 */
(async function () {
    'use strict';

    /** 引擎 ESM bundle 路径（相对本文件：css_js/graph/ → css_js/cdn/sigma/） */
    const SIGMA_BUNDLE = '../cdn/sigma/sigma.bundle.js';

    /** 应用实例（destroyCommon / 调试用） */
    const app = {
        store: null,
        api: null,
        renderer: null,
        layout: null,
        interaction: null,
        panel: null,
        views: null,
        minimap: null,
        bridge: null,
        _inited: false,
        _destroyed: false,
    };

    /** 获取 Tauri invoke：同源 iframe 复用主页面 __TAURI__；浏览器模式为 null */
    function resolveInvoke() {
        try {
            const parentWin = window.parent;
            const tauri = parentWin && parentWin.__TAURI__;
            if (tauri && typeof tauri.core?.invoke === 'function') {
                return tauri.core.invoke.bind(tauri.core);
            }
        } catch (e) { /* 跨源访问失败 → 非 Tauri */ }
        return null;
    }

    /** 读取 iframe dataset 上下文（主页面 graph-bridge.js 写入） */
    function readContext() {
        const ctx = { dirPath: '', focusNodeId: null };
        try {
            const frame = window.frameElement;
            if (frame && frame.dataset) {
                ctx.dirPath = frame.dataset.dirPath || '';
                ctx.focusNodeId = frame.dataset.focusNodeId || null;
            }
        } catch (e) { /* 非 iframe 环境 */ }
        return ctx;
    }

    /** 懒加载 Sigma 引擎（缺失抛错，由调用方降级） */
    async function loadEngine() {
        try {
            const mod = await import(SIGMA_BUNDLE);
            return {
                Sigma: mod.Sigma || null,
                Graph: mod.Graph || mod.default || null,
                forceAtlas2: mod.forceAtlas2 || null,
            };
        } catch (e) {
            console.warn('[graph-app] Sigma bundle 加载失败（将降级为占位提示）:', e);
            return { Sigma: null, Graph: null, forceAtlas2: null };
        }
    }

    /** 组装全部模块（依赖注入） */
    function compose(engine, invoke, ctx) {
        const model = window.GraphModel;
        const store = new window.GraphStore({ model });
        const api = new window.GraphApiClient({ invoke });
        const renderer = new window.GraphRenderer({
            store,
            container: document.getElementById('kg-canvas'),
            sigmaFactory: engine.Sigma ? () => ({ Sigma: engine.Sigma, Graph: engine.Graph }) : null,
        });
        const layout = new window.GraphLayout({
            engine: engine.forceAtlas2 ? { forceAtlas2: engine.forceAtlas2 } : null,
            Graph: engine.Graph || null, // force 布局需要 graphology 真实实例
        });
        const panel = new window.GraphPanel({ store, interaction: null, api, model });
        const interaction = new window.GraphInteraction({ store, api, renderer, panel, layout, model });
        panel.interaction = interaction; // 循环依赖：panel 需要 interaction，组装时后置注入
        const views = new window.GraphViews({ store, api, renderer, layout, interaction, panel });
        panel.views = views;   // 刷新按钮 → 重跑当前视图

        // 装配到 app 实例
        Object.assign(app, { store, api, renderer, layout, interaction, panel, views });
        return app;
    }

    /** 启动流程（PRD §66 默认：状态 → 聚类 → 全局视图） */
    async function boot() {
        const ctx = readContext();
        const invoke = resolveInvoke();

        // 1. 引擎加载（失败不阻断组装）
        const engine = await loadEngine().catch((e) => {
            console.warn('[graph-app] 引擎加载失败（将降级为占位提示）:', e);
            return { Sigma: null, Graph: null, forceAtlas2: null };
        });

        // 2. 组装
        const appInst = compose(engine, invoke, ctx);
        appInst.store.set({
            dirPath: ctx.dirPath,
            engineReady: !!(engine.Sigma && engine.Graph),
        });

        // 3. 渲染器挂载（引擎缺失 → 占位提示）
        const ok = appInst.renderer.mount();

        // 4. 面板 + 视图 + 交互挂载
        appInst.panel.init();
        appInst.views.init();
        if (ok) {
            appInst.interaction.mount();
        }

        // 5. 构建状态 + 图版本（PRD §66/§73）
        const status = await appInst.api.status(appInst.store.state.dirPath).catch(() => null);
        if (status) {
            appInst.store.set({
                buildStatus: status,
                graphVersion: status.graph_version ?? appInst.store.state.graphVersion,
            });
        }

        // 6. 默认视图：全局视图（L0 聚类；无图谱 → 空态引导）
        if (ok) {
            await appInst.views.switchView('global');
        } else {
            appInst.views.switchView('global').catch(() => { /* 引擎缺失也展示空态/状态 */ });
        }

        // 7. 焦点节点（主页面 graph:focus-request 场景）：定位到文档关系视图并聚焦
        if (ctx.focusNodeId) {
            await appInst.views.switchView('document');
            setTimeout(() => appInst.interaction.onNodeClick(ctx.focusNodeId), 150);
        }

        // 8. 就绪通知 + 桥接监听
        appInst._inited = true;
        notifyParent({ type: 'graph:ready', payload: { ok: true } });
        listenParentMessages(appInst);
    }

    /** 通知主页面（postMessage） */
    function notifyParent(msg) {
        try {
            if (window.parent && window.parent !== window) {
                window.parent.postMessage(msg, '*');
            }
        } catch (e) { /* ignore */ }
    }

    /** 监听主页面消息（graph:focus-request / graph:refresh） */
    function listenParentMessages(appInst) {
        const handler = (event) => {
            const msg = event.data;
            if (!msg || typeof msg.type !== 'string') return;
            switch (msg.type) {
                case 'graph:focus-request': {
                    const nodeId = msg.payload?.nodeId;
                    if (nodeId && appInst.renderer.ready) {
                        appInst.views.switchView('document').then(() => {
                            setTimeout(() => appInst.interaction.onNodeClick(nodeId), 120);
                        });
                    }
                    break;
                }
                case 'graph:refresh': {
                    // watcher 变更 → 图版本可能变化：刷新当前视图
                    appInst.api.version(appInst.store.state.dirPath).then((v) => {
                        if (typeof v === 'number' && v !== appInst.store.state.graphVersion) {
                            appInst.store.set({ graphVersion: v });
                            appInst.views.switchView(appInst.store.state.view);
                        }
                    }).catch(() => { /* ignore */ });
                    break;
                }
                default:
                    break;
            }
        };
        window.addEventListener('message', handler);
        app._messageHandler = handler;
    }

    /**
     * 销毁（iframe 契约）：主页面 cleanupCommonIframe() 调用。
     * 反向销毁：views → minimap → interaction → renderer → store → 监听。
     */
    function destroyCommon() {
        if (app._destroyed) return;
        app._destroyed = true;
        if (app.interaction) app.interaction.dispose();
        if (app.panel) app.panel.dispose();
        if (app.renderer) app.renderer.destroy();
        if (app.store) app.store.dispose();
        if (app._messageHandler) {
            window.removeEventListener('message', app._messageHandler);
            app._messageHandler = null;
        }
        app._inited = false;
        console.log('[graph-app] 已销毁（destroyCommon）');
    }

    // ─── iframe 契约暴露 ───
    window.destroyCommon = destroyCommon;
    // 调试入口
    window.__graphApp = app;

    // ─── 启动 ───
    boot().catch((e) => {
        console.error('[graph-app] 启动失败:', e);
        const el = document.getElementById('kg-canvas-placeholder');
        if (el) {
            el.innerHTML = '<div class="kg-placeholder-text">图谱启动失败: ' + String(e).replace(/</g, '&lt;') + '</div>';
            el.style.display = 'flex';
        }
    });
})();
