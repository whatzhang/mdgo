//! Git 输出的纯解析器（不执行命令，便于单测）。
//!
//! 统一原则：路径用 `-z`/NUL 分隔，避免空格、Tab、换行、中文被错误切分；
//! 解析器输入 `&[u8]`，输出结构化模型，渲染层再做展示转义。

use super::model::{CommitSummary, RefInfo};

/// 解析后的工作区状态（尚未做作用域/严格过滤）。
#[derive(Debug, Clone, Default)]
pub struct ParsedStatus {
    pub branch: Option<String>,
    pub head: Option<String>,
    pub detached: bool,
    pub unborn: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub entries: Vec<ParsedStatusEntry>,
}

/// 解析后的单条状态。
#[derive(Debug, Clone)]
pub struct ParsedStatusEntry {
    pub path: String,
    pub from: Option<String>,
    pub x: String,
    pub y: String,
    pub untracked: bool,
    pub conflicted: bool,
}

/// 解析 `git status --porcelain=v2 --branch -z`。
///
/// 条目格式：
/// - `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`
/// - `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\0<origPath>`
/// - `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`
/// - `? <path>` / `! <path>`（ignored 丢弃）
pub fn parse_status_v2(bytes: &[u8]) -> ParsedStatus {
    let mut st = ParsedStatus::default();
    let mut tokens: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    if tokens.last().map(|t| t.is_empty()).unwrap_or(false) {
        tokens.pop();
    }

    let mut i = 0;
    while i < tokens.len() {
        let raw = tokens[i];
        i += 1;
        if raw.is_empty() {
            continue;
        }
        if raw.starts_with(b"#") {
            let text = String::from_utf8_lossy(raw);
            for line in text.split('\n') {
                parse_status_header(line, &mut st);
            }
            continue;
        }

        let text = String::from_utf8_lossy(raw).into_owned();
        match text.as_bytes().first().copied() {
            Some(b'1') => {
                let parts: Vec<&str> = text.splitn(9, ' ').collect();
                if parts.len() == 9 {
                    if let Some(e) = status_entry(parts[1], parts[8], None, false, false) {
                        st.entries.push(e);
                    }
                }
            }
            Some(b'2') => {
                let parts: Vec<&str> = text.splitn(10, ' ').collect();
                if parts.len() == 10 {
                    let orig = tokens
                        .get(i)
                        .map(|t| String::from_utf8_lossy(t).into_owned());
                    i += 1;
                    if let Some(e) = status_entry(parts[1], parts[9], orig, false, false) {
                        st.entries.push(e);
                    }
                }
            }
            Some(b'u') => {
                let parts: Vec<&str> = text.splitn(11, ' ').collect();
                if parts.len() == 11 {
                    if let Some(e) = status_entry(parts[1], parts[10], None, false, true) {
                        st.entries.push(e);
                    }
                }
            }
            Some(b'?') => {
                if let Some((_, path)) = text.split_once(' ') {
                    if let Some(e) = status_entry("??", path, None, true, false) {
                        st.entries.push(e);
                    }
                }
            }
            Some(b'!') => {}
            _ => {}
        }
    }
    st
}

fn parse_status_header(line: &str, st: &mut ParsedStatus) {
    let rest = line.trim_start_matches('#').trim();
    if let Some(v) = rest.strip_prefix("branch.oid ") {
        let v = v.trim();
        if v == "(initial)" {
            st.unborn = true;
        } else if !v.is_empty() {
            st.head = Some(v.to_string());
        }
    } else if let Some(v) = rest.strip_prefix("branch.head ") {
        let v = v.trim();
        if v == "(detached)" {
            st.detached = true;
        } else if v != "(unknown)" && !v.is_empty() {
            st.branch = Some(v.to_string());
        }
    } else if let Some(v) = rest.strip_prefix("branch.upstream ") {
        let v = v.trim();
        if !v.is_empty() {
            st.upstream = Some(v.to_string());
        }
    } else if let Some(v) = rest.strip_prefix("branch.ab ") {
        let mut it = v.split_whitespace();
        let a = it.next().unwrap_or("");
        let b = it.next().unwrap_or("");
        st.ahead = a.trim_start_matches('+').parse().unwrap_or(0);
        st.behind = b.trim_start_matches('-').parse().unwrap_or(0);
    }
}

