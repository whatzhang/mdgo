use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::Local;
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
        // 目录删除：额外清理 folder 节点与子目录节点（生命周期级联）
        if canon.is_dir() {
            if let Ok(rel) = rel_path_from(&dir, &canon) {
                if let Err(e) = state.graph_engine.remove_path(&dir, &rel) {
                    log::warn!("[fs] 目录图清理失败 ({}): {}", rel, e);
                }
            }
        }
    }
    Ok(())
}

// =====================================================
// 目录移动垃圾箱 / 还原（重逻辑在 Rust 端，前端仅调用）
// =====================================================

/// 垃圾箱目录名（与前端 DELETED_DIR_NAME 一致）
const TRASH_DIR_NAME: &str = "mdgo_trash";
/// 删除索引文件名（与前端 INDEX_DELETED_FILE 一致，存放于 {root}/.mdgo/ 下）
const INDEX_DELETED_FILE: &str = "index_deleted.json";

/// 删除索引记录（与前端 index_deleted.json 结构一致）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeletedRecord {
    #[serde(rename = "originalPath")]
    original_path: String,
    #[serde(rename = "deletedAt")]
    deleted_at: u64,
    #[serde(rename = "isDir", skip_serializing_if = "Option::is_none")]
    is_dir: Option<bool>,
}

/// 从当前打开目录（WatcherService 内存态）解析根目录
fn resolve_root(app: &AppHandle) -> Result<String, String> {
    let state = app.state::<crate::AppState>();
    state
        .watcher
        .get_watch_dir()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "未打开目录".to_string())
}

/// 规范化相对路径：拒绝绝对路径与 `..` 逃逸，去除首尾分隔符并统一为正斜杠
fn normalize_relative(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    if p.is_absolute() {
        return Err("请传入相对路径".to_string());
    }
    if !is_path_safe(p) {
        return Err("路径不安全".to_string());
    }
    Ok(path
        .trim_matches('/')
        .trim_matches('\\')
        .replace('\\', "/"))
}

/// 递归复制目录（含空目录骨架），失败时返回错误信息
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("创建目录失败 ({}): {}", dst.display(), e))?;
    for entry in walkdir::WalkDir::new(src).follow_links(false) {
        let entry = entry.map_err(|e| format!("遍历目录失败: {}", e))?;
        let rel = entry
            .path()
            .strip_prefix(src)
            .map_err(|_| "路径计算失败".to_string())?;
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)
                .map_err(|e| format!("创建目录失败 ({}): {}", target.display(), e))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("创建目录失败 ({}): {}", parent.display(), e))?;
            }
            fs::copy(entry.path(), &target).map_err(|e| {
                format!(
                    "复制文件失败 ({} -> {}): {}",
                    entry.path().display(),
                    target.display(),
                    e
                )
            })?;
        }
    }
    Ok(())
}

/// 读取删除索引 JSON（不存在或为空时返回空表）
fn read_deleted_index(mdgo_dir: &Path) -> Result<HashMap<String, DeletedRecord>, String> {
    let p = mdgo_dir.join(INDEX_DELETED_FILE);
    if !p.exists() {
        return Ok(HashMap::new());
    }
    let text = fs::read_to_string(&p).map_err(|e| format!("读取删除索引失败: {}", e))?;
    if text.trim().is_empty() {
        return Ok(HashMap::new());
    }
    serde_json::from_str(&text).map_err(|e| format!("解析删除索引失败: {}", e))
}

/// 写入删除索引 JSON
fn write_deleted_index(
    mdgo_dir: &Path,
    index: &HashMap<String, DeletedRecord>,
) -> Result<(), String> {
    let p = mdgo_dir.join(INDEX_DELETED_FILE);
    let json = serde_json::to_string_pretty(index).map_err(|e| format!("序列化删除索引失败: {}", e))?;
    fs::write(&p, json).map_err(|e| format!("写入删除索引失败: {}", e))
}

/// 删除目录并清理其知识库索引（磁盘删除前收集目标，删除后逐条清理）
async fn remove_dir_with_index(app: &AppHandle, root: &str, abs_path: &Path) -> Result<(), String> {
    let abs_str = abs_path.to_string_lossy().to_string();
    let state = app.state::<crate::AppState>();
    let mut pending: Vec<String> = Vec::new();
    match state.indexer.collect_remove_targets(root, &abs_str).await {
        Ok(rels) => pending = rels,
        Err(e) => log::warn!("[fs] 收集待清理索引失败 ({}): {}", abs_str, e),
    }
    fs::remove_dir_all(abs_path).map_err(|e| format!("删除目录失败 ({}): {}", abs_str, e))?;
    for rel in &pending {
        if let Err(e) = state.indexer.remove_file(root, rel).await {
            log::error!("[fs] 清理删除目录的索引失败 ({}): {}", rel, e);
        }
    }
    // 目录级图清理（生命周期级联）：删除 folder 节点 + 该目录下全部节点/边
    // （collect_remove_targets 只收集文件；folder 节点与子目录节点需在此批量清）
    if let Ok(rel) = rel_path_from(root, abs_path) {
        if let Err(e) = state.graph_engine.remove_path(root, &rel) {
            log::warn!("[fs] 目录图清理失败 ({}): {}", rel, e);
        }
    }
    Ok(())
}

