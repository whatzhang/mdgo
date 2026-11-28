//! mihomo 内核子进程管理。
//!
//! 职责：spawn（含 Windows 隐藏控制台）、异步读取 stdout/stderr、优雅停止。
//! 健康检查与崩溃重启由 `VpnManager` 负责（它持有 AppHandle 与状态机）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};

pub struct CoreProcess {
    child: tokio::process::Child,
    pub pid: u32,
}

pub struct CoreSupervisor {
    child: Mutex<Option<CoreProcess>>,
}

impl Default for CoreSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl CoreSupervisor {
    pub fn new() -> Self {
        Self {
            child: Mutex::new(None),
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.lock().ok().and_then(|g| g.as_ref().map(|c| c.pid))
    }

    /// 启动内核。日志行通过 `log_tx` 交给上层（写入文件 + 事件 + 环形缓冲）。
    pub fn spawn(
        &self,
        bin: &Path,
        config: &Path,
        cache: &Path,
        log_path: PathBuf,
        log_tx: tokio::sync::mpsc::UnboundedSender<(String, String)>,
    ) -> Result<u32, String> {
        let mut cmd = tokio::process::Command::new(bin);
        cmd.arg("-d")
            .arg(cache)
            .arg("-f")
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(target_os = "windows")]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd.spawn().map_err(|e| format!("启动内核失败: {e}"))?;
        let pid = child.id().ok_or_else(|| "内核进程无 pid".to_string())?;

        // 异步读取 stdout / stderr（防管道阻塞）
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, "info", log_path.clone(), log_tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, "error", log_path, log_tx);
        }

        *self
            .child
            .lock()
            .map_err(|_| "内核状态锁已损坏".to_string())? = Some(CoreProcess { child, pid });
        Ok(pid)
    }

    /// 停止内核：先尝试 terminate，3 秒超时后强杀。
    pub async fn stop(&self) -> Result<(), String> {
        let taken = {
            let mut guard = self
                .child
                .lock()
                .map_err(|_| "内核状态锁已损坏".to_string())?;
            guard.take()
        };
        let Some(mut proc) = taken else { return Ok(()) };

        let start_kill = proc.child.start_kill();
        if let Err(e) = start_kill {
            log::warn!("[vpn] 终止内核失败（可能已退出）: {e}");
        }
        match tokio::time::timeout(Duration::from_secs(3), proc.child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(format!("等待内核退出失败: {e}")),
            Err(_) => {
                let _ = proc.child.kill().await;
                log::warn!("[vpn] 内核 3s 内未退出，已强制结束 pid={}", proc.pid);
                Ok(())
            }
        }
    }

    /// 清空句柄（用于外部已确认进程退出的场景）。
    pub fn forget(&self) {
        if let Ok(mut guard) = self.child.lock() {
            guard.take();
        }
    }
}

fn spawn_reader<R>(
    stream: R,
    level: &'static str,
    log_path: PathBuf,
    tx: tokio::sync::mpsc::UnboundedSender<(String, String)>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let text = line.trim_end().to_string();
            if text.is_empty() {
                continue;
            }
            let lv = classify(&text, level);
            append_log(&log_path, &text);
            let _ = tx.send((lv, text));
        }
    });
}

fn classify(line: &str, fallback: &str) -> String {
    let lower = line.to_ascii_lowercase();
    if lower.contains("error") || lower.contains("fatal") || fallback == "error" {
        "error".into()
    } else if lower.contains("warn") {
        "warn".into()
    } else if lower.contains("debug") {
        "debug".into()
    } else {
        "info".into()
    }
}

fn append_log(path: &PathBuf, line: &str) {
    use std::io::Write;
    let stamp = chrono::Local::now().format("%H:%M:%S");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "[{stamp}] {line}");
    }
    // 简单滚动：>5MB 时保留后半部分
    // 注意：按字节切分时必须落在 UTF-8 字符边界上，否则 `&str` 切片会 panic
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > 5 * 1024 * 1024 {
            if let Ok(bytes) = std::fs::read(path) {
                let half = bytes.len() / 2;
                let start = bytes[half..]
                    .iter()
                    .position(|b| *b == b'\n')
                    .map(|i| half + i + 1)
                    .unwrap_or(half);
                let tail = String::from_utf8_lossy(&bytes[start..]);
                let _ = std::fs::write(path, tail.as_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_levels() {
        assert_eq!(classify("ERROR: bad", "info"), "error");
        assert_eq!(classify("WARN something", "info"), "warn");
        assert_eq!(classify("debug detail", "info"), "debug");
        assert_eq!(classify("Start initial configuration", "info"), "info");
    }

    #[tokio::test]
    async fn stop_without_child_is_ok() {
        let sup = CoreSupervisor::new();
        assert!(sup.stop().await.is_ok());
        assert!(sup.pid().is_none());
    }
}
