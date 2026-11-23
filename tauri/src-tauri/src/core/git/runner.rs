//! Git 子进程执行器（读/写共用）。
//!
//! 关键保证：
//! - **无 shell**：参数独立传入，路径/引用先经校验；
//! - **只读友好**：`GIT_OPTIONAL_LOCKS=0`、`GIT_TERMINAL_PROMPT=0`、`GIT_PAGER=cat`、
//!   `GIT_ATTR_NOSYSTEM=1`、`--no-pager`、`--literal-pathspecs`；
//! - **超时/取消即 kill**：轮询 `try_wait`，到点 `kill + wait`，不泄漏子进程；
//! - **输出有界**：stdout/stderr 各自限制，超出继续 drain 但只计数，避免子进程阻塞；
//! - **单一出口**：所有 git 子进程都应从这里发出。

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use super::error::GitError;

/// 默认单次读命令超时（秒）。
pub const DEFAULT_TIMEOUT_SECS: u64 = 15;
/// stdout 保留上限（字节；超出只计数并标记 truncated）。
pub const MAX_STDOUT_BYTES: usize = 8 * 1024 * 1024;
/// stderr 保留上限（字节）。
pub const MAX_STDERR_BYTES: usize = 64 * 1024;

/// 一次 git 执行的输出（退出码非 0 也返回 `Ok`，由调用方判定）。
#[derive(Debug, Clone, Default)]
pub struct GitOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: Option<i32>,
    pub stdout_truncated: bool,
}

impl GitOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

/// 执行选项。
pub struct RunOptions<'a> {
    pub timeout: Duration,
    pub cancel: Option<&'a AtomicBool>,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

impl Default for RunOptions<'_> {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            cancel: None,
            max_stdout_bytes: MAX_STDOUT_BYTES,
            max_stderr_bytes: MAX_STDERR_BYTES,
        }
    }
}

/// 可克隆的取消标记：工具层持有它，取消时置位，runner 轮询到后 kill 子进程。
#[derive(Debug, Clone, Default)]
pub struct CancelFlag {
    flag: Arc<AtomicBool>,
}

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    #[allow(dead_code)]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub fn as_atomic(&self) -> &AtomicBool {
        &self.flag
    }
}

/// Git 执行器。`program` 默认 `git`；测试可注入假程序。
#[derive(Debug, Clone)]
pub struct GitRunner {
    program: String,
}

impl Default for GitRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl GitRunner {
    pub fn new() -> Self {
        Self {
            program: "git".to_string(),
        }
    }

    #[allow(dead_code)] // 测试注入用
    pub fn with_program(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// 同步执行（命令在 `root` 下运行）。退出码非 0 仍返回 `Ok(GitOutput)`。
    pub fn run(
        &self,
        root: &Path,
        args: &[String],
        opts: &RunOptions<'_>,
    ) -> Result<GitOutput, GitError> {
        if root.as_os_str().is_empty() {
            return Err(GitError::InvalidPath("仓库路径为空".into()));
        }
        if !root.is_dir() {
            return Err(GitError::InvalidPath(format!(
                "仓库目录不存在: {}",
                root.display()
            )));
        }

        let mut cmd = Command::new(&self.program);
        cmd.arg("-C")
            .arg(root)
            .arg("--no-pager")
            .arg("--literal-pathspecs")
            .arg("-c")
            .arg("core.quotepath=false")
            .arg("-c")
            .arg("color.ui=false")
            .arg("-c")
            .arg("core.fsmonitor=false")
            .arg("-c")
            .arg("log.showSignature=false")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_PAGER", "cat")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("LC_ALL", "C");

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                GitError::NotInstalled
            } else {
                GitError::Io(format!("启动 git 失败: {e}"))
            }
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| GitError::Io("无法获取 git stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| GitError::Io("无法获取 git stderr".into()))?;

        let rx_out = spawn_reader(stdout, opts.max_stdout_bytes);
        let rx_err = spawn_reader(stderr, opts.max_stderr_bytes);

        let deadline = Instant::now() + opts.timeout;
        let mut timed_out = false;
        let mut cancelled = false;
        let mut code: Option<i32> = None;

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    code = status.code();
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(GitError::Io(format!("等待 git 进程失败: {e}")));
                }
            }

            if opts
                .cancel
                .map(|c| c.load(Ordering::SeqCst))
                .unwrap_or(false)
            {
                cancelled = true;
                let _ = child.kill();
                let _ = child.wait();
                break;
            }

            if Instant::now() >= deadline {
                timed_out = true;
                let _ = child.kill();
                let _ = child.wait();
                break;
            }

            std::thread::sleep(Duration::from_millis(10));
        }

        // kill 后 pipe 会关闭；给读线程一个短窗口，避免极端情况下卡死。
        let (out_buf, out_truncated) = rx_out
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_default();
        let (err_buf, _) = rx_err
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_default();

        if cancelled {
            return Err(GitError::Cancelled);
        }
        if timed_out {
            return Err(GitError::Timeout(opts.timeout.as_secs()));
        }

        Ok(GitOutput {
            stdout: out_buf,
            stderr: err_buf,
            code,
            stdout_truncated: out_truncated,
        })
    }
}

/// 读线程：最多保留 `cap` 字节，超出继续读但只丢弃，避免子进程因管道满而阻塞。
fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    cap: usize,
) -> mpsc::Receiver<(Vec<u8>, bool)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut total = 0usize;
        let mut truncated = false;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    total = total.saturating_add(n);
                    if buf.len() < cap {
                        let take = (cap - buf.len()).min(n);
                        buf.extend_from_slice(&chunk[..take]);
                    }
                    if total > cap {
                        truncated = true;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send((buf, truncated));
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_flag_round_trip() {
        let flag = CancelFlag::new();
        assert!(!flag.is_cancelled());
        flag.cancel();
        assert!(flag.is_cancelled());
        let cloned = flag.clone();
        assert!(cloned.is_cancelled());
    }

    #[test]
    fn missing_program_maps_to_not_installed() {
        let runner = GitRunner::with_program("definitely-not-a-git-binary-mdgo-test");
        let err = runner
            .run(
                Path::new("."),
                &["--version".to_string()],
                &RunOptions::default(),
            )
            .unwrap_err();
        assert_eq!(err, GitError::NotInstalled);
    }
}
