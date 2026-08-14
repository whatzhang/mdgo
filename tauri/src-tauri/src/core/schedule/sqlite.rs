//! SQLite 事件存储实现（单一职责：ScheduleEvent 的 SQLite 持久化）。
//!
//! - **与 memory / prompts 共用单一全局数据库**：`%APPDATA%/com.mdgo/mdgo.db`
//!   （见 [`crate::core::db::global`]），以 `dir_path` 列做知识库数据隔离（复合主键 `(dir_path, id)`）。
//! - 连接参数：`WAL`（读写并发，读不阻塞写）+ `busy_timeout`（锁竞争等待）+ `synchronous=NORMAL`（WAL 下安全且低延迟）
//! - 全部 SQL 使用参数化绑定，杜绝注入
//! - 连接非 `Sync`：由调用方以 `Arc<Mutex<SqliteStore>>` 串行化访问

use std::path::PathBuf;

use rusqlite::{params, Connection};

use super::store::EventStore;
use super::ScheduleEvent;

/// SQLite 存储实现（每个知识库目录一个实例，但指向同一全局 DB 文件）
pub struct SqliteStore {
    conn: Connection,
    /// 知识库目录（数据隔离列）
    dir_path: String,
}

/// 建表 DDL（幂等）：`dir_path` 为知识库隔离列，复合主键
const CREATE_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS schedule_events (
    dir_path   TEXT NOT NULL,
    id         TEXT NOT NULL,
    title      TEXT NOT NULL,
    start      TEXT NOT NULL,
    end        TEXT NOT NULL,
    color      TEXT NOT NULL DEFAULT '',
    desc       TEXT NOT NULL DEFAULT '',
    cron       TEXT NOT NULL DEFAULT '',
    notify     INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (dir_path, id)
);
CREATE INDEX IF NOT EXISTS idx_schedule_dir_time ON schedule_events(dir_path, created_at)
"#;

/// 列顺序（不含隔离列 dir_path），与 `ScheduleEvent` 字段一一对应
const COLS: &str = "id, title, start, end, color, desc, cron, notify, created_at, updated_at";

impl SqliteStore {
    /// 打开知识库级统一数据库（`{dir_path}/.mdgo/mdgo.db`），实例绑定 `dir_path`（数据隔离列）。
    /// dir_path 先经 `sanitize_kb_dir` 规范化（防穿越 + 同目录多写法归一），列值用规范路径。
    pub fn new(dir_path: &str) -> Result<Self, String> {
        let canonical = crate::core::db::global::sanitize_kb_dir(dir_path)?;
        let db_path = crate::core::db::global::kb_db_path(dir_path)?;
        Self::open_for_dir(&canonical.to_string_lossy(), db_path)
    }

    /// 打开指定 DB 文件（测试用），实例绑定 `dir_path`
    pub fn open_for_dir(dir_path: &str, db_path: impl Into<PathBuf>) -> Result<Self, String> {
        let db_path = db_path.into();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建日程数据目录失败: {}", e))?;
        }
        let conn = Connection::open(&db_path)
            .map_err(|e| format!("打开日程数据库失败: {}", e))?;
        // 统一连接参数（WAL / busy_timeout / synchronous / cache_size / mmap / temp_store）
        crate::core::db::pool::apply_pragmas(&conn)?;
        conn.execute_batch(CREATE_TABLE)
            .map_err(|e| format!("初始化日程数据表失败: {}", e))?;
        Ok(Self {
            conn,
            dir_path: dir_path.to_string(),
        })
    }

    /// 单条插入/更新语句（upsert 语义，按 dir_path 隔离）
    fn upsert_sql(conn: &Connection, dir: &str, e: &ScheduleEvent) -> rusqlite::Result<()> {
        conn.execute(
            &format!(
                "INSERT INTO schedule_events (dir_path, {COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                 ON CONFLICT(dir_path, id) DO UPDATE SET
                    title=excluded.title, start=excluded.start, end=excluded.end,
                    color=excluded.color, desc=excluded.desc, cron=excluded.cron,
                    notify=excluded.notify, updated_at=excluded.updated_at"
            ),
            params![
                dir, e.id, e.title, e.start, e.end, e.color, e.desc, e.cron,
                i64::from(e.notify), e.created_at, e.updated_at
            ],
        )?;
        Ok(())
    }

    fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduleEvent> {
        Ok(ScheduleEvent {
            id: row.get(0)?,
            title: row.get(1)?,
            start: row.get(2)?,
            end: row.get(3)?,
            color: row.get(4)?,
            desc: row.get(5)?,
            cron: row.get(6)?,
            notify: row.get::<_, i64>(7)? != 0,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
        })
    }
}

