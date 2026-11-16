//! Git 核心能力（Tauri 端）
//!
//! 只实现四条主链路：**暂存 / 提交 / 记录 / 对比**。
//! 分支切换、fetch/pull/push、stash、rebase、chery-pick 等一律不实现。
//!
//! 设计约束（对应本次"核心功能必须完全正确"的要求）：
//! 1. 所有路径参数统一走 `--literal-pathspecs` + `--`，避免把 `-x` 文件名当选项；
//! 2. 所有 git 输出用 `-z` 或 `\x00/\x1e` 分隔解析，不依赖空格/`|`，文件名含空格/换行也正确；
//! 3. 不再缓存：路径/引用/状态在每次调用时实时读取，写操作后无需失效逻辑；
//! 4. stdout/stderr 各用一个线程读取，避免大 stderr 时误判超时；
//! 5. 空仓库（unborn HEAD）是一等公民：log 返回空列表、unstage 走 `rm --cached`。

use crate::commands::git_types::*;
use serde::Serialize;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

// ==================== 常量 ====================

/// 单条 git 命令超时（防网络驱动器/巨型仓库无限阻塞）
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// 网络类命令（pull/push）超时：远端握手 + 传输可能远慢于本地命令
const GIT_NETWORK_TIMEOUT: Duration = Duration::from_secs(120);
/// 单文件 diff 的体量上限（超过则不读入内存，前端显示占位）
const MAX_BLOB_BYTES: u64 = 2 * 1024 * 1024;
/// 二进制嗅探字节数
const BINARY_SNIFF_BYTES: usize = 8000;
/// 单次 log 最多返回条数（防御性上限）
const MAX_LOG_DEPTH: u32 = 1000;

// ==================== 进程执行 ====================

/// 创建 git 命令：Windows 下隐藏控制台；统一关闭交互提示
fn git_cmd() -> Command {
    let mut cmd = Command::new("git");
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    // 不弹凭据、不写可选锁、固定 UTF-8 输出，避免 locale 影响解析
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null());
    cmd
}

struct GitOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: Option<i32>,
}

impl GitOutput {
    fn success(&self) -> bool {
        self.code == Some(0)
    }

    fn stderr_text(&self) -> String {
        let s = String::from_utf8_lossy(&self.stderr).trim().to_string();
        if s.chars().count() > 2000 {
            let mut out: String = s.chars().take(2000).collect();
            out.push('…');
            out
        } else {
            s
        }
    }
}

fn wait_stream(rx: mpsc::Receiver<Vec<u8>>, deadline: Instant) -> Option<Vec<u8>> {
    let now = Instant::now();
    if now >= deadline {
        return None;
    }
    rx.recv_timeout(deadline - now).ok()
}

/// 执行 git 命令（stdout/stderr 分别读取，超时 kill）
fn run_git(args: &[&str], dir: &str) -> Result<GitOutput, String> {
    run_git_with_timeout(args, dir, GIT_TIMEOUT)
}

/// 执行 git 命令（可指定超时；网络命令使用更长的超时）
fn run_git_with_timeout(args: &[&str], dir: &str, timeout: Duration) -> Result<GitOutput, String> {
    if dir.is_empty() {
        return Err("仓库路径为空".to_string());
    }
    if !Path::new(dir).is_dir() {
        return Err(format!("仓库目录不存在: {}", dir));
    }

    let mut child = git_cmd()
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 git 失败（请确认已安装 Git 并在 PATH 中）: {}", e))?;

    let stdout = child.stdout.take().ok_or("无法获取 git stdout")?;
    let stderr = child.stderr.take().ok_or("无法获取 git stderr")?;

    let (tx_out, rx_out) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let mut reader = stdout;
        let _ = reader.read_to_end(&mut buf);
        let _ = tx_out.send(buf);
    });
    let (tx_err, rx_err) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let mut reader = stderr;
        let _ = reader.read_to_end(&mut buf);
        let _ = tx_err.send(buf);
    });

    let deadline = Instant::now() + timeout;
    let out_buf = wait_stream(rx_out, deadline);
    let err_buf = wait_stream(rx_err, deadline);

    if out_buf.is_none() || err_buf.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "git 命令超时（{}s）: git {}",
            timeout.as_secs(),
            args.join(" ")
        ));
    }

    let status = child
        .wait()
        .map_err(|e| format!("等待 git 进程失败: {}", e))?;

    Ok(GitOutput {
        stdout: out_buf.unwrap_or_default(),
        stderr: err_buf.unwrap_or_default(),
        code: status.code(),
    })
}

/// 执行 git 命令，失败时把 stderr 作为错误抛出
fn run_git_ok(args: &[&str], dir: &str) -> Result<Vec<u8>, String> {
    let out = run_git(args, dir)?;
    if !out.success() {
        let msg = out.stderr_text();
        return Err(if msg.is_empty() {
            format!("git {} 执行失败", args.join(" "))
        } else {
            msg
        });
    }
    Ok(out.stdout)
}

