//! 事件存储抽象（接口隔离 + 依赖倒置）。
//!
//! - [`EventStore`]：存储抽象，命令层/工具层只依赖此接口，不依赖具体实现；
//! - 具体实现见 [`super::sqlite::SqliteStore`]（SQLite 持久化，与 memory/prompts 共用全局 `mdgo.db`）。

use super::ScheduleEvent;

/// 事件存储抽象（依赖倒置：调用方依赖此接口，不依赖具体存储实现）。
///
/// 实现持有非 `Sync` 的连接（如 rusqlite `Connection`），由调用方以
/// `Arc<Mutex<Store>>` 串行化访问，因此只需 `Send`。
pub trait EventStore: Send {
    /// 全部事件（按创建顺序）
    fn list(&self) -> Result<Vec<ScheduleEvent>, String>;
    /// 新增或按 id 更新（upsert）
    fn upsert(&mut self, event: ScheduleEvent) -> Result<(), String>;
    /// 按 id 删除
    fn remove(&mut self, id: &str) -> Result<(), String>;
    /// 整体替换（批量导入用，事务保证原子性）
    fn replace_all(&mut self, events: Vec<ScheduleEvent>) -> Result<(), String>;
}
