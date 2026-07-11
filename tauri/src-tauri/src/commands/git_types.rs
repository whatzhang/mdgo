use serde::Serialize;

/// 提交信息（与 isomorphic-git 格式完全兼容）
#[derive(Serialize, Clone)]
pub struct CommitInfo {
    pub oid: String,           // SHA-1 hash
    #[serde(rename = "commit")]
    pub commit_inner: CommitInner, // 内部 commit 对象（与 isomorphic-git 一致）
}

/// Commit 内部结构（与 isomorphic-git 一致）
#[derive(Serialize, Clone)]
pub struct CommitInner {
    pub message: String,       // 提交信息
    pub tree: String,          // tree hash
    pub parent: Vec<String>,   // 父提交列表
    pub author: AuthorInfo,
    pub committer: AuthorInfo,
}

/// 作者信息
#[derive(Serialize, Clone)]
pub struct AuthorInfo {
    pub name: String,
    pub email: String,
    pub timestamp: u64,
}

/// 引用信息（分支、标签等）
#[derive(Serialize)]
pub struct RefsInfo {
    pub heads: Vec<RefInfo>,       // 本地分支
    pub remotes: Vec<RefInfo>,     // 远程分支
    pub tags: Vec<RefInfo>,        // 标签
    pub head_hash: Option<String>, // HEAD hash
    pub current_branch: String,    // 当前分支名
}

/// 单个引用信息
#[derive(Serialize)]
pub struct RefInfo {
    pub name: String,
    pub hash: String,
}

/// 文件变更信息（与 gitComputeChangedFiles 格式兼容）
#[derive(Serialize, Clone)]
pub struct FileChange {
    pub path: String,
    pub status: String, // "added", "modified", "removed"
}

/// Blob 读取结果（与 isomorphic-git readBlob 格式兼容）
#[derive(Serialize)]
pub struct BlobResult {
    pub blob: Vec<u8>, // 二进制数据（前端会用 TextDecoder 解码）
}

/// 文件状态条目（兼容 isomorphic-git statusMatrix 格式）
/// 格式: (filepath, head, workdir, stage)
pub type FileStatusEntry = (String, i32, i32, i32);