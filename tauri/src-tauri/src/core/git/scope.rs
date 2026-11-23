//! 仓库作用域与路径/引用校验。
//!
//! 关键语义：
//! - `root`：`git rev-parse --show-toplevel` 的仓库根；
//! - `base`：当前知识库目录；
//! - `prefix`：知识库相对仓库根的路径（空表示知识库就是仓库根）；
//! - 内容类操作默认限制在知识库子树内，元数据（log/refs）可覆盖全仓库。
//!
//! 严格模式（Agent 侧）在 `to_repo_path` 阶段直接拒绝 `.mdgo` 与敏感文件，
//! 而不是先读取再隐藏。

use std::path::{Path, PathBuf};

use super::error::GitError;
use super::filter;
use super::runner::{CancelFlag, GitRunner, RunOptions};

/// 仓库作用域。
#[derive(Debug, Clone)]
pub struct RepoScope {
    pub root: PathBuf,
    #[allow(dead_code)] // 保留知识库绝对路径，供后续读取/诊断使用
    pub base: PathBuf,
    pub prefix: String,
    pub bare: bool,
}

impl RepoScope {
    /// 解析目录所属仓库。非仓库返回 `NotARepo`；裸仓库返回 `bare=true`。
    pub fn resolve(runner: &GitRunner, dir: &str, cancel: &CancelFlag) -> Result<Self, GitError> {
        let dir = dir.trim();
        if dir.is_empty() {
            return Err(GitError::InvalidPath("仓库路径为空".into()));
        }
        let base = std::fs::canonicalize(dir)
            .map_err(|e| GitError::InvalidPath(format!("目录不存在或不可访问: {e}")))?;
        if !base.is_dir() {
            return Err(GitError::InvalidPath(format!(
                "不是目录: {}",
                base.display()
            )));
        }

        let opts = RunOptions {
            timeout: std::time::Duration::from_secs(10),
            cancel: Some(cancel.as_atomic()),
            ..Default::default()
        };
        let top = runner.run(
            &base,
            &["rev-parse".into(), "--show-toplevel".into()],
            &opts,
        )?;
        if top.success() {
            let text = top.stdout_text();
            let root_text = text.trim();
            if root_text.is_empty() {
                return Err(GitError::NotARepo);
            }
            let root = std::fs::canonicalize(root_text)
                .map_err(|e| GitError::Io(format!("无法解析仓库根 {}: {e}", root_text)))?;
            let prefix = relative_prefix(&base, &root).ok_or_else(|| {
                GitError::InvalidPath(format!(
                    "知识库目录 {} 不在仓库根 {} 内",
                    base.display(),
                    root.display()
                ))
            })?;
            return Ok(Self {
                root,
                base,
                prefix,
                bare: false,
            });
        }

        // 不是工作区时，再看是不是裸仓库（裸仓库仍可读 log/refs）。
        let bare = runner.run(
            &base,
            &["rev-parse".into(), "--is-bare-repository".into()],
            &opts,
        )?;
        if bare.success() && bare.stdout_text().trim() == "true" {
            return Ok(Self {
                root: base.clone(),
                base,
                prefix: String::new(),
                bare: true,
            });
        }
        Err(GitError::NotARepo)
    }

    /// 把知识库相对路径转换为仓库根相对路径。`strict=true` 时拒绝 `.mdgo`/敏感文件。
    pub fn to_repo_path(&self, raw: &str, strict: bool) -> Result<String, GitError> {
        let rel = normalize_rel_path(raw)?;
        if strict {
            filter::ensure_agent_readable(&rel).map_err(GitError::SensitivePath)?;
        }
        let repo_path = if self.prefix.is_empty() {
            rel
        } else {
            format!("{}/{}", self.prefix, rel)
        };
        if strict {
            filter::ensure_agent_readable(&repo_path).map_err(GitError::SensitivePath)?;
        }
        if !self.contains(&repo_path) {
            return Err(GitError::InvalidPath(format!(
                "路径越界：{} 不在当前知识库作用域内",
                raw
            )));
        }
        Ok(repo_path)
    }

    /// 仓库相对路径 → 面向用户的展示路径（相对知识库）。
    pub fn display_path(&self, repo_path: &str) -> String {
        let p = repo_path.replace('\\', "/");
        if self.prefix.is_empty() {
            return p;
        }
        let prefix = self.prefix.replace('\\', "/");
        if p == prefix {
            return ".".to_string();
        }
        p.strip_prefix(&(prefix + "/")).unwrap_or(&p).to_string()
    }

