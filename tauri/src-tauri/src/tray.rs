//! 系统托盘模块
//!
//! 提供主流桌面应用的「关闭到托盘」体验：
//! - Windows/Linux：主窗口点击右上角关闭按钮 → 隐藏到系统托盘（拦截 CloseRequested）
//! - macOS：遵循系统原生逻辑，不拦截关闭（红 X 关闭窗口，进程保留、Dock 可见），
//!   点击 Dock 图标或托盘「显示」时重新创建/聚焦主窗口
//! - 托盘左键单击（非 macOS）→ 显示并聚焦主窗口
//! - 托盘/菜单栏图标右键菜单：显示 / VPN 状态 / 开关代理 / 路由模式 / 退出

use std::collections::HashMap;
use std::sync::Mutex;

use tauri::{
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder,
};

use crate::core::vpn::types::{ProxyGroup, ProxyNode, VpnState};

/// 托盘菜单项 ID
const MENU_SHOW: &str = "show";
const MENU_QUIT: &str = "quit";
const MENU_VPN_STATUS: &str = "vpn_status";
const MENU_VPN_TOGGLE: &str = "vpn_toggle";
const MENU_VPN_MODE_RULE: &str = "vpn_mode_rule";
const MENU_VPN_MODE_GLOBAL: &str = "vpn_mode_global";
const MENU_VPN_MODE_DIRECT: &str = "vpn_mode_direct";

/// 主窗口标签（与 tauri.conf.json 一致）
const MAIN_WINDOW_LABEL: &str = "main";

/// 托盘里需要动态更新的菜单项（供 `VpnManager` 推送状态时刷新文案）。
pub struct VpnTrayItems {
    pub status: MenuItem<tauri::Wry>,
    pub toggle: MenuItem<tauri::Wry>,
    pub mode_rule: CheckMenuItem<tauri::Wry>,
    pub mode_global: CheckMenuItem<tauri::Wry>,
    pub mode_direct: CheckMenuItem<tauri::Wry>,
}

impl VpnTrayItems {
    /// 根据 VPN 状态刷新托盘文案。
    pub fn update(&self, state: &VpnState) {
        let (status_text, toggle_text) = build_texts(state);
        let _ = self.status.set_text(status_text);
        let _ = self.toggle.set_text(toggle_text);
        let mode = state.mode.as_str();
        let _ = self.mode_rule.set_checked(mode == "rule");
        let _ = self.mode_global.set_checked(mode == "global");
        let _ = self.mode_direct.set_checked(mode == "direct");
    }
}

/// 托盘「当前节点」级联菜单的运行时状态。
pub struct TrayNodeState {
    menu: Submenu<tauri::Wry>,
    groups: Mutex<Vec<ProxyGroup>>,
    current_node: Mutex<String>,
    signature: Mutex<String>,
    node_map: Mutex<HashMap<String, (String, String)>>,
    rebuild_lock: Mutex<()>,
}

/// 去掉节点名开头的国旗 emoji，避免 Windows 上显示成 GB 前缀。
fn display_node_name(name: &str) -> String {
    if let Some((prefix, rest)) = name.split_once(' ') {
        if !prefix.is_empty()
            && prefix
                .chars()
                .all(|ch| ('\u{1F1E6}'..='\u{1F1FF}').contains(&ch))
        {
            return rest.trim_start().to_string();
        }
    }
    name.to_string()
}

fn node_signature(groups: &[ProxyGroup], current: &str) -> String {
    let mut out = String::new();
    for group in groups {
        out.push_str(&group.name);
        out.push(':');
        out.push_str(&group.now);
        out.push(':');
        out.push_str(&group.nodes.len().to_string());
        out.push('|');
    }
    out.push_str(current);
    out
}

