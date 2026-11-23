//! Git 读能力的统一错误类型与中文映射。
//!
//! 设计约束：
//! - 不把 git 原始 stderr 直接塞给模型：先归类，再给「原因 + 下一步」；
//! - 只读路径（status/log/show/diff/refs）与写路径（commit/checkout）共用；
//! - 错误可序列化/克隆，便于工具层与命令层各自包装。

use std::fmt;

/// Git 操作错误（读/写共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// 未安装 git 或不在 PATH 中
    NotInstalled,
    /// 目录不是 Git 仓库
    NotARepo,
    /// 裸仓库（没有工作区）
    BareRepo,
    /// 仓库还没有任何提交（unborn HEAD）
    UnbornHead,
    /// 路径非法（绝对路径/盘符/.. /控制字符/越界）
    InvalidPath(String),
    /// 敏感文件或 `.mdgo` 内部数据：严格模式下拒绝读取
    SensitivePath(String),
    /// 查询参数非法（author/grep/since 等）
    InvalidQuery(String),
    /// 引用名非法（选项式注入/非法字符）
    InvalidRev(String),
    /// 引用不存在
    RevNotFound(String),
    /// 命令超时
    Timeout(u64),
    /// 调用被取消
    Cancelled,
    /// 输出超过上限
    OutputTooLarge { limit: usize },
    /// index.lock 被占用
    IndexLocked,
    /// Git 因仓库所有者不一致拒绝访问
    UnsafeRepo(String),
    /// git 正常退出但返回非 0
    Git { code: Option<i32>, stderr: String },
    /// IO/进程错误
    Io(String),
}

impl GitError {
    /// 面向用户/模型的主消息（不含「建议」）。
    pub fn message(&self) -> String {
        match self {
            GitError::NotInstalled => {
                "未找到 git 可执行文件；请安装 Git 并确保它在 PATH 中。".to_string()
            }
            GitError::NotARepo => {
                "当前知识库目录不是 Git 仓库（未找到 .git）。请打开仓库目录，或在目录中执行 `git init`。"
                    .to_string()
            }
            GitError::BareRepo => {
                "当前目录是裸仓库（bare），没有工作区；可以查看历史/分支，但无法查看工作区状态与差异。"
                    .to_string()
            }
            GitError::UnbornHead => {
                "仓库尚无提交（unborn HEAD），当前文件均为未跟踪。".to_string()
            }
            GitError::InvalidPath(p) => format!(
                "路径不合法：{p}。路径必须是知识库内的相对路径，不能是绝对路径、不能包含 `..`、不能以 `-` 开头。"
            ),
            GitError::SensitivePath(p) => format!(
                "`{p}` 属于敏感文件或 `.mdgo` 内部数据，严格模式下不允许读取（内容与路径都不会返回）。"
            ),
            GitError::InvalidQuery(m) => format!("查询参数不合法：{m}"),
            GitError::InvalidRev(r) => format!(
                "引用名不合法：{r}。请使用分支名、标签名或提交 SHA；不要包含空白、`..`、`@{{`、`:` 或以 `-` 开头。"
            ),
            GitError::RevNotFound(r) => format!(
                "找不到引用 `{r}`。可用 git_log 查看最近提交，或用 git_refs 查看分支/标签。"
            ),
            GitError::Timeout(secs) => format!(
                "Git 读取超时（{secs}s）。仓库可能过大或位于慢速磁盘；请用 path/limit 缩小范围后重试。"
            ),
            GitError::Cancelled => "Git 读取已取消。".to_string(),
            GitError::OutputTooLarge { limit } => format!(
                "Git 输出超过上限（{limit} 字节）。请用 path 缩小范围，或使用 stat_only=true。"
            ),
            GitError::IndexLocked => {
                "Git 索引被其他操作占用（index.lock 存在）。请稍后重试；不要删除 `.git/index.lock`，除非确认没有 Git 进程在运行。"
                    .to_string()
            }
            GitError::UnsafeRepo(detail) => format!(
                "Git 拒绝访问：仓库所有者与当前用户不一致（dubious ownership）。请在终端执行 `git config --global --add safe.directory <仓库路径>` 后重试。原始信息：{detail}"
            ),
            GitError::Git { code, stderr } => {
                let s = truncate(stderr.trim(), 800);
                match code {
                    Some(c) if !s.is_empty() => format!("Git 执行失败（退出码 {c}）：{s}"),
                    Some(c) => format!("Git 执行失败（退出码 {c}，无错误输出）。"),
                    None => format!("Git 执行失败（进程被终止）：{s}"),
                }
            }
            GitError::Io(e) => format!("Git 执行失败：{e}"),
        }
    }

