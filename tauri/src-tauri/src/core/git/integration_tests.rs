//! 临时仓库集成测试：验证真实 git 行为（只读性、严格过滤、范围、注入拒绝）。

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use tempfile::TempDir;

use super::runner::CancelFlag;
use super::service::{DiffQuery, GitReadService, LogQuery, RefsQuery, ShowQuery, StatusQuery};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .arg("-c")
        .arg("user.name=Test")
        .arg("-c")
        .arg("user.email=test@example.com")
        .arg("-c")
        .arg("commit.gpgsign=false")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| panic!("spawn git {:?} failed: {e}", args));
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn repo() -> TempDir {
    let td = tempfile::tempdir().expect("tempdir");
    let p = td.path();
    git(p, &["init", "-q"]);
    std::fs::write(p.join("README.md"), "# hello\n").unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "feat: initial"]);
    td
}

fn dir_str(td: &TempDir) -> String {
    td.path().to_str().unwrap().to_string()
}

fn service() -> GitReadService {
    GitReadService::new()
}

#[test]
fn status_is_read_only_and_has_no_lock() {
    let td = repo();
    let p = td.path();
    let index = p.join(".git").join("index");
    let before = std::fs::read(&index).unwrap();

    let st = service()
        .status(
            &dir_str(&td),
            &StatusQuery {
                strict: false,
                ..Default::default()
            },
            &CancelFlag::new(),
        )
        .unwrap();
    assert_eq!(
        st.counts.staged + st.counts.unstaged + st.counts.untracked,
        0
    );
    assert_eq!(
        std::fs::read(&index).unwrap(),
        before,
        "status 不应写 index"
    );
    assert!(!p.join(".git").join("index.lock").exists());
}

#[test]
fn status_strict_hides_sensitive_and_mdgo() {
    let td = repo();
    let p = td.path();
    std::fs::write(p.join(".env"), "SECRET=1\n").unwrap();
    std::fs::create_dir_all(p.join(".mdgo")).unwrap();
    std::fs::write(p.join(".mdgo").join("config.yaml"), "token: x\n").unwrap();
    std::fs::write(p.join("notes.md"), "ok\n").unwrap();

    let st = service()
        .status(
            &dir_str(&td),
            &StatusQuery {
                strict: true,
                ..Default::default()
            },
            &CancelFlag::new(),
        )
        .unwrap();
    let text = super::render::render_status(&st);
    assert!(!text.contains(".env"), "敏感文件名不应出现: {text}");
    assert!(!text.contains("/.mdgo"), "mdgo 路径不应出现: {text}");
    assert!(
        !text.contains("config.yaml"),
        "mdgo 内容路径不应出现: {text}"
    );
    assert!(text.contains("notes.md") || st.counts.untracked >= 1);
    assert!(st.hidden_sensitive >= 1);
    assert!(st.hidden_mdgo >= 1);
}

#[test]
fn log_show_and_diff_work() {
    let td = repo();
    let p = td.path();
    std::fs::write(p.join("README.md"), "# hello\n\nworld\n").unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "docs: update readme"]);
    let head = git(p, &["rev-parse", "HEAD"]).trim().to_string();

    let log = service()
        .log(
            &dir_str(&td),
            &LogQuery {
                limit: 10,
                strict: true,
                ..Default::default()
            },
            &CancelFlag::new(),
        )
        .unwrap();
    assert!(log.commits.len() >= 2);
    assert_eq!(log.commits[0].subject, "docs: update readme");
    assert!(!log.commits[0].oid.is_empty());

    let show = service()
        .show(
            &dir_str(&td),
            &ShowQuery {
                rev: head.clone(),
                path: None,
                stat_only: false,
                strict: true,
            },
            &CancelFlag::new(),
        )
        .unwrap();
    assert_eq!(show.summary.oid, head);
    assert!(show.files.iter().any(|f| f.path == "README.md"));
    assert!(show.patch.as_deref().unwrap_or("").contains("world"));

    let diff = service()
        .diff(
            &dir_str(&td),
            &DiffQuery {
                from: Some("HEAD~1".into()),
                to: Some("HEAD".into()),
                strict: true,
                ..Default::default()
            },
            &CancelFlag::new(),
        )
        .unwrap();
    assert!(diff.files.iter().any(|f| f.path == "README.md"));
    assert!(diff.patch.as_deref().unwrap_or("").contains("world"));
}

#[test]
fn sensitive_commit_content_is_never_rendered() {
    let td = repo();
    let p = td.path();
    std::fs::write(p.join(".env"), "SECRET=super-secret\n").unwrap();
    git(p, &["add", "-f", ".env"]);
    git(p, &["commit", "-q", "-m", "chore: add env"]);
    let head = git(p, &["rev-parse", "HEAD"]).trim().to_string();

    let show = service()
        .show(
            &dir_str(&td),
            &ShowQuery {
                rev: head.clone(),
                path: None,
                stat_only: false,
                strict: true,
            },
            &CancelFlag::new(),
        )
        .unwrap();
    let text = super::render::render_show(&show);
    assert!(!text.contains("super-secret"), ".env 内容泄漏: {text}");
    assert!(!text.contains(".env"), ".env 路径泄漏: {text}");
    assert!(show.hidden_sensitive >= 1);
}

#[test]
fn refs_lists_branches_and_current() {
    let td = repo();
    git(td.path(), &["branch", "feature/x"]);
    let refs = service()
        .refs(
            &dir_str(&td),
            &RefsQuery {
                ..Default::default()
            },
            &CancelFlag::new(),
        )
        .unwrap();
    assert!(refs.heads.iter().any(|r| r.name == "feature/x"));
    assert!(refs.current_branch.is_some());
    assert!(refs.head_hash.is_some());
}

#[test]
fn path_and_rev_injection_is_rejected() {
    let td = repo();
    let err = service()
        .status(
            &dir_str(&td),
            &StatusQuery {
                path: Some("--output=C:/tmp/x".into()),
                strict: true,
                ..Default::default()
            },
            &CancelFlag::new(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("路径"), "{err}");

    let err = service()
        .show(
            &dir_str(&td),
            &ShowQuery {
                rev: "--all".into(),
                path: None,
                stat_only: true,
                strict: true,
            },
            &CancelFlag::new(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("引用"), "{err}");
}

#[test]
fn non_repo_returns_actionable_error() {
    let td = tempfile::tempdir().unwrap();
    let err = service()
        .status(
            &dir_str(&td),
            &StatusQuery {
                strict: true,
                ..Default::default()
            },
            &CancelFlag::new(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("不是 Git 仓库"), "{err}");
}

#[test]
fn parallel_status_has_no_index_lock_errors() {
    let td = repo();
    let dir = Arc::new(dir_str(&td));
    let mut handles = Vec::new();
    for _ in 0..10 {
        let dir = dir.clone();
        handles.push(std::thread::spawn(move || {
            service()
                .status(
                    &dir,
                    &StatusQuery {
                        strict: false,
                        ..Default::default()
                    },
                    &CancelFlag::new(),
                )
                .map(|_| ())
        }));
    }
    for h in handles {
        h.join().unwrap().expect("parallel status");
    }
}
