//! 系统托盘模块
//!
//! 提供主流桌面应用的「关闭到托盘」体验：
//! - Windows/Linux：主窗口点击右上角关闭按钮 → 隐藏到系统托盘（拦截 CloseRequested）
//! - macOS：遵循系统原生逻辑，不拦截关闭（红 X 关闭窗口，进程保留、Dock 可见），
//!   点击 Dock 图标或托盘「显示」时重新创建/聚焦主窗口
//! - 托盘左键单击（非 macOS）→ 显示并聚焦主窗口
//! - 托盘/菜单栏图标右键菜单：
//!   - 「显示」→ 显示并聚焦主窗口
//!   - 「退出」→ 立即退出应用，终止进程

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder,
};

/// 托盘菜单项 ID
const MENU_SHOW: &str = "show";
const MENU_QUIT: &str = "quit";

/// 主窗口标签（与 tauri.conf.json 中一致）
const MAIN_WINDOW_LABEL: &str = "main";

/// 创建并注册系统托盘图标与菜单
pub fn setup_tray<R: Runtime>(app: &mut tauri::App<R>) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, MENU_SHOW, "显示", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, MENU_QUIT, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

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
                app.exit(0);
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

    Ok(())
}

/// 显示并聚焦主窗口；若窗口已被原生关闭（macOS 红 X），则重新创建
pub fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        // 依次：显示 → 取消最小化 → 聚焦前台
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    } else {
        // macOS：窗口已被原生关闭 → 按 tauri.conf.json 配置重新创建
        create_main_window(app);
    }
}

/// 按 tauri.conf.json 的主窗口配置重新创建主窗口（macOS 原生关闭后恢复用）
fn create_main_window<R: Runtime>(app: &AppHandle<R>) {
    // 优先从 tauri.conf.json 读取主窗口配置，避免硬编码与配置重复导致漂移
    let result = match app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == MAIN_WINDOW_LABEL)
    {
        Some(cfg) => WebviewWindowBuilder::from_config(app, cfg).and_then(|b| b.build()),
        // 配置缺失时按默认配置兜底
        None => WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::default()).build(),
    };
    match result {
        Ok(window) => {
            let _ = window.set_focus();
        }
        Err(e) => log::error!("[tray] 重新创建主窗口失败: {}", e),
    }
}
