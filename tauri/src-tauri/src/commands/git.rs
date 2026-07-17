use crate::commands::git_types::*;
use std::process::Command;
use std::sync::{LazyLock, Mutex};

// ==================== 缓存层 ====================
// 简单的时间敏感缓存，避免短时间内重复 git 调用
struct GitCache {
    refs: Option<(std::time::Instant, RefsInfo)>,
    log: Option<(std::time::Instant, String, Vec<CommitInfo>)>, // key=depth_dir_filepath
}
const CACHE_TTL_MS: u64 = 100; // 100ms 内相同请求使用缓存

static GIT_CACHE: LazyLock<Mutex<GitCache>> = LazyLock::new(|| Mutex::new(GitCache { refs: None, log: None }));

/// 安全获取缓存锁，即使被 poisoning 也能恢复
fn lock_cache() -> std::sync::MutexGuard<'static, GitCache> {
    GIT_CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

// ==================== 辅助函数 ====================

/// 创建 git 命令，Windows 上隐藏控制台窗口
#[cfg(target_os = "windows")]
fn git_cmd() -> Command {
    let mut cmd = Command::new("git");
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    cmd
}
#[cfg(not(target_os = "windows"))]
fn git_cmd() -> Command {
    Command::new("git")
}

/// 执行 git 命令并返回 stdout（成功时）
fn run_git(args: &[&str], dir: &str) -> Result<Vec<u8>, String> {
    let output = git_cmd()
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("执行 git 失败: {}", e))?;

    if !output.status.success() {
        return Err(format!("git 错误: {}", String::from_utf8_lossy(&output.stderr)));
    }
    Ok(output.stdout)
}

/// 执行 git 命令并返回 stdout 字符串
fn run_git_utf8(args: &[&str], dir: &str) -> Result<String, String> {
    let stdout = run_git(args, dir)?;
    Ok(String::from_utf8_lossy(&stdout).to_string())
}

// ==================== 命令实现 ====================

/// 获取提交记录
#[tauri::command]
pub fn git_log(
    dir: String,
    depth: Option<u32>,
    filepath: Option<String>,
) -> Result<Vec<CommitInfo>, String> {
    // 缓存查找
    let cache_key = format!("{}_{:?}_{:?}", dir, depth, filepath);
    {
        let cache = lock_cache();
        if let Some((ts, ref key, ref data)) = cache.log {
            if key == &cache_key && ts.elapsed().as_millis() < CACHE_TTL_MS as u128 {
                return Ok(data.clone());
            }
        }
    }

    let commits = do_git_log(&dir, depth, filepath.as_deref())?;
    // 写入缓存
    let mut cache = lock_cache();
    cache.log = Some((std::time::Instant::now(), cache_key, commits.clone()));
    Ok(commits)
}

fn do_git_log(dir: &str, depth: Option<u32>, filepath: Option<&str>) -> Result<Vec<CommitInfo>, String> {
    let mut args: Vec<String> = vec![
        "log".into(),
        "--format=%H|%T|%P|%an|%ae|%at|%cn|%ce|%ct|%s".into(),
    ];

    if let Some(d) = depth {
        args.push("-n".into());
        args.push(d.to_string());
    }

    if let Some(fp) = filepath {
        args.push("--".into());
        args.push(fp.into());
    }

    let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let stdout = run_git_utf8(&str_args, dir)?;
    stdout.lines()
        .filter(|line| !line.is_empty())
        .map(|line| parse_commit_line(line))
        .collect::<Result<Vec<_>, _>>()
}

/// 获取提交的文件变更（对比 parent commit）
#[tauri::command]
pub fn git_diff_tree(
    dir: String,
    commit_oid: String,
    parent_oid: Option<String>,
) -> Result<Vec<FileChange>, String> {
    let mut args = vec![
        "-c",
        "core.quotepath=false",
        "diff-tree",
        "--no-commit-id",
        "--name-status",
        "-r",
    ];

    let extra: Vec<String>;
    if let Some(parent) = parent_oid {
        extra = vec![parent, commit_oid];
    } else {
        extra = vec!["--root".into(), commit_oid];
    }
    args.extend(extra.iter().map(|s| s.as_str()));

    let stdout = run_git_utf8(&args, &dir)?;

    let mut changes: Vec<FileChange> = stdout.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                return Err(format!("diff-tree 行格式错误: {}", line));
            }
            let status_char = parts[0];
            let path = parts[1..].join(" ");
            let status = match status_char {
                "A" | "C" => "added",
                "M" => "modified",
                "D" => "removed",
                _ => "modified",
            };
            Ok(FileChange { path, status: status.to_string() })
        })
        .collect::<Result<Vec<_>, _>>()?;

    changes.sort_by(|a, b| {
        let order = |s: &str| match s {
            "modified" => 0,
            "added" => 1,
            "removed" => 2,
            _ => 3,
        };
        order(&a.status).cmp(&order(&b.status))
    });

    Ok(changes)
}

