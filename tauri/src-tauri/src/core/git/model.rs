//! Git 读模型（serde 可直接给 commands/前端复用）。
//!
//! 字段命名保持与既有 `commands/git_types.rs` 的兼容语义：
//! - status：`path/x/y/from`；
//! - commit：`oid/commit.{message,tree,parent,author,committer}`；
//! - file change：`path/status/from/additions/deletions/binary`；
//! - refs：`heads/remotes/tags/head_hash/current_branch/detached/...`。

use serde::Serialize;

/// 工作区状态计数（分组可重叠：一个文件可同时有暂存与未暂存改动）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct StatusCounts {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub conflicted: usize,
}

/// 单条工作区状态。
#[derive(Debug, Clone, Serialize)]
pub struct StatusEntry {
    /// 展示路径（相对当前知识库；UI 命令映射时使用仓库相对）
    pub path: String,
    /// 重命名/复制时的原路径
    pub from: Option<String>,
    /// 暂存区状态字符（A/M/D/R/C/U/.）
    pub x: String,
    /// 工作区状态字符（A/M/D/U/.）
    pub y: String,
    /// added/modified/deleted/renamed/copied/conflicted/untracked/typechange
    pub kind: String,
    pub staged: bool,
    pub unstaged: bool,
}

/// 工作区状态汇总。
#[derive(Debug, Clone, Default, Serialize)]
pub struct GitStatus {
    pub root: String,
    /// 作用域展示文案（仓库根 / 知识库子目录）
    pub scope: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub detached: bool,
    pub unborn: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub entries: Vec<StatusEntry>,
    pub counts: StatusCounts,
    /// 严格模式下被隐藏的 `.mdgo` 条目数
    pub hidden_mdgo: usize,
    /// 严格模式下被隐藏的敏感文件条目数
    pub hidden_sensitive: usize,
    /// 知识库作用域之外的条目数
    pub hidden_outside: usize,
    /// 条目被 max_entries 截断
    pub truncated: bool,
    /// 截断前可见条目总数
    pub total_entries: usize,
}

/// 提交摘要（历史列表 + 提交详情共用）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct CommitSummary {
    pub oid: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    /// Unix 秒
    pub author_timestamp: u64,
    pub committer_name: String,
    pub committer_email: String,
    /// Unix 秒
    pub committer_timestamp: u64,
    /// `HEAD -> main, tag: v1.2` 等（已拆分）
    pub refs: Vec<String>,
    pub subject: String,
    /// 完整提交消息（subject + body），git_show 使用
    pub message: String,
    pub is_merge: bool,
}

/// 历史查询结果。
#[derive(Debug, Clone, Default, Serialize)]
pub struct LogResult {
    pub commits: Vec<CommitSummary>,
    pub limit: u32,
    pub skip: u32,
    pub rev: String,
    pub path: Option<String>,
    pub scope: String,
    pub unborn: bool,
    /// 达到 limit，可能还有更多
    pub maybe_more: bool,
}

/// 提交内单个文件变更。
#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    pub path: String,
    /// added/removed/modified/renamed/copied/conflicted/typechange
    pub status: String,
    pub from: Option<String>,
    pub additions: Option<u32>,
    pub deletions: Option<u32>,
    pub binary: bool,
}

/// 提交详情。
#[derive(Debug, Clone, Default, Serialize)]
pub struct CommitDetail {
    pub summary: CommitSummary,
    pub body: String,
    pub files: Vec<FileChange>,
    pub patch: Option<String>,
    pub patch_truncated: bool,
    pub hidden_files: usize,
    pub hidden_mdgo: usize,
    pub hidden_sensitive: usize,
    pub scope: String,
}

/// 差异结果。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DiffResult {
    /// 范围说明（工作区/暂存区/两个引用）
    pub mode: String,
    pub files: Vec<FileChange>,
    pub patch: Option<String>,
    pub truncated: bool,
    pub hidden_files: usize,
    pub hidden_mdgo: usize,
    pub hidden_sensitive: usize,
    pub scope: String,
}

/// 单个引用。
#[derive(Debug, Clone, Default, Serialize)]
pub struct RefInfo {
    pub name: String,
    pub hash: String,
    pub current: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// upstream 已不存在（`[gone]`）
    pub gone: bool,
}

/// 引用列表。
#[derive(Debug, Clone, Default, Serialize)]
pub struct RefList {
    pub heads: Vec<RefInfo>,
    pub remotes: Vec<RefInfo>,
    pub tags: Vec<RefInfo>,
    pub head_hash: Option<String>,
    pub current_branch: Option<String>,
    pub detached: bool,
    pub unborn: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub remote: Option<String>,
    pub scope: String,
    /// 每组展示上限（用于截断提示）
    pub max: usize,
}

/// 文件内容负载（旧版本文件/暂存内容读取）。
#[derive(Debug, Clone, Serialize)]
pub struct BlobPayload {
    pub data: Vec<u8>,
    pub size: u64,
    pub is_binary: bool,
}