fn run_git_str(args: &[&str], dir: &str) -> Result<String, String> {
    let bytes = run_git_ok(args, dir)?;
    Ok(String::from_utf8_lossy(&bytes).trim_end().to_string())
}

// ==================== 参数校验 ====================

/// 校验相对路径：拒绝绝对路径、盘符、`..` 逃逸；统一为正斜杠
fn normalize_rel_path(raw: &str) -> Result<String, String> {
    let p = raw.trim();
    if p.is_empty() {
        return Err("路径为空".to_string());
    }
    let unified = p.replace('\\', "/");
    if unified.starts_with('/') || (unified.len() > 1 && unified.as_bytes()[1] == b':') {
        return Err(format!("仅支持仓库内相对路径: {}", raw));
    }
    if unified.split('/').any(|seg| seg == "..") {
        return Err(format!("路径不允许包含 ..: {}", raw));
    }
    let trimmed = unified.trim_start_matches("./").trim_matches('/').to_string();
    if trimmed.is_empty() {
        return Err("路径为空".to_string());
    }
    Ok(trimmed)
}

/// literal pathspec，避免 `*`/`?`/`:!` 被当成通配或排除规则
fn literal_pathspec(path: &str) -> String {
    format!(":(literal){}", path)
}

/// 校验引用名：只允许常见字符，且不能以 `-` 开头（防止被当选项）
fn validate_rev(rev: &str) -> Result<(), String> {
    if rev.is_empty() || rev.starts_with('-') || rev.len() > 200 {
        return Err(format!("非法引用: {}", rev));
    }
    let ok = rev.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '@' | '{' | '}' | '~' | '^' | '-')
    });
    if !ok {
        return Err(format!("非法引用: {}", rev));
    }
    Ok(())
}

/// 校验提交对象名（完整/短 hash），不接受任意 rev 表达式
fn validate_oid(oid: &str) -> Result<(), String> {
    if oid.len() < 4 || oid.len() > 64 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("非法提交对象: {}", oid));
    }
    Ok(())
}

// ==================== 解析器（纯函数，带单测） ====================

/// 解析 `git log --format` 输出。
/// 记录分隔 `\x1e`，字段分隔 `\x00`，字段顺序：
/// oid, tree, parents, author name, author email, author ts, committer name, committer email, committer ts, message(%B)
fn parse_log_output(bytes: &[u8]) -> Result<Vec<CommitInfo>, String> {
    let text = String::from_utf8_lossy(bytes);
    let mut commits = Vec::new();
    for raw in text.split('\u{1e}') {
        let rec = raw.trim_matches(|c| c == '\n' || c == '\r');
        if rec.is_empty() {
            continue;
        }
        let fields: Vec<&str> = rec.splitn(10, '\u{0}').collect();
        if fields.len() < 10 {
            return Err(format!("提交记录字段不足（{}）", fields.len()));
        }
        let timestamp = |s: &str| -> Result<u64, String> {
            s.trim()
                .parse::<u64>()
                .map_err(|_| format!("提交时间戳解析失败: {}", s))
        };
        commits.push(CommitInfo {
            oid: fields[0].to_string(),
            commit_inner: CommitInner {
                tree: fields[1].to_string(),
                parent: fields[2]
                    .split_whitespace()
                    .map(|s| s.to_string())
                    .collect(),
                author: AuthorInfo {
                    name: fields[3].to_string(),
                    email: fields[4].to_string(),
                    timestamp: timestamp(fields[5])?,
                },
                committer: AuthorInfo {
                    name: fields[6].to_string(),
                    email: fields[7].to_string(),
                    timestamp: timestamp(fields[8])?,
                },
                message: fields[9].to_string(),
            },
        });
    }
    Ok(commits)
}

/// 解析 `git status --porcelain=v1 -z` 输出。
/// 普通条目：`XY path\0`；重命名：`XY new\0old\0`
fn parse_status_z(bytes: &[u8]) -> Vec<StatusEntry> {
    let mut tokens: Vec<String> = bytes
        .split(|b| *b == 0)
        .map(|b| String::from_utf8_lossy(b).to_string())
        .collect();
    if tokens.last().map(|s| s.is_empty()).unwrap_or(false) {
        tokens.pop();
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let entry = &tokens[i];
        if entry.len() < 3 {
            i += 1;
            continue;
        }
        let x = entry[0..1].to_string();
        let y = entry[1..2].to_string();
        let path = entry[3..].to_string();
        let mut from = None;
        if x == "R" || x == "C" {
            if i + 1 < tokens.len() {
                // git status -z 的重命名顺序是 `XY <new>\0<old>\0`
                from = Some(tokens[i + 1].clone());
                i += 2;
                out.push(StatusEntry {
                    path,
                    x,
                    y,
                    from,
                });
                continue;
            }
        }
        i += 1;
        out.push(StatusEntry {
            path,
            x,
            y,
            from,
        });
    }
    out
}

