use crate::commands::git_types::*;
use std::process::Command;

/// 获取提交记录
#[tauri::command]
pub fn git_log(
    dir: String,
    depth: Option<u32>,
    filepath: Option<String>,
) -> Result<Vec<CommitInfo>, String> {
    // 简化实现：使用 git log 命令
    let mut args = vec![
        "log".to_string(),
        "--format=%H|%T|%P|%an|%ae|%at|%cn|%ce|%ct|%s".to_string(),
    ];

    if let Some(d) = depth {
        args.push("-n".to_string());
        args.push(d.to_string());
    }

    if let Some(fp) = &filepath {
        args.push("--".to_string());
        args.push(fp.clone());
    }

    let output = Command::new("git")
        .args(&args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .current_dir(&dir)
        .output()
        .map_err(|e| format!("执行 git log 失败: {}", e))?;

    if !output.status.success() {
        return Err(format!("git log 错误: {}", String::from_utf8_lossy(&output.stderr)));
    }

    // 解析输出
    let stdout = String::from_utf8_lossy(&output.stdout);
    let commits = stdout.lines()
        .filter(|line| !line.is_empty())
        .map(|line| parse_commit_line(line))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(commits)
}

/// 获取提交的文件变更（对比 parent commit）
#[tauri::command]
pub fn git_diff_tree(
    dir: String,
    commit_oid: String,
    parent_oid: Option<String>,
) -> Result<Vec<FileChange>, String> {
    let mut args = vec![
        "-c".to_string(),
        "core.quotepath=false".to_string(),
        "diff-tree".to_string(),
        "--no-commit-id".to_string(),
        "--name-status".to_string(),
        "-r".to_string(),
    ];

    // 如果有 parent，对比 parent 和 commit；否则对比 commit 和空树
    if let Some(parent) = parent_oid {
        args.push(parent);
        args.push(commit_oid);
    } else {
        // 初始提交：使用 --root 标志
        args.push("--root".to_string());
        args.push(commit_oid);
    }

    let output = Command::new("git")
        .args(&args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .current_dir(&dir)
        .output()
        .map_err(|e| format!("执行 git diff-tree 失败: {}", e))?;

    if !output.status.success() {
        return Err(format!("git diff-tree 错误: {}", String::from_utf8_lossy(&output.stderr)));
    }

    // 解析输出
    let stdout = String::from_utf8_lossy(&output.stdout);
    let changes = stdout.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                return Err(format!("diff-tree 行格式错误: {}", line));
            }

            let status_char = parts[0];
            let path = parts[1..].join(" "); // 文件路径可能包含空格

            let status = match status_char {
                "A" => "added",
                "M" => "modified",
                "D" => "removed",
                "R" => "modified", // 重命名视为修改
                "C" => "added",    // 复制视为新增
                _ => "modified",
            };

            Ok(FileChange {
                path,
                status: status.to_string(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    // 按状态排序（modified, added, removed）
    let mut sorted_changes = changes;
    sorted_changes.sort_by(|a, b| {
        let order = |s: &str| match s {
            "modified" => 0,
            "added" => 1,
            "removed" => 2,
            _ => 3,
        };
        order(&a.status).cmp(&order(&b.status))
    });

    Ok(sorted_changes)
}

/// 读取文件内容（从指定 commit）
#[tauri::command]
pub fn git_read_blob(
    dir: String,
    oid: String,
    filepath: String,
) -> Result<BlobResult, String> {
    // 使用 git show 命令读取文件内容
    let output = Command::new("git")
        .args(&["show", &format!("{}:{}", oid, filepath)])
        .current_dir(&dir)
        .output()
        .map_err(|e| format!("执行 git show 失败: {}", e))?;

    if !output.status.success() {
        return Err(format!("git show 错误: {}", String::from_utf8_lossy(&output.stderr)));
    }

    Ok(BlobResult {
        blob: output.stdout,
    })
}

/// 暂存文件（git add）
#[tauri::command]
pub fn git_add(dir: String, filepath: String) -> Result<(), String> {
    let output = Command::new("git")
        .args(&["add", &filepath])
        .current_dir(&dir)
        .output()
        .map_err(|e| format!("执行 git add 失败: {}", e))?;

    if !output.status.success() {
        return Err(format!("git add 错误: {}", String::from_utf8_lossy(&output.stderr)));
    }

    Ok(())
}

/// 取消暂存文件（git reset）
#[tauri::command]
pub fn git_reset(dir: String, filepath: String) -> Result<(), String> {
    let output = Command::new("git")
        .args(&["reset", "HEAD", &filepath])
        .current_dir(&dir)
        .output()
        .map_err(|e| format!("执行 git reset 失败: {}", e))?;

    if !output.status.success() {
        return Err(format!("git reset 错误: {}", String::from_utf8_lossy(&output.stderr)));
    }

    Ok(())
}

/// 提交暂存（git commit）
#[tauri::command]
pub fn git_commit(dir: String, message: String, author_name: String, author_email: String) -> Result<String, String> {
    let output = Command::new("git")
        .args(&["-c", &format!("user.name={}", author_name), 
                "-c", &format!("user.email={}", author_email),
                "commit", "-m", &message])
        .current_dir(&dir)
        .output()
        .map_err(|e| format!("执行 git commit 失败: {}", e))?;

    if !output.status.success() {
        return Err(format!("git commit 错误: {}", String::from_utf8_lossy(&output.stderr)));
    }

    // 解析提交 SHA
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.starts_with("[") && line.contains(" ") {
            // 格式: [master abc1234] message
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
    let output = Command::new("git")
        .args(&["rev-parse", &ref_name])
        .current_dir(&dir)
        .output()
        .map_err(|e| format!("执行 git rev-parse 失败: {}", e))?;

    if !output.status.success() {
        return Err(format!("git rev-parse 错误: {}", String::from_utf8_lossy(&output.stderr)));
    }

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(sha)
}

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

/// 获取文件状态矩阵（兼容 isomorphic-git 格式）
#[tauri::command]
pub fn git_status_matrix(dir: String) -> Result<Vec<FileStatusEntry>, String> {
    // 使用 -z 参数输出 NUL 分隔的格式，避免 Git 对中文等非 ASCII 路径加引号
    let output = Command::new("git")
        .args(&["-c", "core.quotepath=false", "status", "--porcelain=v1", "-z"])
        .current_dir(&dir)
        .output()
        .map_err(|e| format!("执行 git status 失败: {}", e))?;

    if !output.status.success() {
        return Err(format!("git status 错误: {}", String::from_utf8_lossy(&output.stderr)));
    }

    let mut result = Vec::new();

    // 解析 NUL 分隔的输出
    // 实际格式: XY<space>path<NUL>                     (普通条目)
    //          XY<space>orig<NUL>new<NUL>              (重命名/复制条目)
    //  -z 模式只是用 NUL 替换了 LF，路径前没有多余的 NUL
    let stdout = &output.stdout;
    let mut start = 0;
    while start < stdout.len() {
        if stdout[start] == 0 {
            break;
        }

        // 找到本条目结束的 NUL（总是在 path 后面）
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

        // 路径从 XY + 空格之后开始，到 entry_end 结束
        let path_start = start + 3;
        let filepath_bytes = &stdout[path_start..entry_end];
        let filepath = String::from_utf8_lossy(filepath_bytes);

        if filepath.is_empty() {
            start = entry_end + 1;
            continue;
        }

        // 兼容 isomorphic-git statusMatrix 格式 (head, workdir, stage)
        // git status --porcelain=v1: XY filename (X=index, Y=worktree)
        // head: 0(不在HEAD), 1(在HEAD)
        // workdir/stage: 0(不存在), 1(与HEAD/索引一致), 2(已修改), 3(新增)
        // 处理重命名/复制（R/C）：格式为 XY<space>orig<NUL>new<NUL>
        if index_status == 'R' || index_status == 'C' {
            // 新路径在 entry_end(NUL) 之后
            let new_start = entry_end + 1;
            let new_end = stdout[new_start..].iter().position(|&b| b == 0)
                .map(|pos| new_start + pos)
                .unwrap_or(stdout.len());

            let new_path = String::from_utf8_lossy(&stdout[new_start..new_end]);

            // 旧路径被删除
            result.push((filepath.to_string(), 1, 1, 0));
            // 新路径被添加
            result.push((new_path.to_string(), 0, 2, 3));

            // 跳到 new_path 的 NUL 之后
            start = new_end + 1;
            continue;
        }

        let (head, workdir, stage) = match (index_status, workdir_status) {
            // 'M ' = 已暂存修改（索引有变更，工作区与HEAD一致）
            ('M', ' ') => (1, 1, 2),
            // ' M' = 未暂存修改
            (' ', 'M') => (1, 2, 1),
            // 'MM' = 既暂存又未暂存
            ('M', 'M') => (1, 2, 2),
            // 'A ' = 已暂存新增（工作区与索引一致）
            ('A', ' ') => (0, 2, 3),
            // 'A' + worktree修改（暂存后又修改了工作区）
            ('A', 'M') => (0, 2, 3),
            // 'D ' = 已暂存删除
            ('D', ' ') => (1, 1, 0),
            // ' D' = 工作区删除
            (' ', 'D') => (1, 0, 1),
            // 'DD' = 索引和工作区都删除
            ('D', 'D') => (1, 0, 0),
            // '??' = 未跟踪文件
            ('?', '?') => (0, 2, 0),
            _ => (1, 1, 1),
        };

        result.push((filepath.to_string(), head, workdir, stage));

        // 跳到下一个条目
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
    for filepath in filepaths {
        let mut args = vec!["checkout"];

        if force {
            args.push("--force");
        }

        args.push("--");
        args.push(&filepath);

        let output = Command::new("git")
            .args(&args)
            .current_dir(&dir)
            .output()
            .map_err(|e| format!("执行 git checkout 失败: {}", e))?;

        if !output.status.success() {
            return Err(format!("git checkout 错误: {}", String::from_utf8_lossy(&output.stderr)));
        }
    }

    Ok(())
}

/// 解析引用（分支、标签等）
#[tauri::command]
pub fn git_parse_refs(dir: String) -> Result<RefsInfo, String> {
    let mut heads = Vec::new();
    let mut remotes = Vec::new();
    let mut tags = Vec::new();

    // 获取本地分支
    let output = Command::new("git")
        .args(&["branch", "--list", "--format=%(refname:short) %(objectname)"])
        .current_dir(&dir)
        .output()
        .map_err(|e| format!("执行 git branch 失败: {}", e))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
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

    // 获取远程分支
    let output = Command::new("git")
        .args(&["branch", "-r", "--list", "--format=%(refname:short) %(objectname)"])
        .current_dir(&dir)
        .output()
        .map_err(|e| format!("执行 git branch -r 失败: {}", e))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
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

    // 获取标签
    let output = Command::new("git")
        .args(&["tag", "--list"])
        .current_dir(&dir)
        .output()
        .map_err(|e| format!("执行 git tag 失败: {}", e))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for tag_name in stdout.lines() {
            if tag_name.is_empty() {
                continue;
            }
            // 获取标签对应的 hash
            let hash_output = Command::new("git")
                .args(&["rev-list", "-n", "1", tag_name])
                .current_dir(&dir)
                .output()
                .ok();

            if let Some(hash_output) = hash_output {
                if hash_output.status.success() {
                    let hash = String::from_utf8_lossy(&hash_output.stdout)
                        .lines().next().unwrap_or("").trim().to_string();
                    if !hash.is_empty() {
                        tags.push(RefInfo {
                            name: tag_name.trim().to_string(),
                            hash,
                        });
                    }
                }
            }
        }
    }

    // 获取当前分支
    let output = Command::new("git")
        .args(&["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&dir)
        .output()
        .map_err(|e| format!("执行 git rev-parse 失败: {}", e))?;

    let current_branch = if output.status.success() {
        String::from_utf8_lossy(&output.stdout).lines().next().unwrap_or("HEAD").to_string()
    } else {
        "HEAD".to_string()
    };

    // 获取 HEAD hash
    let output = Command::new("git")
        .args(&["rev-parse", "HEAD"])
        .current_dir(&dir)
        .output()
        .ok();

    let head_hash = output.and_then(|o| {
        if o.status.success() {
            Some(String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").trim().to_string())
        } else {
            None
        }
    });

    Ok(RefsInfo {
        heads,
        remotes,
        tags,
        head_hash,
        current_branch,
    })
}

// ==================== 辅助函数 ====================