pub mod bm25;
pub mod chunk_splitter;
pub mod lance;
pub mod schema;
pub mod utils;

use std::time::Duration;

/// 判断错误是否为 SQLite 锁竞争（SQLITE_BUSY / SQLITE_BUSY_SNAPSHOT）。
/// rusqlite 对这两类错误的文案均为 "database is locked"（BUSY_SNAPSHOT 亦带 busy 语义）。
fn is_busy_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("database is locked") || lower.contains("busy")
}

/// 对写操作应用应用层重试（指数退避），仅对锁竞争类错误重试。
///
/// `busy_timeout` 与 `BEGIN IMMEDIATE` 已消除绝大多数写冲突；
/// 本函数兜底极端场景（如超长写事务持锁超过 busy_timeout）的瞬时 `SQLITE_BUSY`，
/// 避免一次锁冲突直接导致消息 / 指标等关键数据丢失。
/// 非锁竞争类错误（约束冲突、IO 错误等）立即返回，不重试。
pub fn with_busy_retry<T>(
    max_attempts: usize,
    mut f: impl FnMut() -> Result<T, String>,
) -> Result<T, String> {
    let mut last_err = String::new();
    for attempt in 0..max_attempts {
        match f() {
            Ok(value) => return Ok(value),
            Err(err) => {
                last_err = err.clone();
                if !is_busy_error(&err) || attempt + 1 >= max_attempts {
                    return Err(err);
                }
                std::thread::sleep(Duration::from_millis(50 << attempt));
            }
        }
    }
    Err(last_err)
}