/// 解析 `git diff-tree -r -M --name-status -z`：
/// 普通条目 `<status>\0<path>\0`；重命名 `<Rxxx>\0<old>\0<new>\0`
fn parse_name_status_z(bytes: &[u8]) -> Vec<(String, String, Option<String>)> {
    let mut tokens: Vec<String> = bytes
        .split(|b| *b == 0)
        .map(|b| String::from_utf8_lossy(b).to_string())
        .collect();
    if tokens.last().map(|s| s.is_empty()).unwrap_or(false) {
        tokens.pop();
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let status = tokens[i].clone();
        if status.is_empty() {
            i += 1;
            continue;
        }
        let kind = status.chars().next().unwrap_or('M');
        if kind == 'R' || kind == 'C' {
            if i + 2 >= tokens.len() {
                break;
            }
            let old = tokens[i + 1].clone();
            let new = tokens[i + 2].clone();
            out.push((new, status, Some(old)));
            i += 3;
        } else {
            if i + 1 >= tokens.len() {
                break;
            }
            let path = tokens[i + 1].clone();
            out.push((path, status, None));
            i += 2;
        }
    }
    out
}

/// 解析 `git diff-tree -r -M --numstat -z`：
/// 普通条目 `<add>\t<del>\t<path>\0`；重命名 `<add>\t<del>\t\0<old>\0<new>\0`
fn parse_numstat_z(bytes: &[u8]) -> Vec<(String, Option<u32>, Option<u32>, bool)> {
    let mut tokens: Vec<String> = bytes
        .split(|b| *b == 0)
        .map(|b| String::from_utf8_lossy(b).to_string())
        .collect();
    if tokens.last().map(|s| s.is_empty()).unwrap_or(false) {
        tokens.pop();
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let head = tokens[i].clone();
        let mut it = head.splitn(3, '\t');
        let add = it.next().unwrap_or("").to_string();
        let del = it.next().unwrap_or("").to_string();
        let path = it.next().unwrap_or("").to_string();
        i += 1;
        let real_path = if path.is_empty() {
            // 重命名：后面还有 old、new 两个 token
            if i + 1 < tokens.len() {
                let new_path = tokens[i + 1].clone();
                i += 2;
                new_path
            } else {
                break;
            }
        } else {
            path
        };
        let binary = add == "-" || del == "-";
        out.push((
            real_path,
            add.parse::<u32>().ok(),
            del.parse::<u32>().ok(),
            binary,
        ));
    }
    out
}

fn status_label(raw: &str) -> &'static str {
    match raw.chars().next().unwrap_or('M') {
        'A' => "added",
        'D' => "removed",
        'R' => "renamed",
        'C' => "copied",
        'T' => "modified",
        'U' => "conflicted",
        _ => "modified",
    }
}

// ==================== 命令：记录 ====================

/// 提交记录（分页）
///
/// - `depth`：本页条数（默认 100，上限 1000）
/// - `skip`：跳过条数，用于「加载更多」
/// - `filepath`：只取涉及该文件的提交（仓库内相对路径）
/// - `ref_name`：起始引用（默认 HEAD；不传则只看当前分支历史）
#[tauri::command]
pub fn git_log(
    dir: String,
    depth: Option<u32>,
    filepath: Option<String>,
    ref_name: Option<String>,
    skip: Option<u32>,
) -> Result<Vec<CommitInfo>, String> {
    let limit = depth.unwrap_or(100).clamp(1, MAX_LOG_DEPTH);
    let skip = skip.unwrap_or(0);
    let rev = ref_name.unwrap_or_else(|| "HEAD".to_string());
    validate_rev(&rev)?;

    // 空仓库：HEAD 不存在 → 返回空历史，而不是报错
    if !has_head(&dir)? {
        return Ok(Vec::new());
    }

    // %x00 字段分隔 + %x1e 记录分隔，消息正文里的换行/竖线都不会破坏解析
    const FORMAT: &str = "--format=%H%x00%T%x00%P%x00%an%x00%ae%x00%at%x00%cn%x00%ce%x00%ct%x00%B%x00%x1e";
    let limit_s = limit.to_string();
    let skip_s = skip.to_string();

    let mut args: Vec<String> = vec![
        "-c".into(),
        "core.quotepath=false".into(),
        "-c".into(),
        "log.showSignature=false".into(),
        "log".into(),
        "--topo-order".into(),
        "--no-show-signature".into(),
        "--no-color".into(),
        "-n".into(),
        limit_s,
        "--skip".into(),
        skip_s,
        FORMAT.into(),
        rev,
    ];

    let normalized_path = match filepath.as_deref() {
        Some(p) if !p.trim().is_empty() => Some(normalize_rel_path(p)?),
        _ => None,
    };
    if let Some(p) = normalized_path.as_deref() {
        args.push("--".into());
        args.push(literal_pathspec(p));
    }

    let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let out = run_git_ok(&argv, &dir)?;
    parse_log_output(&out)
}