impl EventStore for SqliteStore {
    fn list(&self) -> Result<Vec<ScheduleEvent>, String> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT {COLS} FROM schedule_events WHERE dir_path = ?1 ORDER BY created_at, id"
            ))
            .map_err(|e| format!("查询日程失败: {}", e))?;
        let rows = stmt
            .query_map(params![self.dir_path], Self::row_to_event)
            .map_err(|e| format!("查询日程失败: {}", e))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row.map_err(|e| format!("读取日程记录失败: {}", e))?);
        }
        Ok(events)
    }

    fn upsert(&mut self, event: ScheduleEvent) -> Result<(), String> {
        Self::upsert_sql(&self.conn, &self.dir_path, &event)
            .map_err(|e| format!("写入日程失败: {}", e))
    }

    fn remove(&mut self, id: &str) -> Result<(), String> {
        self.conn
            .execute(
                "DELETE FROM schedule_events WHERE dir_path = ?1 AND id = ?2",
                params![self.dir_path, id],
            )
            .map_err(|e| format!("删除日程失败: {}", e))?;
        Ok(())
    }

    fn replace_all(&mut self, events: Vec<ScheduleEvent>) -> Result<(), String> {
        // IMMEDIATE：WAL 下避免 DEFERRED 读快照升级失败的 SQLITE_BUSY_SNAPSHOT
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| format!("开启事务失败: {}", e))?;
        tx.execute(
            "DELETE FROM schedule_events WHERE dir_path = ?1",
            params![self.dir_path],
        )
        .map_err(|e| format!("清空日程失败: {}", e))?;
        for e in &events {
            Self::upsert_sql(&tx, &self.dir_path, e)
                .map_err(|err| format!("批量写入日程失败: {}", err))?;
        }
        tx.commit().map_err(|e| format!("提交事务失败: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event(id: &str, title: &str) -> ScheduleEvent {
        ScheduleEvent {
            id: id.into(),
            title: title.into(),
            start: "2026-08-13T10:00".into(),
            end: "2026-08-13T11:00".into(),
            color: "blue".into(),
            desc: "".into(),
            cron: "".into(),
            notify: true,
            created_at: "2026-08-13T09:00".into(),
            updated_at: "2026-08-13T09:00".into(),
        }
    }

    fn tmp_store(dir: &str, name: &str) -> (tempfile::TempDir, SqliteStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = SqliteStore::open_for_dir(dir, tmp.path().join(name)).unwrap();
        (tmp, store)
    }

    #[test]
    fn missing_db_returns_empty() {
        let (_d, store) = tmp_store("/kb/a", "empty.db");
        assert_eq!(store.list().unwrap(), Vec::<ScheduleEvent>::new());
    }

    #[test]
    fn upsert_inserts_then_updates() {
        let (_d, mut store) = tmp_store("/kb/a", "upsert.db");
        store.upsert(sample_event("e1", "会议")).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);

        // 同 id 更新（标题/时间变化，不新增）
        let mut updated = sample_event("e1", "评审会");
        updated.start = "2026-08-13T14:00".into();
        updated.end = "2026-08-13T15:00".into();
        store.upsert(updated).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "评审会");
        assert_eq!(list[0].start, "2026-08-13T14:00");
        assert_eq!(list[0].notify, true);
    }

    #[test]
    fn remove_deletes_by_id() {
        let (_d, mut store) = tmp_store("/kb/a", "remove.db");
        store.upsert(sample_event("e1", "a")).unwrap();
        store.upsert(sample_event("e2", "b")).unwrap();
        store.remove("e1").unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "e2");
    }

    #[test]
    fn replace_all_is_atomic() {
        let (_d, mut store) = tmp_store("/kb/a", "replace.db");
        store.upsert(sample_event("e1", "a")).unwrap();
        store
            .replace_all(vec![sample_event("e2", "b"), sample_event("e3", "c")])
            .unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|e| e.id != "e1"));
    }

    #[test]
    fn data_persists_across_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("persist.db");
        {
            let mut store = SqliteStore::open_for_dir("/kb/a", &path).unwrap();
            store.upsert(sample_event("e1", "会议")).unwrap();
        }
        // 重新打开（模拟应用重启），数据仍在
        let store = SqliteStore::open_for_dir("/kb/a", &path).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "会议");
    }

    #[test]
    fn field_mapping_roundtrips() {
        let (_d, mut store) = tmp_store("/kb/a", "fields.db");
        let mut e = sample_event("e1", "带描述的日程");
        e.desc = "备注内容".into();
        e.cron = "0 9 * * 1".into();
        e.notify = false;
        store.upsert(e).unwrap();
        let got = &store.list().unwrap()[0];
        assert_eq!(got.desc, "备注内容");
        assert_eq!(got.cron, "0 9 * * 1");
        assert_eq!(got.notify, false);
    }

    #[test]
    fn dirs_are_isolated_in_shared_db() {
        // 同一 DB 文件，两个知识库目录数据互不可见（复合主键 (dir_path, id)）
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("shared.db");
        {
            let mut a = SqliteStore::open_for_dir("/kb/a", &path).unwrap();
            let mut b = SqliteStore::open_for_dir("/kb/b", &path).unwrap();
            a.upsert(sample_event("e1", "甲库日程")).unwrap();
            b.upsert(sample_event("e1", "乙库日程")).unwrap(); // 同 id，不同 dir
        }
        let a = SqliteStore::open_for_dir("/kb/a", &path).unwrap();
        let b = SqliteStore::open_for_dir("/kb/b", &path).unwrap();
        let a_list = a.list().unwrap();
        let b_list = b.list().unwrap();
        assert_eq!(a_list.len(), 1);
        assert_eq!(b_list.len(), 1);
        assert_eq!(a_list[0].title, "甲库日程");
        assert_eq!(b_list[0].title, "乙库日程");
    }
}