fn status_entry(
    xy: &str,
    path: &str,
    from: Option<String>,
    untracked: bool,
    conflicted: bool,
) -> Option<ParsedStatusEntry> {
    let path = path.trim_end_matches('\r').to_string();
    if path.is_empty() {
        return None;
    }
    let mut chars = xy.chars();
    let x = chars.next().unwrap_or('.').to_string();
    let y = chars.next().unwrap_or('.').to_string();
    Some(ParsedStatusEntry {
        path,
        from: from.filter(|f| !f.is_empty()),
        x,
        y,
        untracked,
        conflicted,
    })
}

/// 解析 `git log -z --format=...`。
///
/// 字段顺序（12 个，NUL 分隔）：
/// oid, tree, parents, authorName, authorEmail, authorTs,
/// committerName, committerEmail, committerTs, decorations, subject, body
pub fn parse_log_z(bytes: &[u8]) -> Vec<CommitSummary> {
    const FIELDS: usize = 12;
    let mut tokens: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    if tokens.last().map(|t| t.is_empty()).unwrap_or(false) {
        tokens.pop();
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i + FIELDS <= tokens.len() {
        // `-z` 可能在提交之间留下空 token；跳过而不破坏字段对齐。
        if tokens[i].is_empty() {
            i += 1;
            continue;
        }
        let field = |idx: usize| -> String {
            String::from_utf8_lossy(tokens[i + idx])
                .trim_end_matches('\n')
                .trim_end_matches('\r')
                .to_string()
        };
        let oid = field(0);
        if oid.is_empty() {
            i += 1;
            continue;
        }
        let parents: Vec<String> = field(2).split_whitespace().map(|s| s.to_string()).collect();
        let subject = field(10);
        let message = field(11);
        let refs = parse_decorations(&field(9));
        out.push(CommitSummary {
            oid,
            tree: field(1),
            is_merge: parents.len() > 1,
            parents,
            author_name: field(3),
            author_email: field(4),
            author_timestamp: field(5).parse().unwrap_or(0),
            committer_name: field(6),
            committer_email: field(7),
            committer_timestamp: field(8).parse().unwrap_or(0),
            refs,
            subject,
            message,
        });
        i += FIELDS;
    }
    out
}

fn parse_decorations(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// 解析 `git diff-tree -r -M --name-status -z`：
/// 普通 `<status>\0<path>\0`；重命名 `<Rxxx>\0<old>\0<new>\0`。
/// 返回 `(new_path, raw_status, old_path)`。
pub fn parse_name_status_z(bytes: &[u8]) -> Vec<(String, String, Option<String>)> {
    let mut tokens: Vec<String> = bytes
        .split(|b| *b == 0)
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect();
    if tokens.last().map(|s| s.is_empty()).unwrap_or(false) {
        tokens.pop();
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let status = tokens[i].clone();
        i += 1;
        if status.is_empty() {
            continue;
        }
        let kind = status.chars().next().unwrap_or('M');
        if kind == 'R' || kind == 'C' {
            if i + 1 >= tokens.len() {
                break;
            }
            let old = tokens[i].clone();
            let new = tokens[i + 1].clone();
            i += 2;
            out.push((new, status, Some(old)));
        } else {
            if i >= tokens.len() {
                break;
            }
            let path = tokens[i].clone();
            i += 1;
            out.push((path, status, None));
        }
    }
    out
}

/// 解析 `git diff-tree -r -M --numstat -z`：
/// 普通 `<add>\t<del>\t<path>\0`；重命名 `<add>\t<del>\t\0<old>\0<new>\0`。
/// 返回 `(new_path, additions, deletions, binary)`。
pub fn parse_numstat_z(bytes: &[u8]) -> Vec<(String, Option<u32>, Option<u32>, bool)> {
    let mut tokens: Vec<String> = bytes
        .split(|b| *b == 0)
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect();
    if tokens.last().map(|s| s.is_empty()).unwrap_or(false) {
        tokens.pop();
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let head = tokens[i].clone();
        i += 1;
        let mut it = head.splitn(3, '\t');
        let add = it.next().unwrap_or("").to_string();
        let del = it.next().unwrap_or("").to_string();
        let path = it.next().unwrap_or("").to_string();
        let real_path = if path.is_empty() {
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
        let binary = add.trim() == "-" || del.trim() == "-";
        out.push((
            real_path,
            add.trim().parse::<u32>().ok(),
            del.trim().parse::<u32>().ok(),
            binary,
        ));
    }
    out
}

/// `for-each-ref` 的一行解析结果。
#[derive(Debug, Clone)]
pub struct ParsedRef {
    pub full_name: String,
    pub oid: String,
    pub peeled: Option<String>,
    pub head: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub gone: bool,
}

/// 解析 `git for-each-ref --format=%(refname)%00%(objectname)%00%(*objectname)%00%(HEAD)%00%(upstream:short)%00%(upstream:track)`。
pub fn parse_refs(bytes: &[u8]) -> Vec<ParsedRef> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\u{0}').collect();
        if parts.len() < 6 {
            continue;
        }
        let full_name = parts[0].trim();
        let oid = parts[1].trim();
        if full_name.is_empty() || oid.is_empty() {
            continue;
        }
        let peeled = Some(parts[2].trim().to_string()).filter(|s| !s.is_empty());
        let head = parts[3].trim() == "*";
        let upstream = Some(parts[4].trim().to_string()).filter(|s| !s.is_empty());
        let (ahead, behind, gone) = parse_track(parts[5]);
        out.push(ParsedRef {
            full_name: full_name.to_string(),
            oid: oid.to_string(),
            peeled,
            head,
            upstream,
            ahead,
            behind,
            gone,
        });
    }
    out
}

fn parse_track(raw: &str) -> (u32, u32, bool) {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("gone") {
        return (0, 0, true);
    }
    let mut ahead = 0;
    let mut behind = 0;
    if let Some(pos) = lower.find("ahead ") {
        ahead = lower[pos + 6..]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
    }
    if let Some(pos) = lower.find("behind ") {
        behind = lower[pos + 7..]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
    }
    (ahead, behind, false)
}

/// 解析 `git rev-list --parents -n 1 <commit>` 的第一父提交。
pub fn parse_first_parent(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut parts = text.split_whitespace();
    let _self_oid = parts.next()?;
    parts.next().map(|s| s.to_string())
}

/// 把 `parse_name_status_z` 的 raw status 映射为模型状态。
pub fn file_status_label(raw: &str) -> &'static str {
    match raw.chars().next().unwrap_or('M') {
        'A' => "added",
        'D' => "removed",
        'R' => "renamed",
        'C' => "copied",
        'T' => "typechange",
        'U' => "conflicted",
        _ => "modified",
    }
}

