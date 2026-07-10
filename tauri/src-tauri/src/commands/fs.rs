use serde::Serialize;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

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
    let mut entries = Vec::new();
    let base_path = Path::new(&path);

    if !base_path.exists() {
        return Err(format!("路径不存在: {}", path));
    }

    let walker = walkdir::WalkDir::new(base_path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // 跳过隐藏目录
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
    let base_path = Path::new(&path);

    if !base_path.exists() {
        return Err(format!("路径不存在: {}", path));
    }
    if !base_path.is_dir() {
        return Err(format!("不是目录: {}", path));
    }

    let mut entries = Vec::new();
    let dir = fs::read_dir(base_path).map_err(|e| e.to_string())?;

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
    fs::read_to_string(&path).map_err(|e| format!("读取文件失败 ({}): {}", path, e))
}

/// 读取文件内容（二进制，返回 base64）
#[tauri::command]
pub fn read_file_binary(path: String) -> Result<Vec<u8>, String> {
    fs::read(&path).map_err(|e| format!("读取文件失败 ({}): {}", path, e))
}

/// 写入文件内容（文本）
#[tauri::command]
pub fn write_file(path: String, content: String) -> Result<(), String> {
    // 确保父目录存在
    if let Some(parent) = Path::new(&path).parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&path, &content).map_err(|e| format!("写入文件失败 ({}): {}", path, e))
}

/// 写入文件内容（二进制）
#[tauri::command]
pub fn write_file_binary(path: String, content: Vec<u8>) -> Result<(), String> {
    if let Some(parent) = Path::new(&path).parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&path, &content).map_err(|e| format!("写入文件失败 ({}): {}", path, e))
}

/// 删除文件或目录
#[tauri::command]
pub fn delete(path: String) -> Result<(), String> {
    let p = Path::new(&path);
    if p.is_dir() {
        fs::remove_dir_all(p).map_err(|e| format!("删除目录失败 ({}): {}", path, e))
    } else {
        fs::remove_file(p).map_err(|e| format!("删除文件失败 ({}): {}", path, e))
    }
}

/// 重命名/移动文件
#[tauri::command]
pub fn rename(src: String, dst: String) -> Result<(), String> {
    // 确保目标父目录存在
    if let Some(parent) = Path::new(&dst).parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::rename(&src, &dst).map_err(|e| format!("重命名失败 ({} -> {}): {}", src, dst, e))
}

/// 创建目录
#[tauri::command]
pub fn create_dir(path: String) -> Result<(), String> {
    fs::create_dir_all(&path).map_err(|e| format!("创建目录失败 ({}): {}", path, e))
}

/// 检查文件/目录是否存在
#[tauri::command]
pub fn exists(path: String) -> bool {
    Path::new(&path).exists()
}

/// 获取文件/目录元信息
#[tauri::command]
pub fn get_file_meta(path: String) -> Result<FileMeta, String> {
    let p = Path::new(&path);
    let metadata = fs::metadata(p).map_err(|e| format!("获取元信息失败 ({}): {}", path, e))?;

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
        name: p
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
