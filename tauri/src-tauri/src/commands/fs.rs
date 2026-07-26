use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// 安全规范化路径：先检查路径存在性，再调用 canonicalize 解析为绝对路径。
/// 用于在文件操作前确保路径有效并阻止符号链接绕过。
fn canonicalize_safe(path: &str) -> Result<PathBuf, String> {
    let p = Path::new(path);
    if !p.exists() {
        return Err(format!("路径不存在: {}", path));
    }
    let canon = p
        .canonicalize()
        .map_err(|e| format!("解析路径失败 ({}): {}", path, e))?;
    Ok(canon)
}

/// 检查路径是否包含 ..（ParentDir），防止目录遍历攻击
fn is_path_safe(path: &Path) -> bool {
    let components: Vec<_> = path.components().collect();
    for c in &components {
        if matches!(c, std::path::Component::ParentDir) {
            return false;
        }
        if matches!(c, std::path::Component::RootDir) {
            continue;
        }
    }
    true
}

#[derive(Debug, Serialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub kind: String, // "file" or "directory"
    pub size: u64,
    pub modified: u64,
    pub created: u64,
}

#[derive(Debug, Serialize)]
pub struct FileMeta {
    pub name: String,
    pub size: u64,
    pub modified: u64,
    pub created: u64,
    pub is_file: bool,
    pub is_dir: bool,
}

/// 递归扫描目录，返回所有文件和目录的扁平列表
#[tauri::command]
pub fn read_dir_recursive(path: String) -> Result<Vec<FileEntry>, String> {
    if !is_path_safe(Path::new(&path)) {
        return Err("路径不安全".into());
    }
    let canon = canonicalize_safe(&path)?;
    if !canon.is_dir() {
        return Err(format!("不是目录: {}", path));
    }

    let mut entries = Vec::new();

    let walker = walkdir::WalkDir::new(&canon)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            !e.file_name()
                .to_str()
                .map(|s| s.starts_with('.'))
                .unwrap_or(false)
        });

    for entry in walker {
        match entry {
            Ok(entry) => {
                let metadata = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                let rel_path = entry
                    .path()
                    .to_string_lossy()
                    .to_string();

                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                let created = metadata
                    .created()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                entries.push(FileEntry {
                    name: entry
                        .file_name()
                        .to_string_lossy()
                        .to_string(),
                    path: rel_path,
                    kind: if metadata.is_dir() {
                        "directory".to_string()
                    } else {
                        "file".to_string()
                    },
                    size: metadata.len(),
                    modified,
                    created,
                });
            }
            Err(_) => continue,
        }
    }

    Ok(entries)
}

/// 扫描单个目录（非递归）
#[tauri::command]
pub fn read_dir(path: String) -> Result<Vec<FileEntry>, String> {
    if !is_path_safe(Path::new(&path)) {
        return Err("路径不安全".into());
    }
    let canon = canonicalize_safe(&path)?;
    if !canon.is_dir() {
        return Err(format!("不是目录: {}", path));
    }

    let mut entries = Vec::new();
    let dir = fs::read_dir(&canon).map_err(|e| e.to_string())?;

    for entry in dir {
        let entry = entry.map_err(|e| e.to_string())?;
        let metadata = entry.metadata().map_err(|e| e.to_string())?;

        let modified = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let created = metadata
            .created()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        entries.push(FileEntry {
            name: entry.file_name().to_string_lossy().to_string(),
            path: entry.path().to_string_lossy().to_string(),
            kind: if metadata.is_dir() {
                "directory".to_string()
            } else {
                "file".to_string()
            },
            size: metadata.len(),
            modified,
            created,
        });
    }

    Ok(entries)
}

/// 读取文件内容（文本）
#[tauri::command]
pub fn read_file(path: String) -> Result<String, String> {
    if !is_path_safe(Path::new(&path)) {
        return Err("路径不安全".into());
    }
    let canon = canonicalize_safe(&path)?;
    if !canon.is_file() {
        return Err(format!("不是文件: {}", path));
    }
    fs::read_to_string(&canon).map_err(|e| format!("读取文件失败 ({}): {}", path, e))
}

