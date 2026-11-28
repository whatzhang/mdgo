//! VPN 模块的 Tauri 命令门面（薄层：仅参数转换 + 调用 `VpnManager`）。

use std::sync::Arc;

use tauri::{AppHandle, Manager};

use crate::core::vpn::types::*;
use crate::core::vpn::VpnManager;

fn manager(app: &AppHandle) -> Arc<VpnManager> {
    app.state::<Arc<VpnManager>>().inner().clone()
}

#[tauri::command]
pub fn vpn_state(app: AppHandle) -> VpnState {
    manager(&app).state()
}

#[tauri::command]
pub async fn vpn_connect(app: AppHandle) -> Result<VpnState, String> {
    manager(&app).connect().await
}

#[tauri::command]
pub async fn vpn_disconnect(app: AppHandle) -> Result<VpnState, String> {
    manager(&app).disconnect().await
}

#[tauri::command]
pub async fn vpn_restart_core(app: AppHandle) -> Result<VpnState, String> {
    manager(&app).restart_core().await
}

#[tauri::command]
pub async fn vpn_diag_run(app: AppHandle) -> Result<Diagnostics, String> {
    Ok(manager(&app).run_diagnostics().await)
}

#[tauri::command]
pub async fn vpn_proxies(app: AppHandle) -> Result<Vec<ProxyGroup>, String> {
    let groups = manager(&app).proxies().await?;
    let current = crate::tray::current_node_from_groups(&groups);
    crate::tray::sync_nodes(&app, groups.clone(), current);
    Ok(groups)
}

#[tauri::command]
pub async fn vpn_select_proxy(app: AppHandle, group: String, name: String) -> Result<(), String> {
    manager(&app).select_proxy(&group, &name).await
}

#[tauri::command]
pub async fn vpn_delay(app: AppHandle, name: String) -> Result<DelayResult, String> {
    manager(&app).delay(&name).await
}

#[tauri::command]
pub async fn vpn_group_delay(
    app: AppHandle,
    group: String,
) -> Result<std::collections::HashMap<String, u64>, String> {
    manager(&app).group_delay(&group).await
}

#[tauri::command]
pub async fn vpn_mode_set(app: AppHandle, mode: String) -> Result<VpnState, String> {
    manager(&app).set_mode(&mode).await
}

#[tauri::command]
pub async fn vpn_rules(app: AppHandle) -> Result<Vec<RuleItem>, String> {
    manager(&app).rules().await
}

#[tauri::command]
pub async fn vpn_connections(app: AppHandle) -> Result<ConnectionsSnapshot, String> {
    manager(&app).connections().await
}

#[tauri::command]
pub async fn vpn_kill_connection(app: AppHandle, id: String) -> Result<(), String> {
    manager(&app).kill_connection(&id).await
}

#[tauri::command]
pub async fn vpn_kill_all(app: AppHandle) -> Result<(), String> {
    manager(&app).kill_all_connections().await
}

#[tauri::command]
pub fn vpn_logs(app: AppHandle, level: Option<String>, limit: Option<usize>) -> Vec<LogEntry> {
    manager(&app).logs(level.as_deref().unwrap_or("all"), limit.unwrap_or(500))
}

#[tauri::command]
pub fn vpn_logs_clear(app: AppHandle) {
    manager(&app).clear_logs();
}

#[tauri::command]
pub fn vpn_profiles(app: AppHandle) -> serde_json::Value {
    let (profiles, active_id) = manager(&app).profiles_list();
    serde_json::json!({ "profiles": profiles, "active_id": active_id })
}

#[tauri::command]
pub async fn vpn_profile_add(app: AppHandle, input: ProfileAddInput) -> Result<Profile, String> {
    manager(&app)
        .add_profile(input)
        .await
        .map_err(|e| crate::core::vpn::mask_secrets(&e))
}

#[tauri::command]
pub async fn vpn_profile_update(app: AppHandle, id: String) -> Result<Profile, String> {
    manager(&app)
        .update_profile(&id)
        .await
        .map_err(|e| crate::core::vpn::mask_secrets(&e))
}

#[tauri::command]
pub fn vpn_profile_remove(app: AppHandle, id: String) -> Result<(), String> {
    manager(&app).remove_profile(&id)
}

#[tauri::command]
pub async fn vpn_profile_activate(app: AppHandle, id: String) -> Result<(), String> {
    manager(&app).activate_profile(&id).await
}

#[tauri::command]
pub fn vpn_profile_versions(app: AppHandle, id: String) -> Vec<ProfileVersion> {
    manager(&app).versions_for(&id)
}

#[tauri::command]
pub async fn vpn_profile_rollback(app: AppHandle, id: String, ts: i64) -> Result<(), String> {
    manager(&app).rollback_profile(&id, ts).await
}

#[tauri::command]
pub fn vpn_settings_get(app: AppHandle) -> serde_json::Value {
    let settings = manager(&app).settings();
    serde_json::json!({
        "mixed_port": settings.mixed_port,
        "controller_port": settings.controller_port,
        "auto_ports": settings.auto_ports,
        "mode": settings.mode,
        "log_level": settings.log_level,
        "auto_system_proxy": settings.auto_system_proxy,
        "http_proxy": settings.http_proxy,
        "https_proxy": settings.https_proxy,
        "socks_proxy": settings.socks_proxy,
        "restore_on_exit": settings.restore_on_exit,
        "recover_on_crash": settings.recover_on_crash,
        "autostart": settings.autostart,
        "auto_connect": settings.auto_connect,
        "auto_connect_last": settings.auto_connect_last,
        "default_interval_hours": settings.default_interval_hours,
        "notify_on_update_fail": settings.notify_on_update_fail,
        "test_url": settings.test_url,
        "test_timeout_ms": settings.test_timeout_ms,
        "diag_timeout_ms": settings.diag_timeout_ms,
        "dns_enable": settings.dns_enable,
        "dns_override": settings.dns_override,
        "dns_nameservers": settings.dns_nameservers,
        "find_process": settings.find_process,
        "mac_service": settings.mac_service,
        "bin_path": settings.bin_path,
        "data_dir": crate::core::vpn::paths::data_dir().to_string_lossy(),
        "has_snapshot": settings.sys_proxy_snapshot.is_some(),
    })
}

#[tauri::command]
pub async fn vpn_settings_set(app: AppHandle, patch: SettingsPatch) -> Result<(), String> {
    manager(&app).apply_settings(patch).await.map(|_| ())
}

#[tauri::command]
pub async fn vpn_sysproxy_set(app: AppHandle, on: bool) -> Result<VpnState, String> {
    manager(&app).set_system_proxy(on).await
}

#[tauri::command]
pub fn vpn_network_state(app: AppHandle) -> NetworkState {
    manager(&app).network_state()
}

#[tauri::command]
pub fn vpn_open_data_dir(app: AppHandle) -> Result<(), String> {
    manager(&app).open_data_dir()
}

/// 选择本地 YAML 配置（iframe 内不直接使用 dialog 插件 API，统一走宿主）。
#[tauri::command]
pub async fn vpn_pick_yaml(app: AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .add_filter("Clash 配置", &["yaml", "yml"])
        .pick_file(move |path| {
            let _ = tx.send(path);
        });
    rx.await
        .ok()
        .flatten()
        .and_then(|path| path.into_path().ok())
        .map(|p| p.to_string_lossy().to_string())
}
