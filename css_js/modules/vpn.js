/**
 * ===== VPN 视图（css_js/modules/vpn.js）=====
 *
 * 【定位】mdgo 主内容区的原生独立视图（不是 iframe）：
 *   - 入口：仪表盘「VPN」按钮（main.html → toggleFile('vpn') → openVpnPage()）
 *   - 挂载：window.vpnApp.mount(#vpn-container)，卸载：window.vpnApp.destroy()
 *
 * 【与其它模块保持一致】
 *   - 所有类名/ID 使用 `vpn-` 前缀，JS 常量使用 `VPN_`、函数使用 `vpn` 前缀；
 *   - 颜色/圆角/阴影复用 :root 设计变量，按钮复用 .btn，输入复用 .setting-text，
 *     弹窗复用 .setting-modal*，通知复用宿主 showNotification；
 *   - 直接调用 window.__TAURI__.core.invoke / event.listen（Tauri 适配层异步注入，调用前等待）。
 *
 * 【依赖后端】tauri/src-tauri/src/commands/vpn.rs（29 个命令）+ core/vpn 模块。
 */
(function () {
    'use strict';

    // ─────────────────────────── 常量 ───────────────────────────
    const VPN_VERSION = '1.0.0';
    const VPN_TABS = [
        ['proxies', '代理', 'M4 7h16M4 12h10M4 17h7'],
        ['profiles', '订阅', 'M12 3v12m0 0l4-4m-4 4l-4-4M4 17v3h16v-3'],
        ['rules', '规则', 'M5 6h14M5 12h14M5 18h9'],
        ['connections', '连接', 'M8 12a4 4 0 014-4h4a4 4 0 010 8h-1M16 12a4 4 0 01-4 4H8a4 4 0 010-8h1'],
        ['logs', '日志', 'M6 4h12v16H6zM9 9h6M9 13h6M9 17h4'],
        ['settings', '设置', 'M12 15a3 3 0 100-6 3 3 0 000 6zM19.4 15a1.7 1.7 0 00.3 1.9l.1.1a2 2 0 11-2.8 2.8l-.1-.1a1.7 1.7 0 00-1.9-.3 1.7 1.7 0 00-1 1.5V21a2 2 0 11-4 0v-.1a1.7 1.7 0 00-1.1-1.5 1.7 1.7 0 00-1.9.3l-.1.1a2 2 0 11-2.8-2.8l.1-.1a1.7 1.7 0 00.3-1.9 1.7 1.7 0 00-1.5-1H3a2 2 0 110-4h.1a1.7 1.7 0 001.5-1.1 1.7 1.7 0 00-.3-1.9l-.1-.1a2 2 0 112.8-2.8l.1.1a1.7 1.7 0 001.9.3H9a1.7 1.7 0 001-1.5V3a2 2 0 114 0v.1a1.7 1.7 0 001 1.5 1.7 1.7 0 001.9-.3l.1-.1a2 2 0 112.8 2.8l-.1.1a1.7 1.7 0 00-.3 1.9V9a1.7 1.7 0 001.5 1H21a2 2 0 110 4h-.1a1.7 1.7 0 00-1.5 1z'],
    ];
    const VPN_EVENTS = [
        'vpn://state', 'vpn://progress', 'vpn://traffic', 'vpn://log',
        'vpn://diag', 'vpn://profiles', 'vpn://network', 'vpn://notice',
    ];
    const VPN_EMPTY_FILL = '<div class="vpn-muted vpn-small" style="padding:16px">暂无数据</div>';

    // ─────────────────────────── 状态 ───────────────────────────
    const vpnState = {
        state: null,
        settings: null,
        profiles: [],
        activeId: '',
        proxies: [],
        rules: [],
        conns: { connections: [], upload: 0, download: 0 },
        logs: [],
        diag: null,
        tab: 'proxies',
        groupIdx: 0,
        pickerGroup: null,
        testingGroup: '',
        nodeQuery: '',
        nodeSort: 'default',
        nodeLimit: 120,
        logsPaused: false,
        logsLevel: 'all',
        logsQuery: '',
        ruleQuery: '',
        connQuery: '',
        progress: '',
        traffic: { up: 0, down: 0, history: [] },
        modal: '',
        modalData: null,
        busy: false,
    };

    // ─────────────────────────── 运行时句柄 ───────────────────────────
    let vpnRoot = null;
    let vpnMounted = false;
    let vpnToken = 0;
    let vpnUnlisten = [];
    let vpnTimers = [];


    // ─────────────────────────── 基础工具 ───────────────────────────
    const vpnSleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

    function vpnEsc(value) {
        return String(value == null ? '' : value).replace(/[&<>"']/g, (ch) => ({
            '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
        }[ch]));
    }

    function vpnIcon(path, size) {
        const s = size || 15;
        return `<svg viewBox="0 0 24 24" width="${s}" height="${s}" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="${path}"/></svg>`;
    }

    /** 统一标签：中性不带点；状态类带 5px 圆点。tone: ok / warn / bad / info */
    function vpnTag(text, tone) {
        const cls = tone ? ' is-' + tone : '';
        const dot = (tone === 'ok' || tone === 'warn' || tone === 'bad') ? '<i class="vpn-dot"></i>' : '';
        return `<span class="vpn-tag${cls}">${dot}${vpnEsc(text)}</span>`;
    }

    function vpnEl(id) {
        return vpnRoot ? vpnRoot.querySelector('#' + id) : null;
    }

    /** 解析节点名开头的国旗 emoji，返回 ISO 代码与去掉国旗后的国名。 */
    function vpnNodeParts(name) {
        const text = String(name == null ? '' : name);
        const match = text.match(/^([\u{1F1E6}-\u{1F1FF}]{2})\s*/u);
        if (!match) return { code: '', text };
        const code = Array.from(match[1]).map((ch) => String.fromCharCode(ch.codePointAt(0) - 0x1F1E6 + 65)).join('');
        return { code, text: text.slice(match[0].length) || text };
    }

    function vpnStripFlag(name) {
        return vpnNodeParts(name).text;
    }

    function vpnFmtBytes(n) {
        const v = Number(n) || 0;
        if (v < 1024) return v + ' B';
        if (v < 1048576) return (v / 1024).toFixed(1) + ' KB';
        if (v < 1073741824) return (v / 1048576).toFixed(1) + ' MB';
        return (v / 1073741824).toFixed(2) + ' GB';
    }

    function vpnFmtRate(n) {
        const v = Number(n) || 0;
        if (v < 1024) return v + ' B/s';
        if (v < 1048576) return (v / 1024).toFixed(1) + ' KB/s';
        return (v / 1048576).toFixed(2) + ' MB/s';
    }

    function vpnFmtTime(ts) {
        if (!ts) return '—';
        const d = new Date(ts * 1000);
        const p = (x) => String(x).padStart(2, '0');
        return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
    }

    function vpnFmtExpire(ts) {
        if (!ts) return '—';
        const days = Math.ceil((ts * 1000 - Date.now()) / 86400000);
        if (days < 0) return '已过期';
        if (days <= 7) return days + ' 天后到期';
        const d = new Date(ts * 1000);
        return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
    }

    function getDomain(url) {
        if (typeof url !== 'string' || !url.trim()) return '';
        let input = url.trim();
        if (!/^[a-zA-Z][a-zA-Z\d+\-.]*:\/\//.test(input)) {
            input = 'http://' + input;
        }
        try {
            const u = new URL(input);
            return u.hostname || '';
        } catch {
            return '';
        }
    }

    /** 统一走宿主已有的右下角气泡通知（Notify/showNotification）。 */
    function vpnToast(message, type) {
        const text = String(message || '');
        const tone = type || 'info';
        if (typeof window.showNotification === 'function') {
            window.showNotification(text, tone);
            return;
        }
        if (window.Notify && typeof window.Notify.show === 'function') {
            window.Notify.show(text, tone);
            return;
        }
        console.warn('[vpn]', text);
    }

    // ─────────────────────────── Tauri 调用（适配层异步注入） ───────────────────────────
    async function vpnTauri(timeout) {
        const limit = timeout || 10000;
        const start = Date.now();
        while (Date.now() - start < limit) {
            const tauri = window.__TAURI__;
            if (tauri && tauri.core && tauri.core.invoke) return tauri;
            await vpnSleep(100);
        }
        throw new Error('Tauri API 尚未就绪，请稍后重试');
    }

    async function vpnInvoke(cmd, payload, timeout) {
        const tauri = await vpnTauri();
        const limit = timeout || 20000;
        return Promise.race([
            tauri.core.invoke(cmd, payload || {}),
            new Promise((_, reject) => setTimeout(() => reject(new Error('请求超时: ' + cmd)), limit)),
        ]);
    }

    async function vpnSubscribe(token) {
        const tauri = await vpnTauri();
        if (!tauri.event || !tauri.event.listen) return;
        const unlisteners = [];
        for (const eventName of VPN_EVENTS) {
            try {
                const un = await tauri.event.listen(eventName, (event) => {
                    if (vpnToken === token) vpnOnEvent(eventName, event.payload);
                });
                unlisteners.push(un);
            } catch (e) {
                console.warn('[vpn] 事件订阅失败:', eventName, e);
            }
        }
        if (vpnToken !== token) {
            unlisteners.forEach((fn) => { try { fn(); } catch (e) { /* ignore */ } });
            return;
        }
        vpnUnlisten.push(...unlisteners);
    }

    // ─────────────────────────── 模板 ───────────────────────────
    function vpnTemplate() {
        return `
            <header class="vpn-statusbar" id="vpn-statusbar"></header>
            <nav class="vpn-tabs" id="vpn-tabs"></nav>
            <main class="vpn-body" id="vpn-body"></main>
            <div id="vpn-layer"></div>`;
    }

    // ─────────────────────────── 事件分发 ───────────────────────────
    function vpnBindEvents() {
        vpnRoot.addEventListener('click', vpnOnClick);
        vpnRoot.addEventListener('input', vpnOnInput);
        vpnRoot.addEventListener('change', vpnOnChange);
    }

    /** select 类控件（日志级别）走 change 委托。 */
    function vpnOnChange(event) {
        const target = event.target;
        if (!target || !target.id) return;
        if (target.id === 'vpn-log-level') {
            vpnState.logsLevel = target.value;
            vpnRender();
        }
    }

    function vpnOnClick(event) {
        const tabBtn = event.target.closest('[data-vpn-tab]');
        if (tabBtn && vpnRoot.contains(tabBtn)) {
            vpnSwitchTab(tabBtn.getAttribute('data-vpn-tab'));
            return;
        }
        const groupItem = event.target.closest('[data-vpn-groupidx]');
        if (groupItem && vpnRoot.contains(groupItem)) {
            vpnState.groupIdx = Number(groupItem.getAttribute('data-vpn-groupidx')) || 0;
            vpnState.pickerGroup = (vpnState.proxies[vpnState.groupIdx] || {}).name || null;
            vpnState.nodeLimit = 120;
            vpnRender();
            return;
        }
        const node = event.target.closest('[data-vpn-node]');
        if (node && vpnRoot.contains(node)) {
            vpnSelectNode(node.getAttribute('data-vpn-nodegroup'), node.getAttribute('data-vpn-node'));
            return;
        }
        const toggle = event.target.closest('[data-vpn-toggle]');
        if (toggle && vpnRoot.contains(toggle)) {
            vpnToggleSetting(toggle.getAttribute('data-vpn-toggle'), toggle);
            return;
        }
        const action = event.target.closest('[data-vpn-act]');
        if (action && vpnRoot.contains(action)) {
            vpnOnAction(action.getAttribute('data-vpn-act'), action);
            return;
        }
        const overlay = event.target.closest('[data-vpn-close]');
        if (overlay && event.target === overlay) vpnCloseLayer();
    }

    function vpnOnInput(event) {
        const target = event.target;
        if (!target || !target.id) return;
        switch (target.id) {
            case 'vpn-node-search':
                vpnState.nodeQuery = target.value;
                clearTimeout(target._vpnTimer);
                target._vpnTimer = setTimeout(() => vpnRefreshNodes(), 120);
                break;
            case 'vpn-rule-search':
                vpnState.ruleQuery = target.value;
                vpnRefreshRuleRows();
                break;
            case 'vpn-conn-search':
                vpnState.connQuery = target.value;
                vpnRefreshConnRows();
                break;
            case 'vpn-log-search':
                vpnState.logsQuery = target.value;
                vpnRefreshLogList();
                break;
            default:
                break;
        }
    }

    // ─────────────────────────── 顶部状态条 ───────────────────────────
    function vpnStatusModel() {
        const s = vpnState.state || {};
        const core = s.core || { state: 'stopped', message: '' };
        const proxy = s.system_proxy || { state: 'disabled' };
        const conn = s.connection || { state: 'disconnected', step: '' };
        const coreCls = core.state === 'running' ? 'is-ok' : core.state === 'error' ? 'is-bad'
            : (core.state === 'starting' || core.state === 'stopping') ? 'is-warn' : '';
        const coreText = { running: '运行中', starting: '启动中', stopping: '停止中', error: '异常', stopped: '未运行' }[core.state] || '未运行';
        const sysCls = proxy.state === 'enabled' ? 'is-ok' : proxy.state === 'error' ? 'is-bad'
            : (proxy.state === 'enabling' || proxy.state === 'restoring') ? 'is-warn' : '';
        const sysText = { enabled: '已启用', enabling: '启用中', restoring: '恢复中', error: '异常', disabled: '未启用' }[proxy.state] || '未启用';
        const powerOn = conn.state === 'connected' || conn.state === 'degraded';
        const powerBusy = !!vpnState.busy || conn.state === 'connecting';
        const stateText = vpnState.busy ? (vpnState.progress || '处理中…')
            : powerBusy ? (vpnState.progress || conn.step || '连接中…') : powerOn ? '已开启' : '已关闭';
        const ports = s.ports || {};
        const coreTitle = `内核：${coreText}${core.version ? ' · ' + core.version : ''}${core.pid ? ' · PID ' + core.pid : ''} · 混合端口 ${ports.mixed || '—'}`;
        const proxyTitle = `系统代理：${sysText}${proxy.detail ? ' · ' + proxy.detail : ''}${ports.mixed ? ' · 127.0.0.1:' + ports.mixed : ''}`;
        const powerTitle = powerBusy ? stateText : powerOn ? '代理已开启' : '代理已关闭';
        const groups = vpnState.proxies;
        let node = '—';
        if (groups.length) {
            const group = groups.find((g) => (g.nodes || []).some((n) => n.name === g.now)) || groups[0];
            const hit = (group.nodes || []).find((n) => n.name === group.now);
            node = (vpnStripFlag(group.now) || '—') + (hit && hit.delay ? ' · ' + hit.delay + 'ms' : '');
        }
        const mode = (s.mode) || (vpnState.settings && vpnState.settings.mode) || 'rule';
        return { powerOn, powerBusy, stateText, powerTitle, coreCls, coreText, coreTitle, sysCls, sysText, proxyTitle, node, mode };
    }

    function vpnStatusTipRows(m) {
        return `
            <div class="vpn-tip-row"><span class="vpn-tip-k">代理</span><span class="vpn-tip-v ${m.powerOn ? 'is-ok' : ''}">${vpnEsc(m.powerOn ? '已开启' : '已关闭')}</span></div>
            <div class="vpn-tip-row"><span class="vpn-tip-k">内核</span><span class="vpn-tip-v">${vpnEsc(m.coreTitle.replace(/^内核：/, ''))}</span></div>
            <div class="vpn-tip-row"><span class="vpn-tip-k">系统代理</span><span class="vpn-tip-v">${vpnEsc(m.proxyTitle.replace(/^系统代理：/, ''))}</span></div>
            <div class="vpn-tip-hint"></div>`;
    }

    /** 当前节点所在的策略组（找不到时取第一个组）。 */
    function vpnCurrentGroup() {
        const groups = vpnState.proxies;
        return groups.find((group) => (group.nodes || []).some((node) => node.name === group.now)) || groups[0] || null;
    }

    function vpnSortedNodes(group) {
        const nodes = (group.nodes || []).slice();
        nodes.sort((a, b) => vpnDelayRank(a) - vpnDelayRank(b) || String(a.name).localeCompare(String(b.name)));
        return nodes;
    }

    function vpnPickerGroup() {
        const groups = vpnState.proxies;
        return groups.find((item) => item.name === vpnState.pickerGroup) || vpnCurrentGroup() || groups[0] || null;
    }

    function vpnNodePickerHtml() {
        const groups = vpnState.proxies;
        if (!groups.length) return '<div class="vpn-picker-empty">暂无节点</div>';
        const group = vpnPickerGroup();
        if (!group) return '<div class="vpn-picker-empty">暂无节点</div>';
        const nodes = vpnSortedNodes(group);
        const rows = nodes.length ? nodes.map((node) => {
            const parts = vpnNodeParts(node.name);
            return `<button class="vpn-picker-node ${node.name === group.now ? 'is-active' : ''}" data-vpn-node="${vpnEsc(node.name)}" data-vpn-nodegroup="${vpnEsc(group.name)}" title="${vpnEsc(parts.text)}">
                ${parts.code ? `<span class="vpn-flag" title="${vpnEsc(parts.code)}">${parts.code}</span>` : ''}
                <span class="vpn-picker-name">${vpnEsc(parts.text)}</span>
                ${vpnLatChip(node)}
                <span class="vpn-picker-check">${vpnIcon('M5 12l5 5 9-9', 13)}</span>
            </button>`;
        }).join('') : '<div class="vpn-picker-empty">该策略组暂无节点</div>';
        const busy = Boolean(vpnState.testingGroup);
        const testing = vpnState.testingGroup === group.name;
        return `<div class="vpn-picker-head"><span>选择节点</span><span class="vpn-hint">按延迟排序</span></div>
            <div class="vpn-picker-groups">
                ${groups.map((item) => `<button class="vpn-picker-group ${item.name === group.name ? 'is-active' : ''}" data-vpn-act="pick-group" data-vpn-group="${vpnEsc(item.name)}">${vpnEsc(item.name)}</button>`).join('')}
            </div>
            <div class="vpn-picker-tools">
                <span class="vpn-picker-count">${nodes.length} 个节点</span>
                <button class="btn btn-primary btn-sm vpn-picker-test" data-vpn-act="test-picker" ${busy ? 'disabled' : ''}>${testing ? '测速中…' : '测速本组'}</button>
            </div>
            <div class="vpn-picker-list">${rows}</div>`;
    }

    function vpnRefreshNodePicker() {
        const panel = vpnEl('vpn-node-picker');
        if (panel) panel.innerHTML = vpnNodePickerHtml();
    }

    function vpnRenderStatus() {
        const bar = vpnEl('vpn-statusbar');
        if (!bar) return;
        const m = vpnStatusModel();
        bar.innerHTML = `
            <div class="vpn-status-group">
                <button class="vpn-power ${m.powerOn ? 'is-on' : ''} ${m.powerBusy ? 'is-busy' : ''}" id="vpn-power" ${vpnState.busy ? 'disabled' : ''} aria-label="${vpnEsc(m.powerTitle)}">
                    <span class="vpn-switch ${m.powerOn ? 'on' : ''}"></span>
                    <span class="vpn-power-label">代理</span>
                    <span class="vpn-power-state">${vpnEsc(m.stateText)}</span>
                </button>
                <span class="vpn-vsep"></span>
                <div class="vpn-meta">
                    <span class="vpn-meta-item ${m.coreCls}"><i class="vpn-dot"></i>内核<b>${vpnEsc(m.coreText)}</b></span>
                    <span class="vpn-meta-item ${m.sysCls}"><i class="vpn-dot"></i>系统代理<b>${vpnEsc(m.sysText)}</b></span>
                </div>
                <div class="vpn-status-tip">${vpnStatusTipRows(m)}</div>
            </div>
            <div class="vpn-mode-seg ${vpnState.profiles.length ? '' : 'is-disabled'}">
                <button title="基于预设规则智能判断流量走向" data-vpn-act="mode" data-vpn-mode="rule" class="${m.mode === 'rule' ? 'active' : ''}">规则</button>
                <button title="都有流量均通过代理服务器" data-vpn-act="mode" data-vpn-mode="global" class="${m.mode === 'global' ? 'active' : ''}">全局</button>
                <button title="所有流量不经过代理节点，但经过内核转发连接目标服务器" data-vpn-act="mode" data-vpn-mode="direct" class="${m.mode === 'direct' ? 'active' : ''}">直连</button>
            </div>
            <div class="vpn-now-wrap">
                <span class="vpn-now" data-vpn-act="locate" title="点击到代理页定位当前节点；悬停选择节点">当前节点 <b>${vpnEsc(m.node)}</b>${vpnIcon('M9 6l6 6-6 6', 12)}</span>
                <div class="vpn-node-picker" id="vpn-node-picker">${vpnNodePickerHtml()}</div>
            </div>
            <div class="vpn-spacer"></div>
            <div class="vpn-rate">
                <span class="vpn-rate-up">↑ <b id="vpn-rate-up">${vpnFmtRate(vpnState.traffic.up)}</b></span>
                <span class="vpn-rate-down">↓ <b id="vpn-rate-down">${vpnFmtRate(vpnState.traffic.down)}</b></span>
            </div>
            <button class="btn btn-primary btn-sm" data-vpn-act="diag">诊断</button>`;
        const power = vpnEl('vpn-power');
        if (power) power.addEventListener('click', vpnToggleConnect);
    }

    function vpnUpdateRates() {
        const up = vpnEl('vpn-rate-up');
        const down = vpnEl('vpn-rate-down');
        if (!up || !down) return;
        up.textContent = vpnFmtRate(vpnState.traffic.up);
        down.textContent = vpnFmtRate(vpnState.traffic.down);
    }

    function vpnRenderTabs() {
        const tabs = vpnEl('vpn-tabs');
        if (!tabs) return;
        const empty = !vpnState.profiles.length;
        tabs.innerHTML = VPN_TABS.map(([id, name, iconPath]) => {
            const disabled = empty && id !== 'profiles' && id !== 'settings';
            return `<button class="vpn-tab ${vpnState.tab === id ? 'is-active' : ''}" data-vpn-tab="${id}" ${disabled ? 'disabled' : ''}>${vpnIcon(iconPath, 15)}<span>${name}</span></button>`;
        }).join('');
    }

    function vpnSwitchTab(tab) {
        vpnState.tab = tab;
        vpnState.modal = '';
        vpnRender();
        if (tab === 'proxies' && !vpnState.proxies.length) vpnLoadProxies().then(() => vpnRender());
        if (tab === 'rules' && !vpnState.rules.length) vpnLoadRules().then(() => vpnRender());
        if (tab === 'connections') vpnLoadConnections();
        if (tab === 'logs') vpnLoadLogs().then(() => vpnRender());
        if (tab === 'settings') vpnLoadSettings().then(() => { if (vpnState.tab === 'settings') vpnRender(); });
    }

    // ─────────────────────────── 总渲染 ───────────────────────────
    function vpnRender() {
        if (!vpnMounted || !vpnRoot) return;
        vpnRenderStatus();
        vpnRenderTabs();
        const body = vpnEl('vpn-body');
        if (!body) return;
        if (!vpnState.state) {
            body.innerHTML = `<div class="vpn-empty"><div class="vpn-empty-ic">${vpnIcon('M12 3v18M5 8l7-5 7 5v8l-7 5-7-5V8z', 28)}</div>
                <h2>正在读取状态…</h2><p>首次加载需要与后台建立连接。</p></div>`;
            vpnRenderLayer();
            return;
        }
        if (!vpnState.profiles.length && vpnState.tab !== 'settings') vpnState.tab = 'profiles';
        vpnRenderTabs();
        const renderer = {
            proxies: vpnRenderProxies,
            profiles: vpnRenderProfiles,
            rules: vpnRenderRules,
            connections: vpnRenderConnections,
            logs: vpnRenderLogs,
            settings: vpnRenderSettings,
        }[vpnState.tab] || vpnRenderProxies;
        body.innerHTML = renderer();
        vpnRenderLayer();
    }

    // ─────────────────────────── 空态 ───────────────────────────
    function vpnRenderEmpty() {
        return `<div class="vpn-empty">
            <div class="vpn-empty-ic">${vpnIcon('M12 3l7 3v6c0 4.4-2.9 7.4-7 9-4.1-1.6-7-4.6-7-9V6l7-3z M9 12l2 2 4-4', 28)}</div>
            <h2>还没有任何订阅</h2>
            <p>导入一份 Clash / mihomo 配置后即可开始使用。本模块只接管系统代理，不创建虚拟网卡。</p>
            <div class="vpn-empty-actions">
                <button class="btn btn-primary" data-vpn-act="import">导入订阅</button>
                <button class="btn btn-primary" data-vpn-act="import-file">导入本地 YAML</button>
            </div>
            <div class="vpn-empty-steps">
                <div class="vpn-empty-step"><b>① 导入</b><span>粘贴订阅链接，或选择本地 .yaml 文件</span></div>
                <div class="vpn-empty-step"><b>② 开启代理</b><span>启动内核并开启系统代理（macOS 需授权一次）</span></div>
                <div class="vpn-empty-step"><b>③ 选节点</b><span>在代理页切换、测速；顶部可随时诊断</span></div>
            </div>
        </div>`;
    }

    // ─────────────────────────── 代理页 ───────────────────────────
    function vpnFilteredNodes(group) {
        let list = (group.nodes || []).slice();
        const query = vpnState.nodeQuery.trim().toLowerCase();
        if (query) list = list.filter((n) => (String(n.name) + ' ' + vpnNodeParts(n.name).code).toLowerCase().includes(query));
        if (vpnState.nodeSort === 'delay') {
            list.sort((a, b) => vpnDelayRank(a) - vpnDelayRank(b) || String(a.name).localeCompare(String(b.name)));
        }
        return list;
    }

    /** 排序用：超时/未测速/非数字延迟统一放到最后。 */
    function vpnDelayRank(node) {
        const value = Number(node.delay);
        if (!Number.isFinite(value) || value <= 0 || node.delay_state === 'timeout' || node.delay_state === 'untested') {
            return Number.MAX_SAFE_INTEGER;
        }
        return value;
    }

    function vpnLatChip(node) {
        if (node.delay_state === 'testing') return '<span class="vpn-lat is-testing">测速中…</span>';
        if (node.delay == null || node.delay_state === 'untested') return '<span class="vpn-lat is-untested">未测速</span>';
        if (node.delay === 0 || node.delay_state === 'timeout') return '<span class="vpn-lat is-timeout">超时</span>';
        if (node.delay < 150) return `<span class="vpn-lat is-fast">${node.delay}ms</span>`;
        if (node.delay < 300) return `<span class="vpn-lat is-slow">${node.delay}ms</span>`;
        return `<span class="vpn-lat is-timeout">${node.delay}ms</span>`;
    }

    function vpnNodeCard(node, group) {
        const parts = vpnNodeParts(node.name);
        return `<button class="vpn-node ${node.name === group.now ? 'is-active' : ''}" data-vpn-node="${vpnEsc(node.name)}" data-vpn-nodegroup="${vpnEsc(group.name)}">
            ${parts.code ? `<span class="vpn-flag" title="${vpnEsc(parts.code)}">${parts.code}</span>` : ''}
            <span class="vpn-node-name" title="${vpnEsc(parts.text)}">${vpnEsc(parts.text)}</span>
            ${vpnTag(node.type || node.kind || '')}
            ${vpnLatChip(node)}
            <span class="vpn-node-check">${vpnIcon('M5 12l5 5 9-9', 13)}</span>
        </button>`;
    }

    function vpnNodeFooter(group) {
        const total = vpnFilteredNodes(group).length;
        const shown = Math.min(total, vpnState.nodeLimit);
        return `已显示 ${shown} / ${total} 个节点${total > shown ? ' · <button class="btn btn-primary btn-sm" data-vpn-act="more">加载更多</button>' : ''}`;
    }

    function vpnRenderProxies() {
        if (!vpnState.profiles.length) return vpnRenderEmpty();
        const groups = vpnState.proxies;
        if (!groups.length) {
            return `<div class="vpn-card" style="padding:24px;text-align:center"><div class="vpn-muted">尚未读取到策略组。请先开启代理。</div></div>`;
        }
        vpnState.groupIdx = Math.min(vpnState.groupIdx, groups.length - 1);
        const group = groups[vpnState.groupIdx];
        const degraded = vpnState.state && vpnState.state.connection && vpnState.state.connection.state === 'degraded';
        const banner = degraded ? `<div class="vpn-banner">
            <div class="vpn-banner-body">
                <div class="vpn-banner-title">内核运行正常，但系统代理未生效</div>
                <div class="vpn-hint">${vpnEsc((vpnState.state.connection.reasons || []).join('；') || '可能在系统里被手动关闭，或网络切换后被重置。')}</div>
            </div>
            <button class="btn btn-sm btn-primary" data-vpn-act="sysproxy-on">重新开启</button>
        </div>` : '';
        const nodes = vpnFilteredNodes(group).slice(0, vpnState.nodeLimit);
        return `${banner}
        <div class="vpn-proxy-layout">
            <aside class="vpn-card vpn-group-card">
                <div class="vpn-card-head"><span>策略组</span>${vpnTag(String(groups.length))}</div>
                <div class="vpn-group-list">
                    ${groups.map((item, index) => `<div class="vpn-group ${index === vpnState.groupIdx ? 'is-active' : ''}" data-vpn-groupidx="${index}">
                        <span class="vpn-group-name">${vpnEsc(item.name)}</span>
                        <span class="vpn-group-count">${(item.nodes || []).length}</span>
                        <span class="vpn-gtype">${vpnEsc(item.type || item.kind || '')}</span>
                    </div>`).join('')}
                </div>
            </aside>
            <section class="vpn-card vpn-nodes-card">
                <div class="vpn-nodes-head">
                    <span class="vpn-nodes-title">${vpnEsc(group.name)}</span>
                    ${vpnTag((group.nodes || []).length + ' 个节点')}
                    ${vpnTag('当前 ' + (vpnStripFlag(group.now) || '—'), 'ok')}
                    <div class="vpn-spacer"></div>
                    <label class="vpn-search">${vpnIcon('M10.5 4a6.5 6.5 0 014.6 11.1l4.4 4.4-1.4 1.4-4.4-4.4A6.5 6.5 0 1110.5 4z', 13)}
                        <input class="setting-text" id="vpn-node-search" placeholder="搜索节点" value="${vpnEsc(vpnState.nodeQuery)}">
                    </label>
                    <button class="btn btn-primary btn-sm" data-vpn-act="sort">${vpnState.nodeSort === 'delay' ? '恢复默认' : '按延迟排序'}</button>
                    <button class="btn btn-primary btn-sm" data-vpn-act="test-group">测速本组</button>
                </div>
                <div class="vpn-nodes" id="vpn-node-grid">${nodes.map((n) => vpnNodeCard(n, group)).join('') || '<div class="vpn-muted vpn-small" style="padding:10px">没有匹配的节点</div>'}</div>
                <div class="vpn-nodes-foot vpn-muted" id="vpn-node-footer">${vpnNodeFooter(group)}</div>
            </section>
        </div>`;
    }

    function vpnRefreshNodes() {
        const groups = vpnState.proxies;
        if (!groups.length) return;
        const group = groups[Math.min(vpnState.groupIdx, groups.length - 1)];
        const grid = vpnEl('vpn-node-grid');
        if (grid) {
            grid.innerHTML = vpnFilteredNodes(group).slice(0, vpnState.nodeLimit).map((n) => vpnNodeCard(n, group)).join('')
                || '<div class="vpn-muted vpn-small" style="padding:10px">没有匹配的节点</div>';
        }
        const footer = vpnEl('vpn-node-footer');
        if (footer) footer.innerHTML = vpnNodeFooter(group);
    }

    // ─────────────────────────── 订阅页 ───────────────────────────
    function vpnRenderProfiles() {
        if (!vpnState.profiles.length) return vpnRenderEmpty();
        const list = vpnState.profiles;
        return `<div class="vpn-list-head">
            ${vpnTag(list.length + ' 个配置')}
            <span class="vpn-hint">自动更新失败会保留旧配置 · 最多保留 3 个版本</span>
            <div class="vpn-spacer"></div>
            <button class="btn btn-primary" data-vpn-act="update-all">全部更新</button>
            <button class="btn btn-primary" data-vpn-act="import">导入订阅</button>
        </div>
        <div class="vpn-card">
        ${list.map((profile) => {
            const active = profile.id === vpnState.activeId;
            const notes = [];
            if (profile.last_error) notes.push(`<div class="vpn-fail">${vpnIcon('M12 3L2 21h20L12 3z M12 9v5 M12 18h.01', 12)}<span>上次更新失败：${vpnEsc(profile.last_error)}</span></div>`);
            if (profile.updated_note) notes.push(`<div class="vpn-fail is-warn">${vpnIcon('M12 3L2 21h20L12 3z M12 9v5 M12 18h.01', 12)}<span>${vpnEsc(profile.updated_note)}</span></div>`);
            let usage = '';
            if (profile.total) {
                const used = (profile.upload || 0) + (profile.download || 0);
                const percent = Math.min(100, (used / profile.total) * 100);
                const expireSoon = profile.expire_at && (new Date(profile.expire_at * 1000) - Date.now() < 7 * 86400000);
                const expirePill = profile.expire_at ? vpnTag(vpnFmtExpire(profile.expire_at), expireSoon ? 'warn' : '') : '';
                usage = `<div class="vpn-usage">
                    <span class="vpn-progress"><i style="width:${percent}%"></i></span>
                    <span>${Math.round(percent)}%</span>
                    <span class="vpn-usage-num">${vpnFmtBytes(used)} / ${vpnFmtBytes(profile.total)}</span>
                    ${expirePill}
                </div>`;
            }
            return `<div class="vpn-profile">
                <div class="vpn-profile-thumb">${profile.kind === 'url' ? 'URL' : 'YML'}</div>
                <div class="vpn-profile-main">
                    <div class="vpn-profile-title">
                        <span class="vpn-profile-name">${vpnEsc(profile.name)}</span>
                        ${active ? vpnTag('使用中', 'ok') : vpnTag('未启用')}
                        ${vpnTag((profile.nodes || 0) + ' 节点')}
                        ${vpnTag((profile.rules || 0) + ' 规则')}
                        ${profile.version_count ? vpnTag(profile.version_count + ' 个版本') : ''}
                    </div>
                    <div class="vpn-profile-meta">${profile.kind === 'url' ? vpnEsc(getDomain(profile.url)) : vpnEsc(profile.path)} &nbsp;&nbsp;·&nbsp;&nbsp; 更新于 ${vpnFmtTime(profile.updated_at)} &nbsp;&nbsp;·&nbsp;&nbsp;  ${profile.interval_hours ? '每 ' + profile.interval_hours + ' 小时' : '手动'}</div>
                    ${usage}${notes.join('')}
                </div>
                <div class="vpn-profile-actions">
                    ${active ? '' : `<button class="btn btn-success btn-sm" data-vpn-act="activate" data-vpn-id="${profile.id}">使用</button>`}
                    <button class="btn btn-primary btn-sm" data-vpn-act="update" data-vpn-id="${profile.id}">更新</button>
                    <button class="btn btn-primary btn-sm" data-vpn-act="versions" data-vpn-id="${profile.id}">版本</button>
                    <button class="btn btn-danger btn-sm" data-vpn-act="remove" data-vpn-id="${profile.id}">${vpnIcon('M4 7h16 M9 7V4h6v3 M6 7l1 14h10l1-14 M10 11v6 M14 11v6', 12)}删除</button>
                </div>
            </div>`;
        }).join('')}
        </div>
        <div class="vpn-hint vpn-mt">订阅令牌默认打码显示；「版本」可回滚最近 3 次更新。</div>`;
    }

    // ─────────────────────────── 规则页 ───────────────────────────
    function vpnRenderRules() {
        if (!vpnState.profiles.length) return vpnRenderEmpty();
        return `<div class="vpn-toolbar">
            ${vpnTag('共 ' + vpnState.rules.length + ' 条')}
            ${vpnTag('只读', 'info')}
            <span class="vpn-hint">规则来自当前激活配置；在 UI 修改不会写回订阅</span>
            <div class="vpn-spacer"></div>
            <label class="vpn-search">${vpnIcon('M10.5 4a6.5 6.5 0 014.6 11.1l4.4 4.4-1.4 1.4-4.4-4.4A6.5 6.5 0 1110.5 4z', 13)}
                <input class="setting-text" id="vpn-rule-search" placeholder="搜索域名 / IP / 策略" value="${vpnEsc(vpnState.ruleQuery)}">
            </label>
        </div>
        <div class="vpn-card vpn-table-card">
            <table class="vpn-table">
                <thead><tr><th style="width:56px">#</th><th style="width:156px">类型</th><th>内容</th><th style="width:180px">策略</th><th style="width:84px">备注</th></tr></thead>
                <tbody id="vpn-rule-body">${vpnRuleRows()}</tbody>
            </table>
        </div>
        <div class="vpn-hint vpn-mt">末条 <b>MATCH</b> 为兜底规则：前面规则都未命中时使用。</div>`;
    }

    function vpnRuleRows() {
        const query = (vpnState.ruleQuery || '').trim().toLowerCase();
        const list = query
            ? vpnState.rules.filter((r) => (r.payload + ' ' + r.proxy + ' ' + r.type).toLowerCase().includes(query))
            : vpnState.rules;
        if (!list.length) return '<tr><td colspan="5" class="vpn-muted" style="padding:16px">没有匹配的规则</td></tr>';
        return list.map((rule) => `<tr class="${rule.is_final ? 'is-final' : ''}">
            <td class="vpn-col-idx">${rule.index}</td>
            <td>${vpnTag(rule.type, rule.is_final ? 'warn' : 'info')}</td>
            <td class="vpn-mono">${vpnEsc(rule.payload)}</td>
            <td>${vpnEsc(rule.proxy)}</td>
            <td class="vpn-muted">${rule.is_final ? vpnTag('兜底', 'warn') : ''}</td>
        </tr>`).join('');
    }

    function vpnRefreshRuleRows() {
        const body = vpnEl('vpn-rule-body');
        if (body) body.innerHTML = vpnRuleRows();
    }

    // ─────────────────────────── 连接页 ───────────────────────────
    function vpnShowProcess() {
        return !!(vpnState.settings && vpnState.settings.find_process);
    }

    function vpnRenderConnections() {
        if (!vpnState.profiles.length) return vpnRenderEmpty();
        const data = vpnState.conns;
        const list = data.connections || [];
        const showProcess = vpnShowProcess();
        return `<div class="vpn-toolbar">
            ${vpnTag('实时 · 2s', 'ok')}
            <span class="vpn-stat">连接 <b>${list.length}</b> 条</span>
            <span class="vpn-stat vpn-stat-up">↑ <b>${vpnFmtBytes(data.upload)}</b></span>
            <span class="vpn-stat vpn-stat-down">↓ <b>${vpnFmtBytes(data.download)}</b></span>
            <div class="vpn-spacer"></div>
            <label class="vpn-search">${vpnIcon('M10.5 4a6.5 6.5 0 014.6 11.1l4.4 4.4-1.4 1.4-4.4-4.4A6.5 6.5 0 1110.5 4z', 13)}
                <input class="setting-text" id="vpn-conn-search" placeholder="搜索域名${showProcess ? ' / 进程' : ''}" value="${vpnEsc(vpnState.connQuery)}">
            </label>
            <button class="btn btn-danger" data-vpn-act="kill-all">${vpnIcon('M6 6l12 12M18 6L6 18', 12)}全部断开</button>
        </div>
        <div class="vpn-card vpn-table-card">
            <table class="vpn-table">
                <thead><tr><th>主机</th><th style="width:64px">协议</th><th>出口链</th>${showProcess ? '<th style="width:108px">进程</th>' : ''}<th class="vpn-col-num" style="width:84px">上行</th><th class="vpn-col-num" style="width:84px">下行</th><th style="width:72px"></th></tr></thead>
                <tbody id="vpn-conn-body">${vpnConnRows()}</tbody>
            </table>
        </div>
        ${showProcess ? '' : '<div class="vpn-hint vpn-mt">进程列可在「设置 → 高级 → 连接页显示进程」开启</div>'}`;
    }

    function vpnConnRows() {
        const showProcess = vpnShowProcess();
        const list = vpnState.conns.connections || [];
        const query = (vpnState.connQuery || '').trim().toLowerCase();
        const filtered = query
            ? list.filter((c) => (c.host + ' ' + (c.process || '') + ' ' + (c.chains || []).join(' ')).toLowerCase().includes(query))
            : list;
        const colspan = showProcess ? 7 : 6;
        if (!filtered.length) return `<tr><td colspan="${colspan}" class="vpn-muted" style="padding:13px;text-align:center">暂无连接</td></tr>`;
        return filtered.map((c) => `<tr>
            <td>${vpnEsc(c.host)}</td><td>${vpnTag(c.protocol)}</td>
            <td class="vpn-chain">${vpnEsc((c.chains || []).join(' → ') || '—')}</td>
            ${showProcess ? `<td class="vpn-muted">${vpnEsc(c.process || '—')}</td>` : ''}
            <td class="vpn-col-num">${vpnFmtBytes(c.upload)}</td><td class="vpn-col-num">${vpnFmtBytes(c.download)}</td>
            <td class="vpn-col-num"><button class="btn btn-danger btn-sm" data-vpn-act="kill" data-vpn-id="${vpnEsc(c.id)}">断开</button></td>
        </tr>`).join('');
    }

    function vpnRefreshConnRows() {
        const body = vpnEl('vpn-conn-body');
        if (body) body.innerHTML = vpnConnRows();
    }

    // ─────────────────────────── 日志页 ───────────────────────────
    function vpnLogRowHtml(entry) {
        const tone = { warn: 'warn', error: 'bad', debug: 'info' }[entry.level] || 'info';
        return `<div class="vpn-log-row">
            <span class="vpn-log-time">${vpnEsc(entry.time)}</span>
            <span class="vpn-log-level">${vpnTag(entry.level, tone)}</span>
            <span class="vpn-log-msg">${vpnEsc(entry.message)}</span>
        </div>`;
    }

    function vpnLogRows() {
        const query = vpnState.logsQuery.trim().toLowerCase();
        const list = vpnState.logs.filter((entry) => (vpnState.logsLevel === 'all' || entry.level === vpnState.logsLevel)
            && (!query || String(entry.message).toLowerCase().includes(query)));
        return list.slice(-400).map(vpnLogRowHtml).join('') || '<div class="vpn-muted" style="padding:16px">暂无日志</div>';
    }

    function vpnRenderLogs() {
        if (!vpnState.profiles.length) return vpnRenderEmpty();
        return `<div class="vpn-toolbar">
            <select class="setting-text vpn-log-select" id="vpn-log-level" title="日志级别">
                ${['all', 'info', 'warn', 'error', 'debug'].map((level) => `<option value="${level}" ${vpnState.logsLevel === level ? 'selected' : ''}>${level === 'all' ? '全部级别' : level}</option>`).join('')}
            </select>
            <label class="vpn-search">${vpnIcon('M10.5 4a6.5 6.5 0 014.6 11.1l4.4 4.4-1.4 1.4-4.4-4.4A6.5 6.5 0 1110.5 4z', 13)}
                <input class="setting-text" id="vpn-log-search" placeholder="过滤关键字" value="${vpnEsc(vpnState.logsQuery)}">
            </label>
            <div class="vpn-spacer"></div>
            <button class="btn btn-primary btn-sm" data-vpn-act="logs-pause">${vpnState.logsPaused ? '继续' : '暂停'}</button>
            <button class="btn btn-primary btn-sm" data-vpn-act="logs-copy">${vpnIcon('M9 9h11v11H9z M5 15H4V4h11v1', 12)}复制</button>
            <button class="btn btn-primary btn-sm" data-vpn-act="logs-clear">清空</button>
        </div>
        <div class="vpn-card vpn-log-card">
            <div class="vpn-logbox" id="vpn-log-list">${vpnLogRows()}</div>
            <div class="vpn-log-foot"><span>已显示最近 ${Math.min(vpnState.logs.length, 400)} 条 · ${vpnState.logsPaused ? '已暂停' : '自动滚动'}</span><span>已脱敏</span></div>
        </div>`;
    }

    function vpnAppendLogRow(entry) {
        const box = vpnEl('vpn-log-list');
        if (!box) return;
        if (vpnState.logsLevel !== 'all' && entry.level !== vpnState.logsLevel) return;
        const query = vpnState.logsQuery.trim().toLowerCase();
        if (query && !String(entry.message).toLowerCase().includes(query)) return;
        const nearBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 120;
        box.insertAdjacentHTML('beforeend', vpnLogRowHtml(entry));
        while (box.children.length > 600) box.removeChild(box.firstElementChild);
        if (nearBottom) box.scrollTop = box.scrollHeight;
    }

    function vpnRefreshLogList() {
        const box = vpnEl('vpn-log-list');
        if (box) box.innerHTML = vpnLogRows();
    }

    // ─────────────────────────── 设置页 ───────────────────────────
    function vpnSwitch(key, on, small) {
        return `<button class="vpn-switch ${small ? 'vpn-switch-sm' : ''} ${on ? 'on' : ''}" data-vpn-toggle="${key}" aria-pressed="${on}"></button>`;
    }

    function vpnRenderSettings() {
        const s = vpnState.settings || {};
        const st = vpnState.state || {};
        const sysOn = st.system_proxy && st.system_proxy.state === 'enabled';
        const core = st.core || {};
        const ports = st.ports || {};
        const row = (label, hint, ctrl) => `<div class="vpn-set-row"><div class="vpn-set-label">${label}${hint ? `<span class="vpn-hint">${hint}</span>` : ''}</div><div class="vpn-set-ctrl">${ctrl}</div></div>`;
        const rowSwitch = (label, hint, key, on, small) => row(label, hint, vpnSwitch(key, on, small));
        const sysTag = st.system_proxy && st.system_proxy.state === 'enabled' ? vpnTag('已启用', 'ok')
            : st.system_proxy && st.system_proxy.state === 'error' ? vpnTag('异常', 'bad') : vpnTag('未启用');
        const dnsExplicit = !(s.dns_enable === null || s.dns_enable === undefined);
        return `<div class="vpn-settings-grid">
            <section class="vpn-card vpn-set-card">
                <div class="vpn-set-head">${vpnIcon('M4 7h16 M4 12h10 M4 17h7', 14)}连接与端口</div>
                <div class="vpn-set-body">
                    ${row('混合端口', '自动探测：' + (ports.mixed || '—') + '；修改后自动重启内核', `<input class="setting-number" id="vpn-set-mixed" value="${s.mixed_port || 7890}">`)}
                    ${row('控制器端口', '仅监听 127.0.0.1', `<input class="setting-number" id="vpn-set-controller" value="${s.controller_port || 9090}">`)}
                </div>
            </section>

            <section class="vpn-card vpn-set-card">
                <div class="vpn-set-head">${vpnIcon('M12 3v12m0 0 4-4m-4 4-4-4 M4 17v3h16v-3', 14)}订阅与更新</div>
                <div class="vpn-set-body">
                    ${row('默认更新周期', '新建订阅时使用', `<select class="setting-text vpn-select" id="vpn-set-interval">
                        ${[0, 6, 12, 24].map((hour) => `<option value="${hour}" ${(s.default_interval_hours || 0) === hour ? 'selected' : ''}>${hour ? '每 ' + hour + ' 小时' : '手动'}</option>`).join('')}
                    </select>`)}
                    ${rowSwitch('更新失败时通知', '', 'notify_on_update_fail', s.notify_on_update_fail !== false)}
                </div>
            </section>

            <section class="vpn-card vpn-set-card">
                <div class="vpn-set-head">${vpnIcon('M12 3a9 9 0 100 18 9 9 0 000-18z M12 11v5 M12 8h.01', 14)}内核</div>
                <div class="vpn-set-body">
                    ${row('mihomo', `PID ${core.pid || '—'} · 混合端口 ${ports.mixed || '—'} · 控制器 127.0.0.1:${ports.controller || '—'}`,
                    `<span class="vpn-set-tag">${vpnTag(core.state === 'running' ? '运行中' : '未运行', core.state === 'running' ? 'ok' : '')}</span>
                         <button class="btn btn-primary btn-sm" data-vpn-act="restart-core">重启内核</button>`)}
                    ${row('版本', '内置 mihomo 内核', `<span class="vpn-tag vpn-tag-mono">${vpnEsc(core.version || 'v1.x')}</span>`)}
                </div>
            </section>

            <section class="vpn-card vpn-set-card">
                <div class="vpn-set-head">${vpnIcon('M13 2L4 14h7l-1 8 9-12h-7l1-8z', 14)}启动</div>
                <div class="vpn-set-body">
                    ${rowSwitch('开机自启', '静默进入托盘', 'autostart', !!s.autostart)}
                    ${rowSwitch('启动时自动连接代理', '', 'auto_connect', !!s.auto_connect)}
                </div>
            </section>
            <section class="vpn-card vpn-set-card">
                <div class="vpn-set-head">${vpnIcon('M8 12a4 4 0 014-4h3a4 4 0 010 8h-1 M16 12a4 4 0 01-4 4H9a4 4 0 010-8h1', 14)}系统代理</div>
                <div class="vpn-set-body">
                    ${row('系统代理', 'Windows WinINet；WinHTTP 应用不受影响', `<span class="vpn-set-tag">${sysTag}</span>${vpnSwitch('sysproxy', sysOn)}`)}
                    ${row('系统代理协议',
            '仅影响系统代理条目',
            `<span class="vpn-proto">${vpnSwitch('http_proxy', s.http_proxy !== false, true)}HTTP</span>
                         <span class="vpn-proto">${vpnSwitch('https_proxy', s.https_proxy !== false, true)}HTTPS</span>
                         <span class="vpn-proto">${vpnSwitch('socks_proxy', !!s.socks_proxy, true)}SOCKS</span>`)}
                    ${rowSwitch('连接时自动开启系统代理', '', 'auto_system_proxy', s.auto_system_proxy !== false)}
                    ${rowSwitch('退出应用时恢复系统代理', '', 'restore_on_exit', s.restore_on_exit !== false)}
                    ${rowSwitch('异常退出后自动恢复', '崩溃/强杀后，下次启动恢复原始配置', 'recover_on_crash', s.recover_on_crash !== false)}
                    ${row('原始代理快照', s.has_snapshot ? '已保存，可用于一键恢复' : '无（未开启过系统代理）',
                `<span class="vpn-set-tag">${s.has_snapshot ? vpnTag('已保存', 'ok') : vpnTag('无')}</span>
                         <button class="btn btn-primary btn-sm" data-vpn-act="restore-sysproxy">立即恢复</button>`)}
                </div>
            </section>

            <section class="vpn-card vpn-set-card">
                <div class="vpn-set-head">${vpnIcon('M13 2L4 14h7l-1 8 9-12h-7l1-8z', 14)}高级</div>
                <div class="vpn-set-body">
                    ${rowSwitch('连接页显示进程', '需内核 find-process-mode，有少量开销', 'find_process', !!s.find_process)}
                    ${row('DNS 增强', '默认继承订阅，不修改 nameserver', `<span class="vpn-set-tag">${dnsExplicit ? vpnTag('已覆盖', 'warn') : vpnTag('继承订阅', 'info')}</span>${vpnSwitch('dns_enable', s.dns_enable === true)}`)}
                    ${rowSwitch('覆盖上游 DNS', '开启后用下方列表替换 dns.nameserver', 'dns_override', !!s.dns_override)}
                    <div class="vpn-set-row">
                        <textarea class="setting-text vpn-dns" id="vpn-set-dns" ${s.dns_override ? '' : 'disabled'} placeholder="https://223.5.5.5/dns-query&#10;https://1.12.12.12/dns-query">${vpnEsc((s.dns_nameservers || []).join('\n'))}</textarea>
                    </div>
                    ${dnsExplicit ? `<div class="vpn-set-row"><span class="vpn-hint">已显式覆盖 dns.enable，可恢复为完全继承订阅</span><div class="vpn-spacer"></div><button class="btn btn-primary btn-sm" data-vpn-act="dns-inherit">恢复继承</button></div>` : ''}
                    ${row('测速 URL', '用于节点延迟测试', `<input class="setting-text vpn-long" id="vpn-set-test-url" value="${vpnEsc(s.test_url || '')}">`)}
                    ${row('测速超时', '毫秒', `<input class="setting-number" id="vpn-set-test-timeout" value="${s.test_timeout_ms || 5000}">`)}
                    ${row('日志级别', '', `<select class="setting-text vpn-select" id="vpn-set-log-level">
                        ${['info', 'debug', 'warning', 'error'].map((level) => `<option ${s.log_level === level ? 'selected' : ''}>${level}</option>`).join('')}
                    </select>`)}
                    ${row('数据目录', `<span class="vpn-mono">${vpnEsc(s.data_dir || '')}</span>`, `<button class="btn btn-primary btn-sm" data-vpn-act="open-dir">打开</button>`)}
                </div>
            </section>

            <div class="vpn-set-actions">
                <span class="vpn-hint">修改后需保存</span>
                <button class="btn btn-primary" data-vpn-act="settings-reload">重新读取</button>
                <button class="btn btn-success" data-vpn-act="settings-save">保存设置</button>
            </div>
        </div>`;
    }

    function vpnCaptureForm() {
        const ids = ['vpn-set-mixed', 'vpn-set-controller', 'vpn-set-test-url', 'vpn-set-test-timeout', 'vpn-set-dns', 'vpn-set-log-level', 'vpn-set-interval'];
        const snapshot = {};
        ids.forEach((id) => {
            const el = vpnEl(id);
            if (el) snapshot[id] = el.value;
        });
        return snapshot;
    }

    function vpnRestoreForm(snapshot) {
        Object.entries(snapshot).forEach(([id, value]) => {
            const el = vpnEl(id);
            if (el) el.value = value;
        });
    }

    async function vpnRefreshSettingsView() {
        await Promise.all([vpnLoadSettings(), vpnRefreshState()]);
        if (!vpnMounted) return;
        if (vpnState.tab !== 'settings') {
            if (vpnState.tab === 'proxies') vpnRender();
            return;
        }
        const snapshot = vpnCaptureForm();
        vpnRender();
        vpnRestoreForm(snapshot);
    }

    // ─────────────────────────── 弹层 ───────────────────────────
    // ─────────────────────────── 弹层 ───────────────────────────
    function vpnRenderLayer() {
        const layer = vpnEl('vpn-layer');
        if (!layer) return;
        if (!vpnState.modal) {
            layer.innerHTML = '';
            return;
        }
        if (vpnState.modal === 'diag') layer.innerHTML = vpnDiagModal();
        else if (vpnState.modal === 'import') layer.innerHTML = vpnImportModal();
        else if (vpnState.modal === 'killall') layer.innerHTML = vpnConfirmModal('断开全部连接？', '将断开当前所有活动连接（下载中的任务会中断），代理本身保持开启。', '全部断开', 'killall');
        else if (vpnState.modal === 'remove-profile') layer.innerHTML = vpnConfirmModal('删除这个订阅？', '订阅文件与历史版本都会删除，无法恢复。', '删除', 'remove');
        else if (vpnState.modal === 'versions') layer.innerHTML = vpnVersionsModal();
    }

    function vpnOpenModal(name, data) {
        vpnState.modal = name;
        vpnState.modalData = data || null;
        vpnRenderLayer();
    }

    function vpnCloseLayer() {
        vpnState.modal = '';
        vpnState.modalData = null;
        vpnRenderLayer();
    }

    function vpnDiagModal() {
        const diag = vpnState.diag;
        const checks = (diag && diag.checks) || [];
        const names = { core: '内核', port: '本地端口', sysproxy: '系统代理', node: '当前节点', egress: '外网连通' };
        const body = checks.length
            ? checks.map((check) => `<div class="vpn-diag-row" title="${vpnEsc(check.name + '：' + check.message)}">
                    <i class="vpn-diag-dot ${check.ok ? '' : 'is-bad'}"></i>
                    <span class="vpn-diag-name">${vpnEsc(names[check.id] || check.name)}</span>
                    <span class="vpn-diag-val ${check.ok ? '' : 'is-bad'}">${vpnEsc(check.message)}</span>
                </div>`).join('')
            : '<div class="vpn-muted" style="padding:10px 0">正在检测…</div>';
        const conclusion = diag ? `<div class="vpn-diag-summary ${diag.ok ? 'is-ok' : ''}">
                <div class="vpn-diag-ct">${vpnEsc(diag.conclusion || '')}</div>
                ${(diag.suggestions || []).map((tip) => `<div class="vpn-diag-cd">建议：${vpnEsc(tip)}</div>`).join('')}
            </div>` : '';
        return `<div class="setting-modal-overlay" data-vpn-close="1">
            <div class="setting-modal vpn-modal" style="width:408px;max-width:408px">
                <div class="setting-modal-header">连接诊断
                    ${diag ? vpnTag(diag.ok ? '全部通过' : '有异常', diag.ok ? 'ok' : 'bad') : ''}
                    <div class="vpn-spacer"></div><button class="vpn-close" data-vpn-act="close" title="关闭">${vpnIcon('M6 6l12 12M18 6L6 18', 12)}</button></div>
                <div class="setting-modal-body vpn-diag-body">${body}${conclusion}</div>
                <div class="setting-modal-footer"><button class="btn btn-primary" data-vpn-act="close">关闭</button><button class="btn btn-primary" data-vpn-act="rerun-diag">${vpnIcon('M20 12a8 8 0 11-2.3-5.7 M20 4v5h-5', 13)}重新检测</button></div>
            </div>
        </div>`;
    }

    function vpnImportModal() {
        return `<div class="setting-modal-overlay" data-vpn-close="1">
            <div class="setting-modal vpn-modal" style="width:480px;max-width:480px">
                <div class="setting-modal-header">导入订阅</div>
                <div class="setting-modal-body vpn-form-body">
                    <div class="vpn-field"><label class="vpn-field-label">名称</label><input class="input-modal-input" id="vpn-imp-name" placeholder="机场 A · 主力"></div>
                    <div class="vpn-field"><label class="vpn-field-label">订阅 URL</label><input class="input-modal-input" id="vpn-imp-url" placeholder="https://ex.com/sub?token=…"></div>
                    <div class="vpn-field"><label class="vpn-field-label">或本地 YAML 路径</label><input class="input-modal-input" id="vpn-imp-path" placeholder="/Users/me/nodes/self.yaml"></div>
                    <div class="vpn-field"><label class="vpn-field-label">自动更新</label>
                        <select class="input-modal-input" id="vpn-imp-interval">${[0, 6, 12, 24].map((hour) => `<option value="${hour}" ${hour === 12 ? 'selected' : ''}>${hour ? '每 ' + hour + ' 小时' : '不自动更新'}</option>`).join('')}</select></div>
                    <div class="vpn-hint">导入后自动校验：YAML 语法 → 结构 → mihomo -t；任一失败不会保存。</div>
                </div>
                <div class="setting-modal-footer"><button class="btn btn-primary" data-vpn-act="close">取消</button><button class="btn btn-success" data-vpn-act="do-import">导入</button></div>
            </div>
        </div>`;
    }

    function vpnConfirmModal(title, text, label, action) {
        const tone = (action === 'killall' || action === 'remove') ? 'btn-danger' : 'btn-success';
        return `<div class="setting-modal-overlay" data-vpn-close="1">
            <div class="setting-modal vpn-modal" style="width:380px;max-width:380px">
                <div class="setting-modal-header">${vpnEsc(title)}</div>
                <div class="setting-modal-body vpn-form-body"><div class="vpn-hint">${vpnEsc(text)}</div></div>
                <div class="setting-modal-footer"><button class="btn btn-primary" data-vpn-act="close">取消</button><button class="btn ${tone}" data-vpn-act="${action}">${vpnEsc(label)}</button></div>
            </div>
        </div>`;
    }

    function vpnVersionsModal() {
        const data = vpnState.modalData || { versions: [] };
        const rows = (data.versions || []).length
            ? data.versions.map((version) => `<div class="vpn-version-row">
                    <div class="vpn-version-main"><b>${vpnFmtTime(version.ts)}</b><div class="vpn-hint">${version.nodes} 节点 · ${version.groups} 策略组 · ${version.rules} 规则</div></div>
                    <button class="btn btn-primary btn-sm" data-vpn-act="rollback" data-vpn-ts="${version.ts}">回滚</button>
                </div>`).join('')
            : '<div class="vpn-muted">还没有历史版本（更新一次后会生成）</div>';
        return `<div class="setting-modal-overlay" data-vpn-close="1">
            <div class="setting-modal vpn-modal" style="width:200px;max-width:280px">
                <div class="setting-modal-header">历史版本</div>
                <div class="setting-modal-body vpn-form-body">${rows}</div>
                <div class="setting-modal-footer"><button class="btn btn-primary" data-vpn-act="close">关闭</button></div>
            </div>
        </div>`;
    }

    async function vpnRefreshState() {
        const token = vpnToken;
        try {
            const state = await vpnInvoke('vpn_state', {}, 8000);
            if (vpnToken !== token) return;
            vpnState.state = state;
            if (vpnMounted) vpnRenderStatus();
        } catch (e) {
            if (vpnToken === token) console.warn('[vpn] 读取状态失败:', e);
        }
    }

    async function vpnLoadProxies() {
        const token = vpnToken;
        try {
            const groups = await vpnInvoke('vpn_proxies', {}, 15000);
            if (vpnToken !== token) return;
            // 去掉内核自带的 GLOBAL 策略组：它与当前订阅节点重复，且容易误操作
            vpnState.proxies = (groups || []).filter((group) => String(group.name || '').trim().toUpperCase() !== 'GLOBAL');
            if (vpnState.groupIdx >= vpnState.proxies.length) vpnState.groupIdx = 0;
        } catch (e) {
            if (vpnToken === token) vpnState.proxies = [];
        }
    }

    async function vpnLoadRules() {
        const token = vpnToken;
        try {
            const rules = await vpnInvoke('vpn_rules', {}, 15000);
            if (vpnToken === token) vpnState.rules = rules || [];
        } catch (e) {
            if (vpnToken === token) vpnState.rules = [];
        }
    }

    async function vpnLoadConnections() {
        const token = vpnToken;
        try {
            const snapshot = await vpnInvoke('vpn_connections', {}, 15000);
            if (vpnToken !== token) return;
            vpnState.conns = snapshot || { connections: [], upload: 0, download: 0 };
            vpnRefreshConnRows();
        } catch (e) {
            /* 轮询失败保持旧数据 */
        }
    }

    async function vpnLoadLogs() {
        const token = vpnToken;
        try {
            const logs = await vpnInvoke('vpn_logs', { level: 'all', limit: 400 }, 10000);
            if (vpnToken === token) vpnState.logs = logs || [];
        } catch (e) {
            /* 读取失败保留旧日志 */
        }
    }

    async function vpnLoadProfiles() {
        const token = vpnToken;
        try {
            const data = await vpnInvoke('vpn_profiles', {}, 10000);
            if (vpnToken !== token) return;
            vpnState.profiles = data.profiles || [];
            vpnState.activeId = data.active_id || '';
        } catch (e) {
            if (vpnToken === token) vpnState.profiles = [];
        }
    }

    async function vpnLoadSettings() {
        const token = vpnToken;
        try {
            const settings = await vpnInvoke('vpn_settings_get', {}, 10000);
            if (vpnToken === token) vpnState.settings = settings;
        } catch (e) {
            if (vpnToken === token && !vpnState.settings) vpnState.settings = {};
        }
    }

    // ─────────────────────────── 动作 ───────────────────────────
    async function vpnToggleConnect() {
        if (vpnState.busy) return;
        const conn = vpnState.state && vpnState.state.connection.state;
        const closing = conn === 'connected' || conn === 'degraded' || conn === 'connecting';
        vpnState.busy = true;
        vpnState.progress = closing ? '正在关闭…' : '正在启动…';
        vpnRenderStatus();
        try {
            await vpnInvoke(closing ? 'vpn_disconnect' : 'vpn_connect', {}, closing ? 60000 : 90000);
            vpnToast(closing ? '已关闭代理' : '代理已开启');
        } catch (e) {
            vpnToast(String(e.message || e), 'error');
        } finally {
            vpnState.busy = false;
            vpnState.progress = '';
            await vpnRefreshState();
            if (vpnMounted) vpnRender();
        }
    }

    async function vpnSetMode(mode) {
        const previous = vpnState.settings ? vpnState.settings.mode : 'rule';
        if (vpnState.settings) vpnState.settings.mode = mode;
        if (vpnState.state) vpnState.state.mode = mode;
        vpnRenderStatus();
        try {
            await vpnInvoke('vpn_mode_set', { mode }, 15000);
            await vpnRefreshState();
        } catch (e) {
            if (vpnState.settings) vpnState.settings.mode = previous;
            if (vpnState.state) vpnState.state.mode = previous;
            vpnRenderStatus();
            vpnToast(String(e.message || e), 'error');
        }
    }

    async function vpnSelectNode(group, name) {
        try {
            await vpnInvoke('vpn_select_proxy', { group, name }, 20000);
            await vpnLoadProxies();
            vpnRender();
            vpnToast('已切换到 ' + name);
        } catch (e) {
            vpnToast(String(e.message || e), 'error');
        }
    }

    async function vpnTestGroup() {
        const groups = vpnState.proxies;
        if (!groups.length) return;
        const group = groups[Math.min(vpnState.groupIdx, groups.length - 1)];
        vpnToast('正在测速：' + group.name);
        try {
            const result = await vpnInvoke('vpn_group_delay', { group: group.name }, 40000);
            await vpnLoadProxies();
            vpnRender();
            const ok = Object.values(result || {}).filter((v) => v > 0).length;
            vpnToast(`测速完成：${ok} 个可用`);
        } catch (e) {
            vpnToast(String(e.message || e), 'error');
        }
    }

    async function vpnTestPickerGroup() {
        if (vpnState.testingGroup) return;
        const group = vpnPickerGroup();
        if (!group) return;
        vpnState.testingGroup = group.name;
        vpnRefreshNodePicker();
        try {
            const result = await vpnInvoke('vpn_group_delay', { group: group.name }, 40000);
            const fresh = await vpnInvoke('vpn_proxies', {}, 15000);
            vpnState.proxies = (fresh || []).filter((item) => String(item.name || '').trim().toUpperCase() !== 'GLOBAL');
            if (vpnState.groupIdx >= vpnState.proxies.length) vpnState.groupIdx = 0;
            const ok = Object.values(result || {}).filter((value) => value > 0).length;
            vpnToast(`测速完成：${ok} 个可用`);
        } catch (e) {
            vpnToast(String(e.message || e), 'error');
        } finally {
            vpnState.testingGroup = '';
            // 只刷新级联面板，不整页重绘，保证悬停菜单保持展开
            vpnRefreshNodePicker();
        }
    }

    function vpnLocate() {
        const groups = vpnState.proxies;
        const index = groups.findIndex((group) => (group.nodes || []).some((node) => node.name === group.now));
        if (index < 0) return vpnToast('没有找到当前节点');
        vpnState.tab = 'proxies';
        vpnState.groupIdx = Math.max(0, index);
        vpnState.nodeQuery = groups[index].now;
        vpnRender();
    }

    async function vpnUpdateProfile(id) {
        vpnToast('正在更新订阅…');
        try {
            const profile = await vpnInvoke('vpn_profile_update', { id }, 60000);
            await vpnLoadProfiles();
            vpnRender();
            vpnToast(profile.updated_note ? '更新完成：' + profile.updated_note : '订阅已更新');
        } catch (e) {
            await vpnLoadProfiles();
            vpnRender();
            vpnToast('更新失败：' + (e.message || e), 'error');
        }
    }

    async function vpnUpdateAll() {
        const targets = vpnState.profiles.filter((profile) => profile.kind === 'url');
        if (!targets.length) return vpnToast('没有可自动更新的订阅');
        let ok = 0;
        let failed = 0;
        for (const profile of targets) {
            try {
                await vpnInvoke('vpn_profile_update', { id: profile.id }, 60000);
                ok += 1;
            } catch (e) {
                failed += 1;
            }
        }
        await vpnLoadProfiles();
        vpnRender();
        if (!failed) vpnToast(`全部更新完成（${ok} 个）`);
        else if (!ok) vpnToast(`全部更新失败（${failed} 个），已保留旧配置`);
        else vpnToast(`更新完成：成功 ${ok} 个，失败 ${failed} 个（失败项已保留旧配置）`);
    }

    async function vpnActivateProfile(id) {
        try {
            await vpnInvoke('vpn_profile_activate', { id }, 60000);
            await Promise.all([vpnLoadProfiles(), vpnRefreshState(), vpnLoadProxies()]);
            vpnRender();
            vpnToast('已切换配置');
        } catch (e) {
            await Promise.all([vpnLoadProfiles(), vpnRefreshState()]);
            vpnRender();
            vpnToast(String(e.message || e), 'error');
        }
    }

    async function vpnRemoveProfile(id) {
        try {
            await vpnInvoke('vpn_profile_remove', { id }, 20000);
            await vpnLoadProfiles();
            vpnCloseLayer();
            vpnRender();
            vpnToast('已删除');
        } catch (e) {
            vpnToast(String(e.message || e), 'error');
        }
    }

    async function vpnSubmitImport() {
        const name = (vpnEl('vpn-imp-name') || {}).value || '';
        const url = ((vpnEl('vpn-imp-url') || {}).value || '').trim();
        const path = ((vpnEl('vpn-imp-path') || {}).value || '').trim();
        const intervalEl = vpnEl('vpn-imp-interval');
        const interval = intervalEl ? Number(intervalEl.value) : 12;
        if (!url && !path) return vpnToast('请填写订阅 URL 或本地路径');
        try {
            await vpnInvoke('vpn_profile_add', { input: { name: name.trim(), url, path, interval_hours: interval } }, 60000);
            await Promise.all([vpnLoadProfiles(), vpnRefreshState()]);
            vpnCloseLayer();
            vpnRender();
            vpnToast('订阅已导入');
        } catch (e) {
            vpnToast('导入失败：' + (e.message || e), 'error');
        }
    }

    async function vpnPickLocalFile() {
        try {
            const path = await vpnInvoke('vpn_pick_yaml', {}, 120000);
            if (!path) return;
            await vpnInvoke('vpn_profile_add', { input: { name: String(path).split(/[\\/]/).pop(), url: '', path, interval_hours: 0 } }, 60000);
            await vpnLoadProfiles();
            vpnRender();
            vpnToast('本地配置已导入');
        } catch (e) {
            vpnToast('导入失败：' + (e.message || e), 'error');
        }
    }

    async function vpnCopyLogs() {
        const text = vpnState.logs.map((entry) => `[${entry.time}] ${String(entry.level).toUpperCase()} ${entry.message}`).join('\n');
        const copied = await vpnCopyText(text);
        vpnToast(copied ? '日志已复制' : '复制失败：当前环境不允许访问剪贴板');
    }

    async function vpnCopyText(text) {
        try {
            await navigator.clipboard.writeText(text);
            return true;
        } catch (e) { /* 回退 */ }
        try {
            const area = document.createElement('textarea');
            area.value = text;
            area.style.position = 'fixed';
            area.style.opacity = '0';
            document.body.appendChild(area);
            area.select();
            const ok = document.execCommand('copy');
            area.remove();
            return ok;
        } catch (e) {
            return false;
        }
    }

    function vpnToggleSetting(key, element) {
        const setSwitch = (on) => {
            if (element) {
                element.classList.toggle('on', on);
                element.setAttribute('aria-pressed', String(on));
            }
        };
        if (key === 'sysproxy') {
            const on = vpnState.state && vpnState.state.system_proxy && vpnState.state.system_proxy.state === 'enabled';
            vpnInvoke('vpn_sysproxy_set', { on: !on }, 60000)
                .then(() => vpnRefreshSettingsView())
                .catch((e) => vpnToast(String(e.message || e), 'error'));
            return;
        }
        if (key === 'dns_enable') {
            vpnState.settings.dns_enable = vpnState.settings.dns_enable === true ? false : true;
            setSwitch(vpnState.settings.dns_enable === true);
            return;
        }
        const booleanKeys = ['http_proxy', 'https_proxy', 'socks_proxy', 'auto_system_proxy', 'restore_on_exit',
            'recover_on_crash', 'autostart', 'auto_connect', 'notify_on_update_fail', 'dns_override', 'find_process'];
        if (booleanKeys.includes(key)) {
            vpnState.settings[key] = !vpnState.settings[key];
            setSwitch(!!vpnState.settings[key]);
            if (key === 'dns_override') {
                const area = vpnEl('vpn-set-dns');
                if (area) area.disabled = !vpnState.settings.dns_override;
            }
        }
    }

    async function vpnSaveSettings() {
        const readNumber = (id, fallback) => {
            const el = vpnEl(id);
            const value = el ? Number(el.value) : NaN;
            return Number.isFinite(value) && value > 0 ? value : fallback;
        };
        const readText = (id) => (vpnEl(id) ? vpnEl(id).value : '');
        const intervalEl = vpnEl('vpn-set-interval');
        const interval = intervalEl ? Number(intervalEl.value) : Number(vpnState.settings.default_interval_hours);
        const patch = {
            mixed_port: readNumber('vpn-set-mixed', vpnState.settings.mixed_port),
            controller_port: readNumber('vpn-set-controller', vpnState.settings.controller_port),
            http_proxy: !!vpnState.settings.http_proxy,
            https_proxy: !!vpnState.settings.https_proxy,
            socks_proxy: !!vpnState.settings.socks_proxy,
            auto_system_proxy: !!vpnState.settings.auto_system_proxy,
            restore_on_exit: !!vpnState.settings.restore_on_exit,
            recover_on_crash: !!vpnState.settings.recover_on_crash,
            autostart: !!vpnState.settings.autostart,
            auto_connect: !!vpnState.settings.auto_connect,
            notify_on_update_fail: !!vpnState.settings.notify_on_update_fail,
            dns_enable: vpnState.settings.dns_enable === undefined ? null : vpnState.settings.dns_enable,
            dns_override: !!vpnState.settings.dns_override,
            find_process: !!vpnState.settings.find_process,
            dns_nameservers: readText('vpn-set-dns').split('\n').map((line) => line.trim()).filter(Boolean),
            default_interval_hours: Number.isFinite(interval) ? interval : 12,
            test_url: readText('vpn-set-test-url') || vpnState.settings.test_url,
            test_timeout_ms: readNumber('vpn-set-test-timeout', 5000),
            log_level: readText('vpn-set-log-level') || vpnState.settings.log_level,
        };
        try {
            await vpnInvoke('vpn_settings_set', { patch }, 60000);
            await vpnLoadSettings();
            vpnRender();
            vpnToast('设置已保存');
        } catch (e) {
            vpnToast('保存失败：' + (e.message || e), 'error');
        }
    }

    async function vpnRunDiag() {
        try {
            const diag = await vpnInvoke('vpn_diag_run', {}, 30000);
            if (!vpnMounted) return;
            vpnState.diag = diag;
            if (vpnState.modal === 'diag') vpnRenderLayer();
        } catch (e) {
            vpnToast('诊断失败：' + (e.message || e), 'error');
        }
    }

    function vpnOpenVersions(id) {
        vpnInvoke('vpn_profile_versions', { id }, 15000)
            .then((versions) => vpnOpenModal('versions', { id, versions: versions || [] }))
            .catch((e) => vpnToast(String(e.message || e), 'error'));
    }

    async function vpnRollback(ts) {
        const data = vpnState.modalData || {};
        try {
            await vpnInvoke('vpn_profile_rollback', { id: data.id, ts: Number(ts) }, 60000);
            await Promise.all([vpnLoadProfiles(), vpnLoadProxies(), vpnRefreshState()]);
            vpnCloseLayer();
            vpnRender();
            vpnToast('已回滚');
        } catch (e) {
            vpnToast(String(e.message || e), 'error');
        }
    }

    function vpnOnAction(action, element) {
        const id = element.getAttribute('data-vpn-id') || (vpnState.modalData && vpnState.modalData.id);
        switch (action) {
            case 'mode': vpnSetMode(element.getAttribute('data-vpn-mode')); break;
            case 'pick-group':
                vpnState.pickerGroup = element.getAttribute('data-vpn-group') || null;
                vpnRefreshNodePicker();
                break;
            case 'locate': vpnLocate(); break;
            case 'sort': vpnState.nodeSort = vpnState.nodeSort === 'delay' ? 'default' : 'delay'; vpnRender(); break;
            case 'test-group': vpnTestGroup(); break;
            case 'test-picker': vpnTestPickerGroup(); break;
            case 'more': vpnState.nodeLimit += 120; vpnRender(); break;
            case 'diag': vpnOpenModal('diag'); vpnRunDiag(); break;
            case 'rerun-diag': vpnRunDiag(); break;
            case 'close': vpnCloseLayer(); break;
            case 'import': vpnOpenModal('import'); break;
            case 'import-file': vpnPickLocalFile(); break;
            case 'do-import': vpnSubmitImport(); break;
            case 'update': vpnUpdateProfile(id); break;
            case 'update-all': vpnUpdateAll(); break;
            case 'activate': vpnActivateProfile(id); break;
            case 'remove': vpnOpenModal('remove-profile', id); break;
            case 'versions': vpnOpenVersions(id); break;
            case 'rollback': vpnRollback(element.getAttribute('data-vpn-ts')); break;
            case 'kill': vpnInvoke('vpn_kill_connection', { id }, 15000).then(() => vpnLoadConnections()).catch((e) => vpnToast(String(e.message || e), 'error')); break;
            case 'killall': vpnInvoke('vpn_kill_all', {}, 20000).then(() => { vpnCloseLayer(); return vpnLoadConnections(); }).catch((e) => vpnToast(String(e.message || e), 'error')); break;
            case 'restore-sysproxy':
                vpnInvoke('vpn_sysproxy_set', { on: false }, 60000).then(() => vpnRefreshSettingsView()).then(() => vpnToast('已恢复系统代理')).catch((e) => vpnToast(String(e.message || e), 'error'));
                break;
            case 'sysproxy-on':
                vpnInvoke('vpn_sysproxy_set', { on: true }, 60000).then(() => vpnRefreshSettingsView()).then(() => vpnToast('系统代理已开启')).catch((e) => vpnToast(String(e.message || e), 'error'));
                break;
            case 'restart-core':
                vpnInvoke('vpn_restart_core', {}, 90000).then(() => vpnRefreshSettingsView()).catch((e) => vpnToast(String(e.message || e), 'error'));
                break;
            case 'dns-inherit':
                vpnInvoke('vpn_settings_set', { patch: { dns_enable_clear: true } }, 30000).then(() => vpnRefreshSettingsView()).then(() => vpnToast('DNS 已恢复为继承订阅')).catch((e) => vpnToast(String(e.message || e), 'error'));
                break;
            case 'open-dir': vpnInvoke('vpn_open_data_dir', {}, 10000).catch((e) => vpnToast(String(e.message || e), 'error')); break;
            case 'settings-save': vpnSaveSettings(); break;
            case 'settings-reload': vpnRefreshSettingsView().then(() => vpnToast('已重新读取')); break;
            case 'logs-pause': vpnState.logsPaused = !vpnState.logsPaused; vpnRender(); break;
            case 'logs-copy': vpnCopyLogs(); break;
            case 'logs-clear':
                vpnInvoke('vpn_logs_clear', {}, 10000).then(() => { vpnState.logs = []; vpnRender(); }).catch((e) => vpnToast(String(e.message || e), 'error'));
                break;
            case 'log-level': vpnState.logsLevel = element.getAttribute('data-vpn-level'); vpnRender(); break;
            case 'close-modal': vpnCloseLayer(); break;
            case 'retry': {
                if (!vpnRoot) return;
                const container = vpnRoot;
                vpnDestroy();
                vpnMount(container);
                break;
            }
            default: break;
        }
    }

    // ─────────────────────────── 事件 ───────────────────────────
    function vpnOnEvent(eventName, payload) {
        switch (eventName) {
            case 'vpn://state':
                vpnState.state = payload;
                vpnRenderStatus();
                if (vpnState.tab === 'proxies') vpnRender();
                break;
            case 'vpn://progress':
                if (!payload) return;
                if (payload.ok === false) {
                    vpnState.progress = '';
                    vpnRenderStatus();
                    vpnToast(payload.error || payload.label || '操作失败');
                    return;
                }
                if (payload.label) {
                    vpnState.progress = payload.label;
                    vpnRenderStatus();
                }
                break;
            case 'vpn://traffic':
                vpnState.traffic.up = payload.up || 0;
                vpnState.traffic.down = payload.down || 0;
                vpnState.traffic.history.push(payload.down || 0);
                if (vpnState.traffic.history.length > 60) vpnState.traffic.history.shift();
                vpnUpdateRates();
                break;
            case 'vpn://log':
                vpnState.logs.push(payload);
                if (vpnState.logs.length > 800) vpnState.logs.shift();
                if (vpnState.tab === 'logs' && !vpnState.logsPaused) vpnAppendLogRow(payload);
                break;
            case 'vpn://diag':
                vpnState.diag = payload;
                if (vpnState.modal === 'diag') vpnRenderLayer();
                break;
            case 'vpn://profiles':
                vpnState.profiles = payload.profiles || [];
                vpnState.activeId = payload.active_id || '';
                if (vpnState.tab === 'profiles' || !vpnState.profiles.length) vpnRender();
                break;
            case 'vpn://notice':
                if (payload && payload.message) vpnToast(payload.message);
                break;
            default:
                break;
        }
    }

    // ─────────────────────────── 定时器 ───────────────────────────
    function vpnStartTimers() {
        vpnStopTimers();
        vpnTimers.push(setInterval(() => {
            if (!vpnMounted || document.visibilityState !== 'visible') return;
            if (vpnState.tab === 'connections') vpnLoadConnections();
        }, 2000));
        vpnTimers.push(setInterval(() => {
            if (!vpnMounted || document.visibilityState !== 'visible') return;
            if (vpnState.tab === 'proxies' && vpnState.state && vpnState.state.core.state === 'running') {
                vpnLoadProxies().then(() => { if (vpnMounted && vpnState.tab === 'proxies') vpnRender(); });
            }
        }, 15000));
    }

    function vpnStopTimers() {
        vpnTimers.forEach((timer) => clearInterval(timer));
        vpnTimers = [];
    }

    // ─────────────────────────── 挂载 / 卸载 ───────────────────────────
    async function vpnMount(container, options) {
        if (!container) return;
        if (vpnMounted && vpnRoot === container) {
            vpnRoot.style.display = 'flex';
            return;
        }
        vpnDestroy(true);
        vpnRoot = container;
        vpnMounted = true;
        const token = ++vpnToken;
        container.classList.add('vpn-view');
        container.style.display = 'flex';
        container.style.flexDirection = 'column';
        container.innerHTML = vpnTemplate();
        vpnBindEvents();
        vpnRenderStatus();
        vpnRenderTabs();
        try {
            for (let attempt = 0; attempt < 10; attempt += 1) {
                await Promise.all([vpnRefreshState(), vpnLoadSettings(), vpnLoadProfiles()]);
                if (vpnToken !== token) return;
                if (vpnState.state) break;
                await vpnSleep(300);
            }
            if (!vpnState.state) throw new Error('后台尚未就绪，请稍后重试');
            if (vpnState.profiles.length) await vpnLoadProxies();
            if (options && options.tab) vpnState.tab = options.tab;
            // 深链/会话恢复进入非默认 tab 时，补齐该 tab 所需数据
            if (vpnState.tab === 'rules') await vpnLoadRules();
            if (vpnState.tab === 'logs') await vpnLoadLogs();
            if (vpnState.tab === 'connections') await vpnLoadConnections();
            vpnRender();
            vpnStartTimers();
            vpnSubscribe(token).catch((e) => console.warn('[vpn] 事件订阅失败:', e));
        } catch (e) {
            if (vpnToken !== token) return;
            console.error('[vpn] 挂载失败', e);
            const body = vpnEl('vpn-body');
            if (body) {
                body.innerHTML = `<div class="vpn-empty"><div class="vpn-empty-ic">${vpnIcon('M12 3v18M5 8l7-5 7 5v8l-7 5-7-5V8z', 28)}</div>
                    <h2>加载失败</h2><p>${vpnEsc(e && e.message)}</p>
                    <div class="vpn-empty-actions"><button class="btn btn-primary" data-vpn-act="retry">重试</button></div></div>`;
            }
        }
    }

    function vpnDestroy(silent) {
        vpnToken += 1;
        vpnMounted = false;
        vpnStopTimers();
        vpnUnlisten.forEach((fn) => {
            try { fn(); } catch (e) { /* ignore */ }
        });
        vpnUnlisten = [];
        if (vpnRoot) {
            vpnRoot.removeEventListener('click', vpnOnClick);
            vpnRoot.removeEventListener('input', vpnOnInput);
            vpnRoot.removeEventListener('change', vpnOnChange);
            vpnRoot.innerHTML = '';
        }
        vpnRoot = null;
        if (!silent) {
            vpnState.state = null;
            vpnState.profiles = [];
            vpnState.proxies = [];
            vpnState.rules = [];
            vpnState.logs = [];
            vpnState.conns = { connections: [], upload: 0, download: 0 };
            vpnState.settings = null;
            vpnState.diag = null;
            vpnState.modal = '';
            vpnState.modalData = null;
            vpnState.tab = 'proxies';
            vpnState.groupIdx = 0;
            vpnState.pickerGroup = null;
            vpnState.testingGroup = '';
            vpnState.nodeQuery = '';
            vpnState.ruleQuery = '';
            vpnState.connQuery = '';
            vpnState.logsQuery = '';
            vpnState.logsLevel = 'all';
            vpnState.logsPaused = false;
            vpnState.busy = false;
            vpnState.progress = '';
            vpnState.traffic = { up: 0, down: 0, history: [] };
        }
    }

    window.vpnApp = {
        version: VPN_VERSION,
        mount: vpnMount,
        destroy: vpnDestroy,
        isMounted: () => vpnMounted,
    };
})();