/// 读取文件内容（二进制，返回原始字节数组）
#[tauri::command]
pub fn read_file_binary(path: String) -> Result<Vec<u8>, String> {
    if !is_path_safe(Path::new(&path)) {
        return Err("路径不安全".into());
    }
    let canon = canonicalize_safe(&path)?;
    if !canon.is_file() {
        return Err(format!("不是文件: {}", path));
    }
    fs::read(&canon).map_err(|e| format!("读取文件失败 ({}): {}", path, e))
}

/// 写入文件内容（文本）
#[tauri::command]
pub fn write_file(path: String, content: String) -> Result<(), String> {
    if !is_path_safe(Path::new(&path)) {
        return Err("路径不安全".into());
    }
    let p = Path::new(&path);
    if let Some(parent) = p.parent() {
        // 对父目录执行 canonicalize 防止符号链接绕过
        let _ = canonicalize_safe(&parent.to_string_lossy()).map_err(|e| {
            format!("父目录路径不安全 ({}): {}", parent.display(), e)
        })?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(p, &content).map_err(|e| format!("写入文件失败 ({}): {}", path, e))
}

/// 写入文件内容（二进制）
#[tauri::command]
pub fn write_file_binary(path: String, content: Vec<u8>) -> Result<(), String> {
    if !is_path_safe(Path::new(&path)) {
        return Err("路径不安全".into());
    }
    let p = Path::new(&path);
    if let Some(parent) = p.parent() {
        // 对父目录执行 canonicalize 防止符号链接绕过
        let _ = canonicalize_safe(&parent.to_string_lossy()).map_err(|e| {
            format!("父目录路径不安全 ({}): {}", parent.display(), e)
        })?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(p, &content).map_err(|e| format!("写入文件失败 ({}): {}", path, e))
}

/// 删除文件或目录
#[tauri::command]
pub fn delete(path: String) -> Result<(), String> {
    if !is_path_safe(Path::new(&path)) {
        return Err("路径不安全".into());
    }
    let canon = canonicalize_safe(&path)?;
    if canon.is_dir() {
        fs::remove_dir_all(&canon).map_err(|e| format!("删除目录失败 ({}): {}", path, e))
    } else {
        fs::remove_file(&canon).map_err(|e| format!("删除文件失败 ({}): {}", path, e))
    }
}

/// 重命名/移动文件
#[tauri::command]
pub fn rename(src: String, dst: String) -> Result<(), String> {
    if !is_path_safe(Path::new(&src)) || !is_path_safe(Path::new(&dst)) {
        return Err("路径不安全".into());
    }
    let canon_src = canonicalize_safe(&src)?;
    let dst_path = Path::new(&dst);
    if let Some(parent) = dst_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::rename(&canon_src, dst_path).map_err(|e| format!("重命名失败 ({} -> {}): {}", src, dst, e))
}

/// 创建目录
#[tauri::command]
pub fn create_dir(path: String) -> Result<(), String> {
    if !is_path_safe(Path::new(&path)) {
        return Err("路径不安全".into());
    }
    fs::create_dir_all(&path).map_err(|e| format!("创建目录失败 ({}): {}", path, e))
}

/// 检查文件/目录是否存在
#[tauri::command]
pub fn exists(path: String) -> bool {
    if !is_path_safe(Path::new(&path)) {
        return false;
    }
    Path::new(&path).exists()
}

/// 获取文件/目录元信息
#[tauri::command]
pub fn get_file_meta(path: String) -> Result<FileMeta, String> {
    if !is_path_safe(Path::new(&path)) {
        return Err("路径不安全".into());
    }
    let canon = canonicalize_safe(&path)?;
    let metadata = fs::metadata(&canon).map_err(|e| format!("获取元信息失败 ({}): {}", path, e))?;

    let modified = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let created = metadata
        .created()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Ok(FileMeta {
        name: canon
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        size: metadata.len(),
        modified,
        created,
        is_file: metadata.is_file(),
        is_dir: metadata.is_dir(),
    })
}