/// 某次提交的变更文件列表（含重命名、真实增删行数、二进制标记）
#[tauri::command]
pub fn git_commit_files(
    dir: String,
    commit: String,
    parent: Option<String>,
) -> Result<Vec<FileChange>, String> {
    validate_oid(&commit)?;
    let parent = match parent {
        Some(p) if !p.trim().is_empty() => {
            validate_oid(&p)?;
            Some(p)
        }
        _ => first_parent(&dir, &commit)?,
    };

    let mut name_args: Vec<String> = vec![
        "-c".into(),
        "core.quotepath=false".into(),
        "diff-tree".into(),
        "-r".into(),
        "-M".into(),
        "--no-commit-id".into(),
        "--name-status".into(),
        "-z".into(),
    ];
    let mut num_args: Vec<String> = vec![
        "-c".into(),
        "core.quotepath=false".into(),
        "diff-tree".into(),
        "-r".into(),
        "-M".into(),
        "--no-commit-id".into(),
        "--numstat".into(),
        "-z".into(),
    ];
    match &parent {
        Some(p) => {
            name_args.push(p.clone());
            name_args.push(commit.clone());
            num_args.push(p.clone());
            num_args.push(commit.clone());
        }
        None => {
            name_args.push("--root".into());
            name_args.push(commit.clone());
            num_args.push("--root".into());
            num_args.push(commit.clone());
        }
    }

    let name_argv: Vec<&str> = name_args.iter().map(|s| s.as_str()).collect();
    let num_argv: Vec<&str> = num_args.iter().map(|s| s.as_str()).collect();
    let name_out = run_git_ok(&name_argv, &dir)?;
    let num_out = run_git_ok(&num_argv, &dir)?;

    let mut stats: std::collections::HashMap<String, (Option<u32>, Option<u32>, bool)> =
        std::collections::HashMap::new();
    for (path, add, del, binary) in parse_numstat_z(&num_out) {
        stats.insert(path, (add, del, binary));
    }

    let mut changes: Vec<FileChange> = parse_name_status_z(&name_out)
        .into_iter()
        .map(|(path, raw, from)| {
            let (additions, deletions, binary) =
                stats.get(&path).copied().unwrap_or((None, None, false));
            FileChange {
                status: status_label(&raw).to_string(),
                path,
                from,
                additions,
                deletions,
                binary,
            }
        })
        .collect();

    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}

/// 读取某个提交里的文件内容（带体积上限与二进制判定）
#[tauri::command]
pub fn git_read_blob(dir: String, oid: String, filepath: String) -> Result<BlobPayload, String> {
    validate_oid(&oid)?;
    let rel = normalize_rel_path(&filepath)?;
    read_blob_spec(&dir, &format!("{}:{}", oid, rel))
}

/// 读取已暂存（index, stage 0）的文件内容，用于「已暂存」视角的对比
#[tauri::command]
pub fn git_read_index_blob(dir: String, filepath: String) -> Result<BlobPayload, String> {
    let rel = normalize_rel_path(&filepath)?;
    read_blob_spec(&dir, &format!(":0:{}", rel))
}

