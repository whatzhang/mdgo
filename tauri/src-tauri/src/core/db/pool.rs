//! 读写分离 SQLite 连接池（统一连接参数 + 周期性维护）。
//!
//! # 设计
//!
//! - **单写连接（每 Store 池）**：写操作在**本池**内串行化到一条连接，配合 `busy_timeout`
//!   兜底其他池/进程对同一文件的写竞争。注意：同一 DB 文件（如 `{dir}/.mdgo/mdgo.db`）
//!   会被 ChatStore / AiHistoryStore / PromptStore / Schedule / Skill 等各自打开独立的
//!   `DbPool`（同文件存在多个写连接），跨池写写竞争仍由 SQLite 的 busy_timeout +
//!   应用层重试（[`crate::core::db::with_busy_retry`]）兜底——并非「全局单写连接」。
//! - **多读连接**：WAL 模式天然支持多读一写；读操作按轮询分配到 `READ_CONNECTIONS`
//!   条只读连接上并行执行，读不再被本池写锁或互斥锁串行化。
//! - **统一 PRAGMA**（[`apply_pragmas`]）：WAL / busy_timeout / synchronous=NORMAL /
//!   foreign_keys / cache_size / mmap_size / temp_store=MEMORY / wal_autocheckpoint，
//!   全库一份，消除各域手写 PRAGMA 的漂移。
//! - **周期性维护**（[`DbPool::with_write`] 内部触发）：距上次维护写计数差或时间间隔达到
//!   阈值时顺带执行 `PRAGMA optimize` + `wal_checkpoint(TRUNCATE)`，防止 WAL 无限膨胀
//!   拖慢写入，无需外部定时器。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

/// 只读连接池大小（桌面应用并发读场景：列表/消息/搜索/索引追平同时发生）
pub const READ_CONNECTIONS: usize = 4;

/// 距上次维护的最小间隔（毫秒）：30 分钟
const MAINTENANCE_INTERVAL_MS: u64 = 30 * 60 * 1000;

/// 写操作计数维护阈值：每 500 次写顺带维护一次（快速路径先看计数，成本极低）
const WRITE_MAINTENANCE_THRESHOLD: usize = 500;

/// 统一连接参数（全部 DB 域共用同一份，避免参数漂移）。
///
/// - `journal_mode=WAL`：多连接并发读写的基础；
/// - `busy_timeout=5000`：写写互斥时的忙等待兜底；
/// - `synchronous=NORMAL`：WAL 推荐值，提交不 fsync（实测 Windows 单事务 FULL 同步高延迟）；
/// - `foreign_keys=ON`：聊天等表依赖外键级联删除（无 FK 的表无副作用）；
/// - `cache_size=-16384`：16MiB 页缓存（负数 = KiB）；
/// - `mmap_size=268435456`：256MiB 内存映射读（减少系统调用）；
/// - `temp_store=MEMORY`：排序/临时表放内存（数据量小，避免磁盘 IO）；
/// - `wal_autocheckpoint=1000`：显式声明（默认值），防 WAL 膨胀。
pub fn apply_pragmas(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;
         PRAGMA synchronous=NORMAL;
         PRAGMA foreign_keys=ON;
         PRAGMA cache_size=-16384;
         PRAGMA mmap_size=268435456;
         PRAGMA temp_store=MEMORY;
         PRAGMA wal_autocheckpoint=1000;",
    )
    .map_err(|e| format!("初始化数据库连接失败: {}", e))
}

/// 读写分离连接池：1 条写连接 + `READ_CONNECTIONS` 条只读连接，全部指向同一文件。
///
/// `Connection` 本身 `!Sync`，各连接分别由 `Mutex` 保护；读连接按原子轮询分配，
/// 多条读可并行（彼此不互斥），写连接全局唯一（串行写）。
pub struct DbPool {
    write: Mutex<Connection>,
    readers: Vec<Mutex<Connection>>,
    rr: AtomicUsize,
    write_count: AtomicUsize,
    last_maintenance: AtomicU64,
    /// 上次维护时的写计数（用于差值判定，避免 write_count 只增不减导致恒触发）
    last_maintenance_count: AtomicUsize,
}

