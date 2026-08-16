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
    /// 记录一次已推送的提醒（幂等：同 `(event_id, trigger_at)` 首次返回 `true`，重复返回 `false`）。
    ///
    /// 提醒调度器据此保证"同一触发点只推一次"，避免窗口期内（如提前提醒的
    /// `[start - notify_before, end)` 长窗口）重复推送 / 应用重启后重复弹窗。
    fn record_reminder(&mut self, event_id: &str, trigger_at: &str) -> Result<bool, String>;
    /// 清理 `before` 之前（含）的提醒推送记录（防止日志表无限增长）。
    fn cleanup_reminders(&mut self, before: &str) -> Result<(), String>;
}
