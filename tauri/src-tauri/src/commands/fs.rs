use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use tauri::{AppHandle, Manager};

use crate::core::db::utils::IgnoreMatcher;

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

// =====================================================
// 全量扫描（单次 IPC）— 供前端 scanDirToIndexByLocal 使用
// 忽略规则复用 core::db::utils::IgnoreMatcher（与知识库索引/监视/Agent 工具共用同一套黑名单语义）
// =====================================================

/// 全量扫描目录：walkdir 单次遍历 + IgnoreMatcher 黑名单过滤，一次 IPC 返回全部文件/目录条目。
/// 返回 path 为相对根目录的相对路径（正斜杠分隔），不含根目录自身；根目录自身条目被排除。
#[tauri::command]
pub fn scan_dir_full(
    path: String,
    ignore_dirs: Vec<String>,
    ignore_files: Vec<String>,
    max_depth: Option<usize>,
) -> Result<Vec<FileEntry>, String> {
    if !is_path_safe(Path::new(&path)) {
        return Err("路径不安全".into());
    }
    let canon = canonicalize_safe(&path)?;
    if !canon.is_dir() {
        return Err(format!("不是目录: {}", path));
    }

    let ignore = IgnoreMatcher::new(&ignore_dirs, &ignore_files);
    let depth = max_depth.unwrap_or(usize::MAX);

    let mut entries = Vec::new();
    let walker = walkdir::WalkDir::new(&canon)
        .follow_links(false)
        .max_depth(depth)
        .into_iter()
        .filter_entry(|e| {
            // 根目录自身不参与过滤（walkdir 的入口）
            if e.depth() == 0 {
                return true;
            }
            let rel = match e.path().strip_prefix(&canon) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => return true,
            };
            if e.file_type().is_dir() {
                !ignore.should_skip_dir(&rel)
            } else {
                !ignore.should_skip_file(&rel)
            }
        });

    for entry in walker {
        let Ok(entry) = entry else { continue };
        // 排除根目录自身
        if entry.depth() == 0 {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let rel = match entry.path().strip_prefix(&canon) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if rel.is_empty() {
            continue;
        }

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
            path: rel,
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

/// 删除文件或目录（同时清理知识库索引：LanceDB 向量 + BM25 倒排索引）
#[tauri::command]
pub async fn delete(app: AppHandle, path: String) -> Result<(), String> {
    if !is_path_safe(Path::new(&path)) {
        return Err("路径不安全".into());
    }
    let canon = canonicalize_safe(&path)?;
    let canon_str = canon.to_string_lossy().to_string();
    let state = app.state::<crate::AppState>();

    // 磁盘删除前收集待清理索引的文件（目录需可遍历，否则无法获知目录下文件列表）
    let kb_dir = state.watcher.get_watch_dir();
    let mut pending: Vec<String> = Vec::new();
    if let Some(ref dir) = kb_dir {
        match state.indexer.collect_remove_targets(dir, &canon_str).await {
            Ok(rels) => pending = rels,
            Err(e) => log::warn!("[fs] 收集待清理索引失败 ({}): {}", path, e),
        }
    }

    // 删除磁盘文件/目录
    if canon.is_dir() {
        fs::remove_dir_all(&canon).map_err(|e| format!("删除目录失败 ({}): {}", path, e))?;
    } else {
        fs::remove_file(&canon).map_err(|e| format!("删除文件失败 ({}): {}", path, e))?;
    }

    // 同步清理知识库索引（磁盘已删除，清理失败不阻塞删除，仅记录）
    if let Some(dir) = kb_dir {
        for rel in &pending {
            if let Err(e) = state.indexer.remove_file(&dir, rel).await {
                log::error!("[fs] 清理删除文件的索引失败 ({}): {}", rel, e);
            }
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 与前端默认黑名单保持一致
    fn default_ignore_dirs() -> Vec<String> {
        [
            ".*/",
            "$*/",
            "assets/",
            "node_modules/",
            "vendor/",
            "dist/",
            "build/",
            "target/",
            "__pycache__/",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn default_ignore_files() -> Vec<String> {
        [".*", "$*", "*.tmp", "*.log", "!.gitignore"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn make_temp_scan_tree(root: &Path) {
        fs::create_dir_all(root.join("sub/deep")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("a.md"), "a").unwrap();
        fs::write(root.join(".hidden.md"), "h").unwrap();
        fs::write(root.join(".gitignore"), "git").unwrap();
        fs::write(root.join("sub/b.txt"), "b").unwrap();
        fs::write(root.join("sub/deep/c.tmp"), "c").unwrap();
        fs::write(root.join("node_modules/pkg/x.js"), "x").unwrap();
    }

    #[test]
    fn test_scan_dir_full_filters_and_relative_paths() {
        let root = std::env::temp_dir().join(format!(
            "mdgo_scan_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        make_temp_scan_tree(&root);

        let entries = scan_dir_full(
            root.to_string_lossy().to_string(),
            default_ignore_dirs(),
            default_ignore_files(),
            Some(20),
        )
        .unwrap();

        let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
        // 根目录自身不出现
        assert!(!paths.contains(&String::new()));
        // 黑名单目录及其内容被过滤
        assert!(!paths.iter().any(|p| p.starts_with("node_modules")));
        assert!(!paths.iter().any(|p| p.starts_with(".hidden.md")));
        // 黑名单文件被过滤，但 .gitignore 因 !.gitignore 取反保留
        assert!(!paths.iter().any(|p| p.ends_with(".tmp")));
        assert!(paths.contains(&"a.md".to_string()));
        assert!(paths.contains(&".gitignore".to_string()));
        assert!(paths.contains(&"sub/b.txt".to_string()));
        // 空目录保留
        assert!(paths.contains(&"sub/deep".to_string()));
        assert!(paths.contains(&"sub".to_string()));
        // 路径统一正斜杠分隔
        assert!(paths.iter().all(|p| !p.contains('\\')));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_scan_dir_full_max_depth() {
        let root = std::env::temp_dir().join(format!(
            "mdgo_scan_depth_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("l1/l2/l3/l4")).unwrap();
        fs::write(root.join("l1/l2/l3/l4/x.txt"), "x").unwrap();

        let entries = scan_dir_full(
            root.to_string_lossy().to_string(),
            default_ignore_dirs(),
            default_ignore_files(),
            Some(3),
        )
        .unwrap();
        // 深度限制：l1/l2/l3/l4 在第 4 层，应被过滤
        assert!(!entries.iter().any(|e| e.path.starts_with("l1/l2/l3/l4")));
        assert!(entries.iter().any(|e| e.path == "l1"));
        assert!(entries.iter().any(|e| e.path == "l1/l2"));
        assert!(entries.iter().any(|e| e.path == "l1/l2/l3"));

        let _ = fs::remove_dir_all(&root);
    }
}