/// 读取文件内容（从指定 commit）
#[tauri::command]
pub fn git_read_blob(
    dir: String,
    oid: String,
    filepath: String,
) -> Result<BlobResult, String> {
    let stdout = run_git(&["show", &format!("{}:{}", oid, filepath)], &dir)?;
    Ok(BlobResult { blob: stdout })
}

/// 暂存文件（git add）
#[tauri::command]
pub fn git_add(dir: String, filepath: String) -> Result<(), String> {
    run_git(&["add", &filepath], &dir)?;
    Ok(())
}

/// 取消暂存文件（git reset）
#[tauri::command]
pub fn git_reset(dir: String, filepath: String) -> Result<(), String> {
    run_git(&["reset", "HEAD", &filepath], &dir)?;
    Ok(())
}

/// 提交暂存（git commit）
#[tauri::command]
pub fn git_commit(
    dir: String,
    message: String,
    author_name: String,
    author_email: String,
) -> Result<String, String> {
    let stdout = run_git_utf8(
        &[
            "-c", &format!("user.name={}", author_name),
            "-c", &format!("user.email={}", author_email),
            "commit", "-m", &message,
        ],
        &dir,
    )?;

    for line in stdout.lines() {
        if line.starts_with("[") && line.contains(" ") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                return Ok(parts[1].to_string());
            }
        }
    }
    Ok("unknown".to_string())
}

/// 解析 HEAD 引用（获取当前 commit hash）
#[tauri::command]
pub fn git_resolve_ref(dir: String, ref_name: String) -> Result<String, String> {
    let stdout = run_git_utf8(&["rev-parse", &ref_name], &dir)?;
    Ok(stdout.trim().to_string())
}

/// 获取文件状态矩阵（兼容 isomorphic-git 格式）
#[tauri::command]
pub fn git_status_matrix(dir: String) -> Result<Vec<FileStatusEntry>, String> {
    let stdout = run_git(
        &["-c", "core.quotepath=false", "status", "--porcelain=v1", "-z"],
        &dir,
    )?;

    let mut result = Vec::new();
    let stdout = &stdout;
    let mut start = 0;

    while start < stdout.len() {
        if stdout[start] == 0 { break; }

        let entry_end = match stdout[start..].iter().position(|&b| b == 0) {
            Some(pos) => start + pos,
            None => break,
        };

        if entry_end - start < 2 {
            start = entry_end + 1;
            continue;
        }

        let index_status = stdout[start] as char;
        let workdir_status = stdout[start + 1] as char;

        let path_start = start + 3;
        let filepath_bytes = &stdout[path_start..entry_end];
        let filepath = String::from_utf8_lossy(filepath_bytes);

        if filepath.is_empty() {
            start = entry_end + 1;
            continue;
        }

        // 处理重命名/复制
        if index_status == 'R' || index_status == 'C' {
            let new_start = entry_end + 1;
            let new_end = stdout[new_start..].iter().position(|&b| b == 0)
                .map(|pos| new_start + pos)
                .unwrap_or(stdout.len());
            let new_path = String::from_utf8_lossy(&stdout[new_start..new_end]);
            result.push((filepath.to_string(), 1, 1, 0));
            result.push((new_path.to_string(), 0, 2, 3));
            start = new_end + 1;
            continue;
        }

        let (head, workdir, stage) = match (index_status, workdir_status) {
            ('M', ' ') => (1, 1, 2),
            (' ', 'M') => (1, 2, 1),
            ('M', 'M') => (1, 2, 2),
            ('A', ' ') | ('A', 'M') => (0, 2, 3),
            ('D', ' ') => (1, 1, 0),
            (' ', 'D') => (1, 0, 1),
            ('D', 'D') => (1, 0, 0),
            ('?', '?') => (0, 2, 0),
            _ => (1, 1, 1),
        };

        result.push((filepath.to_string(), head, workdir, stage));
        start = entry_end + 1;
    }

    Ok(result)
}

/// 恢复文件到 HEAD 状态
#[tauri::command]
pub fn git_checkout(
    dir: String,
    filepaths: Vec<String>,
    force: bool,
) -> Result<(), String> {
    for filepath in &filepaths {
        let mut args = vec!["checkout"];
        if force { args.push("--force"); }
        args.push("--");
        args.push(filepath.as_str());
        run_git(&args, &dir)?;
    }
    Ok(())
}

