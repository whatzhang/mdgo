//! 知识库级统一数据库（每个知识库目录一个 `.mdgo/mdgo.db`，承载 memory / prompts /
//! schedule / skills 多域表，避免维护多个 DB）。
//!
//! 各域持有独立连接（WAL 支持多连接并发，写竞争由 `busy_timeout` 兜底），
//! 但全部指向**同一文件** `{知识库根目录}/.mdgo/mdgo.db`，数据与业务逻辑均为知识库级。
//!
//! # 安全
//!
//! `dir_path` 来自前端 invoke 参数，**不可信**：所有入口先经 [`sanitize_kb_dir`] 校验
//! （拒绝相对路径 / `..` 穿越 / 空串，要求目录存在并 `canonicalize` 归一），
//! 防止任意路径写与同目录多写法分叉。

use std::path::{Path, PathBuf};

/// 知识库数据库文件名（位于各知识库 `.mdgo/` 目录下）
pub const KB_DB_FILENAME: &str = "mdgo.db";
/// 校验并规范化知识库目录（防路径穿越 / 任意路径写）。
///
/// 规则：非空、绝对路径、不含 `..` 段、目录必须已存在；
/// 返回 `canonicalize` 后的规范路径（解析符号链接、归一化相对片段）。
pub fn sanitize_kb_dir(dir_path: &str) -> Result<PathBuf, String> {
    let trimmed = dir_path.trim();
    if trimmed.is_empty() {
        return Err("知识库目录不能为空".to_string());
    }
    let p = Path::new(trimmed);
    if !p.is_absolute() {
        return Err("知识库目录必须是绝对路径".to_string());
    }
    if p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err("知识库目录包含非法路径段（..）".to_string());
    }
    if !p.exists() {
        return Err(format!("知识库目录不存在: {}", trimmed));
    }
    p.canonicalize()
        .map_err(|e| format!("解析知识库目录失败 ({}): {}", trimmed, e))
}

/// 知识库数据库路径：`{dir_path}/.mdgo/mdgo.db`（`dir_path` 先经 [`sanitize_kb_dir`] 规范化）
pub fn kb_db_path(dir_path: &str) -> Result<PathBuf, String> {
    Ok(sanitize_kb_dir(dir_path)?.join(".mdgo").join(KB_DB_FILENAME))
}

/// 系统级 memory 数据库路径：`{系统数据目录}/com.mdgo/memory.db`
///
/// 记忆为**两级模型**（P0-3）：`scope='project'` 的记忆通过 `memory_items.dir_path` 列
/// 归属知识库（切换目录即隔离）；`scope='global'` 的记忆 `dir_path=''`，跨知识库共享。
/// 物理存储位于系统数据目录——与应用打开的知识库目录解耦，避免随知识库移动/删除而丢记忆。
pub fn system_memory_db_path() -> Result<PathBuf, String> {
    let data_dir = dirs::data_dir().ok_or_else(|| "无法定位系统数据目录".to_string())?;
    Ok(data_dir.join("com.mdgo").join("memory.db"))
}

/// 打开系统级 memory 数据库的**读写分离连接池**（`{系统数据目录}/com.mdgo/memory.db`）。
pub fn open_system_memory_pool() -> Result<crate::core::db::pool::DbPool, String> {
    crate::core::db::pool::DbPool::open(system_memory_db_path()?)
}