/// 解析引用（分支 / 远程 / 标签 / HEAD 状态）
#[tauri::command]
pub fn git_parse_refs(dir: String) -> Result<RefsInfo, String> {
    // Windows 上单个 git 进程约 50ms：把 6 个互不依赖的查询并发执行，
    // 首屏耗时只取决于最慢的一条，而不是累加。
    let (refs_out, branch_out, head_out, remote_out, upstream_out, ab_out) =
        std::thread::scope(|s| {
            let h_refs = s.spawn(|| {
                run_git_str(
                    &[
                        "-c",
                        "core.quotepath=false",
                        "for-each-ref",
                        "--format=%(refname)%00%(objectname)%00%(*objectname)",
                        "refs/heads",
                        "refs/remotes",
                        "refs/tags",
                    ],
                    &dir,
                )
            });
            let h_branch = s.spawn(|| run_git(&["symbolic-ref", "-q", "--short", "HEAD"], &dir));
            let h_head = s.spawn(|| run_git(&["rev-parse", "--verify", "-q", "HEAD"], &dir));
            let h_remote = s.spawn(|| run_git_str(&["remote"], &dir));
            let h_up = s.spawn(|| {
                run_git(
                    &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
                    &dir,
                )
            });
            let h_ab = s.spawn(|| {
                run_git(
                    &["rev-list", "--left-right", "--count", "@{u}...HEAD"],
                    &dir,
                )
            });
            (
                h_refs
                    .join()
                    .unwrap_or_else(|_| Err("refs 线程崩溃".to_string())),
                h_branch
                    .join()
                    .unwrap_or_else(|_| Err("branch 线程崩溃".to_string())),
                h_head
                    .join()
                    .unwrap_or_else(|_| Err("HEAD 线程崩溃".to_string())),
                h_remote
                    .join()
                    .unwrap_or_else(|_| Err("remote 线程崩溃".to_string())),
                h_up
                    .join()
                    .unwrap_or_else(|_| Err("upstream 线程崩溃".to_string())),
                h_ab
                    .join()
                    .unwrap_or_else(|_| Err("ahead/behind 线程崩溃".to_string())),
            )
        });

    let out = refs_out?;

    let mut heads = Vec::new();
    let mut remotes = Vec::new();
    let mut tags = Vec::new();

    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.split('\u{0}');
        let full = parts.next().unwrap_or("");
        let oid = parts.next().unwrap_or("");
        let peeled = parts.next().unwrap_or("");
        if full.is_empty() || oid.is_empty() {
            continue;
        }
        let resolved = if peeled.is_empty() { oid } else { peeled };
        if let Some(name) = full.strip_prefix("refs/heads/") {
            heads.push(RefInfo {
                name: name.to_string(),
                hash: resolved.to_string(),
            });
        } else if let Some(name) = full.strip_prefix("refs/remotes/") {
            remotes.push(RefInfo {
                name: name.to_string(),
                hash: resolved.to_string(),
            });
        } else if let Some(name) = full.strip_prefix("refs/tags/") {
            tags.push(RefInfo {
                name: name.to_string(),
                hash: resolved.to_string(),
            });
        }
    }

    let (current_branch, detached) = match branch_out {
        Ok(o) if o.success() => {
            let name = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if name.is_empty() {
                ("HEAD".to_string(), true)
            } else {
                (name, false)
            }
        }
        _ => ("HEAD".to_string(), true),
    };

    let head_hash = head_out
        .ok()
        .filter(|o| o.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    let mut remote_names: Vec<String> = remote_out
        .unwrap_or_default()
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

    let upstream = upstream_out
        .ok()
        .filter(|o| o.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    // 左=上游独有（behind），右=本地独有（ahead）
    let (mut ahead, mut behind) = (0u32, 0u32);
    if let Ok(o) = ab_out {
        if o.success() {
            let text = String::from_utf8_lossy(&o.stdout);
            let mut it = text.split_whitespace();
            behind = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            ahead = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        }
    }

    Ok(RefsInfo {
        heads,
        remotes,
        tags,
        head_hash,
        current_branch,
        detached,
        remote,
        upstream,
        ahead,
        behind,
    })
}

// ==================== 命令：暂存 / 提交 ====================

/// 暂存文件（支持新增 / 修改 / 删除；目录会递归）
#[tauri::command]
pub fn git_stage(dir: String, paths: Vec<String>) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut args: Vec<String> = vec!["--literal-pathspecs".into(), "add".into(), "-A".into(), "--".into()];
    for p in &paths {
        args.push(normalize_rel_path(p)?);
    }
    let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_git_ok(&argv, &dir)?;
    Ok(())
}

/// 取消暂存（空仓库时用 `rm --cached`，有 HEAD 时用 `reset HEAD --`）
#[tauri::command]
pub fn git_unstage(dir: String, paths: Vec<String>) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let normalized: Vec<String> = paths
        .iter()
        .map(|p| normalize_rel_path(p))
        .collect::<Result<Vec<_>, _>>()?;

    if has_head(&dir)? {
        let mut args: Vec<String> = vec![
            "--literal-pathspecs".into(),
            "reset".into(),
            "-q".into(),
            "HEAD".into(),
            "--".into(),
        ];
        args.extend(normalized);
        let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        run_git_ok(&argv, &dir)?;
    } else {
        let mut args: Vec<String> = vec![
            "--literal-pathspecs".into(),
            "rm".into(),
            "--cached".into(),
            "-q".into(),
            "--ignore-unmatch".into(),
            "--".into(),
        ];
        args.extend(normalized);
        let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        run_git_ok(&argv, &dir)?;
    }
    Ok(())
}

/// 放弃工作区更改（把工作区恢复到 **index** 内容，保留已暂存的内容）
///
/// 对应 UI 上「变更」列表里的放弃：只丢弃该文件尚未暂存的修改，
/// 不会顺手把已暂存的内容也撤回（已暂存内容要用「取消暂存」）。
/// 未跟踪文件/目录的删除由前端走回收站或显式确认，不在此命令内。
#[tauri::command]
pub fn git_discard(dir: String, paths: Vec<String>) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut args: Vec<String> = vec![
        "--literal-pathspecs".into(),
        "restore".into(),
        "--worktree".into(),
        "--".into(),
    ];
    for p in &paths {
        args.push(normalize_rel_path(p)?);
    }
    let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_git_ok(&argv, &dir)?;
    Ok(())
}

/// 提交身份（读 git config；未配置时返回空字符串）
#[tauri::command]
pub fn git_identity(dir: String) -> Result<Identity, String> {
    // 一次 git 调用同时取 name/email（原来两次进程，各约 50ms）
    let out = run_git(&["config", "--get-regexp", "^user\\.(name|email)$"], &dir)?;
    let mut name = String::new();
    let mut email = String::new();
    if out.success() {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut parts = line.splitn(2, ' ');
            let key = parts.next().unwrap_or("").trim();
            let value = parts.next().unwrap_or("").trim();
            if key == "user.name" {
                name = value.to_string();
            } else if key == "user.email" {
                email = value.to_string();
            }
        }
    }
    Ok(Identity { name, email })
}

