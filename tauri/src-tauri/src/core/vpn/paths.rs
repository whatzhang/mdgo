//! VPN 路径解析：数据目录、内核二进制、端口探测。

use std::net::TcpListener;
use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager, Runtime};

/// VPN 数据根目录。
///
/// - macOS: `~/Library/Application Support/mdgo/vpn`
/// - Windows: `%APPDATA%/mdgo/vpn`
/// - 其他: `~/.local/share/mdgo/vpn`
pub fn data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("mdgo")
            .join("vpn")
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA")
            .map(|p| PathBuf::from(p).join("mdgo").join("vpn"))
            .unwrap_or_else(|_| PathBuf::from("C:\\mdgo\\vpn"))
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("mdgo")
            .join("vpn")
    }
}

pub fn ensure_dirs() -> std::io::Result<()> {
    for dir in [
        data_dir(),
        profiles_dir(),
        cache_dir(),
        bin_dir(),
    ] {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(())
}

pub fn profiles_dir() -> PathBuf {
    data_dir().join("profiles")
}
pub fn cache_dir() -> PathBuf {
    data_dir().join("cache")
}
pub fn bin_dir() -> PathBuf {
    data_dir().join("bin")
}
pub fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}
pub fn profiles_path() -> PathBuf {
    data_dir().join("profiles.json")
}
pub fn config_path() -> PathBuf {
    data_dir().join("config.yaml")
}
pub fn log_path() -> PathBuf {
    data_dir().join("mihomo.log")
}
/// 记录当前内核 pid（用于调试中断/崩溃后清理孤儿进程）。
pub fn pid_path() -> PathBuf {
    data_dir().join("mihomo.pid")
}

/// 写入 pid 记录：`pid\n<exe>\n<config>`，用于排查与兜底清理。
pub fn write_pid_record(pid: u32, exe: &Path, config: &Path) {
    let content = format!(
        "{}\n{}\n{}\n",
        pid,
        exe.to_string_lossy(),
        config.to_string_lossy()
    );
    let _ = atomic_write(&pid_path(), content.as_bytes());
}

pub fn clear_pid_record() {
    let _ = std::fs::remove_file(pid_path());
}

pub fn profile_dir(id: &str) -> PathBuf {
    profiles_dir().join(id)
}
pub fn profile_raw(id: &str) -> PathBuf {
    profile_dir(id).join("raw.yaml")
}

pub fn profile_versions(id: &str) -> PathBuf {
    profile_dir(id).join("versions")
}

/// 当前平台对应的内核文件名（不含扩展名）。
pub fn core_file_stem() -> &'static str {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "mihomo-windows-x64"
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "mihomo-darwin-arm64"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "mihomo-darwin-amd64"
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        "mihomo-linux-amd64"
    }
}

fn with_exe(stem: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// 解析可用的 mihomo 二进制。
///
/// 顺序：手动配置 → 环境变量 `MDGO_MIHOMO_BIN` → 资源目录 `vpn/<stem>` →
/// 数据目录 `bin/<stem>` → 数据目录 `bin/mihomo[.exe]` → PATH 中的 `mihomo`。
pub fn resolve_core_bin<R: Runtime>(app: &AppHandle<R>, manual: &str) -> Option<PathBuf> {
    let manual = manual.trim();
    if !manual.is_empty() {
        let p = PathBuf::from(manual);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(v) = std::env::var("MDGO_MIHOMO_BIN") {
        let p = PathBuf::from(v.trim());
        if p.is_file() {
            return Some(p);
        }
    }
    let file = with_exe(core_file_stem());
    // 开发环境：源码 resources 目录（未打包时 resource_dir 指向 target/debug）
    if cfg!(debug_assertions) {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("vpn")
            .join(&file);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(res) = app.path().resource_dir() {
        let p = res.join("vpn").join(&file);
        if p.is_file() {
            return Some(p);
        }
        // 开发环境：源码 resources 目录
        let p2 = res.join("resources").join("vpn").join(&file);
        if p2.is_file() {
            return Some(p2);
        }
    }
    let p = bin_dir().join(&file);
    if p.is_file() {
        return Some(p);
    }
    let p = bin_dir().join(with_exe("mihomo"));
    if p.is_file() {
        return Some(p);
    }
    // PATH 查找
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(with_exe("mihomo"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 端口是否可绑定。
pub fn port_available(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

pub fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

pub fn now_secs() -> i64 {
    chrono::Local::now().timestamp()
}

pub fn clock_now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// 把 path 安全落盘（先写临时文件再 rename）。
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp{}-{}", std::process::id(), now_ms()));
    std::fs::write(&tmp, bytes)?;
    // Windows 的 std::fs::rename 使用 MoveFileExW(MOVEFILE_REPLACE_EXISTING)，
    // 可直接覆盖已存在文件；仅在失败时兜底删除再重试一次。
    if let Err(first) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(path);
        std::fs::rename(&tmp, path).map_err(|_| first)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_available_detects_free_port() {
        // 17890 在测试环境几乎不会被占用
        assert!(port_available(17890) || port_available(17891));
    }

    #[test]
    fn core_stem_not_empty() {
        assert!(!core_file_stem().is_empty());
    }
}