fn build_texts(state: &VpnState) -> (String, String) {
    let conn = state.connection.state.as_str();
    let status = match conn {
        "connected" => "VPN：已连接".to_string(),
        "connecting" => "VPN：连接中…".to_string(),
        "degraded" => "VPN：已开启（系统代理异常）".to_string(),
        _ => match state.core.state.as_str() {
            "running" => "VPN：内核运行中".to_string(),
            "error" => "VPN：内核异常".to_string(),
            _ => "VPN：未开启".to_string(),
        },
    };
    let toggle = match conn {
        "connected" | "connecting" | "degraded" => "关闭代理".to_string(),
        _ => "开启代理".to_string(),
    };
    (status, toggle)
}

/// 创建并注册系统托盘图标与菜单
pub fn setup_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, MENU_SHOW, "显示", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, MENU_QUIT, "退出", true, None::<&str>)?;
    let vpn_status = MenuItem::with_id(app, MENU_VPN_STATUS, "VPN：未开启", false, None::<&str>)?;
    let vpn_toggle = MenuItem::with_id(app, MENU_VPN_TOGGLE, "开启代理", true, None::<&str>)?;
    let initial_mode = app
        .try_state::<std::sync::Arc<crate::core::vpn::VpnManager>>()
        .map(|state| state.inner().state().mode)
        .unwrap_or_else(|| "rule".to_string());
    let mode_rule = CheckMenuItem::with_id(
        app,
        MENU_VPN_MODE_RULE,
        "规则模式",
        true,
        initial_mode == "rule",
        None::<&str>,
    )?;
    let mode_global = CheckMenuItem::with_id(
        app,
        MENU_VPN_MODE_GLOBAL,
        "全局模式",
        true,
        initial_mode == "global",
        None::<&str>,
    )?;
    let mode_direct = CheckMenuItem::with_id(
        app,
        MENU_VPN_MODE_DIRECT,
        "直连模式",
        true,
        initial_mode == "direct",
        None::<&str>,
    )?;
    let vpn_mode = Submenu::with_items(
        app,
        "路由模式",
        true,
        &[&mode_rule, &mode_global, &mode_direct],
    )?;
    let node_menu = Submenu::with_id(app, "vpn_current_node", "当前节点：—", true)?;
    let node_empty = MenuItem::with_id(app, "vpn_node_empty", "暂无节点", false, None::<&str>)?;
    node_menu.append(&node_empty)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[
            &show_item,
            &sep1,
            &vpn_status,
            &vpn_toggle,
            &vpn_mode,
            &node_menu,
            &sep2,
            &quit_item,
        ],
    )?;

    // 优先使用应用默认图标；兜底读取内置 32x32 图标
    let icon = app.default_window_icon().cloned().unwrap_or_else(|| {
        tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))
            .expect("加载托盘图标失败")
    });

    // macOS：左键单击菜单栏图标弹出菜单（系统原生习惯，右键也是菜单）；
    // Windows/Linux：左键单击直接显示主窗口，右键弹出菜单。
    let show_menu_on_left_click = cfg!(target_os = "macos");

    TrayIconBuilder::with_id("main-tray")
        .icon(icon)
        .tooltip("mdgo")
        .menu(&menu)
        .show_menu_on_left_click(show_menu_on_left_click)
        .on_menu_event(|app, event| match event.id.as_ref() {
            MENU_SHOW => show_main_window(app),
            MENU_QUIT => {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(state) = handle.try_state::<std::sync::Arc<crate::core::vpn::VpnManager>>() {
                        state.inner().clone().shutdown().await;
                    }
                    handle.exit(0);
                });
            }
            MENU_VPN_TOGGLE => {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    match handle.try_state::<std::sync::Arc<crate::core::vpn::VpnManager>>() {
                        Some(state) => {
                            let manager = state.inner().clone();
                            let current = manager.state();
                            let result = match current.connection.state.as_str() {
                                "connected" | "degraded" | "connecting" => {
                                    manager.disconnect().await
                                }
                                _ => manager.connect().await,
                            };
                            if let Err(e) = result {
                                log::warn!("[tray] VPN 切换失败: {e}");
                            }
                        }
                        None => log::warn!("[tray] VPN 管理器未初始化"),
                    }
                });
            }
            MENU_VPN_MODE_RULE | MENU_VPN_MODE_GLOBAL | MENU_VPN_MODE_DIRECT => {
                let mode = match event.id.as_ref() {
                    MENU_VPN_MODE_GLOBAL => "global",
                    MENU_VPN_MODE_DIRECT => "direct",
                    _ => "rule",
                };
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(state) = handle.try_state::<std::sync::Arc<crate::core::vpn::VpnManager>>() {
                        if let Err(e) = state.inner().clone().set_mode(mode).await {
                            log::warn!("[tray] 切换路由模式失败: {e}");
                        }
                    }
                });
            }
            id if id.starts_with("vpn_node_") => {
                let id = id.to_string();
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    select_tray_node(handle, id).await;
                });
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // 非 macOS：左键单击托盘图标 → 显示并聚焦主窗口（主流应用行为）
            if cfg!(not(target_os = "macos")) {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    show_main_window(tray.app_handle());
                }
            }
        })
        .build(app.handle())?;

    // 保存需要动态更新的菜单项，供 VPN 状态变化时刷新
    app.manage(VpnTrayItems {
        status: vpn_status,
        toggle: vpn_toggle,
        mode_rule,
        mode_global,
        mode_direct,
    });
    app.manage(TrayNodeState {
        menu: node_menu,
        groups: Mutex::new(Vec::new()),
        current_node: Mutex::new(String::new()),
        signature: Mutex::new(String::new()),
        node_map: Mutex::new(HashMap::new()),
        rebuild_lock: Mutex::new(()),
    });

    // 后台保持托盘节点菜单与内核同步（连接完成/自动选组/周期刷新都会更新）
    let handle = app.handle().clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let Some(state) = handle.try_state::<std::sync::Arc<crate::core::vpn::VpnManager>>() else {
                continue;
            };
            let running = state.inner().state().core.state == "running";
            if !running {
                continue;
            }
            if let Ok(groups) = state.inner().proxies().await {
                let current = current_node_from_groups(&groups);
                sync_nodes(&handle, groups, current);
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        }
    });

    Ok(())
}

