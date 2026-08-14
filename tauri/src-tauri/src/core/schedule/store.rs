//! 事件存储层（单一职责：ScheduleEvent 的持久化）。
//!
//! - [`EventStore`]：存储抽象（接口隔离），命令层/工具层只依赖此 trait。
//! - [`JsonFileStore`]：JSON 文件实现，读写 `{dir}/.mdgo/index_schedule.json`
//!   （格式 `{"events":[...]}`，与前端既有数据完全兼容），原子写（临时文件 + rename）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::ScheduleEvent;

/// 事件存储抽象（依赖倒置：调用方依赖此接口，不依赖具体存储实现）
pub trait EventStore: Send + Sync {
    /// 全部事件（按创建顺序）
    fn list(&self) -> Result<Vec<ScheduleEvent>, String>;
    /// 新增或按 id 更新
    fn upsert(&mut self, event: ScheduleEvent) -> Result<(), String>;
    /// 按 id 删除
    fn remove(&mut self, id: &str) -> Result<(), String>;
    /// 整体替换（迁移/批量导入用）
    fn replace_all(&mut self, events: Vec<ScheduleEvent>) -> Result<(), String>;
}

/// JSON 文件存储实现（{dir}/.mdgo/index_schedule.json）
#[derive(Debug, Clone)]
pub struct JsonFileStore {
    path: PathBuf,
}

/// 文件外层包装：`{"events": [...]}`
#[derive(Debug, Serialize, Deserialize)]
struct ScheduleFile {
    events: Vec<ScheduleEvent>,
}

impl JsonFileStore {
    /// dir_path：知识库根目录；数据文件位于 `{dir_path}/.mdgo/index_schedule.json`
    pub fn new(dir_path: &str) -> Self {
        let path = Path::new(dir_path).join(".mdgo").join("index_schedule.json");
        Self { path }
    }

    /// 测试用：指定精确路径
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn load(&self) -> Result<Vec<ScheduleEvent>, String> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("读取日程文件失败: {}", e))?;
        let file: ScheduleFile =
            serde_json::from_str(&raw).map_err(|e| format!("解析日程文件失败: {}", e))?;
        Ok(file.events)
    }

    fn save(&self, events: &[ScheduleEvent]) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建日程数据目录失败: {}", e))?;
        }
        let json = serde_json::to_string_pretty(&ScheduleFile {
            events: events.to_vec(),
        })
        .map_err(|e| format!("序列化日程数据失败: {}", e))?;
        // 原子写：临时文件 + rename，避免写一半损坏
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("写入日程文件失败: {}", e))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("提交日程文件失败: {}", e))?;
        Ok(())
    }
}

impl EventStore for JsonFileStore {
    fn list(&self) -> Result<Vec<ScheduleEvent>, String> {
        self.load()
    }

    fn upsert(&mut self, event: ScheduleEvent) -> Result<(), String> {
        let mut events = self.load()?;
        if let Some(existing) = events.iter_mut().find(|e| e.id == event.id) {
            *existing = event;
        } else {
            events.push(event);
        }
        self.save(&events)
    }

    fn remove(&mut self, id: &str) -> Result<(), String> {
        let mut events = self.load()?;
        events.retain(|e| e.id != id);
        self.save(&events)
    }

    fn replace_all(&mut self, events: Vec<ScheduleEvent>) -> Result<(), String> {
        self.save(&events)
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

    fn tmp_store(name: &str) -> (tempfile::TempDir, JsonFileStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonFileStore::with_path(dir.path().join(name));
        (dir, store)
    }

    #[test]
    fn missing_file_returns_empty() {
        let (_d, store) = tmp_store("missing.json");
        assert_eq!(store.list().unwrap(), Vec::<ScheduleEvent>::new());
    }

    #[test]
    fn upsert_inserts_then_updates() {
        let (_d, mut store) = tmp_store("upsert.json");
        store.upsert(sample_event("e1", "会议")).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);

        // 同 id 更新（标题变化，不新增）
        let mut updated = sample_event("e1", "评审会");
        updated.start = "2026-08-13T14:00".into();
        updated.end = "2026-08-13T15:00".into();
        store.upsert(updated).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "评审会");
        assert_eq!(list[0].start, "2026-08-13T14:00");
    }

    #[test]
    fn remove_deletes_by_id() {
        let (_d, mut store) = tmp_store("remove.json");
        store.upsert(sample_event("e1", "a")).unwrap();
        store.upsert(sample_event("e2", "b")).unwrap();
        store.remove("e1").unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "e2");
    }

    #[test]
    fn json_format_matches_legacy() {
        let (_d, mut store) = tmp_store("legacy.json");
        store.upsert(sample_event("e1", "会议")).unwrap();
        let raw = std::fs::read_to_string(store.path).unwrap();
        // 外层是 {"events": [...]}
        assert!(raw.contains("\"events\""));
        assert!(raw.contains("\"start\": \"2026-08-13T10:00\""));
        // 旧数据（前端序列化格式）可被解析
        let legacy = r#"{"events":[{"id":"x","title":"旧事件","start":"2026-01-01T09:00","end":"2026-01-01T10:00"}]}"#;
        let file: ScheduleFile = serde_json::from_str(legacy).unwrap();
        assert_eq!(file.events[0].title, "旧事件");
        assert_eq!(file.events[0].color, ""); // 缺省字段取默认
        assert_eq!(file.events[0].notify, true);
    }

    #[test]
    fn corrupt_file_returns_error() {
        let (dir, store) = tmp_store("corrupt.json");
        std::fs::write(dir.path().join("corrupt.json"), "{not json").unwrap();
        assert!(store.list().is_err());
    }
}