/// 写入仓库级提交身份（`git config --local`）
#[tauri::command]
pub fn git_set_identity(dir: String, name: String, email: String) -> Result<(), String> {
    let name = name.trim();
    let email = email.trim();
    if name.is_empty() || email.is_empty() {
        return Err("提交姓名与邮箱都不能为空".to_string());
    }
    if name.contains('\n') || email.contains('\n') {
        return Err("提交身份不能包含换行".to_string());
    }
    run_git_ok(&["config", "--local", "user.name", name], &dir)?;
    run_git_ok(&["config", "--local", "user.email", email], &dir)?;
    Ok(())
}

/// 提交暂存内容，返回完整 commit SHA
///
/// - 未显式传作者时使用 git config（仓库级优先）
/// - `--no-gpg-sign`：避免 `commit.gpgsign=true` 的仓库卡在 pinentry 直到超时
#[tauri::command]
pub fn git_commit(
    dir: String,
    message: String,
    author_name: Option<String>,
    author_email: Option<String>,
) -> Result<String, String> {
    let message = message.trim_end().to_string();
    if message.trim().is_empty() {
        return Err("提交消息不能为空".to_string());
    }

    // 没有暂存内容就不要生成空提交
    let cached = run_git(&["diff", "--cached", "--quiet"], &dir)?;
    if cached.success() {
        return Err("没有已暂存的更改，无法提交".to_string());
    }

    let mut args: Vec<String> = vec!["-c".into(), "core.quotepath=false".into()];
    if let (Some(name), Some(email)) = (author_name.as_deref(), author_email.as_deref()) {
        let name = name.trim();
        let email = email.trim();
        if !name.is_empty() && !email.is_empty() {
            args.push("-c".into());
            args.push(format!("user.name={}", name));
            args.push("-c".into());
            args.push(format!("user.email={}", email));
        }
    }
    args.push("commit".into());
    args.push("--no-gpg-sign".into());
    args.push("-m".into());
    args.push(message);

    let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_git_ok(&argv, &dir)?;

    // 返回值必须是真实 SHA（不要解析本地化的 "[main abc1234]" 输出）
    let sha = run_git_str(&["rev-parse", "HEAD"], &dir)?;
    if sha.is_empty() {
        return Err("提交成功但无法读取 HEAD".to_string());
    }
    Ok(sha)
}

// ==================== 命令：工作区状态 ====================

/// 工作区状态（原始 porcelain XY 码，未跟踪文件展开到文件级）
#[tauri::command]
pub fn git_status(dir: String) -> Result<Vec<StatusEntry>, String> {
    let out = run_git_ok(
        &[
            "-c",
            "core.quotepath=false",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
        ],
        &dir,
    )?;
    Ok(parse_status_z(&out))
}

// ==================== 内部辅助 ====================

fn has_head(dir: &str) -> Result<bool, String> {
    let out = run_git(&["rev-parse", "--verify", "-q", "HEAD"], dir)?;
    Ok(out.success())
}

fn first_parent(dir: &str, commit: &str) -> Result<Option<String>, String> {
    let out = run_git_str(&["rev-list", "--parents", "-n", "1", commit], dir)?;
    let mut parts = out.split_whitespace();
    let _self_oid = parts.next();
    Ok(parts.next().map(|s| s.to_string()))
}

fn read_blob_spec(dir: &str, spec: &str) -> Result<BlobPayload, String> {
    let size_text = run_git_str(&["cat-file", "-s", spec], dir)?;
    let size: u64 = size_text
        .trim()
        .parse()
        .map_err(|_| format!("无法读取对象大小: {}", spec))?;
    if size > MAX_BLOB_BYTES {
        return Err(format!(
            "文件过大（{:.1} MB），跳过内容加载",
            size as f64 / (1024.0 * 1024.0)
        ));
    }
    let data = run_git_ok(&["cat-file", "blob", spec], dir)?;
    let is_binary = data.iter().take(BINARY_SNIFF_BYTES).any(|b| *b == 0);
    Ok(BlobPayload {
        size: data.len() as u64,
        is_binary,
        data,
    })
}

// ==================== 远端同步：拉取 / 推送 ====================

/// 远端状态：是否配置 remote、当前分支、上游分支、ahead/behind
#[derive(Debug, Clone, Serialize)]
pub struct RemoteStatus {
    pub has_remote: bool,
    pub remote: Option<String>,
    pub branch: String,
    pub detached: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

/// 拉取 / 推送结果
#[derive(Debug, Clone, Serialize)]
pub struct SyncResult {
    pub ok: bool,
    pub message: String,
    pub detail: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

fn output_detail(out: &GitOutput) -> String {
    let mut text = String::new();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !stderr.is_empty() {
        text.push_str(&stderr);
    }
    if !stdout.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&stdout);
    }
    if text.chars().count() > 4000 {
        text = text.chars().take(4000).collect::<String>() + "\n…";
    }
    text
}

