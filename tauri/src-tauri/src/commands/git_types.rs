use serde::Serialize;

/// 提交信息（与前端约定：`c.oid` / `c.commit.*`）
#[derive(Serialize, Clone)]
pub struct CommitInfo {
    pub oid: String,
    #[serde(rename = "commit")]
    pub commit_inner: CommitInner,
}

/// 提交内容。`message` 为完整消息（subject + 空行 + body，来自 `%B`）
#[derive(Serialize, Clone)]
pub struct CommitInner {
    pub message: String,
    pub tree: String,
    pub parent: Vec<String>,
    pub author: AuthorInfo,
    pub committer: AuthorInfo,
}

/// 作者 / 提交者
#[derive(Serialize, Clone)]
pub struct AuthorInfo {
    pub name: String,
    pub email: String,
    /// Unix 秒
    pub timestamp: u64,
}

/// 引用信息
#[derive(Serialize)]
pub struct RefsInfo {
    pub heads: Vec<RefInfo>,
    pub remotes: Vec<RefInfo>,
    pub tags: Vec<RefInfo>,
    pub head_hash: Option<String>,
    pub current_branch: String,
    /// HEAD 是否处于游离状态
    pub detached: bool,
}

#[derive(Serialize, Clone)]
pub struct RefInfo {
    pub name: String,
    pub hash: String,
}

/// 提交内的单个文件变更
#[derive(Serialize, Clone)]
pub struct FileChange {
    pub path: String,
    /// added | removed | modified | renamed | copied | conflicted
    pub status: String,
    /// 重命名 / 复制时的原路径
    pub from: Option<String>,
    /// 新增行数（二进制或未知为 None）
    pub additions: Option<u32>,
    /// 删除行数（二进制或未知为 None）
    pub deletions: Option<u32>,
    pub binary: bool,
}

/// 工作区状态条目（原始 porcelain XY 码）
#[derive(Serialize, Clone)]
pub struct StatusEntry {
    pub path: String,
    /// 暂存区状态字符（A/M/D/R/C/U/?）
    pub x: String,
    /// 工作区状态字符（A/M/D/U/?）
    pub y: String,
    /// 重命名 / 复制时的原路径
    pub from: Option<String>,
}

/// 文件内容负载（带体积与二进制判定，前端据此避免乱码 diff）
#[derive(Serialize)]
pub struct BlobPayload {
    pub data: Vec<u8>,
    pub size: u64,
    pub is_binary: bool,
}

/// 提交身份
#[derive(Serialize)]
pub struct Identity {
    pub name: String,
    pub email: String,
}
