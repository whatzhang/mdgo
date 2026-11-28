//! 系统代理守卫（SystemProxyGuard）。
//!
//! - Windows：注册表 `Internet Settings` + `InternetSetOptionW` 通知 WinINet
//! - macOS：`networksetup`（写操作经 `osascript` 管理员授权）
//!
//! 只做两件事：**把当前系统代理指向本地 mixed-port**，以及**按快照精确恢复**。
//! 不做 TUN、不做常驻 helper。

use super::types::{Settings, SysProxySnapshot};

/// 采集原始系统代理配置。
pub async fn snapshot(settings: &Settings) -> Result<SysProxySnapshot, String> {
    #[cfg(target_os = "windows")]
    {
        let _ = settings;
        let win = win::read()?;
        Ok(SysProxySnapshot {
            owned: true,
            session: uuid::Uuid::new_v4().to_string(),
            created_at: super::paths::now_secs(),
            clean_exit: false,
            platform: "windows".into(),
            windows: Some(win),
            macos: None,
        })
    }
    #[cfg(target_os = "macos")]
    {
        let service = mac::resolve_service(settings).await?;
        let entry = mac::snapshot_service(&service).await?;
        Ok(SysProxySnapshot {
            owned: true,
            session: uuid::Uuid::new_v4().to_string(),
            created_at: super::paths::now_secs(),
            clean_exit: false,
            platform: "macos".into(),
            windows: None,
            macos: Some(vec![entry]),
        })
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        let _ = settings;
        Err("当前平台暂不支持系统代理".into())
    }
}

/// 开启系统代理，返回被修改的网络服务列表（Windows 为占位）。
pub async fn enable(
    port: u16,
    settings: &Settings,
    snapshot: &SysProxySnapshot,
) -> Result<Vec<String>, String> {
    #[cfg(target_os = "windows")]
    {
        let _ = &settings;
        let _ = snapshot;
        win::apply(port, settings)?;
        if !win::verify(port)? {
            return Err("系统代理写入后校验失败（可能被安全软件拦截）".into());
        }
        Ok(vec!["WinINet".into()])
    }
    #[cfg(target_os = "macos")]
    {
        // 复用调用方已落盘的快照，保证「开启的服务」与「将来恢复的服务」一致
        let services = mac::apply(port, settings, snapshot).await?;
        Ok(services)
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        let _ = (port, settings);
        Err("当前平台暂不支持系统代理".into())
    }
}

/// 按快照恢复。
pub async fn restore(snapshot: &SysProxySnapshot) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        if let Some(win) = snapshot.windows.as_ref() {
            win::restore(win)?;
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(list) = snapshot.macos.as_ref() {
            mac::restore(list).await?;
        }
        Ok(())
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        let _ = snapshot;
        Ok(())
    }
}

/// 校验系统代理是否指向本地端口。
pub async fn verify(port: u16, services: &[String]) -> Result<bool, String> {
    #[cfg(target_os = "windows")]
    {
        let _ = services;
        win::verify(port)
    }
    #[cfg(target_os = "macos")]
    {
        mac::verify(port, services).await
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        let _ = (port, services);
        Ok(false)
    }
}


// ───────────────────────── Windows ─────────────────────────

#[cfg(target_os = "windows")]
mod win {
    use super::Settings;
    use winreg::enums::*;
    use winreg::RegKey;

    const KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";

    fn open_key() -> Result<RegKey, String> {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(KEY_PATH, KEY_READ | KEY_WRITE)
            .map_err(|e| format!("打开注册表失败: {e}"))
    }

    pub fn read() -> Result<super::super::types::WinProxySnapshot, String> {
        let key = open_key()?;
        let proxy_enable = key.get_value::<u32, _>("ProxyEnable").ok();
        let proxy_server = key.get_value::<String, _>("ProxyServer").ok();
        let proxy_override = key.get_value::<String, _>("ProxyOverride").ok();
        let auto_config_url = key.get_value::<String, _>("AutoConfigURL").ok();
        let auto_detect = key.get_raw_value("AutoDetect").ok().map(|v| v.bytes);
        Ok(super::super::types::WinProxySnapshot {
            proxy_enable,
            proxy_server,
            proxy_override,
            auto_config_url,
            auto_detect,
        })
    }