    /// 仓库相对路径是否在知识库作用域内。
    pub fn contains(&self, repo_path: &str) -> bool {
        if self.prefix.is_empty() {
            return true;
        }
        let prefix = self.prefix.replace('\\', "/");
        let p = repo_path.replace('\\', "/");
        p == prefix || p.starts_with(&(prefix + "/"))
    }

    /// 作用域展示文案。
    pub fn label(&self) -> String {
        if self.bare {
            "裸仓库".to_string()
        } else if self.prefix.is_empty() {
            "当前仓库根".to_string()
        } else {
            format!("当前知识库子目录（{}）", self.prefix)
        }
    }

    /// 展示用仓库根（去掉 Windows canonical `\\?\` 前缀）。
    pub fn root_display(&self) -> String {
        display_path_string(&self.root)
    }

    /// 校验并解析引用为完整 commit SHA。
    ///
    /// 先做字符白名单校验，再 `rev-parse --verify --quiet`；`--end-of-options`
    /// 不被旧版 Git 支持时回退（引用已严格校验，回退仍安全）。
    pub fn resolve_commit(
        &self,
        runner: &GitRunner,
        rev: &str,
        cancel: &CancelFlag,
    ) -> Result<String, GitError> {
        validate_rev(rev)?;
        let spec = format!("{rev}^{{commit}}");
        let opts = RunOptions {
            timeout: std::time::Duration::from_secs(10),
            cancel: Some(cancel.as_atomic()),
            ..Default::default()
        };
        let with_guard = vec![
            "rev-parse".to_string(),
            "--verify".into(),
            "--quiet".into(),
            "--end-of-options".into(),
            spec.clone(),
        ];
        let out = runner.run(&self.root, &with_guard, &opts)?;
        if out.success() {
            let sha = out.stdout_text().trim().to_string();
            if !sha.is_empty() {
                return Ok(sha);
            }
        }
        // 旧版 Git 不认识 --end-of-options；引用已经过白名单校验，回退安全。
        let fallback = vec![
            "rev-parse".to_string(),
            "--verify".into(),
            "--quiet".into(),
            spec,
        ];
        let out = runner.run(&self.root, &fallback, &opts)?;
        if out.success() {
            let sha = out.stdout_text().trim().to_string();
            if !sha.is_empty() {
                return Ok(sha);
            }
        }
        Err(GitError::RevNotFound(rev.to_string()))
    }

    /// 当前 HEAD 是否存在（空仓库返回 false）。
    pub fn has_head(&self, runner: &GitRunner, cancel: &CancelFlag) -> Result<bool, GitError> {
        let opts = RunOptions {
            timeout: std::time::Duration::from_secs(10),
            cancel: Some(cancel.as_atomic()),
            ..Default::default()
        };
        let out = runner.run(
            &self.root,
            &[
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                "HEAD".into(),
            ],
            &opts,
        )?;
        Ok(out.success() && !out.stdout_text().trim().is_empty())
    }

    /// 第一父提交（根提交返回 None）。
    pub fn first_parent(
        &self,
        runner: &GitRunner,
        commit: &str,
        cancel: &CancelFlag,
    ) -> Result<Option<String>, GitError> {
        let opts = RunOptions {
            timeout: std::time::Duration::from_secs(10),
            cancel: Some(cancel.as_atomic()),
            ..Default::default()
        };
        let out = runner.run(
            &self.root,
            &[
                "rev-list".into(),
                "--parents".into(),
                "-n".into(),
                "1".into(),
                commit.to_string(),
            ],
            &opts,
        )?;
        if !out.success() {
            return Err(GitError::from_stderr(&out.stderr_text()));
        }
        Ok(super::parse::parse_first_parent(&out.stdout))
    }
}

/// 规范化知识库相对路径：拒绝绝对路径/盘符/`..`/控制字符/`:`/`-` 开头。
pub fn normalize_rel_path(raw: &str) -> Result<String, GitError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(GitError::InvalidPath("路径为空".into()));
    }
    if raw.chars().any(|c| c == '\u{0}' || c.is_control()) {
        return Err(GitError::InvalidPath(format!("路径包含控制字符: {raw}")));
    }
    let unified = raw.replace('\\', "/");
    if unified.starts_with('/') {
        return Err(GitError::InvalidPath(format!("仅支持相对路径: {raw}")));
    }
    let bytes = unified.as_bytes();
    if bytes.len() > 1 && bytes[1] == b':' {
        return Err(GitError::InvalidPath(format!("仅支持相对路径: {raw}")));
    }
    if unified.starts_with('-') {
        return Err(GitError::InvalidPath(format!(
            "路径不能以 - 开头（防止被当成 git 选项）: {raw}"
        )));
    }
    if unified.contains(':') {
        return Err(GitError::InvalidPath(format!(
            "路径不能包含 `:`（与 rev:path 语法冲突）: {raw}"
        )));
    }

    let mut out: Vec<&str> = Vec::new();
    for seg in unified.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return Err(GitError::InvalidPath(format!("路径不允许包含 `..`: {raw}"))),
            s => out.push(s),
        }
    }
    if out.is_empty() {
        return Err(GitError::InvalidPath("路径为空".into()));
    }
    let joined = out.join("/");
    if filter::is_mdgo_internal(&joined) {
        return Err(GitError::SensitivePath(joined));
    }
    Ok(joined)
}

