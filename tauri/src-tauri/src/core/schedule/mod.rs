//! 日程引擎（逻辑权威 · SOLID 分层）。
//!
//! # 设计
//!
//! - [`ScheduleEvent`]：日程事件领域模型，字段与前端数据逐一对齐
//!   （id/title/start/end/color/desc/cron/notify/created_at/updated_at），时间格式 `YYYY-MM-DDTHH:MM`。
//! - [`store::EventStore`]：存储抽象（trait，接口隔离 + 依赖倒置），上层只依赖此接口；
//! - [`sqlite::SqliteStore`]：SQLite 持久化实现（**知识库级统一数据库** `{知识库}/.mdgo/mdgo.db`，
//!   与 memory/prompts 共用；`dir_path` 列做知识库隔离，WAL，全参数化 SQL）。
//! - [`rules`]：纯函数规则引擎（Cron 展开 / 冲突检测 / 提醒计算 / 时间校验）。
//! - [`lunar`]：农历 / 节假日 / 调休信息提供（DayInfoProvider trait）。
//!
//! 依赖倒置：IPC 命令层与 Agent 工具只依赖 [`store::EventStore`] / [`lunar::DayInfoProvider`]
//! 接口；存储路径、节假日数据源、当前时间均由调用方注入。

pub mod analyze;
pub mod lunar;
pub mod planner;
pub mod rules;
pub mod scheduler;
pub mod sqlite;
pub mod store;

use serde::{Deserialize, Serialize};

/// 日程关联的知识库对象（知识图谱联动：文档 / 任务 / Git 提交）
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RelatedLinks {
    /// 关联文档（知识库内相对路径，如 "project/rag.md"）
    #[serde(default)]
    pub docs: Vec<String>,
    /// 关联任务（看板任务 id / 标题）
    #[serde(default)]
    pub tasks: Vec<String>,
    /// 关联 Git 提交 / 分支
    #[serde(default)]
    pub git: Vec<String>,
}

/// AI 规划元数据（由 AI 排期时写入，供时间分析 / 复盘使用）
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AiMeta {
    /// 任务类别（如 development / meeting / study，用于 optimize 聚合）
    #[serde(default)]
    pub category: String,
    /// 精力类型（deep_work / shallow / rest，用于安排最佳工作时间）
    #[serde(default)]
    pub energy: String,
    /// 预估投入小时数（plan 拆解产出）
    #[serde(default)]
    pub estimated_hours: f64,
}

/// 日程事件（持久化视图，字段与前端 JSON 逐一对齐）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ScheduleEvent {
    /// 事件唯一 ID
    pub id: String,
    /// 事件标题
    pub title: String,
    /// 开始时间（本地时间 `YYYY-MM-DDTHH:MM`）
    pub start: String,
    /// 结束时间（本地时间 `YYYY-MM-DDTHH:MM`）
    pub end: String,
    /// 颜色标记（前端日历展示用；如 "blue"）
    #[serde(default = "default_color")]
    pub color: String,
    /// 描述
    #[serde(default)]
    pub desc: String,
    /// Cron 表达式（5 字段）；空字符串表示不重复
    #[serde(default)]
    pub cron: String,
    /// 是否提醒
    #[serde(default = "default_true")]
    pub notify: bool,
    /// 提前提醒分钟数（0 = 开始即提醒；如 10 = 开始前 10 分钟提醒）
    #[serde(default)]
    pub notify_before: i64,
    /// 事件类型（work/meeting/focus/personal/task 等，供时间分析聚合）
    ///
    /// 统一 JSON 键为 `event_type`（前端全程使用该键）；`alias="type"` 兼容早期
    /// `rename="type"` 的存量数据/调用方，避免字段名漂移导致校验与展示失效。
    #[serde(default, alias = "type")]
    pub event_type: String,
    /// 优先级（high/medium/low）
    #[serde(default)]
    pub priority: String,
    /// 关联的知识库对象（文档 / 任务 / Git）
    #[serde(default)]
    pub related: RelatedLinks,
    /// AI 规划元数据（category / energy / estimated_hours）
    #[serde(default)]
    pub ai: AiMeta,
    /// 创建时间（`YYYY-MM-DDTHH:MM`）
    #[serde(default)]
    pub created_at: String,
    /// 更新时间（`YYYY-MM-DDTHH:MM`）
    #[serde(default)]
    pub updated_at: String,
}

fn default_true() -> bool {
    true
}

/// 日程默认颜色（前端日历蓝；不传 color 时缺省 blue）
fn default_color() -> String {
    "blue".into()
}