impl DbPool {
    /// 打开（或创建）指定路径的读写分离连接池。
    pub fn open(path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建数据库目录失败: {}", e))?;
        }
        let write = Connection::open(&path).map_err(|e| format!("打开数据库失败: {}", e))?;
        apply_pragmas(&write)?;
        let mut readers = Vec::with_capacity(READ_CONNECTIONS);
        for _ in 0..READ_CONNECTIONS {
            let conn = Connection::open(&path).map_err(|e| format!("打开数据库失败: {}", e))?;
            apply_pragmas(&conn)?;
            readers.push(Mutex::new(conn));
        }
        Ok(Self {
            write: Mutex::new(write),
            readers,
            rr: AtomicUsize::new(0),
            write_count: AtomicUsize::new(0),
            last_maintenance: AtomicU64::new(0),
            last_maintenance_count: AtomicUsize::new(0),
        })
    }

    /// 打开知识库级统一数据库（`{dir_path}/.mdgo/mdgo.db`）的读写分离连接池。
    pub fn open_kb(dir_path: &str) -> Result<Self, String> {
        let path = crate::core::db::global::kb_db_path(dir_path)?;
        Self::open(path)
    }

    /// 在读连接池上执行只读闭包（轮询分配读连接，多读并行）。
    ///
    /// 锁被污染（闭包 panic）时恢复使用（poison 恢复：读连接无事务状态，可安全复用）。
    pub fn with_read<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        let idx = self.rr.fetch_add(1, Ordering::Relaxed) % self.readers.len();
        let mut conn = self
            .readers
            .get(idx)
            .expect("readers 非空（构造时固定填充 READ_CONNECTIONS 条）")
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        f(&mut conn)
    }

    /// 在唯一写连接上执行写闭包（串行写；结束后顺带检查周期性维护）。
    pub fn with_write<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        // poison 恢复：写连接无残留事务时可安全复用（事务性写入走 with_write_txn）
        let mut conn = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let r = f(&mut conn);
        drop(conn);
        self.maybe_maintenance();
        r
    }

    /// 在唯一写连接上执行「单写事务」闭包：`BEGIN IMMEDIATE`（WAL 下避免 DEFERRED
    /// 读快照升级失败的 `SQLITE_BUSY_SNAPSHOT`）。
    ///
    /// - 闭包返回 `Err` → 自动 `ROLLBACK`；
    /// - 闭包 **panic** → `catch_unwind` 捕获后 `ROLLBACK` 再 `resume_unwind`，
    ///   保证连接不残留未提交事务（否则复用会带着旧事务执行）；
    /// - `COMMIT` 失败 → 尝试 `ROLLBACK` 后返回错误，连接回到干净状态；
    /// - 闭包内**不要再手动** `BEGIN/COMMIT/ROLLBACK`。
    pub fn with_write_txn<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut conn = self.write.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")
            .map_err(|e| format!("开启事务失败: {}", e))?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut conn)));
        match result {
            Ok(Ok(v)) => {
                if let Err(e) = conn.execute_batch("COMMIT") {
                    // COMMIT 失败：事务状态未知，回滚到干净状态后再上报
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(format!("提交事务失败: {}", e));
                }
                drop(conn);
                self.maybe_maintenance();
                Ok(v)
            }
            Ok(Err(e)) => {
                conn.execute_batch("ROLLBACK").ok();
                drop(conn);
                self.maybe_maintenance();
                Err(e)
            }
            Err(panic) => {
                // 闭包 panic：回滚未提交事务后继续 unwind（保持原 panic 语义）
                conn.execute_batch("ROLLBACK").ok();
                std::panic::resume_unwind(panic);
            }
        }
    }

    /// 周期维护：`PRAGMA optimize`（重建查询计划统计）+ `wal_checkpoint(TRUNCATE)`
    /// （回收 WAL 空间）。由写路径按「距上次维护的写计数差 / 时间间隔」双条件触发，
    /// 成本低且无需外部定时器。
    fn maybe_maintenance(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last = self.last_maintenance.load(Ordering::Relaxed);
        let last_count = self.last_maintenance_count.load(Ordering::Relaxed);
        let writes = self.write_count.fetch_add(1, Ordering::Relaxed) + 1;
        // 差值判定：writes 只增不减，必须用「自上次维护以来的写次数」而非绝对计数
        if now.saturating_sub(last) >= MAINTENANCE_INTERVAL_MS
            || writes.saturating_sub(last_count) >= WRITE_MAINTENANCE_THRESHOLD
        {
            // compare_exchange 保证并发下仅一个线程执行维护；
            // expected 取 last（load 值），并发修改时失败则跳过本次
            if self
                .last_maintenance
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                // try_lock：维护是尽力而为，拿不到写锁（写密集）则跳过
                if let Ok(conn) = self.write.try_lock() {
                    let _ = conn.execute_batch("PRAGMA optimize; PRAGMA wal_checkpoint(TRUNCATE);");
                    // 维护真正执行后才推进计数；try_lock 失败（写密集）时不推进，
                    // 下次写仍满足计数条件重试——避免「已推进计数但未执行」导致维护被推迟
                    self.last_maintenance_count
                        .store(writes, Ordering::Relaxed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_open_and_basic_io() {
        let dir = tempfile::tempdir().unwrap();
        let pool = DbPool::open(dir.path().join("t.db")).unwrap();
        pool.with_write(|conn| {
            conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);")
                .map_err(|e| e.to_string())
        })
        .unwrap();
        pool.with_write_txn(|conn| {
            conn.execute("INSERT INTO t (v) VALUES (?1)", rusqlite::params!["a"])
                .map_err(|e| e.to_string())?;
            Ok(())
        })
        .unwrap();
        let v: String = pool
            .with_read(|conn| {
                conn.query_row("SELECT v FROM t WHERE id = 1", [], |r| r.get(0))
                    .map_err(|e| e.to_string())
            })
            .unwrap();
        assert_eq!(v, "a");
    }

    #[test]
    fn txn_rolls_back_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let pool = DbPool::open(dir.path().join("t.db")).unwrap();
        pool.with_write(|conn| {
            conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);")
                .map_err(|e| e.to_string())
        })
        .unwrap();
        let r = pool.with_write_txn(|conn| {
            conn.execute("INSERT INTO t (v) VALUES (?1)", rusqlite::params!["x"])
                .map_err(|e| e.to_string())?;
            Err::<(), _>("模拟失败".to_string())
        });
        assert!(r.is_err());
        let count: i64 = pool
            .with_read(|conn| {
                conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
                    .map_err(|e| e.to_string())
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn readers_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let pool = DbPool::open(dir.path().join("t.db")).unwrap();
        pool.with_write(|conn| {
            conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);")
                .map_err(|e| e.to_string())
        })
        .unwrap();
        // 并发读（轮询分配多条读连接）不报错
        let pool = std::sync::Arc::new(pool);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let pool = pool.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        pool.with_read(|conn| {
                            conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0))
                                .map_err(|e| e.to_string())
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn maintenance_triggers_after_500_writes_not_each_write() {
        // 回归测试：修复前 write_count 只增不减 + CAS expected 恒真，
        // 第 500 次写后每次写都触发 checkpoint（持续写放大）。
        let dir = tempfile::tempdir().unwrap();
        let pool = DbPool::open(dir.path().join("t.db")).unwrap();
        pool.with_write(|conn| {
            conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);")
                .map_err(|e| e.to_string())
        })
        .unwrap();
        // 隔离「首次触发」（last_maintenance 初始 0 会因时间条件触发一次，属预期）：
        // 重置时间基准与计数，使循环内仅计数条件生效
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        pool.last_maintenance.store(now, Ordering::Relaxed);
        pool.write_count.store(0, Ordering::Relaxed);
        pool.last_maintenance_count.store(0, Ordering::Relaxed);
        // 驱动 600 次写计数
        for _ in 0..600 {
            pool.maybe_maintenance();
        }
        // 差值判定：第 500 次触发一次，之后 100 次差值 < 500 不再触发；
        // 修复前 last_maintenance_count 会在第 500 次后的每次调用时更新（≈600）
        assert_eq!(
            pool.last_maintenance_count.load(Ordering::Relaxed),
            WRITE_MAINTENANCE_THRESHOLD as usize,
            "维护应仅在写计数差达 500 时触发一次"
        );
        // 自上次维护以来的差值 < 阈值（600 - 500 = 100）
        assert!(
            pool.write_count.load(Ordering::Relaxed)
                .saturating_sub(pool.last_maintenance_count.load(Ordering::Relaxed))
                < WRITE_MAINTENANCE_THRESHOLD
        );
    }

    #[test]
    fn maintenance_not_counted_when_write_lock_held() {
        // 写锁被持有（写密集）时 try_lock 失败：计数不得推进，维护在下次写重试；
        // 若计数提前推进会出现「已计数但未执行维护」导致维护被推迟
        let dir = tempfile::tempdir().unwrap();
        let pool = DbPool::open(dir.path().join("t.db")).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        pool.last_maintenance.store(now, Ordering::Relaxed);
        pool.write_count.store(0, Ordering::Relaxed);
        pool.last_maintenance_count.store(0, Ordering::Relaxed);
        // 持有写锁模拟写密集：期间 maybe_maintenance 的 try_lock 全部失败
        let _guard = pool.write.lock().unwrap();
        for _ in 0..600 {
            pool.maybe_maintenance();
        }
        drop(_guard);
        assert_eq!(
            pool.last_maintenance_count.load(Ordering::Relaxed),
            0,
            "写锁被持有期间维护不应推进计数"
        );
    }

    #[test]
    fn txn_panic_rolls_back_and_connection_reusable() {
        // 闭包 panic → catch_unwind 捕获 → ROLLBACK → resume_unwind；
        // 连接不得残留未提交事务，后续写入正常
        let dir = tempfile::tempdir().unwrap();
        let pool = DbPool::open(dir.path().join("t.db")).unwrap();
        pool.with_write(|conn| {
            conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);")
                .map_err(|e| e.to_string())
        })
        .unwrap();
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<(), String> = pool.with_write_txn(|conn| {
                conn.execute("INSERT INTO t (v) VALUES (?1)", rusqlite::params!["x"])
                    .map_err(|e| e.to_string())?;
                panic!("模拟业务 panic");
            });
        }));
        assert!(panic_result.is_err(), "panic 应继续传播");
        // 连接可复用且无残留事务：panic 事务已回滚，只有后续写入落库
        pool.with_write(|conn| {
            conn.execute_batch("INSERT INTO t (v) VALUES ('y');")
                .map_err(|e| e.to_string())
        })
        .unwrap();
        let count: i64 = pool
            .with_read(|conn| {
                conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
                    .map_err(|e| e.to_string())
            })
            .unwrap();
        assert_eq!(count, 1, "panic 事务已回滚，仅保留后续写入");
    }
}