fn remote_status_internal(dir: &str) -> Result<RemoteStatus, String> {
    // 复用 parse_refs 的并发查询结果，不再重复跑一遍 git
    let refs = git_parse_refs(dir.to_string())?;
    Ok(RemoteStatus {
        has_remote: refs.remote.is_some(),
        remote: refs.remote,
        branch: refs.current_branch,
        detached: refs.detached,
        upstream: refs.upstream,
        ahead: refs.ahead,
        behind: refs.behind,
    })
}

/// 把 git 的原始错误转成可操作的中文提示
fn friendly_sync_error(raw: &str) -> String {
    let m = raw.to_lowercase();
    if m.contains("authentication failed")
        || m.contains("could not read username")
        || m.contains("permission denied")
        || m.contains("terminal prompts disabled")
    {
        return format!(
            "认证失败：请在终端配置 Git 凭据或 SSH key 后重试。\n{}",
            raw
        );
    }
    if m.contains("not possible to fast-forward") {
        return "本地与远端已分叉，无法快进合并。请先手动合并后再拉取。".to_string();
    }
    if m.contains("local changes")
        || m.contains("would be overwritten")
        || m.contains("please commit your changes")
        || m.contains("please stash")
    {
        return "本地存在未提交的改动，拉取会覆盖这些文件；请先提交或暂存后再拉取。".to_string();
    }
    if m.contains("no upstream branch") {
        return "当前分支没有上游分支，请先推送并设置上游（git push -u origin <branch>）。"
            .to_string();
    }
    if m.contains("couldn't find remote ref")
        || m.contains("does not appear to be a git repository")
        || m.contains("repository not found")
        || m.contains("remote branch not found")
    {
        return format!("远端仓库或分支不存在：{}", raw);
    }
    if m.contains("non-fast-forward")
        || m.contains("fetch first")
        || m.contains("rejected")
        || m.contains("failed to push some refs")
    {
        return "推送被拒绝：远端有本地没有的提交，请先拉取合并后再推送。".to_string();
    }
    if raw.trim().is_empty() {
        "同步失败（git 未返回错误信息）".to_string()
    } else {
        raw.to_string()
    }
}

/// 远端状态（是否配置 remote、上游分支、ahead/behind）
#[tauri::command]
pub fn git_remote_status(dir: String) -> Result<RemoteStatus, String> {
    remote_status_internal(dir.trim())
}

/// 拉取：只做快进合并，避免留下半成品 merge 状态
///
/// 网络操作可能耗时较长，放到 blocking 线程执行，避免阻塞 IPC。
#[tauri::command]
pub async fn git_pull(dir: String) -> Result<SyncResult, String> {
    tokio::task::spawn_blocking(move || git_pull_impl(dir))
        .await
        .map_err(|e| format!("拉取任务执行失败: {}", e))?
}

fn git_pull_impl(dir: String) -> Result<SyncResult, String> {
    let dir = dir.trim().to_string();
    let st = remote_status_internal(&dir)?;
    if st.detached {
        return Err("当前处于游离 HEAD 状态，无法拉取".to_string());
    }
    if st.upstream.is_none() {
        return Err(format!("当前分支 {} 未设置上游分支，无法拉取", st.branch));
    }

    let out = run_git_with_timeout(&["pull", "--ff-only"], &dir, GIT_NETWORK_TIMEOUT)?;
    if !out.success() {
        return Err(friendly_sync_error(&out.stderr_text()));
    }

    let after = remote_status_internal(&dir)?;
    Ok(SyncResult {
        ok: true,
        message: format!(
            "已拉取 {}",
            after.upstream.clone().unwrap_or_else(|| "远端".to_string())
        ),
        detail: output_detail(&out),
        upstream: after.upstream,
        ahead: after.ahead,
        behind: after.behind,
    })
}

/// 推送：有上游直接 push；无上游但有 remote 时用 `-u` 建立跟踪
#[tauri::command]
pub async fn git_push(dir: String) -> Result<SyncResult, String> {
    tokio::task::spawn_blocking(move || git_push_impl(dir))
        .await
        .map_err(|e| format!("推送任务执行失败: {}", e))?
}

fn git_push_impl(dir: String) -> Result<SyncResult, String> {
    let dir = dir.trim().to_string();
    let st = remote_status_internal(&dir)?;
    if st.detached {
        return Err("当前处于游离 HEAD 状态，无法推送".to_string());
    }
    if !st.has_remote {
        return Err("没有配置远程仓库（remote），无法推送".to_string());
    }

    let args: Vec<String> = if st.upstream.is_some() {
        vec!["push".to_string()]
    } else {
        vec![
            "push".to_string(),
            "-u".to_string(),
            st.remote
                .clone()
                .unwrap_or_else(|| "origin".to_string()),
            st.branch.clone(),
        ]
    };
    let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let out = run_git_with_timeout(&argv, &dir, GIT_NETWORK_TIMEOUT)?;
    if !out.success() {
        return Err(friendly_sync_error(&out.stderr_text()));
    }

    let after = remote_status_internal(&dir)?;
    Ok(SyncResult {
        ok: true,
        message: after
            .upstream
            .clone()
            .map(|u| format!("已推送 {}", u))
            .unwrap_or_else(|| "已推送".to_string()),
        detail: output_detail(&out),
        upstream: after.upstream,
        ahead: after.ahead,
        behind: after.behind,
    })
}