/// 从策略组中推导当前节点（优先非 GLOBAL 组）。
pub fn current_node_from_groups(groups: &[ProxyGroup]) -> String {
    groups
        .iter()
        .find(|group| !group.name.eq_ignore_ascii_case("GLOBAL") && !group.now.is_empty())
        .or_else(|| groups.iter().find(|group| !group.now.is_empty()))
        .map(|group| group.now.clone())
        .unwrap_or_default()
}

fn node_rank(node: &ProxyNode) -> u64 {
    match node.delay {
        Some(delay) if delay > 0 => delay,
        _ => u64::MAX,
    }
}

fn node_delay_label(node: &ProxyNode) -> String {
    match node.delay {
        Some(0) => "超时".to_string(),
        Some(delay) => format!("{delay}ms"),
        None => "未测速".to_string(),
    }
}

/// 用最新策略组重建托盘「当前节点」级联菜单；相同快照直接跳过。
pub fn sync_nodes(app: &AppHandle, groups: Vec<ProxyGroup>, current_node: String) {
    let Some(state) = app.try_state::<TrayNodeState>() else {
        return;
    };
    let mut groups = groups;
    groups.retain(|group| !group.name.trim().eq_ignore_ascii_case("GLOBAL"));
    let current = if current_node.trim().is_empty() {
        current_node_from_groups(&groups)
    } else {
        current_node
    };
    let signature = node_signature(&groups, &current);
    let _guard = match state.rebuild_lock.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    if state
        .signature
        .lock()
        .map(|value| value.as_str() == signature)
        .unwrap_or(false)
    {
        return;
    }
    match rebuild_node_menu(app, &state, &groups, &current) {
        Ok(node_map) => {
            if let Ok(mut map) = state.node_map.lock() {
                *map = node_map;
            }
            if let Ok(mut cache) = state.groups.lock() {
                *cache = groups;
            }
            if let Ok(mut node) = state.current_node.lock() {
                *node = current;
            }
            if let Ok(mut sig) = state.signature.lock() {
                *sig = signature;
            }
        }
        Err(e) => log::warn!("[tray] 更新当前节点菜单失败: {e}"),
    }
}