    /// 可操作建议（无则返回 None）。
    pub fn suggestion(&self) -> Option<&'static str> {
        match self {
            GitError::NotInstalled => Some("安装 Git 后重启应用，或在设置中确认 Git 路径。"),
            GitError::NotARepo => Some("打开包含 .git 的目录，或先执行 git init。"),
            GitError::BareRepo => {
                Some("查看历史用 git_log、查看分支用 git_refs；工作区状态不可用。")
            }
            GitError::UnbornHead => Some("先完成一次提交，再查看历史/差异。"),
            GitError::InvalidPath(_) | GitError::SensitivePath(_) => {
                Some("传知识库内的相对路径；敏感文件与 .mdgo 不参与只读查询。")
            }
            GitError::InvalidQuery(_) => {
                Some("检查 author/grep/since/until 等过滤条件，去掉换行与控制字符。")
            }
            GitError::InvalidRev(_) | GitError::RevNotFound(_) => {
                Some("先用 git_refs 查看分支/标签，或 git_log 查看最近提交。")
            }
            GitError::Timeout(_) | GitError::OutputTooLarge { .. } => {
                Some("缩小范围：加 path、降低 limit、或使用 stat_only=true。")
            }
            GitError::IndexLocked => Some("等待其他 Git 操作结束后重试。"),
            GitError::UnsafeRepo(_) => Some("按提示把仓库路径加入 safe.directory 后重试。"),
            _ => None,
        }
    }

    /// 工具层使用的完整文案（主消息 + 建议）。
    pub fn to_user_message(&self) -> String {
        match self.suggestion() {
            Some(s) => format!("{}\n建议：{}", self.message(), s),
            None => self.message(),
        }
    }

    /// 从 git stderr 归类常见错误；未识别时保留原始输出。
    pub fn from_stderr(stderr: &str) -> Self {
        let raw = stderr.trim();
        let lower = raw.to_lowercase();
        if lower.contains("not a git repository") {
            return GitError::NotARepo;
        }
        if lower.contains("dubious ownership") {
            return GitError::UnsafeRepo(truncate(raw, 400));
        }
        if lower.contains("index.lock") {
            return GitError::IndexLocked;
        }
        if lower.contains("does not have any commits yet")
            || lower.contains("your current branch") && lower.contains("does not have any commits")
        {
            return GitError::UnbornHead;
        }
        if lower.contains("unknown revision")
            || lower.contains("bad revision")
            || lower.contains("ambiguous argument")
            || lower.contains("not a valid object name")
            || lower.contains("needed a single revision")
            || lower.contains("bad object")
        {
            return GitError::RevNotFound(rev_from_stderr(raw).unwrap_or_default());
        }
        if lower.contains("pathspec")
            || lower.contains("no such path")
            || lower.contains("does not exist in")
        {
            return GitError::InvalidPath(truncate(raw, 300));
        }
        if lower.contains("is a bare repository") {
            return GitError::BareRepo;
        }
        GitError::Git {
            code: None,
            stderr: truncate(raw, 800),
        }
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for GitError {}

impl From<std::io::Error> for GitError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::NotFound {
            GitError::NotInstalled
        } else {
            GitError::Io(e.to_string())
        }
    }
}

fn rev_from_stderr(raw: &str) -> Option<String> {
    // 典型：fatal: ambiguous argument 'HEAD~99': unknown revision...
    for quote in ['\'', '`'] {
        if let Some(start) = raw.find(quote) {
            if let Some(end) = raw[start + 1..].find(quote) {
                let candidate = &raw[start + 1..start + 1 + end];
                if !candidate.is_empty() && candidate.len() <= 200 {
                    return Some(candidate.to_string());
                }
            }
        }
    }
    None
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_stderr() {
        assert_eq!(
            GitError::from_stderr(
                "fatal: not a git repository (or any of the parent directories): .git"
            ),
            GitError::NotARepo
        );
        assert_eq!(
            GitError::from_stderr("fatal: Unable to create 'G:/x/.git/index.lock': File exists."),
            GitError::IndexLocked
        );
        match GitError::from_stderr(
            "fatal: ambiguous argument 'HEAD~99': unknown revision or path not in the working tree.",
        ) {
            GitError::RevNotFound(rev) => assert_eq!(rev, "HEAD~99"),
            other => panic!("expected RevNotFound, got {other:?}"),
        }
        assert_eq!(
            GitError::from_stderr("fatal: detected dubious ownership in repository"),
            GitError::UnsafeRepo("fatal: detected dubious ownership in repository".into())
        );
    }

    #[test]
    fn user_message_keeps_suggestion_separate() {
        let msg = GitError::NotARepo.to_user_message();
        assert!(msg.contains("不是 Git 仓库"));
        assert!(msg.contains("建议"));
    }
}
