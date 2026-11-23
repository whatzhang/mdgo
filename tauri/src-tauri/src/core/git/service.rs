//! Git 只读服务：status / log / show / diff / refs。
//!
//! 所有方法同步执行（commands 直接调用）；Agent 工具用 `spawn_blocking` 包装，
//! 并通过 [`CancelFlag`] 在取消时 kill 子进程。
//!
//! 严格模式（`strict=true`）只由 Agent 工具传入：
//! - 内容限制在知识库子树；
//! - `.mdgo` 与敏感文件的路径/统计/内容全部隐藏；
//! - 显式 path 指向隐藏目标时直接报错。
//! UI commands 传 `strict=false`，保持用户可见自己的仓库全貌。

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use super::error::GitError;
use super::filter::{self, HiddenKind};
use super::limits::{BLOB_MAX_BYTES, GitLimits};
use super::model::{
    BlobPayload, CommitDetail, DiffResult, FileChange, GitStatus, LogResult, RefInfo, RefList,
    StatusCounts, StatusEntry,
};
use super::parse;
use super::runner::{CancelFlag, GitOutput, GitRunner, MAX_STDOUT_BYTES, RunOptions};
use super::scope::RepoScope;

const LOG_FORMAT: &str =
    "%H%x00%T%x00%P%x00%an%x00%ae%x00%at%x00%cn%x00%ce%x00%ct%x00%D%x00%s%x00%b";

/// status 查询。
#[derive(Debug, Clone)]
pub struct StatusQuery {
    pub path: Option<String>,
    pub max_entries: usize,
    pub strict: bool,
}

impl Default for StatusQuery {
    fn default() -> Self {
        Self {
            path: None,
            max_entries: super::limits::STATUS_MAX_ENTRIES,
            strict: true,
        }
    }
}

/// log 查询。
#[derive(Debug, Clone)]
pub struct LogQuery {
    pub limit: u32,
    pub skip: u32,
    pub rev: Option<String>,
    pub path: Option<String>,
    pub author: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub grep: Option<String>,
    pub pickaxe: Option<String>,
    pub ignore_case: bool,
    pub strict: bool,
}

impl Default for LogQuery {
    fn default() -> Self {
        Self {
            limit: super::limits::LOG_DEFAULT_LIMIT,
            skip: 0,
            rev: None,
            path: None,
            author: None,
            since: None,
            until: None,
            grep: None,
            pickaxe: None,
            ignore_case: false,
            strict: true,
        }
    }
}

/// show 查询。
#[derive(Debug, Clone)]
pub struct ShowQuery {
    pub rev: String,
    pub path: Option<String>,
    pub stat_only: bool,
    pub strict: bool,
}

/// diff 查询。
#[derive(Debug, Clone)]
pub struct DiffQuery {
    pub staged: bool,
    pub from: Option<String>,
    pub to: Option<String>,
    pub path: Option<String>,
    pub stat_only: bool,
    pub context: u8,
    pub ignore_whitespace: bool,
    pub strict: bool,
}

impl Default for DiffQuery {
    fn default() -> Self {
        Self {
            staged: false,
            from: None,
            to: None,
            path: None,
            stat_only: false,
            context: 3,
            ignore_whitespace: false,
            strict: true,
        }
    }
}

/// refs 查询。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    All,
    Branches,
    Tags,
    Remotes,
}

/// refs 查询参数。
#[derive(Debug, Clone)]
pub struct RefsQuery {
    pub kind: RefKind,
    pub max: usize,
}

impl Default for RefsQuery {
    fn default() -> Self {
        Self {
            kind: RefKind::All,
            max: super::limits::REFS_MAX_PER_GROUP,
        }
    }
}

/// Git 只读服务。
#[derive(Debug, Clone, Default)]
pub struct GitReadService {
    runner: GitRunner,
    limits: GitLimits,
}