fn rebuild_node_menu(
    app: &AppHandle,
    state: &TrayNodeState,
    groups: &[ProxyGroup],
    current: &str,
) -> tauri::Result<HashMap<String, (String, String)>> {
    while let Ok(Some(_)) = state.menu.remove_at(0) {}
    let title = if current.is_empty() {
        "当前节点：—".to_string()
    } else {
        format!("当前节点：{}", display_node_name(current))
    };
    let _ = state.menu.set_text(title);

    let mut node_map = HashMap::new();
    if groups.is_empty() {
        let empty = MenuItem::with_id(app, "vpn_node_empty", "暂无节点", false, None::<&str>)?;
        state.menu.append(&empty)?;
        return Ok(node_map);
    }

    for (group_index, group) in groups.iter().enumerate() {
        let mut nodes = group.nodes.clone();
        nodes.sort_by(|a, b| node_rank(a).cmp(&node_rank(b)).then_with(|| a.name.cmp(&b.name)));
        if nodes.is_empty() {
            let empty = MenuItem::with_id(
                app,
                format!("vpn_group_empty_{group_index}"),
                "暂无节点",
                false,
                None::<&str>,
            )?;
            let submenu = Submenu::with_items(app, group.name.clone(), true, &[&empty])?;
            state.menu.append(&submenu)?;
            continue;
        }
        let mut items: Vec<CheckMenuItem<tauri::Wry>> = Vec::new();
        for (node_index, node) in nodes.iter().enumerate() {
            let id = format!("vpn_node_{group_index}_{node_index}");
            node_map.insert(id.clone(), (group.name.clone(), node.name.clone()));
            let label = format!("{}  ·  {}", display_node_name(&node.name), node_delay_label(node));
            items.push(CheckMenuItem::with_id(
                app,
                id,
                label,
                true,
                node.name == group.now,
                None::<&str>,
            )?);
        }
        let refs: Vec<&dyn IsMenuItem<tauri::Wry>> =
            items.iter().map(|item| item as &dyn IsMenuItem<tauri::Wry>).collect();
        let submenu = Submenu::with_items(app, group.name.clone(), true, &refs)?;
        state.menu.append(&submenu)?;
    }
    Ok(node_map)
}

async fn select_tray_node(app: AppHandle, id: String) {
    let Some(state) = app.try_state::<TrayNodeState>() else {
        return;
    };
    let target = state
        .node_map
        .lock()
        .ok()
        .and_then(|map| map.get(&id).cloned());
    let Some((group, node)) = target else {
        return;
    };
    let Some(manager) = app.try_state::<std::sync::Arc<crate::core::vpn::VpnManager>>() else {
        return;
    };
    if let Err(e) = manager.inner().clone().select_proxy(&group, &node).await {
        log::warn!("[tray] 切换节点失败: {e}");
        return;
    }
    let (groups, current) = {
        let Ok(mut cache) = state.groups.lock() else {
            return;
        };
        if let Some(group) = cache.iter_mut().find(|item| item.name == group) {
            group.now = node.clone();
        }
        (cache.clone(), node)
    };
    sync_nodes(&app, groups, current);
}

/// 显示并聚焦主窗口；若窗口已被原生关闭（macOS 红 X），则重新创建
pub fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    } else {
        create_main_window(app);
    }
}

/// 按 tauri.conf.json 的主窗口配置重新创建主窗口（macOS 原生关闭后恢复用）
fn create_main_window<R: Runtime>(app: &AppHandle<R>) {
    let result = match app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == MAIN_WINDOW_LABEL)
    {
        Some(cfg) => WebviewWindowBuilder::from_config(app, cfg).and_then(|b| b.build()),
        None => WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::default()).build(),
    };
    match result {
        Ok(window) => {
            let _ = window.set_focus();
        }
        Err(e) => log::error!("[tray] 重新创建主窗口失败: {}", e),
    }
}