/// 校验引用名：只允许常见字符，拒绝选项式注入与 rev 语法歧义。
pub fn validate_rev(rev: &str) -> Result<(), GitError> {
    let rev = rev.trim();
    if rev.is_empty() || rev.len() > 200 || rev.starts_with('-') {
        return Err(GitError::InvalidRev(rev.to_string()));
    }
    if rev.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(GitError::InvalidRev(rev.to_string()));
    }
    if rev.contains("..") || rev.contains("@{") || rev.contains('\\') || rev.contains(':') {
        return Err(GitError::InvalidRev(rev.to_string()));
    }
    let ok = rev.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '.' | '_' | '/' | '@' | '{' | '}' | '~' | '^' | '+' | '-')
    });
    if !ok {
        return Err(GitError::InvalidRev(rev.to_string()));
    }
    Ok(())
}

/// 校验提交对象名（完整/短 hash）。
#[allow(dead_code)]
pub fn validate_oid(oid: &str) -> Result<(), GitError> {
    if oid.len() < 4 || oid.len() > 64 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(GitError::InvalidRev(oid.to_string()));
    }
    Ok(())
}

fn relative_prefix(base: &Path, root: &Path) -> Option<String> {
    if let Ok(rel) = base.strip_prefix(root) {
        let s = rel.to_string_lossy().replace('\\', "/");
        return Some(s.trim_matches('/').to_string());
    }
    // Windows 大小写差异：退回字符串比较（仅用于前缀判断）。
    let base_key = path_key(base);
    let root_key = path_key(root);
    if base_key == root_key {
        return Some(String::new());
    }
    base_key
        .strip_prefix(&(root_key + "/"))
        .map(|s| s.to_string())
}

fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.strip_prefix("//?/").unwrap_or(&s).to_string();
    #[cfg(target_os = "windows")]
    {
        s.to_ascii_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        s
    }
}

pub fn display_path_string(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    s.strip_prefix("//?/").unwrap_or(&s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_rejects_escape_and_absolute() {
        assert!(normalize_rel_path("../x").is_err());
        assert!(normalize_rel_path("C:/x").is_err());
        assert!(normalize_rel_path("/x").is_err());
        assert!(normalize_rel_path("-p").is_err());
        assert!(normalize_rel_path("a:b").is_err());
        assert_eq!(normalize_rel_path("a\\b\\c.md").unwrap(), "a/b/c.md");
        assert_eq!(normalize_rel_path("./a.md").unwrap(), "a.md");
        assert_eq!(normalize_rel_path("a//b/./c.md").unwrap(), "a/b/c.md");
    }

    #[test]
    fn normalize_rejects_mdgo() {
        assert!(normalize_rel_path(".mdgo/config.yaml").is_err());
        assert!(normalize_rel_path("a/.MDGO/x").is_err());
    }

    #[test]
    fn validate_rejects_option_like_rev() {
        assert!(validate_rev("--upload-pack=x").is_err());
        assert!(validate_rev("HEAD --output=x").is_err());
        assert!(validate_rev("HEAD:path").is_err());
        assert!(validate_rev("a..b").is_err());
        assert!(validate_rev("a@{1}").is_err());
        assert!(validate_rev("HEAD").is_ok());
        assert!(validate_rev("origin/main").is_ok());
        assert!(validate_rev("HEAD~2").is_ok());
        assert!(validate_oid("abc123").is_ok());
        assert!(validate_oid("--hard").is_err());
    }

    #[test]
    fn scope_paths_are_relative_to_kb() {
        let scope = RepoScope {
            root: PathBuf::from("G:/repo"),
            base: PathBuf::from("G:/repo/notes"),
            prefix: "notes".to_string(),
            bare: false,
        };
        assert_eq!(scope.to_repo_path("a.md", true).unwrap(), "notes/a.md");
        assert_eq!(scope.display_path("notes/a.md"), "a.md");
        assert!(scope.contains("notes/a.md"));
        assert!(!scope.contains("src/a.rs"));
        assert_eq!(scope.label(), "当前知识库子目录（notes）");
    }
}