impl GitReadService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(limits: GitLimits) -> Self {
        Self {
            runner: GitRunner::new(),
            limits,
        }
    }

    // ─────────────────────────── status ───────────────────────────

    pub fn status(
        &self,
        dir: &str,
        q: &StatusQuery,
        cancel: &CancelFlag,
    ) -> Result<GitStatus, GitError> {
        let scope = RepoScope::resolve(&self.runner, dir, cancel)?;
        if scope.bare {
            return Err(GitError::BareRepo);
        }
        let out = self.run_ok_strs(
            &scope.root,
            &[
                "status",
                "--porcelain=v2",
                "--branch",
                "--untracked-files=all",
                "-z",
            ],
            10,
            cancel,
        )?;
        if out.stdout_truncated {
            return Err(GitError::OutputTooLarge {
                limit: MAX_STDOUT_BYTES,
            });
        }
        let parsed = parse::parse_status_v2(&out.stdout);
        let path_filter = match q.path.as_deref() {
            Some(p) => Some(scope.to_repo_path(p, q.strict)?),
            None => None,
        };

        let mut hidden_mdgo = 0usize;
        let mut hidden_sensitive = 0usize;
        let mut hidden_outside = 0usize;
        let mut counts = StatusCounts::default();
        let mut entries: Vec<StatusEntry> = Vec::new();

        for e in parsed.entries {
            if q.strict {
                if !scope.contains(&e.path) {
                    hidden_outside += 1;
                    continue;
                }
                match filter::hidden_kind(&e.path) {
                    Some(HiddenKind::Mdgo) => {
                        hidden_mdgo += 1;
                        continue;
                    }
                    Some(HiddenKind::Sensitive) => {
                        hidden_sensitive += 1;
                        continue;
                    }
                    None => {}
                }
            }
            if let Some(filter_path) = &path_filter {
                if !path_matches(&e.path, filter_path) {
                    continue;
                }
            }

            let staged = !e.untracked && !e.conflicted && e.x != "." && e.x != " ";
            let unstaged = !e.untracked && !e.conflicted && e.y != "." && e.y != " ";
            if staged {
                counts.staged += 1;
            }
            if unstaged {
                counts.unstaged += 1;
            }
            if e.untracked {
                counts.untracked += 1;
            }
            if e.conflicted {
                counts.conflicted += 1;
            }

            entries.push(StatusEntry {
                path: scope.display_path(&e.path),
                from: e.from.as_deref().map(|p| scope.display_path(p)),
                kind: parse::status_kind(&e.x, &e.y, e.untracked, e.conflicted).to_string(),
                x: e.x,
                y: e.y,
                staged,
                unstaged,
            });
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let total_entries = entries.len();
        let truncated = total_entries > q.max_entries;
        entries.truncate(q.max_entries);

        Ok(GitStatus {
            root: scope.root_display(),
            scope: scope.label(),
            branch: parsed.branch,
            head: parsed.head,
            detached: parsed.detached,
            unborn: parsed.unborn,
            upstream: parsed.upstream,
            ahead: parsed.ahead,
            behind: parsed.behind,
            entries,
            counts,
            hidden_mdgo,
            hidden_sensitive,
            hidden_outside,
            truncated,
            total_entries,
        })
    }

    // ─────────────────────────── log ───────────────────────────

    pub fn log(&self, dir: &str, q: &LogQuery, cancel: &CancelFlag) -> Result<LogResult, GitError> {
        let scope = RepoScope::resolve(&self.runner, dir, cancel)?;
        let limit = q.limit.clamp(1, self.limits.log_max_limit);
        let rev = q.rev.clone().unwrap_or_else(|| "HEAD".to_string());
        if !scope.has_head(&self.runner, cancel)? {
            return Ok(LogResult {
                limit,
                skip: q.skip,
                rev,
                path: q.path.clone(),
                scope: scope.label(),
                unborn: true,
                maybe_more: false,
                commits: Vec::new(),
            });
        }
        let sha = scope.resolve_commit(&self.runner, &rev, cancel)?;
        let mut args: Vec<String> = vec![
            "log".into(),
            "-z".into(),
            "--no-color".into(),
            "--no-show-signature".into(),
            "--topo-order".into(),
            "--decorate=short".into(),
            "-n".into(),
            limit.to_string(),
            "--skip".into(),
            q.skip.to_string(),
            format!("--format={LOG_FORMAT}"),
            sha,
        ];
        if let Some(path) = q.path.as_deref() {
            let repo_path = scope.to_repo_path(path, q.strict)?;
            args.push("--".into());
            args.push(repo_path);
        }
        if let Some(author) = q.author.as_deref() {
            check_query_text(author, "author")?;
            args.push(format!("--author={author}"));
        }
        if let Some(grep) = q.grep.as_deref() {
            check_query_text(grep, "grep")?;
            args.push("--fixed-strings".into());
            args.push(format!("--grep={grep}"));
            if q.ignore_case {
                args.push("--regexp-ignore-case".into());
            }
        }
        if let Some(since) = q.since.as_deref() {
            check_query_text(since, "since")?;
            args.push(format!("--since={since}"));
        }
        if let Some(until) = q.until.as_deref() {
            check_query_text(until, "until")?;
            args.push(format!("--until={until}"));
        }
        if let Some(pickaxe) = q.pickaxe.as_deref() {
            check_query_text(pickaxe, "pickaxe")?;
            args.push(format!("-S{pickaxe}"));
        }

        let out = self.run_ok_owned(&scope.root, args, 20, cancel)?;
        if out.stdout_truncated {
            return Err(GitError::OutputTooLarge {
                limit: MAX_STDOUT_BYTES,
            });
        }
        let commits = parse::parse_log_z(&out.stdout);
        let maybe_more = commits.len() >= limit as usize;
        Ok(LogResult {
            commits,
            limit,
            skip: q.skip,
            rev,
            path: q.path.clone(),
            scope: scope.label(),
            unborn: false,
            maybe_more,
        })
    }

    // ─────────────────────────── show ───────────────────────────

    pub fn show(
        &self,
        dir: &str,
        q: &ShowQuery,
        cancel: &CancelFlag,
    ) -> Result<CommitDetail, GitError> {
        let scope = RepoScope::resolve(&self.runner, dir, cancel)?;
        if !scope.has_head(&self.runner, cancel)? {
            return Err(GitError::UnbornHead);
        }
        let sha = scope.resolve_commit(&self.runner, &q.rev, cancel)?;
        let meta = self.run_ok_owned(
            &scope.root,
            vec![
                "log".into(),
                "-1".into(),
                "-z".into(),
                "--no-color".into(),
                "--no-show-signature".into(),
                format!("--format={LOG_FORMAT}"),
                sha.clone(),
            ],
            10,
            cancel,
        )?;
        let mut commits = parse::parse_log_z(&meta.stdout);
        let summary = commits
            .pop()
            .ok_or_else(|| GitError::RevNotFound(q.rev.clone()))?;
        let body = truncate_chars(&summary.message, self.limits.meta_body_max_chars);
        let parent = scope.first_parent(&self.runner, &sha, cancel)?;

        let path_filter = match q.path.as_deref() {
            Some(p) => Some(scope.to_repo_path(p, q.strict)?),
            None => {
                if q.strict && !scope.prefix.is_empty() {
                    Some(scope.prefix.clone())
                } else {
                    None
                }
            }
        };

        let name_out = self.diff_tree(
            &scope,
            &sha,
            parent.as_deref(),
            true,
            path_filter.as_deref(),
            cancel,
        )?;
        let num_out = self.diff_tree(
            &scope,
            &sha,
            parent.as_deref(),
            false,
            path_filter.as_deref(),
            cancel,
        )?;
        if name_out.stdout_truncated || num_out.stdout_truncated {
            return Err(GitError::OutputTooLarge {
                limit: MAX_STDOUT_BYTES,
            });
        }
        let mut stats: HashMap<String, (Option<u32>, Option<u32>, bool)> = HashMap::new();
        for (path, add, del, binary) in parse::parse_numstat_z(&num_out.stdout) {
            stats.insert(path, (add, del, binary));
        }

        let mut hidden_files = 0usize;
        let mut hidden_mdgo = 0usize;
        let mut hidden_sensitive = 0usize;
        let mut files: Vec<FileChange> = Vec::new();
        for (path, raw, from) in parse::parse_name_status_z(&name_out.stdout) {
            if q.strict {
                let mut hidden_candidates: Vec<&str> = vec![path.as_str()];
                if let Some(f) = from.as_deref() {
                    hidden_candidates.push(f);
                }
                match filter::hidden_kind_any(&hidden_candidates) {
                    Some(HiddenKind::Mdgo) => {
                        hidden_mdgo += 1;
                        continue;
                    }
                    Some(HiddenKind::Sensitive) => {
                        hidden_sensitive += 1;
                        continue;
                    }
                    None => {}
                }
            }
            let (additions, deletions, binary) =
                stats.get(&path).copied().unwrap_or((None, None, false));
            files.push(FileChange {
                path: if q.strict {
                    scope.display_path(&path)
                } else {
                    path.clone()
                },
                from: from.map(|p| if q.strict { scope.display_path(&p) } else { p }),
                status: parse::file_status_label(&raw).to_string(),
                additions,
                deletions,
                binary,
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        if files.len() > self.limits.show_max_files {
            hidden_files += files.len() - self.limits.show_max_files;
            files.truncate(self.limits.show_max_files);
        }

        let (patch, patch_truncated) = if q.stat_only {
            (None, false)
        } else {
            let patch_raw = self.diff_tree_patch(
                &scope,
                &sha,
                parent.as_deref(),
                3,
                path_filter.as_deref(),
                cancel,
            )?;
            let (text, mdgo, sensitive, other, truncated) = filter_patch(
                &patch_raw.stdout_text(),
                q.strict,
                self.limits.patch_max_chars,
            );
            hidden_mdgo += mdgo;
            hidden_sensitive += sensitive;
            hidden_files += other;
            let text = if text.trim().is_empty() && patch_raw.stdout_truncated {
                String::new()
            } else {
                text
            };
            (
                if text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                },
                truncated || patch_raw.stdout_truncated,
            )
        };

        Ok(CommitDetail {
            summary,
            body,
            files,
            patch,
            patch_truncated,
            hidden_files,
            hidden_mdgo,
            hidden_sensitive,
            scope: scope.label(),
        })
    }

    // ─────────────────────────── diff ───────────────────────────

    pub fn diff(
        &self,
        dir: &str,
        q: &DiffQuery,
        cancel: &CancelFlag,
    ) -> Result<DiffResult, GitError> {
        if q.staged && (q.from.is_some() || q.to.is_some()) {
            return Err(GitError::InvalidQuery(
                "staged 与 from/to 不能同时使用".into(),
            ));
        }
        if q.to.is_some() && q.from.is_none() {
            return Err(GitError::InvalidQuery(
                "给了 to 就必须同时给 from（否则无法确定比较基准）".into(),
            ));
        }
        let scope = RepoScope::resolve(&self.runner, dir, cancel)?;
        if scope.bare {
            return Err(GitError::BareRepo);
        }

        let from_sha = match q.from.as_deref() {
            Some(r) => Some(scope.resolve_commit(&self.runner, r, cancel)?),
            None => None,
        };
        let to_sha = match q.to.as_deref() {
            Some(r) => Some(scope.resolve_commit(&self.runner, r, cancel)?),
            None => None,
        };

        let path_filter = match q.path.as_deref() {
            Some(p) => Some(scope.to_repo_path(p, q.strict)?),
            None => {
                if q.strict && !scope.prefix.is_empty() {
                    Some(scope.prefix.clone())
                } else {
                    None
                }
            }
        };

        let mode = if q.staged {
            "已暂存（index vs HEAD）".to_string()
        } else if let (Some(from), Some(to)) = (q.from.as_deref(), q.to.as_deref()) {
            format!("{from}..{to}（tree 对比）")
        } else if let Some(from) = q.from.as_deref() {
            format!("工作区 vs {from}（含已暂存与未暂存）")
        } else {
            "工作区 vs 暂存区（未暂存改动）".to_string()
        };

        let mut num_args: Vec<String> = vec![
            "diff".into(),
            "--numstat".into(),
            "-z".into(),
            "-M".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
        ];
        if q.staged {
            num_args.push("--cached".into());
        } else if let (Some(from), Some(to)) = (&from_sha, &to_sha) {
            num_args.push(from.clone());
            num_args.push(to.clone());
        } else if let Some(from) = &from_sha {
            num_args.push(from.clone());
        }
        if q.ignore_whitespace {
            num_args.push("--ignore-all-space".into());
        }
        if let Some(path) = &path_filter {
            num_args.push("--".into());
            num_args.push(path.clone());
        }
        let mut name_args = num_args.clone();
        for a in &mut name_args {
            if a == "--numstat" {
                *a = "--name-status".to_string();
            }
        }
        let num_out = self.run_ok_owned(&scope.root, num_args, 20, cancel)?;
        let name_out = self.run_ok_owned(&scope.root, name_args, 20, cancel)?;
        if num_out.stdout_truncated || name_out.stdout_truncated {
            return Err(GitError::OutputTooLarge {
                limit: MAX_STDOUT_BYTES,
            });
        }
        let mut statuses: HashMap<String, (String, Option<String>)> = HashMap::new();
        for (path, raw, from) in parse::parse_name_status_z(&name_out.stdout) {
            statuses.insert(path, (parse::file_status_label(&raw).to_string(), from));
        }

        let mut hidden_files = 0usize;
        let mut hidden_mdgo = 0usize;
        let mut hidden_sensitive = 0usize;
        let mut files: Vec<FileChange> = Vec::new();
        for (path, add, del, binary) in parse::parse_numstat_z(&num_out.stdout) {
            let (status, from) = statuses
                .get(&path)
                .cloned()
                .unwrap_or_else(|| ("modified".to_string(), None));
            if q.strict {
                let mut hidden_candidates: Vec<&str> = vec![path.as_str()];
                if let Some(f) = from.as_deref() {
                    hidden_candidates.push(f);
                }
                match filter::hidden_kind_any(&hidden_candidates) {
                    Some(HiddenKind::Mdgo) => {
                        hidden_mdgo += 1;
                        continue;
                    }
                    Some(HiddenKind::Sensitive) => {
                        hidden_sensitive += 1;
                        continue;
                    }
                    None => {}
                }
            }
            files.push(FileChange {
                path: if q.strict {
                    scope.display_path(&path)
                } else {
                    path
                },
                status,
                from: from.map(|p| if q.strict { scope.display_path(&p) } else { p }),
                additions: add,
                deletions: del,
                binary,
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));

        let (patch, truncated) = if q.stat_only {
            (None, false)
        } else {
            let mut patch_args: Vec<String> = vec![
                "diff".into(),
                "--patch".into(),
                "--no-color".into(),
                "--no-ext-diff".into(),
                "--no-textconv".into(),
                format!("--unified={}", q.context.min(10)),
            ];
            if q.staged {
                patch_args.push("--cached".into());
            } else if let (Some(from), Some(to)) = (&from_sha, &to_sha) {
                patch_args.push(from.clone());
                patch_args.push(to.clone());
            } else if let Some(from) = &from_sha {
                patch_args.push(from.clone());
            }
            if q.ignore_whitespace {
                patch_args.push("--ignore-all-space".into());
            }
            if let Some(path) = &path_filter {
                patch_args.push("--".into());
                patch_args.push(path.clone());
            }
            let out = self.run_ok_owned(&scope.root, patch_args, 20, cancel)?;
            let (text, mdgo, sensitive, other, mut cut) =
                filter_patch(&out.stdout_text(), q.strict, self.limits.patch_max_chars);
            hidden_mdgo += mdgo;
            hidden_sensitive += sensitive;
            hidden_files += other;
            cut |= out.stdout_truncated;
            (
                if text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                },
                cut,
            )
        };

        Ok(DiffResult {
            mode,
            files,
            patch,
            truncated,
            hidden_files,
            hidden_mdgo,
            hidden_sensitive,
            scope: scope.label(),
        })
    }

    // ─────────────────────────── refs ───────────────────────────

    pub fn refs(&self, dir: &str, q: &RefsQuery, cancel: &CancelFlag) -> Result<RefList, GitError> {
        let scope = RepoScope::resolve(&self.runner, dir, cancel)?;
        let format = "%(refname)%00%(objectname)%00%(*objectname)%00%(HEAD)%00%(upstream:short)%00%(upstream:track)";
        let out = self.run_ok_owned(
            &scope.root,
            vec![
                "for-each-ref".into(),
                format!("--format={format}"),
                "refs/heads".into(),
                "refs/remotes".into(),
                "refs/tags".into(),
            ],
            10,
            cancel,
        )?;
        if out.stdout_truncated {
            return Err(GitError::OutputTooLarge {
                limit: MAX_STDOUT_BYTES,
            });
        }

        let branch_out = self.run_strs(
            &scope.root,
            &["symbolic-ref", "-q", "--short", "HEAD"],
            5,
            cancel,
        )?;
        let current_branch = if branch_out.success() {
            let b = branch_out.stdout_text().trim().to_string();
            if b.is_empty() { None } else { Some(b) }
        } else {
            None
        };
        let head_out = self.run_strs(&scope.root, &["rev-parse", "HEAD"], 5, cancel)?;
        let head_hash = if head_out.success() {
            let h = head_out.stdout_text().trim().to_string();
            if h.is_empty() { None } else { Some(h) }
        } else {
            None
        };
        let remote_out = self.run_strs(&scope.root, &["remote"], 5, cancel)?;
        let mut remote_names: Vec<String> = remote_out
            .stdout_text()
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        remote_names.sort();
        let remote = if remote_names.iter().any(|n| n == "origin") {
            Some("origin".to_string())
        } else {
            remote_names.first().cloned()
        };

        let mut heads: Vec<RefInfo> = Vec::new();
        let mut remotes: Vec<RefInfo> = Vec::new();
        let mut tags: Vec<RefInfo> = Vec::new();
        for parsed in parse::parse_refs(&out.stdout) {
            if let Some(name) = parsed.full_name.strip_prefix("refs/heads/") {
                heads.push(parse::ref_info(&parsed, name.to_string()));
            } else if let Some(name) = parsed.full_name.strip_prefix("refs/remotes/") {
                remotes.push(parse::ref_info(&parsed, name.to_string()));
            } else if let Some(name) = parsed.full_name.strip_prefix("refs/tags/") {
                tags.push(parse::ref_info(&parsed, name.to_string()));
            }
        }
        if let Some(current) = &current_branch {
            for h in &mut heads {
                if &h.name == current {
                    h.current = true;
                }
            }
        }
        let sort_refs = |refs: &mut Vec<RefInfo>| {
            refs.sort_by(|a, b| (!a.current).cmp(&(!b.current)).then(a.name.cmp(&b.name)));
        };
        sort_refs(&mut heads);
        sort_refs(&mut remotes);
        sort_refs(&mut tags);

        let current_ref = heads.iter().find(|h| h.current).cloned();
        let upstream = current_ref.as_ref().and_then(|h| h.upstream.clone());
        let ahead = current_ref.as_ref().map(|h| h.ahead).unwrap_or(0);
        let behind = current_ref.as_ref().map(|h| h.behind).unwrap_or(0);
        let unborn = head_hash.is_none();
        let detached = !unborn && current_branch.is_none();
        let max = q.max.max(1);

        let mut refs = RefList {
            heads: heads.clone(),
            remotes: remotes.clone(),
            tags: tags.clone(),
            head_hash,
            current_branch: current_branch.clone(),
            detached,
            unborn,
            upstream,
            ahead,
            behind,
            remote,
            scope: scope.label(),
            max,
        };
        match q.kind {
            RefKind::All => {}
            RefKind::Branches => {
                refs.remotes.clear();
                refs.tags.clear();
            }
            RefKind::Tags => {
                refs.heads.clear();
                refs.remotes.clear();
            }
            RefKind::Remotes => {
                refs.heads.clear();
                refs.tags.clear();
            }
        }
        refs.heads.truncate(max);
        refs.remotes.truncate(max);
        refs.tags.truncate(max);
        Ok(refs)
    }

    // ─────────────────────────── 供 UI 复用的提交文件列表 ───────────────────────────

    pub fn commit_files(
        &self,
        dir: &str,
        commit: &str,
        parent: Option<&str>,
        strict: bool,
        cancel: &CancelFlag,
    ) -> Result<Vec<FileChange>, GitError> {
        let scope = RepoScope::resolve(&self.runner, dir, cancel)?;
        let sha = scope.resolve_commit(&self.runner, commit, cancel)?;
        let parent_sha = match parent {
            Some(p) => Some(scope.resolve_commit(&self.runner, p, cancel)?),
            None => scope.first_parent(&self.runner, &sha, cancel)?,
        };
        let name_out = self.diff_tree(&scope, &sha, parent_sha.as_deref(), true, None, cancel)?;
        let num_out = self.diff_tree(&scope, &sha, parent_sha.as_deref(), false, None, cancel)?;
        let mut stats: HashMap<String, (Option<u32>, Option<u32>, bool)> = HashMap::new();
        for (path, add, del, binary) in parse::parse_numstat_z(&num_out.stdout) {
            stats.insert(path, (add, del, binary));
        }
        let mut changes = Vec::new();
        for (path, raw, from) in parse::parse_name_status_z(&name_out.stdout) {
            if strict {
                let mut hidden_candidates: Vec<&str> = vec![path.as_str()];
                if let Some(f) = from.as_deref() {
                    hidden_candidates.push(f);
                }
                if filter::hidden_kind_any(&hidden_candidates).is_some() {
                    continue;
                }
            }
            let (additions, deletions, binary) =
                stats.get(&path).copied().unwrap_or((None, None, false));
            changes.push(FileChange {
                path,
                from,
                status: parse::file_status_label(&raw).to_string(),
                additions,
                deletions,
                binary,
            });
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(changes)
    }

    /// 读取任意 git 对象内容（调用方负责构造安全 spec，如 `<sha>:<repo_path>`）。
    pub fn read_blob(
        &self,
        dir: &str,
        spec: &str,
        cancel: &CancelFlag,
    ) -> Result<BlobPayload, GitError> {
        let scope = RepoScope::resolve(&self.runner, dir, cancel)?;
        self.read_blob_in_scope(&scope, spec, cancel)
    }

    /// 读取某个提交里的文件内容（严格模式下拒绝 `.mdgo`/敏感文件）。
    #[allow(dead_code)]
    pub fn read_file_at_rev(
        &self,
        dir: &str,
        rev: &str,
        path: &str,
        strict: bool,
        cancel: &CancelFlag,
    ) -> Result<BlobPayload, GitError> {
        let scope = RepoScope::resolve(&self.runner, dir, cancel)?;
        let sha = scope.resolve_commit(&self.runner, rev, cancel)?;
        let repo_path = scope.to_repo_path(path, strict)?;
        self.read_blob_in_scope(&scope, &format!("{sha}:{repo_path}"), cancel)
    }

    fn read_blob_in_scope(
        &self,
        scope: &RepoScope,
        spec: &str,
        cancel: &CancelFlag,
    ) -> Result<BlobPayload, GitError> {
        let size_out = self.run_ok_owned(
            &scope.root,
            vec!["cat-file".into(), "-s".into(), spec.to_string()],
            10,
            cancel,
        )?;
        let size: u64 = size_out
            .stdout_text()
            .trim()
            .parse()
            .map_err(|_| GitError::InvalidQuery(format!("无法读取对象大小: {spec}")))?;
        if size > BLOB_MAX_BYTES {
            return Err(GitError::OutputTooLarge {
                limit: BLOB_MAX_BYTES as usize,
            });
        }
        let out = self.run_ok_owned(
            &scope.root,
            vec!["cat-file".into(), "blob".into(), spec.to_string()],
            15,
            cancel,
        )?;
        if out.stdout_truncated {
            return Err(GitError::OutputTooLarge {
                limit: MAX_STDOUT_BYTES,
            });
        }
        let is_binary = out.stdout.iter().take(8000).any(|b| *b == 0);
        Ok(BlobPayload {
            size: out.stdout.len() as u64,
            data: out.stdout,
            is_binary,
        })
    }

    // ─────────────────────────── 内部执行辅助 ───────────────────────────

    fn run_strs(
        &self,
        root: &Path,
        args: &[&str],
        timeout_secs: u64,
        cancel: &CancelFlag,
    ) -> Result<GitOutput, GitError> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.run_owned(root, owned, timeout_secs, cancel)
    }

    fn run_owned(
        &self,
        root: &Path,
        args: Vec<String>,
        timeout_secs: u64,
        cancel: &CancelFlag,
    ) -> Result<GitOutput, GitError> {
        let opts = RunOptions {
            timeout: Duration::from_secs(timeout_secs),
            cancel: Some(cancel.as_atomic()),
            ..Default::default()
        };
        self.runner.run(root, &args, &opts)
    }

    fn run_ok_strs(
        &self,
        root: &Path,
        args: &[&str],
        timeout_secs: u64,
        cancel: &CancelFlag,
    ) -> Result<GitOutput, GitError> {
        let out = self.run_strs(root, args, timeout_secs, cancel)?;
        if !out.success() {
            return Err(GitError::from_stderr(&out.stderr_text()));
        }
        Ok(out)
    }

    fn run_ok_owned(
        &self,
        root: &Path,
        args: Vec<String>,
        timeout_secs: u64,
        cancel: &CancelFlag,
    ) -> Result<GitOutput, GitError> {
        let out = self.run_owned(root, args, timeout_secs, cancel)?;
        if !out.success() {
            return Err(GitError::from_stderr(&out.stderr_text()));
        }
        Ok(out)
    }

    fn diff_tree(
        &self,
        scope: &RepoScope,
        sha: &str,
        parent: Option<&str>,
        name_status: bool,
        path: Option<&str>,
        cancel: &CancelFlag,
    ) -> Result<GitOutput, GitError> {
        let mut args: Vec<String> = vec![
            "diff-tree".into(),
            "-r".into(),
            "-M".into(),
            "--no-commit-id".into(),
            if name_status {
                "--name-status".into()
            } else {
                "--numstat".into()
            },
            "-z".into(),
        ];
        if parent.is_none() {
            args.push("--root".into());
        }
        match parent {
            Some(p) => {
                args.push(p.to_string());
                args.push(sha.to_string());
            }
            None => args.push(sha.to_string()),
        }
        if let Some(path) = path {
            args.push("--".into());
            args.push(path.to_string());
        }
        self.run_ok_owned(&scope.root, args, 20, cancel)
    }

    fn diff_tree_patch(
        &self,
        scope: &RepoScope,
        sha: &str,
        parent: Option<&str>,
        context: u8,
        path: Option<&str>,
        cancel: &CancelFlag,
    ) -> Result<GitOutput, GitError> {
        let mut args: Vec<String> = vec![
            "diff-tree".into(),
            "-p".into(),
            "-r".into(),
            "-M".into(),
            "--no-commit-id".into(),
            "--no-color".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            format!("--unified={}", context.min(10)),
        ];
        if parent.is_none() {
            args.push("--root".into());
        }
        match parent {
            Some(p) => {
                args.push(p.to_string());
                args.push(sha.to_string());
            }
            None => args.push(sha.to_string()),
        }
        if let Some(path) = path {
            args.push("--".into());
            args.push(path.to_string());
        }
        self.run_ok_owned(&scope.root, args, 20, cancel)
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

fn check_query_text(value: &str, name: &str) -> Result<(), GitError> {
    if value.chars().any(|c| c == '\0' || c.is_control()) || value.chars().count() > 200 {
        return Err(GitError::InvalidQuery(format!(
            "{name} 包含控制字符或超过 200 字符"
        )));
    }
    Ok(())
}

fn path_matches(repo_path: &str, filter_path: &str) -> bool {
    repo_path == filter_path || repo_path.starts_with(&format!("{filter_path}/"))
}

/// 按 `diff --git` 分块过滤补丁：隐藏 `.mdgo`/敏感文件，严格模式跳过生成物/二进制；
/// 并按字符预算裁剪。
fn filter_patch(patch: &str, strict: bool, limit: usize) -> (String, usize, usize, usize, bool) {
    if patch.trim().is_empty() {
        return (String::new(), 0, 0, 0, false);
    }
    let mut blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in patch.lines() {
        if line.starts_with("diff --git ") && !current.is_empty() {
            blocks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.is_empty() {
        blocks.push(current);
    }

    let mut out = String::new();
    let mut hidden_mdgo = 0usize;
    let mut hidden_sensitive = 0usize;
    let mut hidden_other = 0usize;
    let mut truncated = false;

    for block in blocks {
        let paths = patch_block_paths(&block);
        if !paths.is_empty() {
            if strict {
                let refs: Vec<&str> = paths.iter().map(|p| p.as_str()).collect();
                match filter::hidden_kind_any(&refs) {
                    Some(HiddenKind::Mdgo) => {
                        hidden_mdgo += 1;
                        continue;
                    }
                    Some(HiddenKind::Sensitive) => {
                        hidden_sensitive += 1;
                        continue;
                    }
                    None => {}
                }
                if paths.iter().any(|p| filter::is_generated_path(p))
                    || block.contains("Binary files ")
                    || block.contains("GIT binary patch")
                {
                    hidden_other += 1;
                    continue;
                }
            }
        }
        let chars = block.chars().count();
        let used = out.chars().count();
        if used + chars > limit {
            let remaining = limit.saturating_sub(used);
            out.extend(block.chars().take(remaining));
            out.push_str("\n…（补丁已截断）\n");
            truncated = true;
            break;
        }
        out.push_str(&block);
    }
    (out, hidden_mdgo, hidden_sensitive, hidden_other, truncated)
}

fn patch_block_paths(block: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            push_patch_path(&mut paths, rest);
        } else if let Some(rest) = line.strip_prefix("--- a/") {
            push_patch_path(&mut paths, rest);
        }
    }
    if paths.is_empty() {
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("diff --git ") {
                if let Some(pos) = rest.rfind(" b/") {
                    let old = rest[..pos].trim_start_matches("a/");
                    push_patch_path(&mut paths, old);
                    push_patch_path(&mut paths, &rest[pos + 3..]);
                }
            }
        }
    }
    paths
}

fn push_patch_path(paths: &mut Vec<String>, raw: &str) {
    let p = raw.trim();
    if !p.is_empty() && p != "/dev/null" && !paths.iter().any(|x| x == p) {
        paths.push(p.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_patch_hides_sensitive_and_mdgo() {
        let patch = "diff --git a/.env b/.env\n+SECRET=1\n\
diff --git a/src/a.rs b/src/a.rs\n+ok\n\
diff --git a/.mdgo/config.yaml b/.mdgo/config.yaml\n+x\n";
        let (out, mdgo, sensitive, other, cut) = filter_patch(patch, true, 10_000);
        assert_eq!(mdgo, 1);
        assert_eq!(sensitive, 1);
        assert_eq!(other, 0);
        assert!(!cut);
        assert!(out.contains("src/a.rs"));
        assert!(!out.contains(".env"));
        assert!(!out.contains(".mdgo"));
    }

    #[test]
    fn filter_patch_truncates_on_budget() {
        let patch = "diff --git a/a.rs b/a.rs\n+1234567890\n";
        let (out, _, _, _, cut) = filter_patch(patch, true, 20);
        assert!(cut);
        assert!(out.contains("补丁已截断"));
    }

    #[test]
    fn path_matches_file_and_dir() {
        assert!(path_matches("src/a.rs", "src/a.rs"));
        assert!(path_matches("src/a.rs", "src"));
        assert!(!path_matches("src2/a.rs", "src"));
    }

    #[test]
    fn query_text_rejects_controls() {
        assert!(check_query_text("normal", "author").is_ok());
        assert!(check_query_text("bad\nline", "author").is_err());
    }
}