/// 状态字符 → 展示用 kind。
pub fn status_kind(x: &str, y: &str, untracked: bool, conflicted: bool) -> &'static str {
    if untracked {
        return "untracked";
    }
    if conflicted {
        return "conflicted";
    }
    match (x, y) {
        ("R", _) => "renamed",
        ("C", _) => "copied",
        ("A", _) => "added",
        ("D", _) => "deleted",
        ("T", _) | (_, "T") => "typechange",
        ("U", _) | (_, "U") => "conflicted",
        _ => "modified",
    }
}

/// 构建 RefInfo（供 service 使用）。
pub fn ref_info(parsed: &ParsedRef, display_name: String) -> RefInfo {
    RefInfo {
        name: display_name,
        hash: parsed.peeled.clone().unwrap_or_else(|| parsed.oid.clone()),
        current: parsed.head,
        upstream: parsed.upstream.clone(),
        ahead: parsed.ahead,
        behind: parsed.behind,
        gone: parsed.gone,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parses_branch_and_groups() {
        let raw = b"# branch.oid abc123\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaa bbb src/a.rs\0\
1 M. N... 100644 100644 100644 aaa bbb src/b.rs\0\
2 R. N... 100644 100644 100644 aaa bbb R100 new.md\0old.md\0\
? notes/today.md\0\
u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.md\0";
        let st = parse_status_v2(raw);
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.head.as_deref(), Some("abc123"));
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.ahead, st.behind), (2, 1));
        assert_eq!(st.entries.len(), 5);
        assert_eq!(st.entries[0].path, "src/a.rs");
        assert_eq!(st.entries[0].x, ".");
        assert_eq!(st.entries[1].x, "M");
        assert_eq!(st.entries[2].path, "new.md");
        assert_eq!(st.entries[2].from.as_deref(), Some("old.md"));
        assert!(st.entries[3].untracked);
        assert!(st.entries[4].conflicted);
    }

    #[test]
    fn status_parses_unborn_and_detached() {
        let unborn = parse_status_v2(b"# branch.oid (initial)\0# branch.head main\0? a.md\0");
        assert!(unborn.unborn);
        assert!(unborn.head.is_none());
        let detached = parse_status_v2(b"# branch.oid abc\0# branch.head (detached)\0");
        assert!(detached.detached);
        assert!(detached.branch.is_none());
    }

    #[test]
    fn log_parses_fields_and_body_with_newlines() {
        let raw = b"abc\0tree1\0p1 p2\0Alice\0a@x.com\01700000000\0Bob\0b@x.com\01700000001\0HEAD -> main, tag: v1.0\0subject | x\0subject | x\n\nbody line\n\0";
        let commits = parse_log_z(raw);
        assert_eq!(commits.len(), 1);
        let c = &commits[0];
        assert_eq!(c.oid, "abc");
        assert_eq!(c.parents, vec!["p1", "p2"]);
        assert!(c.is_merge);
        assert_eq!(c.author_timestamp, 1_700_000_000);
        assert_eq!(c.refs, vec!["HEAD -> main", "tag: v1.0"]);
        assert_eq!(c.subject, "subject | x");
        assert!(c.message.contains("body line"));
    }

    #[test]
    fn log_skips_separator_tokens() {
        let one = b"a\0t\0\0A\0a@x\01\0A\0a@x\01\0\0s\0m\0";
        let commits = parse_log_z(one);
        assert_eq!(commits.len(), 1);
    }

    #[test]
    fn name_status_handles_rename_and_plain() {
        let raw = b"R100\0old.md\0new.md\0M\0a.txt\0A\0b.txt\0D\0c.txt\0";
        let items = parse_name_status_z(raw);
        assert_eq!(items.len(), 4);
        assert_eq!(
            items[0],
            ("new.md".into(), "R100".into(), Some("old.md".into()))
        );
        assert_eq!(items[1].0, "a.txt");
    }

    #[test]
    fn numstat_handles_rename_and_binary() {
        let raw = b"0\t0\t\0old.md\0new.md\03\t1\ta.txt\0-\t-\timg.png\0";
        let items = parse_numstat_z(raw);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].0, "new.md");
        assert_eq!(items[1], ("a.txt".into(), Some(3), Some(1), false));
        assert!(items[2].3);
    }

    #[test]
    fn refs_parse_annotated_and_tracking() {
        let raw = b"refs/heads/main\0abc\0\0*\0origin/main\0[ahead 2, behind 1]\nrefs/tags/v1\0tagobj\0peeled\0 \0\0\n";
        let refs = parse_refs(raw);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].full_name, "refs/heads/main");
        assert!(refs[0].head);
        assert_eq!(refs[0].ahead, 2);
        assert_eq!(refs[0].behind, 1);
        assert_eq!(refs[1].peeled.as_deref(), Some("peeled"));
    }

    #[test]
    fn track_gone_and_empty() {
        assert_eq!(parse_track("[gone]"), (0, 0, true));
        assert_eq!(parse_track(""), (0, 0, false));
        assert_eq!(parse_track("[behind 3]"), (0, 3, false));
    }

    #[test]
    fn first_parent_skips_self() {
        assert_eq!(parse_first_parent(b"abc p1 p2\n").as_deref(), Some("p1"));
        assert_eq!(parse_first_parent(b"abc\n"), None);
    }
}