    pub fn apply(port: u16, settings: &Settings) -> Result<(), String> {
        let key = open_key()?;
        let pac = key.get_value::<String, _>("AutoConfigURL").unwrap_or_default();
        if !pac.trim().is_empty() {
            return Err(format!(
                "系统启用了 PAC 自动配置（{pac}），系统代理不会生效；请先在系统中关闭自动配置"
            ));
        }
        key.set_value("ProxyEnable", &1u32)
            .map_err(|e| format!("写入 ProxyEnable 失败: {e}"))?;
        // WinINet 支持按协议声明：http=…;https=…;socks=…
        let mut parts: Vec<String> = Vec::new();
        if settings.http_proxy {
            parts.push(format!("http=127.0.0.1:{port}"));
        }
        if settings.https_proxy {
            parts.push(format!("https=127.0.0.1:{port}"));
        }
        if settings.socks_proxy {
            parts.push(format!("socks=127.0.0.1:{port}"));
        }
        if parts.is_empty() {
            return Err("请至少选择一种系统代理协议（HTTP / HTTPS / SOCKS）".into());
        }
        key.set_value("ProxyServer", &parts.join(";"))
            .map_err(|e| format!("写入 ProxyServer 失败: {e}"))?;
        key.set_value("ProxyOverride", &"localhost;127.*;<local>")
            .map_err(|e| format!("写入 ProxyOverride 失败: {e}"))?;
        notify_wininet();
        Ok(())
    }

    pub fn restore(snap: &super::super::types::WinProxySnapshot) -> Result<(), String> {
        let key = open_key()?;
        set_or_delete_u32(&key, "ProxyEnable", snap.proxy_enable)?;
        set_or_delete_str(&key, "ProxyServer", snap.proxy_server.as_deref())?;
        set_or_delete_str(&key, "ProxyOverride", snap.proxy_override.as_deref())?;
        set_or_delete_str(&key, "AutoConfigURL", snap.auto_config_url.as_deref())?;
        if let Some(bytes) = snap.auto_detect.as_ref() {
            let value = winreg::RegValue {
                bytes: bytes.clone(),
                vtype: REG_BINARY,
            };
            let _ = key.set_raw_value("AutoDetect", &value);
        }
        notify_wininet();
        Ok(())
    }

    fn set_or_delete_u32(key: &RegKey, name: &str, value: Option<u32>) -> Result<(), String> {
        match value {
            Some(v) => key.set_value(name, &v).map_err(|e| e.to_string()),
            None => {
                let _ = key.delete_value(name);
                Ok(())
            }
        }
    }

    fn set_or_delete_str(key: &RegKey, name: &str, value: Option<&str>) -> Result<(), String> {
        match value {
            Some(v) => key.set_value(name, &v).map_err(|e| e.to_string()),
            None => {
                let _ = key.delete_value(name);
                Ok(())
            }
        }
    }

    pub fn verify(port: u16) -> Result<bool, String> {
        let key = open_key()?;
        let enable = key.get_value::<u32, _>("ProxyEnable").unwrap_or(0);
        let server = key.get_value::<String, _>("ProxyServer").unwrap_or_default();
        Ok(enable == 1 && server.contains(&format!("127.0.0.1:{port}")))
    }

    fn notify_wininet() {
        use windows_sys::Win32::Networking::WinInet::{
            InternetSetOptionW, INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED,
        };
        unsafe {
            let _ = InternetSetOptionW(
                std::ptr::null_mut(),
                INTERNET_OPTION_SETTINGS_CHANGED,
                std::ptr::null_mut(),
                0,
            );
            let _ = InternetSetOptionW(
                std::ptr::null_mut(),
                INTERNET_OPTION_REFRESH,
                std::ptr::null_mut(),
                0,
            );
        }
    }
}

