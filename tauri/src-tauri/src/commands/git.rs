use crate::commands::git_types::*;
use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// ==================== 常量 ====================

/// Git 命令执行超时（防止网络驱动器/NFS 挂载导致无限阻塞）
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// 缓存 TTL：100ms 内相同请求使用缓存
const CACHE_TTL_MS: u64 = 100;
/// log 缓存最大条目数（LRU 简单实现：超出时清除过期项）
const LOG_CACHE_MAX: usize = 20;

// ==================== 缓存层 ====================
// 简单的时间敏感缓存，避免短时间内重复 git 调用
struct GitCache {
    refs: Option<(Instant, RefsInfo)>,
    log: HashMap<String, (Instant, Vec<CommitInfo>)>, // key=depth_dir_filepath
}

static GIT_CACHE: LazyLock<Mutex<GitCache>> = LazyLock::new(|| {
    Mutex::new(GitCache {
        refs: None,
        log: HashMap::new(),
    })
});

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

/// 执行 git 命令并返回 stdout（带超时保护，超时后 kill 子进程）。
///
/// 设计：子线程负责读取 stdout/stderr，主线程持有 Child 句柄负责超时 kill。
/// 子线程读取完毕后通过 channel 发送数据，主线程再 wait 回收。
fn run_git(args: &[&str], dir: &str) -> Result<Vec<u8>, String> {
    let mut child = git_cmd()
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 git 失败: {}", e))?;

    let mut stdout = child.stdout.take().ok_or("无法获取 stdout")?;
    let mut stderr = child.stderr.take().ok_or("无法获取 stderr")?;

    let (tx, rx) = mpsc::channel();

    // 子线程：只负责读取输出，不持有 Child
    thread::spawn(move || {
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        let _ = stdout.read_to_end(&mut stdout_buf);
        let _ = stderr.read_to_end(&mut stderr_buf);

        let _ = tx.send((stdout_buf, stderr_buf));
    });

    match rx.recv_timeout(GIT_TIMEOUT) {
        Ok((stdout_buf, stderr_buf)) => {
            // 读取完成，等待进程退出
            let status = child.wait().map_err(|e| format!("等待 git 进程失败: {}", e))?;
            if status.success() {
                Ok(stdout_buf)
            } else {
                Err(format!(
                    "git 错误: {}",
                    String::from_utf8_lossy(&stderr_buf)
                ))
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // 超时：主动 kill 子进程
            let _ = child.kill();
            let _ = child.wait();
            Err(format!(
                "git 命令超时 ({}s): {} {:?}",
                GIT_TIMEOUT.as_secs(),
                dir,
                args
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = child.kill();
            let _ = child.wait();
            Err("git 命令线程意外退出".to_string())
        }
    }
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
    // 缓存查找（命中且未过期则直接返回）
    let cache_key = format!("{}_{:?}_{:?}", dir, depth, filepath);
    {
        let cache = lock_cache();
        if let Some((ts, data)) = cache.log.get(&cache_key) {
            if ts.elapsed().as_millis() < CACHE_TTL_MS as u128 {
                return Ok(data.clone());
            }
        }
    }

    let commits = do_git_log(&dir, depth, filepath.as_deref())?;

    // 写入缓存：超出容量时清理过期项
    let mut cache = lock_cache();
    if cache.log.len() >= LOG_CACHE_MAX {
        let ttl = CACHE_TTL_MS;
        cache.log.retain(|_, (ts, _)| ts.elapsed().as_millis() < ttl as u128);
        // 如果清理过期项后仍满，按插入顺序删除最早的一半
        if cache.log.len() >= LOG_CACHE_MAX {
            let to_remove: Vec<String> = cache.log.keys().take(LOG_CACHE_MAX / 2).cloned().collect();
            for k in to_remove {
                cache.log.remove(&k);
            }
        }
    }
    cache.log.insert(cache_key, (Instant::now(), commits.clone()));
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

    // 输出格式: "[main 1234567] commit message"
    // parts[1] 为 "1234567]" 包含尾部 ']'
    for line in stdout.lines() {
        if line.starts_with('[') && line.contains(' ') {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let hash = parts[1].trim_end_matches(']');
                return Ok(hash.to_string());
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

/// 解析引用（分支、标签等）—— 高并发版：scoped threads + 批量 tag hash
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

    // scoped threads: 无需 clone dir，自动借用，减少线程创建开销
    let (local_branches, remote_branches, tag_list, branch_name, head_hash_res) =
        std::thread::scope(|s| {
            let h1 = s.spawn(|| {
                run_git_utf8(
                    &["-c", "core.quotepath=false", "branch", "--list", "--format=%(refname:short) %(objectname)"],
                    &dir,
                )
            });
            let h2 = s.spawn(|| {
                run_git_utf8(
                    &["-c", "core.quotepath=false", "branch", "-r", "--list", "--format=%(refname:short) %(objectname)"],
                    &dir,
                )
            });
            // 批量获取 tag 名称和 hash，避免 N+1 个 git 调用
            let h3 = s.spawn(|| {
                run_git_utf8(
                    &["-c", "core.quotepath=false", "tag", "--list", "--format=%(refname:short)%00%(objectname)"],
                    &dir,
                )
            });
            let h4 = s.spawn(|| {
                run_git_utf8(&["rev-parse", "--abbrev-ref", "HEAD"], &dir)
            });
            let h5 = s.spawn(|| {
                run_git_utf8(&["rev-parse", "HEAD"], &dir)
            });

            (
                // panic 时返回 Err，与原有行为一致
                h1.join().unwrap_or(Err("分支线程崩溃".to_string())),
                h2.join().unwrap_or(Err("远程分支线程崩溃".to_string())),
                h3.join().unwrap_or(Err("标签线程崩溃".to_string())),
                h4.join().unwrap_or(Err("分支名线程崩溃".to_string())),
                h5.join().unwrap_or(Err("HEAD 线程崩溃".to_string())),
            )
        });

    let mut heads = Vec::new();
    if let Ok(stdout) = local_branches {
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                heads.push(RefInfo {
                    name: parts[0].to_string(),
                    hash: parts[1].to_string(),
                });
            }
        }
    }

    let mut remotes = Vec::new();
    if let Ok(stdout) = remote_branches {
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                remotes.push(RefInfo {
                    name: parts[0].to_string(),
                    hash: parts[1].to_string(),
                });
            }
        }
    }

    // 批量解析 tag hash: 格式 "refname\0hash" 每行，一次 git 调用获取所有 tag
    let mut tags = Vec::new();
    if let Ok(stdout) = tag_list {
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // 按 '\x00' 分割，格式为 "tagname\0hash"
            let mut parts = line.splitn(2, '\0');
            if let (Some(name), Some(hash)) = (parts.next(), parts.next()) {
                let name = name.trim();
                let hash = hash.trim();
                if !name.is_empty() && !hash.is_empty() {
                    tags.push(RefInfo {
                        name: name.to_string(),
                        hash: hash.to_string(),
                    });
                }
            } else if !line.is_empty() {
                // fallback: 只有 tag 名，单独获取 hash
                if let Ok(hash_out) = run_git_utf8(&["rev-list", "-n", "1", line], &dir) {
                    if let Some(hash) = hash_out.lines().next() {
                        let hash = hash.trim().to_string();
                        if !hash.is_empty() {
                            tags.push(RefInfo {
                                name: line.to_string(),
                                hash,
                            });
                        }
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

    let head_hash = head_hash_res
        .ok()
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()));

    let refs = RefsInfo {
        heads,
        remotes,
        tags,
        head_hash,
        current_branch,
    };

    // 写入缓存（避免 clone 整个 refs）
    let mut cache = lock_cache();
    cache.refs = Some((
        Instant::now(),
        RefsInfo {
            heads: refs.heads.clone(),
            remotes: refs.remotes.clone(),
            tags: refs.tags.clone(),
            head_hash: refs.head_hash.clone(),
            current_branch: refs.current_branch.clone(),
        },
    ));

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
