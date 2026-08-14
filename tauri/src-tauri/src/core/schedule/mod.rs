//! 日程引擎（逻辑权威 · SOLID 分层）。
//!
//! # 设计
//!
//! - [`ScheduleEvent`]：日程事件领域模型，字段与前端 `index_schedule.json` 逐一对齐
//!   （id/title/start/end/color/desc/cron/notify/created_at/updated_at），时间格式 `YYYY-MM-DDTHH:MM`。
//! - [`store::EventStore`]：存储抽象（trait），`store::JsonFileStore` 为 JSON 文件实现
//!   （读写 `{dir}/.mdgo/index_schedule.json`，原子写），可被 SQLite 实现替换而不改上层。
//! - [`rules`]：纯函数规则引擎（Cron 展开 / 冲突检测 / 提醒计算 / 时间校验）。
//! - [`lunar`]：农历 / 节假日 / 调休信息提供（DayInfoProvider trait）。
//!
//! 依赖倒置：IPC 命令层与 Agent 工具只依赖 [`store::EventStore`] / [`lunar::DayInfoProvider`]
//! 接口；存储路径、节假日数据源、当前时间均由调用方注入。

pub mod lunar;
pub mod planner;
pub mod rules;
pub mod scheduler;
pub mod store;

use serde::{Deserialize, Serialize};

/// 日程事件（持久化视图，字段与前端 JSON 逐一对齐）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    #[serde(default)]
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
        if end <= start {
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
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub cron: String,
    #[serde(default = "default_true")]
    pub notify: bool,
}
