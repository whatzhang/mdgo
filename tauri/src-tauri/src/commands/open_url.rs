use std::process::Command as StdCommand;
use tauri::{command, AppHandle, Manager};

#[command]
pub fn open_url(url: String) -> Result<(), String> {
    open::that(&url).map_err(|e| format!("打开 URL 失败: {}", e))
}

/// 将前端传入的相对路径与当前打开目录自动拼接为绝对路径；
/// 已传绝对路径时原样使用。
///
/// 当前打开目录常驻在 WatcherService 内存中（前端打开目录时经
/// kb_watcher_start 写入 watch_dir，与 commands/fs.rs 的 delete 命令同源），
/// 避免每次调用都读取磁盘配置文件。
fn resolve_root_path(app: &AppHandle, path: &str) -> Result<String, String> {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return Ok(path.to_string());
    }
    let state = app.state::<crate::AppState>();
    let root = state
        .watcher
        .get_watch_dir()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "未打开目录，无法解析相对路径".to_string())?;
    Ok(std::path::Path::new(&root)
        .join(path)
        .to_string_lossy()
        .to_string())
}

/// 在系统文件管理器中显示文件/目录。
/// - 文件（is_file=true）：打开所在目录并定位选中该文件。
///   Windows 用 `explorer /select,`，macOS 用 `open -R`，Linux 打开所在目录。
/// - 目录（is_file=false）：直接打开该目录。
/// path 为相对路径时，后端按当前打开目录自动拼接为绝对路径。
/// 目标不存在时降级为打开其父目录（若存在）。
#[command]
pub fn show_file_dir_window(
    app: AppHandle,
    path: String,
    is_file: bool,
) -> Result<(), String> {
    let full_path = resolve_root_path(&app, &path)?;
    let target = std::path::Path::new(&full_path);
    if !target.exists() {
        // 目标已不存在（如文件被删除）时，降级为打开其父目录
        let parent = target.parent().filter(|p| p.exists());
        return match parent {
            Some(p) => open_dir(&p.to_string_lossy()),
            None => Err(format!("路径不存在: {}", full_path)),
        };
    }

    if is_file {
        // 定位并高亮选中文件
        #[cfg(target_os = "windows")]
        {
            let win_path = full_path.replace('/', "\\");
            StdCommand::new("explorer")
                .arg(format!("/select,{}", win_path))
                .spawn()
                .map_err(|e| format!("打开资源管理器失败: {}", e))?;
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        {
            StdCommand::new("open")
                .arg("-R")
                .arg(&full_path)
                .spawn()
                .map_err(|e| format!("打开 Finder 失败: {}", e))?;
            return Ok(());
        }
        #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
        {
            return match target.parent() {
                Some(dir) => open_dir(&dir.to_string_lossy()),
                None => Err(format!("无法获取 {} 的所在目录", full_path)),
            };
        }
    }

    open_dir(&target.to_string_lossy())
}

/// 打开目录：Windows explorer / macOS open / Linux xdg-open
fn open_dir(dir: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        StdCommand::new("explorer")
            .arg(dir.replace('/', "\\"))
            .spawn()
            .map_err(|e| format!("打开资源管理器失败: {}", e))?;
    }
    #[cfg(target_os = "macos")]
    {
        StdCommand::new("open")
            .arg(dir)
            .spawn()
            .map_err(|e| format!("打开 Finder 失败: {}", e))?;
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        StdCommand::new("xdg-open")
            .arg(dir)
            .spawn()
            .map_err(|e| format!("打开文件管理器失败: {}", e))?;
    }
    Ok(())
}