impl ScheduleEvent {
    /// 基本校验：标题非空、起止时间可解析、结束不早于开始
    pub fn validate(&self) -> Result<(), String> {
        if self.title.trim().is_empty() {
            return Err("日程标题不能为空".into());
        }
        let start = rules::parse_local_time(&self.start)
            .ok_or_else(|| format!("开始时间格式无效: {}", self.start))?;
        let end = rules::parse_local_time(&self.end)
            .ok_or_else(|| format!("结束时间格式无效: {}", self.end))?;
        // 提醒（event_type=reminder）为单点时间：允许 start == end；其余类型必须 end > start
        if end < start || (end == start && self.event_type != "reminder") {
            return Err("结束时间必须晚于开始时间".into());
        }
        Ok(())
    }
}

/// 事件输入（IPC / 工具层反序列化；id/created_at/updated_at 由引擎生成）
#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleEventInput {
    pub title: String,
    pub start: String,
    pub end: String,
    #[serde(default = "default_color")]
    pub color: String,
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub cron: String,
    #[serde(default = "default_true")]
    pub notify: bool,
    /// 提前提醒分钟数（0 = 开始即提醒）
    #[serde(default)]
    pub notify_before: i64,
    /// 事件类型（work/meeting/focus/personal/task 等）
    ///
    /// 统一 JSON 键为 `event_type`（与前端一致）；`alias="type"` 兼容早期 `rename="type"`
    /// 的存量数据/调用方，避免字段名漂移导致校验（如单点提醒 start==end）与展示失效。
    #[serde(default, alias = "type")]
    pub event_type: String,
    /// 优先级（high/medium/low）
    #[serde(default)]
    pub priority: String,
    /// 关联的知识库对象
    #[serde(default)]
    pub related: RelatedLinks,
    /// AI 规划元数据
    #[serde(default)]
    pub ai: AiMeta,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_color_defaults_to_blue() {
        // 不传 color（IPC / 工具层缺省）→ 默认 blue
        let input: ScheduleEventInput =
            serde_json::from_str(r#"{"title":"会议","start":"2026-08-18T14:00","end":"2026-08-18T15:00"}"#)
                .unwrap();
        assert_eq!(input.color, "blue");
        // 显式传 color 时保留
        let input: ScheduleEventInput = serde_json::from_str(
            r#"{"title":"会议","start":"2026-08-18T14:00","end":"2026-08-18T15:00","color":"red"}"#,
        )
        .unwrap();
        assert_eq!(input.color, "red");
    }

    #[test]
    fn event_color_defaults_to_blue() {
        // ScheduleEvent 反序列化缺 color → blue（与前端 todo-blue 类名对齐）
        let e: ScheduleEvent =
            serde_json::from_str(r#"{"id":"e1","title":"会议","start":"2026-08-18T14:00","end":"2026-08-18T15:00"}"#)
                .unwrap();
        assert_eq!(e.color, "blue");
    }

    #[test]
    fn validate_allows_reminder_single_point() {
        // 提醒（event_type=reminder）为单点时间：允许 start == end
        let e = ScheduleEvent {
            id: "r1".into(),
            title: "吃药提醒".into(),
            start: "2026-08-18T14:00".into(),
            end: "2026-08-18T14:00".into(),
            event_type: "reminder".into(),
            ..Default::default()
        };
        assert!(e.validate().is_ok());
        // 非提醒类型仍要求 end > start
        let bad = ScheduleEvent { event_type: "work".into(), ..e.clone() };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn event_type_json_key_is_event_type_with_type_alias() {
        // 前端全程使用 event_type 键：反序列化必须正确识别，否则单点提醒校验（start==end）会误报
        let input: ScheduleEventInput = serde_json::from_str(
            r#"{"title":"提醒","start":"2026-08-18T14:00","end":"2026-08-18T14:00","event_type":"reminder"}"#,
        )
        .unwrap();
        assert_eq!(input.event_type, "reminder", "event_type 键必须反序列化成功");
        // 兼容早期 rename="type" 的存量调用方
        let legacy: ScheduleEventInput = serde_json::from_str(
            r#"{"title":"提醒","start":"2026-08-18T14:00","end":"2026-08-18T14:00","type":"reminder"}"#,
        )
        .unwrap();
        assert_eq!(legacy.event_type, "reminder", "type 键（旧契约）也应兼容");
        // 序列化输出 event_type 键（前端 todoE.event_type 读取依赖此键）
        let e = ScheduleEvent {
            id: "r1".into(),
            title: "提醒".into(),
            start: "2026-08-18T14:00".into(),
            end: "2026-08-18T14:00".into(),
            event_type: "reminder".into(),
            ..Default::default()
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["event_type"], "reminder", "序列化键应为 event_type");
        assert!(json.get("type").is_none(), "不应再输出旧键 type");
    }
}