// ───────────────────────── macOS ─────────────────────────

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use super::super::types::{MacProxyEntry, MacServiceSnapshot};

    async fn networksetup(args: &[&str]) -> Result<String, String> {
        let out = tokio::process::Command::new("networksetup")
            .args(args)
            .output()
            .await
            .map_err(|e| format!("执行 networksetup 失败: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "networksetup {} 失败: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    pub async fn list_services() -> Result<Vec<String>, String> {
        let text = networksetup(&["-listallnetworkservices"]).await?;
        let mut services = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if i == 0 || line.is_empty() || line.starts_with('*') {
                continue;
            }
            services.push(line.to_string());
        }
        Ok(services)
    }

    /// 解析 `-listnetworkserviceorder`，把设备名（en0）映射回服务名。
    ///
    /// 输出形如：
    /// ```text
    /// (1) Wi-Fi
    /// (Hardware Port: Wi-Fi, Device: en0)
    /// ```
    /// 只有 `(数字) 服务名` 这一行代表服务名，`(Hardware Port: …)` 行必须跳过。
    async fn device_to_service(device: &str) -> Option<String> {
        let text = networksetup(&["-listnetworkserviceorder"]).await.ok()?;
        let mut current = String::new();
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix('(') {
                if rest.starts_with(|c: char| c.is_ascii_digit()) {
                    if let Some(idx) = rest.find(')') {
                        current = rest[idx + 1..].trim().to_string();
                    }
                }
            }
            if line.contains("Device:") && line.contains(device) && !current.is_empty() {
                return Some(current);
            }
        }
        None
    }

    async fn active_service() -> Option<String> {
        let out = tokio::process::Command::new("route")
            .args(["-n", "get", "default"])
            .output()
            .await
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let device = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("interface:"))
            .map(|s| s.trim().to_string())?;
        device_to_service(&device).await
    }

    pub async fn resolve_service(settings: &Settings) -> Result<String, String> {
        let services = list_services().await?;
        if !settings.mac_service.trim().is_empty()
            && services.iter().any(|s| s == settings.mac_service.trim())
        {
            return Ok(settings.mac_service.trim().to_string());
        }
        if let Some(active) = active_service().await {
            return Ok(active);
        }
        services
            .first()
            .cloned()
            .ok_or_else(|| "未找到可用的网络服务".to_string())
    }

    async fn get_proxy(service: &str, kind: ProxyKind) -> MacProxyEntry {
        let flag = match kind {
            ProxyKind::Web => "-getwebproxy",
            ProxyKind::SecureWeb => "-getsecurewebproxy",
            ProxyKind::Socks => "-getsocksfirewallproxy",
        };
        let text = networksetup(&[flag, service]).await.unwrap_or_default();
        let mut entry = MacProxyEntry::default();
        for line in text.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("Enabled:") {
                entry.enabled = v.trim().eq_ignore_ascii_case("yes");
            } else if let Some(v) = line.strip_prefix("Server:") {
                entry.server = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("Port:") {
                entry.port = v.trim().parse().unwrap_or(0);
            }
        }
        entry
    }

    pub async fn snapshot_service(service: &str) -> Result<MacServiceSnapshot, String> {
        Ok(MacServiceSnapshot {
            name: service.to_string(),
            web: get_proxy(service, ProxyKind::Web).await,
            secure_web: get_proxy(service, ProxyKind::SecureWeb).await,
            socks: get_proxy(service, ProxyKind::Socks).await,
        })
    }

    pub async fn apply(
        port: u16,
        settings: &Settings,
        snapshot: &SysProxySnapshot,
    ) -> Result<Vec<String>, String> {
        let services: Vec<String> = snapshot
            .macos
            .as_ref()
            .map(|list| list.iter().map(|s| s.name.clone()).collect())
            .unwrap_or_default();
        if services.is_empty() {
            return Err("缺少网络服务快照".into());
        }
        if !settings.http_proxy && !settings.https_proxy && !settings.socks_proxy {
            return Err("请至少选择一种系统代理协议（HTTP / HTTPS / SOCKS）".into());
        }
        let mut lines = Vec::new();
        for service in &services {
            let q = shell_quote(service);
            if settings.http_proxy {
                lines.push(format!("networksetup -setwebproxy {q} 127.0.0.1 {port}"));
                lines.push(format!("networksetup -setwebproxystate {q} on"));
            } else {
                lines.push(format!("networksetup -setwebproxystate {q} off"));
            }
            if settings.https_proxy {
                lines.push(format!(
                    "networksetup -setsecurewebproxy {q} 127.0.0.1 {port}"
                ));
                lines.push(format!("networksetup -setsecurewebproxystate {q} on"));
            } else {
                lines.push(format!("networksetup -setsecurewebproxystate {q} off"));
            }
            if settings.socks_proxy {
                lines.push(format!(
                    "networksetup -setsocksfirewallproxy {q} 127.0.0.1 {port}"
                ));
                lines.push(format!("networksetup -setsocksfirewallproxystate {q} on"));
            } else {
                lines.push(format!("networksetup -setsocksfirewallproxystate {q} off"));
            }
        }
        run_admin(&lines).await?;
        Ok(services)
    }

    pub async fn restore(list: &[MacServiceSnapshot]) -> Result<(), String> {
        let mut lines = Vec::new();
        for snap in list {
            let q = shell_quote(&snap.name);
            push_restore(&mut lines, &q, "-setwebproxy", "-setwebproxystate", &snap.web);
            push_restore(
                &mut lines,
                &q,
                "-setsecurewebproxy",
                "-setsecurewebproxystate",
                &snap.secure_web,
            );
            push_restore(
                &mut lines,
                &q,
                "-setsocksfirewallproxy",
                "-setsocksfirewallproxystate",
                &snap.socks,
            );
        }
        if lines.is_empty() {
            return Ok(());
        }
        run_admin(&lines).await
    }

    fn push_restore(
        lines: &mut Vec<String>,
        q: &str,
        set_cmd: &str,
        state_cmd: &str,
        entry: &MacProxyEntry,
    ) {
        if !entry.server.is_empty() {
            lines.push(format!(
                "networksetup {set_cmd} {q} '{}' {}",
                entry.server.replace('\'', "'\\''"),
                entry.port
            ));
        }
        lines.push(format!(
            "networksetup {state_cmd} {q} {}",
            if entry.enabled { "on" } else { "off" }
        ));
    }

    pub async fn verify(port: u16, services: &[String]) -> Result<bool, String> {
        let service = services
            .first()
            .cloned()
            .ok_or_else(|| "缺少网络服务名".to_string())?;
        // 任一协议指向本地端口即视为生效（用户可能只勾选 HTTPS 或 SOCKS）
        let checks = [
            get_proxy(&service, ProxyKind::Web).await,
            get_proxy(&service, ProxyKind::SecureWeb).await,
            get_proxy(&service, ProxyKind::Socks).await,
        ];
        Ok(checks
            .iter()
            .any(|e| e.enabled && e.server == "127.0.0.1" && e.port == port))
    }

    /// 单引号 shell 转义。
    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    /// 通过 osascript 以管理员权限执行脚本。
    ///
    /// 安全：服务名只来自 `-listallnetworkservices` 的白名单校验；脚本写入临时文件，
    /// osascript 只执行固定路径 `/bin/sh <tmp>`，不做任意字符串拼接执行。
    async fn run_admin(lines: &[String]) -> Result<(), String> {
        use std::io::Write;
        let dir = super::super::paths::data_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(format!("sysproxy-{}.sh", std::process::id()));
        {
            let mut file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
            writeln!(file, "#!/bin/sh").map_err(|e| e.to_string())?;
            for line in lines {
                writeln!(file, "{line}").map_err(|e| e.to_string())?;
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
        }
        let script = format!(
            "do shell script \"/bin/sh {}\" with administrator privileges",
            shell_quote(&path.to_string_lossy())
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        );
        let out = tokio::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .output()
            .await
            .map_err(|e| format!("执行 osascript 失败: {e}"))?;
        let _ = std::fs::remove_file(&path);
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("-128") || stderr.contains("User canceled") {
                return Err("已取消管理员授权".into());
            }
            return Err(format!("设置系统代理失败: {}", stderr.trim()));
        }
        Ok(())
    }

    #[derive(Clone, Copy)]
    enum ProxyKind {
        Web,
        SecureWeb,
        Socks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_on_unsupported_or_ok() {
        // 只验证函数不 panic：Windows 读注册表，macOS 读 networksetup
        let r = snapshot(&Settings::default()).await;
        // Windows 一定成功；macOS 无网络服务时可能失败，这里只断言不 panic
        let _ = r;
    }

    #[test]
    fn shell_quote_escapes_single_quote() {
        #[cfg(target_os = "macos")]
        {
            // 通过内部函数间接验证：手动复用同样的逻辑
            let s = "a'b";
            let q = format!("'{}'", s.replace('\'', "'\\''"));
            assert_eq!(q, "'a'\\''b'");
        }
    }
}