// ==================== 单测：解析器 ====================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_sync_error_maps_common_cases() {
        assert!(friendly_sync_error("fatal: Not possible to fast-forward, aborting.")
            .contains("分叉"));
        assert!(friendly_sync_error(
            "error: Your local changes to the following files would be overwritten by merge"
        )
        .contains("未提交"));
        assert!(friendly_sync_error(
            "fatal: could not read Username for 'https://example.com': terminal prompts disabled"
        )
        .contains("认证"));
        assert!(
            friendly_sync_error("! [rejected] main -> main (fetch first)").contains("推送被拒绝")
        );
        assert_eq!(friendly_sync_error(""), "同步失败（git 未返回错误信息）");
    }

    #[test]
    fn parse_log_handles_pipes_and_body() {
        let record = "abc123\u{0}tree1\u{0}p1 p2\u{0}Alice\u{0}a@x.com\u{0}1700000000\u{0}Bob\u{0}b@x.com\u{0}1700000001\u{0}subject | with pipe\n\nbody line 1\nbody line 2\n\u{0}\u{1e}";
        let commits = parse_log_output(record.as_bytes()).expect("parse ok");
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].oid, "abc123");
        assert_eq!(commits[0].commit_inner.parent, vec!["p1", "p2"]);
        assert_eq!(commits[0].commit_inner.author.name, "Alice");
        assert_eq!(commits[0].commit_inner.author.timestamp, 1_700_000_000);
        assert!(commits[0].commit_inner.message.contains("subject | with pipe"));
        assert!(commits[0].commit_inner.message.contains("body line 2"));
    }

    #[test]
    fn parse_status_renames_first_token_is_new_path() {
        // 实测：git status -z 的重命名是 `R  new\0old\0`
        let raw = b"R  new.md\0old.md\0M  a.txt\0?? d/e.md\0";
        let entries = parse_status_z(raw);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, "new.md");
        assert_eq!(entries[0].from.as_deref(), Some("old.md"));
        assert_eq!(entries[0].x, "R");
        assert_eq!(entries[1].path, "a.txt");
        assert_eq!(entries[1].x, "M");
        assert_eq!(entries[2].x, "?");
        assert_eq!(entries[2].path, "d/e.md");
    }

    #[test]
    fn parse_name_status_renames_and_plain() {
        let raw = b"R100\0old.md\0new.md\0M\0a.txt\0A\0b.txt\0D\0c.txt\0";
        let items = parse_name_status_z(raw);
        assert_eq!(items.len(), 4);
        assert_eq!(items[0], ("new.md".into(), "R100".into(), Some("old.md".into())));
        assert_eq!(items[1].0, "a.txt");
        assert_eq!(items[2].0, "b.txt");
        assert_eq!(items[3].0, "c.txt");
    }

    #[test]
    fn parse_numstat_handles_rename_and_binary() {
        let raw = b"0\t0\t\0old.md\0new.md\03\t1\ta.txt\0-\t-\timg.png\0";
        let items = parse_numstat_z(raw);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].0, "new.md");
        assert_eq!(items[1], ("a.txt".into(), Some(3), Some(1), false));
        assert_eq!(items[2].0, "img.png");
        assert!(items[2].3);
    }

    #[test]
    fn status_label_maps_codes() {
        assert_eq!(status_label("A"), "added");
        assert_eq!(status_label("D"), "removed");
        assert_eq!(status_label("R100"), "renamed");
        assert_eq!(status_label("C75"), "copied");
        assert_eq!(status_label("M"), "modified");
        assert_eq!(status_label("T"), "modified");
        assert_eq!(status_label("U"), "conflicted");
    }

    #[test]
    fn normalize_rejects_escape_and_absolute() {
        assert!(normalize_rel_path("../x").is_err());
        assert!(normalize_rel_path("C:/x").is_err());
        assert!(normalize_rel_path("/x").is_err());
        assert_eq!(normalize_rel_path("a\\b\\c.md").unwrap(), "a/b/c.md");
        assert_eq!(normalize_rel_path("./a.md").unwrap(), "a.md");
    }

    #[test]
    fn validate_rejects_option_like_rev() {
        assert!(validate_rev("--upload-pack=x").is_err());
        assert!(validate_rev("HEAD").is_ok());
        assert!(validate_rev("origin/main").is_ok());
        assert!(validate_oid("--hard").is_err());
        assert!(validate_oid("abc123").is_ok());
    }
}