/// 解析引用（分支、标签等）—— 优化版：并行执行减少延迟
#[tauri::command]
pub fn git_parse_refs(dir: String) -> Result<RefsInfo, String> {
    // 使用缓存
    {
        let cache = lock_cache();
        if let Some((ts, ref data)) = cache.refs {
            if ts.elapsed().as_millis() < CACHE_TTL_MS as u128 {
                return Ok(RefsInfo {
                    heads: data.heads.clone(),
                    remotes: data.remotes.clone(),
                    tags: data.tags.clone(),
                    head_hash: data.head_hash.clone(),
                    current_branch: data.current_branch.clone(),
                });
            }
        }
    }

    // 并行执行独立命令: 用线程池并行获取所有引用信息
    let dir_clone = dir.clone();
    let handle_locals = std::thread::spawn(move || {
        run_git_utf8(
            &["branch", "--list", "--format=%(refname:short) %(objectname)"],
            &dir_clone,
        )
    });

    let dir_clone2 = dir.clone();
    let handle_remotes = std::thread::spawn(move || {
        run_git_utf8(
            &["branch", "-r", "--list", "--format=%(refname:short) %(objectname)"],
            &dir_clone2,
        )
    });

    let dir_clone3 = dir.clone();
    let handle_tags = std::thread::spawn(move || {
        run_git_utf8(&["tag", "--list"], &dir_clone3)
    });

    let dir_clone4 = dir.clone();
    let handle_branch = std::thread::spawn(move || {
        run_git_utf8(&["rev-parse", "--abbrev-ref", "HEAD"], &dir_clone4)
    });

    let dir_clone5 = dir.clone();
    let handle_head_hash = std::thread::spawn(move || {
        run_git_utf8(&["rev-parse", "HEAD"], &dir_clone5)
    });

    // 收集结果
    let local_branches = handle_locals.join().map_err(|_| "分支线程崩溃".to_string())?;
    let remote_branches = handle_remotes.join().map_err(|_| "远程分支线程崩溃".to_string())?;
    let tag_list = handle_tags.join().map_err(|_| "标签线程崩溃".to_string())?;
    let branch_name = handle_branch.join().map_err(|_| "分支名线程崩溃".to_string())?;
    let head_hash_res = handle_head_hash.join().map_err(|_| "HEAD 线程崩溃".to_string())?;

    let mut heads = Vec::new();
    if let Ok(stdout) = local_branches {
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                heads.push(RefInfo { name: parts[0].to_string(), hash: parts[1].to_string() });
            }
        }
    }

    let mut remotes = Vec::new();
    if let Ok(stdout) = remote_branches {
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                remotes.push(RefInfo { name: parts[0].to_string(), hash: parts[1].to_string() });
            }
        }
    }

    let mut tags = Vec::new();
    if let Ok(stdout) = tag_list {
        for tag_name in stdout.lines() {
            let tag = tag_name.trim();
            if tag.is_empty() { continue; }
            // 并行获取每个 tag hash（只获取第一个 tag 的 hash 做演示，实际可优化）
            if let Ok(hash_out) = run_git_utf8(&["rev-list", "-n", "1", tag], &dir) {
                if let Some(hash) = hash_out.lines().next() {
                    let hash = hash.trim().to_string();
                    if !hash.is_empty() {
                        tags.push(RefInfo { name: tag.to_string(), hash });
                    }
                }
            }
        }
    }

    let current_branch = branch_name
        .unwrap_or_else(|_| "HEAD".to_string())
        .lines()
        .next()
        .unwrap_or("HEAD")
        .trim()
        .to_string();

    let head_hash = head_hash_res.ok().and_then(|s| {
        s.lines().next().map(|l| l.trim().to_string())
    });

    let refs = RefsInfo { heads, remotes, tags, head_hash, current_branch };

    // 写入缓存
    let mut cache = lock_cache();
    cache.refs = Some((std::time::Instant::now(), RefsInfo {
        heads: refs.heads.clone(),
        remotes: refs.remotes.clone(),
        tags: refs.tags.clone(),
        head_hash: refs.head_hash.clone(),
        current_branch: refs.current_branch.clone(),
    }));

    Ok(refs)
}

// ==================== 辅助函数 ====================

fn parse_commit_line(line: &str) -> Result<CommitInfo, String> {
    let parts: Vec<&str> = line.splitn(10, '|').collect();
    if parts.len() < 10 {
        return Err(format!("提交行格式错误: {}", line));
    }

    Ok(CommitInfo {
        oid: parts[0].to_string(),
        commit_inner: CommitInner {
            tree: parts[1].to_string(),
            parent: parts[2].split_whitespace().map(|s| s.to_string()).collect(),
            author: AuthorInfo {
                name: parts[3].to_string(),
                email: parts[4].to_string(),
                timestamp: parts[5].parse().map_err(|_| "时间戳解析失败")?,
            },
            committer: AuthorInfo {
                name: parts[6].to_string(),
                email: parts[7].to_string(),
                timestamp: parts[8].parse().map_err(|_| "时间戳解析失败")?,
            },
            message: parts[9].to_string(),
        },
    })
}