/// 计算 abs_path 相对知识库根 root 的相对路径（正斜杠；不在根内返回 Err）。
fn rel_path_from(root: &str, abs_path: &Path) -> Result<String, String> {
    let root_norm = Path::new(root);
    let rel = abs_path
        .strip_prefix(root_norm)
        .map_err(|_| "路径不在知识库目录内".to_string())?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

/// 将目录移动到垃圾箱（仅 Tauri 调用）
///
/// 把 path（相对当前打开目录）整体递归复制到 {root}/mdgo_trash/{yyyy-MM-dd}_{目录名}，
/// 删除原目录并清理知识库索引，最后写入删除索引记录。返回垃圾箱中的目录相对路径。
#[tauri::command]
pub async fn move_dir_to_trash(app: AppHandle, path: String) -> Result<String, String> {
    let root = resolve_root(&app)?;
    let rel = normalize_relative(&path)?;
    let src = Path::new(&root).join(&rel);
    if !src.is_dir() {
        return Err(format!("目录不存在: {}", path));
    }
    let dir_name = src
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| "无效的目录名".to_string())?;
    let date_prefix = Local::now().format("%Y-%m-%d").to_string();
    let trash_name = format!("{}_{}", date_prefix, dir_name);
    let trash_rel = format!("{}/{}", TRASH_DIR_NAME, trash_name);
    let trash_abs = Path::new(&root).join(&trash_rel);

    // 1. 递归复制到垃圾箱（复制成功后才删除原目录，避免中途失败丢失数据）
    copy_dir_recursive(&src, &trash_abs)?;

    // 2. 删除原目录并清理索引
    remove_dir_with_index(&app, &root, &src).await?;

    // 3. 更新删除索引
    let mdgo_dir = Path::new(&root).join(".mdgo");
    fs::create_dir_all(&mdgo_dir).map_err(|e| format!("创建数据目录失败: {}", e))?;
    let mut index = read_deleted_index(&mdgo_dir)?;
    index.insert(
        trash_name.to_string(),
        DeletedRecord {
            original_path: rel.clone(),
            deleted_at: std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            is_dir: Some(true),
        },
    );
    write_deleted_index(&mdgo_dir, &index)?;

    Ok(trash_rel)
}

/// 还原目录（仅 Tauri 调用）
///
/// 将垃圾箱（mdgo_trash）中的目录 path 递归复制回删除时的初始位置，
/// 删除垃圾箱副本并清理索引，最后移除删除索引记录。返回还原后的原始相对路径。
#[tauri::command]
pub async fn restore_dir_from_trash(app: AppHandle, path: String) -> Result<String, String> {
    let root = resolve_root(&app)?;
    let rel = normalize_relative(&path)?;
    if !rel.starts_with(&format!("{}/", TRASH_DIR_NAME)) {
        return Err("只能还原垃圾箱内的目录".to_string());
    }
    let trash_abs = Path::new(&root).join(&rel);
    if !trash_abs.is_dir() {
        return Err(format!("垃圾箱目录不存在: {}", path));
    }
    let trash_name = trash_abs
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| "无效的目录名".to_string())?;

    // 1. 从删除索引查找原始路径
    let mdgo_dir = Path::new(&root).join(".mdgo");
    let mut index = read_deleted_index(&mdgo_dir)?;
    let record = index
        .get(trash_name)
        .cloned()
        .ok_or_else(|| "找不到原始路径信息".to_string())?;
    if record.original_path.trim().is_empty() {
        return Err("删除索引中缺少原始路径".to_string());
    }
    let original_rel = normalize_relative(&record.original_path)?;
    let dst = Path::new(&root).join(&original_rel);
    if dst.exists() {
        return Err(format!("还原失败: 目标位置已存在 {}", original_rel));
    }

    // 2. 复制垃圾箱中的目录内容回初始位置
    copy_dir_recursive(&trash_abs, &dst)?;

    // 3. 删除垃圾箱副本并清理索引
    remove_dir_with_index(&app, &root, &trash_abs).await?;

    // 4. 移除删除索引记录
    index.remove(trash_name);
    write_deleted_index(&mdgo_dir, &index)?;

    Ok(original_rel)
}

/// 清空垃圾箱（仅 Tauri 调用）
///
/// 删除 {root}/mdgo_trash 下所有文件与子目录（保留垃圾箱目录本身），
/// 并重置删除索引记录（index_deleted.json 清空）。返回删除的子项数量。
#[tauri::command]
pub async fn clear_trash(app: AppHandle, path: String) -> Result<u32, String> {
    let root = resolve_root(&app)?;
    let rel = normalize_relative(&path)?;
    if rel != TRASH_DIR_NAME {
        return Err("只能清空垃圾箱目录".to_string());
    }
    let trash_abs = Path::new(&root).join(&rel);
    if !trash_abs.is_dir() {
        return Err(format!("垃圾箱目录不存在: {}", path));
    }

    // 1. 删除垃圾箱下所有子项（保留目录本身），并清理可能残留的知识库索引
    let mut count = 0u32;
    for entry in fs::read_dir(&trash_abs).map_err(|e| format!("读取垃圾箱失败: {}", e))? {
        let entry = entry.map_err(|e| format!("遍历垃圾箱失败: {}", e))?;
        let p = entry.path();
        if p.is_dir() {
            remove_dir_with_index(&app, &root, &p).await?;
        } else {
            fs::remove_file(&p).map_err(|e| format!("删除文件失败 ({}): {}", p.display(), e))?;
        }
        count += 1;
    }

    // 2. 重置删除索引记录
    let mdgo_dir = Path::new(&root).join(".mdgo");
    fs::create_dir_all(&mdgo_dir).map_err(|e| format!("创建数据目录失败: {}", e))?;
    write_deleted_index(&mdgo_dir, &HashMap::new())?;

    Ok(count)
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
