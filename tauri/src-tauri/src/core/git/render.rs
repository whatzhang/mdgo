//! 模型可读文本渲染（分组、计数、预算、截断提示）。
//!
//! 渲染器只负责「把结构化结果变成模型友好的文本」；安全包裹（提示注入）
//! 由工具层统一做，避免 UI 命令也套一层。

use chrono::{DateTime, Local};

use super::model::{
    CommitDetail, CommitSummary, DiffResult, FileChange, GitStatus, LogResult, RefInfo, RefList,
    StatusEntry,
};

/// 提交时间显示（本地时区，带偏移）。
pub fn format_time(ts: u64) -> String {
    if ts == 0 {
        return "未知时间".to_string();
    }
    match DateTime::from_timestamp(ts as i64, 0) {
        Some(dt) => dt
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M %z")
            .to_string(),
        None => "未知时间".to_string(),
    }
}

/// 展示路径转义：控制字符不破坏“一行一个文件”的结构。
pub fn escape_path(path: &str) -> String {
    path.replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

fn short_hash(hash: &str) -> String {
    hash.chars().take(7).collect()
}

/// 工作区状态。
pub fn render_status(s: &GitStatus) -> String {
    let mut out = String::new();
    out.push_str(&format!("Git 仓库：{}\n", s.root));
    out.push_str(&format!("作用域：{}\n", s.scope));

    if s.unborn {
        out.push_str("分支：尚未提交（unborn HEAD）\n");
    } else if s.detached {
        out.push_str("分支：游离 HEAD（detached）\n");
    } else if let Some(branch) = &s.branch {
        match &s.upstream {
            Some(up) => out.push_str(&format!(
                "分支：{}（上游 {}，领先 {}，落后 {}）\n",
                branch, up, s.ahead, s.behind
            )),
            None => out.push_str(&format!("分支：{}（未设置上游）\n", branch)),
        }
    }
    if let Some(head) = &s.head {
        out.push_str(&format!("HEAD：{}\n", short_hash(head)));
    }

    out.push_str(&format!(
        "改动：共 {} 项（已暂存 {} / 未暂存 {} / 未跟踪 {} / 冲突 {}）\n",
        s.total_entries,
        s.counts.staged,
        s.counts.unstaged,
        s.counts.untracked,
        s.counts.conflicted
    ));
    if s.hidden_mdgo + s.hidden_sensitive + s.hidden_outside > 0 {
        out.push_str(&format!(
            "已隐藏：.mdgo {} 项；敏感文件 {} 项；知识库范围外 {} 项\n",
            s.hidden_mdgo, s.hidden_sensitive, s.hidden_outside
        ));
    }

    if s.entries.is_empty() {
        if s.unborn {
            out.push_str("\n仓库尚无提交；当前文件均为未跟踪（未展示）。\n");
        } else if s.hidden_mdgo + s.hidden_sensitive + s.hidden_outside > 0 {
            out.push_str("\n没有可展示的改动（其余条目因 .mdgo/敏感/作用域规则被隐藏）。\n");
        } else {
            out.push_str("\n工作区干净，无任何改动。\n");
        }
        return out;
    }

    let staged: Vec<&StatusEntry> = s.entries.iter().filter(|e| e.staged).collect();
    let unstaged: Vec<&StatusEntry> = s.entries.iter().filter(|e| e.unstaged).collect();
    let untracked: Vec<&StatusEntry> = s.entries.iter().filter(|e| e.kind == "untracked").collect();
    let conflicted: Vec<&StatusEntry> = s
        .entries
        .iter()
        .filter(|e| e.kind == "conflicted")
        .collect();

    render_group(&mut out, "已暂存", &staged);
    render_group(&mut out, "未暂存", &unstaged);
    render_group(&mut out, "未跟踪", &untracked);
    render_group(&mut out, "冲突", &conflicted);

    if s.truncated {
        out.push_str(&format!(
            "\n[提示] 条目已截断：仅展示前 {} 项（共 {} 项）；用 path 过滤或增大 max_entries。\n",
            s.entries.len(),
            s.total_entries
        ));
    }
    out
}

fn render_group(out: &mut String, title: &str, entries: &[&StatusEntry]) {
    if entries.is_empty() {
        return;
    }
    out.push_str(&format!("\n[{}]（{}）\n", title, entries.len()));
    for e in entries {
        let path = escape_path(&e.path);
        let display = match &e.from {
            Some(from) => format!("{} -> {}", escape_path(from), path),
            None => path,
        };
        out.push_str(&format!("{}{} {}\n", e.x, e.y, display));
    }
}

/// 提交历史。
pub fn render_log(r: &LogResult) -> String {
    if r.unborn {
        return format!("仓库尚无提交（unborn HEAD）。\n作用域：{}", r.scope);
    }
    let mut out = String::new();
    let path_hint = r
        .path
        .as_ref()
        .map(|p| format!("，path={}", escape_path(p)))
        .unwrap_or_default();
    out.push_str(&format!(
        "提交历史（{}；显示 {} 条{}）\n",
        r.rev,
        r.commits.len(),
        path_hint
    ));
    out.push_str(&format!("作用域：{}\n", r.scope));
    if r.commits.is_empty() {
        out.push_str("\n该范围没有提交。\n");
        return out;
    }
    for c in &r.commits {
        out.push_str(&render_commit_line(c));
        out.push('\n');
    }
    if r.maybe_more {
        out.push_str(&format!(
            "\n[提示] 结果达到 limit={}，可能还有更早提交；用 skip={} 继续，或加 path/author/since 缩小范围。\n",
            r.limit,
            r.skip.saturating_add(r.limit)
        ));
    }
    out
}

fn render_commit_line(c: &CommitSummary) -> String {
    let merge = if c.is_merge { "（merge）" } else { "" };
    let refs = if c.refs.is_empty() {
        String::new()
    } else {
        format!("  [{}]", c.refs.join(", "))
    };
    format!(
        "* {} {} {}  {}{}{}",
        short_hash(&c.oid),
        format_time(c.author_timestamp),
        truncate_chars(&c.author_name, 24),
        truncate_chars(&c.subject, 120),
        merge,
        refs
    )
}

/// 提交详情。
pub fn render_show(d: &CommitDetail) -> String {
    let c = &d.summary;
    let mut out = String::new();
    out.push_str(&format!(
        "提交 {}（完整 SHA：{}）\n",
        short_hash(&c.oid),
        c.oid
    ));
    out.push_str(&format!("作用域：{}\n", d.scope));
    out.push_str(&format!(
        "作者：{} <{}>  {}\n",
        c.author_name,
        c.author_email,
        format_time(c.author_timestamp)
    ));
    out.push_str(&format!(
        "提交者：{} <{}>  {}\n",
        c.committer_name,
        c.committer_email,
        format_time(c.committer_timestamp)
    ));
    if !c.refs.is_empty() {
        out.push_str(&format!("引用：{}\n", c.refs.join(", ")));
    }
    if c.is_merge {
        out.push_str("类型：合并提交（补丁相对第一父提交）\n");
    }
    out.push_str(&format!("标题：{}\n", c.subject));
    if !d.body.trim().is_empty() {
        out.push_str("正文：\n");
        for line in d.body.lines() {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }

    let (add, del) = sum_changes(&d.files);
    out.push_str(&format!(
        "变更：{} 个文件（+{} -{}）\n",
        d.files.len(),
        add,
        del
    ));
    render_file_list(&mut out, &d.files);
    if d.hidden_files + d.hidden_mdgo + d.hidden_sensitive > 0 {
        out.push_str(&format!(
            "已隐藏：敏感文件 {} 个；.mdgo 文件 {} 个；其他跳过 {} 个\n",
            d.hidden_sensitive, d.hidden_mdgo, d.hidden_files
        ));
    }

    if let Some(patch) = &d.patch {
        if !patch.trim().is_empty() {
            out.push_str("\n补丁：\n");
            out.push_str(patch);
            out.push('\n');
        }
    }
    if d.patch_truncated {
        out.push_str(
            "\n[提示] 补丁已按预算裁剪；用 path=\"...\" 精读单个文件，或 stat_only=true 只看统计。\n",
        );
    }
    out
}

/// 差异。
pub fn render_diff(d: &DiffResult) -> String {
    let mut out = String::new();
    out.push_str(&format!("差异范围：{}\n", d.mode));
    out.push_str(&format!("作用域：{}\n", d.scope));
    let (add, del) = sum_changes(&d.files);
    out.push_str(&format!(
        "文件：{} 个（+{} -{}）\n",
        d.files.len(),
        add,
        del
    ));
    if d.files.is_empty()
        && d.patch
            .as_deref()
            .map(|p| p.trim().is_empty())
            .unwrap_or(true)
    {
        out.push_str("\n该范围无差异。\n");
        return out;
    }
    render_file_list(&mut out, &d.files);
    if d.hidden_files + d.hidden_mdgo + d.hidden_sensitive > 0 {
        out.push_str(&format!(
            "已隐藏：敏感文件 {} 个；.mdgo 文件 {} 个；其他跳过 {} 个\n",
            d.hidden_sensitive, d.hidden_mdgo, d.hidden_files
        ));
    }
    if let Some(patch) = &d.patch {
        if !patch.trim().is_empty() {
            out.push_str("\n补丁：\n");
            out.push_str(patch);
            out.push('\n');
        }
    }
    if d.truncated {
        out.push_str(
            "\n[提示] 补丁已按预算裁剪；用 path=\"...\" 缩小范围，或 stat_only=true 只看统计。\n",
        );
    }
    out
}

fn sum_changes(files: &[FileChange]) -> (u64, u64) {
    let mut add = 0u64;
    let mut del = 0u64;
    for f in files {
        add += f.additions.unwrap_or(0) as u64;
        del += f.deletions.unwrap_or(0) as u64;
    }
    (add, del)
}

fn render_file_list(out: &mut String, files: &[FileChange]) {
    for f in files {
        let path = escape_path(&f.path);
        let display = match &f.from {
            Some(from) => format!("{} -> {}", escape_path(from), path),
            None => path,
        };
        let stats = match (f.additions, f.deletions) {
            (Some(a), Some(d)) if !f.binary => format!(" (+{a} -{d})"),
            _ if f.binary => " (binary)".to_string(),
            _ => String::new(),
        };
        out.push_str(&format!(
            "{} {}{}\n",
            file_status_symbol(&f.status),
            display,
            stats
        ));
    }
}

fn file_status_symbol(status: &str) -> &'static str {
    match status {
        "added" => "A",
        "removed" => "D",
        "renamed" => "R",
        "copied" => "C",
        "conflicted" => "U",
        "typechange" => "T",
        _ => "M",
    }
}

/// 分支/标签/远程引用。
pub fn render_refs(r: &RefList) -> String {
    if r.unborn {
        return format!("仓库尚无提交（unborn HEAD）。\n作用域：{}", r.scope);
    }
    let mut out = String::new();
    match (&r.current_branch, r.detached) {
        (Some(b), false) => {
            if let Some(h) = &r.head_hash {
                out.push_str(&format!("当前分支：{}（HEAD {}）\n", b, short_hash(h)));
            } else {
                out.push_str(&format!("当前分支：{}\n", b));
            }
        }
        _ => {
            if let Some(h) = &r.head_hash {
                out.push_str(&format!("当前：游离 HEAD（{}）\n", short_hash(h)));
            } else {
                out.push_str("当前：游离 HEAD\n");
            }
        }
    }
    out.push_str(&format!("作用域：{}\n", r.scope));

    render_ref_group(&mut out, "本地分支", &r.heads);
    render_ref_group(&mut out, "远程分支", &r.remotes);
    render_ref_group(&mut out, "标签", &r.tags);

    if r.heads.len() >= r.max || r.remotes.len() >= r.max || r.tags.len() >= r.max {
        out.push_str(&format!(
            "\n[提示] 每组最多展示 {} 条；可用 max 增大上限。\n",
            r.max
        ));
    }
    out
}

fn render_ref_group(out: &mut String, title: &str, refs: &[RefInfo]) {
    if refs.is_empty() {
        return;
    }
    out.push_str(&format!("\n{}（{}）：\n", title, refs.len()));
    for r in refs {
        let marker = if r.current { "*" } else { " " };
        let mut line = format!("{} {} {}", marker, r.name, short_hash(&r.hash));
        if let Some(up) = &r.upstream {
            line.push_str(&format!(" [{}", up));
            if r.gone {
                line.push_str(" gone");
            } else if r.ahead > 0 || r.behind > 0 {
                line.push_str(&format!(" ↑{} ↓{}", r.ahead, r.behind));
            }
            line.push(']');
        }
        out.push_str(&line);
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, status: &str, a: u32, d: u32) -> FileChange {
        FileChange {
            path: path.into(),
            status: status.into(),
            from: None,
            additions: Some(a),
            deletions: Some(d),
            binary: false,
        }
    }

    #[test]
    fn status_renders_groups_and_counts() {
        let s = GitStatus {
            root: "G:/repo".into(),
            scope: "当前仓库根".into(),
            branch: Some("main".into()),
            upstream: Some("origin/main".into()),
            ahead: 2,
            behind: 0,
            entries: vec![
                StatusEntry {
                    path: "a.md".into(),
                    from: None,
                    x: "M".into(),
                    y: ".".into(),
                    kind: "modified".into(),
                    staged: true,
                    unstaged: false,
                },
                StatusEntry {
                    path: "b.md".into(),
                    from: None,
                    x: "?".into(),
                    y: "?".into(),
                    kind: "untracked".into(),
                    staged: false,
                    unstaged: false,
                },
            ],
            counts: super::super::model::StatusCounts {
                staged: 1,
                unstaged: 0,
                untracked: 1,
                conflicted: 0,
            },
            total_entries: 2,
            ..Default::default()
        };
        let text = render_status(&s);
        assert!(text.contains("分支：main"));
        assert!(text.contains("已暂存"));
        assert!(text.contains("未跟踪"));
    }

    #[test]
    fn show_renders_stats_and_binary() {
        let mut b = file("img.png", "added", 0, 0);
        b.binary = true;
        b.additions = None;
        b.deletions = None;
        let d = CommitDetail {
            summary: CommitSummary {
                oid: "abcdef1234".into(),
                subject: "feat: x".into(),
                ..Default::default()
            },
            body: "why".into(),
            files: vec![file("a.md", "modified", 2, 1), b],
            ..Default::default()
        };
        let text = render_show(&d);
        assert!(text.contains("+2 -1"));
        assert!(text.contains("(binary)"));
        assert!(text.contains("正文"));
    }

    #[test]
    fn refs_renders_tracking() {
        let r = RefList {
            heads: vec![RefInfo {
                name: "main".into(),
                hash: "abcdef1234".into(),
                current: true,
                upstream: Some("origin/main".into()),
                ahead: 2,
                behind: 1,
                gone: false,
            }],
            max: 50,
            scope: "当前仓库根".into(),
            ..Default::default()
        };
        let text = render_refs(&r);
        assert!(text.contains("↑2 ↓1"));
        assert!(text.contains("* main"));
    }
}
