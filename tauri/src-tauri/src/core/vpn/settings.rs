//! VPN 设置持久化（`settings.json`）与开机自启。

use base64::Engine;
#[cfg(target_os = "macos")]
use std::path::PathBuf;

use super::paths;
use super::types::{Settings, SettingsPatch};

pub fn load() -> Settings {
    let path = paths::settings_path();
    let mut settings = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Settings>(&bytes).unwrap_or_default(),
        Err(_) => Settings::default(),
    };
    if settings.secret.trim().is_empty() {
        settings.secret = new_secret();
        let _ = save(&settings);
    }
    settings
}

pub fn save(settings: &Settings) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?;
    paths::atomic_write(&paths::settings_path(), &bytes).map_err(|e| e.to_string())?;
    restrict_permissions(&paths::settings_path());
    Ok(())
}

fn new_secret() -> String {
    // 不引入额外随机数依赖：用两个 UUID 拼 32 字节再 base64
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {}

pub fn apply_patch(settings: &mut Settings, patch: SettingsPatch) {
    macro_rules! set {
        ($field:ident) => {
            if let Some(v) = patch.$field {
                settings.$field = v;
            }
        };
    }
    set!(mixed_port);
    set!(controller_port);
    set!(auto_ports);
    set!(mode);
    set!(log_level);
    set!(auto_system_proxy);
    set!(http_proxy);
    set!(https_proxy);
    set!(socks_proxy);
    set!(restore_on_exit);
    set!(recover_on_crash);
    set!(autostart);
    set!(auto_connect);
    set!(auto_connect_last);
    set!(default_interval_hours);
    set!(notify_on_update_fail);
    set!(test_url);
    set!(test_timeout_ms);
    set!(diag_timeout_ms);
    if patch.dns_enable_clear.unwrap_or(false) {
        settings.dns_enable = None;
    }
    set!(dns_enable);
    set!(dns_override);
    set!(dns_nameservers);
    set!(find_process);
    set!(mac_service);
    set!(bin_path);
}

/// 开机自启开关。
///
/// - Windows：写 `HKCU\…\Run` 的 `mdgo` 值（exe 路径 + `--autostart`）
/// - macOS：写 `~/Library/LaunchAgents/com.mdgo.autostart.plist`
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_str = exe.to_string_lossy().to_string();

    #[cfg(target_os = "windows")]
    {
        use winreg::enums::*;
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (run, _) = hkcu
            .create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
            .map_err(|e| e.to_string())?;
        if enabled {
            run.set_value("mdgo", &format!("\"{}\" --autostart", exe_str))
                .map_err(|e| e.to_string())?;
        } else {
            let _ = run.delete_value("mdgo");
        }
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").map_err(|e| e.to_string())?;
        let dir = PathBuf::from(home).join("Library").join("LaunchAgents");
        let plist = dir.join("com.mdgo.autostart.plist");
        if enabled {
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let content = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.mdgo.autostart</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>--autostart</string></array>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
"#,
                exe_str.replace('&', "&amp;").replace('<', "&lt;")
            );
            std::fs::write(&plist, content).map_err(|e| e.to_string())?;
        } else {
            let _ = std::fs::remove_file(&plist);
        }
        return Ok(());
    }

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        let _ = enabled;
        let _ = exe_str;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_is_44_chars() {
        let s = new_secret();
        assert_eq!(s.len(), 44, "32 字节 base64 应为 44 字符");
    }

    #[test]
    fn patch_applies_only_some_fields() {
        let mut s = Settings::default();
        apply_patch(
            &mut s,
            SettingsPatch {
                mode: Some("global".into()),
                test_timeout_ms: Some(1234),
                ..Default::default()
            },
        );
        assert_eq!(s.mode, "global");
        assert_eq!(s.test_timeout_ms, 1234);
        assert_eq!(s.mixed_port, 7890);
    }
}
