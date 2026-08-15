//! Agent 后台任务状态中心（Phase 1）。
//!
//! 目标：把 `agent_query` / `kb_llm_query` 的运行状态从「前端页面内存」解耦到后端，
//! 支持「切出页面任务继续、切回页面经快照恢复视图」。
//!
//! 设计约束（SOLID）：
//! - **SRP**：本模块只负责任务状态的存储与快照；任务执行仍由 `agent_query` 编排。
//! - **OCP**：新增事件类型只需增加一个 `record_*` 方法，不修改核心生成循环。
//! - **DIP**：`commands` 层依赖本模块的窄接口（`register`/`record_*`/`get`/`list`）；
//!   本模块不依赖任何 command 类型（sources / trace 事件以 `serde_json::Value` 解耦）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// 任务状态（生命周期终态保留快照，供切回页面恢复查看）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentTaskStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl AgentTaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentTaskStatus::Running => "running",
            AgentTaskStatus::Done => "done",
            AgentTaskStatus::Failed => "failed",
            AgentTaskStatus::Cancelled => "cancelled",
        }
    }
}

/// 工具调用轨迹（对齐前端 `agent:tool_call` / `agent:tool_result` 卡片）。
#[derive(Debug, Clone, Serialize)]
pub struct AgentToolTrace {
    /// 调用序号（同请求内递增）
    pub seq: u64,
    pub tool: String,
    pub args_preview: String,
    /// `None` = 执行中；`Some(true/false)` = 已出结果
    pub ok: Option<bool>,
    pub summary: String,
    /// 触发技能 ID（scope:skill_id，工具轨迹溯源）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_id: Option<String>,
}

/// 后台 Agent 任务快照（前端 `agent_task_get` 返回的完整状态）。
#[derive(Debug, Clone, Serialize)]
pub struct BackgroundAgentTask {
    pub request_id: String,
    /// 关联会话 ID（chat/rag 会话；可能为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// 知识库目录
    pub dir_path: String,
    /// 模式：rag（Agent 编排）/ chat（普通对话）
    pub mode: String,
    pub status: AgentTaskStatus,
    /// 累积的回复文本（增量拼接，切回页面据此重建流式 DOM）
    pub content: String,
    /// 检索来源快照（rag:done 时写入；与前端引用列表结构一致）
    pub sources: Vec<serde_json::Value>,
    /// 工具调用轨迹（按 seq 有序）
    pub tool_traces: Vec<AgentToolTrace>,
    /// trace 阶段事件快照（planning/searching/generating…）
    pub trace_events: Vec<serde_json::Value>,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// 运行状态文本（rag:status，如"正在检索…"）
    pub status_message: String,
    /// 本次请求的用户消息原文（切回页面恢复用户消息显示用）
    pub user_message: String,
    /// 创建时间（Unix 毫秒）
    pub created_at: u64,
    /// 最后更新时间（Unix 毫秒）
    pub updated_at: u64,
    /// 结束时间（终态非空）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    /// 内部治理时间戳（不序列化）
    #[serde(skip)]
    last_active: Instant,
}

/// 任务注册表：`request_id` → 任务快照（进程内，任务生命周期由请求显式收尾）。
///
/// 容量治理：
/// - 总快照上限 `MAX_TRACKED_TASKS`（对齐 TraceBus/ToolCallBus 的 64 桶），
///   超限时淘汰最旧的**终态**任务（running 永不淘汰，保证后台任务不被静默丢弃）；
/// - 终态任务保留 `RETAIN_DONE` 时长后，由写路径惰性清理。
pub struct AgentTaskStore {
    inner: Mutex<HashMap<String, BackgroundAgentTask>>,
}

/// 总快照上限
const MAX_TRACKED_TASKS: usize = 64;
/// 终态任务保留时长（30 分钟；之后惰性清理）
const RETAIN_DONE: Duration = Duration::from_secs(30 * 60);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Default for AgentTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentTaskStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 注册一个新任务（running 态）。同 request_id 重复注册覆盖旧快照。
    pub fn register(
        &self,
        request_id: &str,
        session_id: Option<String>,
        dir_path: &str,
        mode: &str,
        user_message: &str,
    ) {
        let ts = now_ms();
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(
            request_id.to_string(),
            BackgroundAgentTask {
                request_id: request_id.to_string(),
                session_id,
                dir_path: dir_path.to_string(),
                mode: mode.to_string(),
                status: AgentTaskStatus::Running,
                content: String::new(),
                sources: Vec::new(),
                tool_traces: Vec::new(),
                trace_events: Vec::new(),
                prompt_tokens: 0,
                completion_tokens: 0,
                status_message: String::new(),
                user_message: user_message.to_string(),
                created_at: ts,
                updated_at: ts,
                finished_at: None,
                last_active: Instant::now(),
            },
        );
        // 容量治理在 insert 之后执行：淘汰最旧终态至上限（新任务为 running，
        // 不会被淘汰；running 任务永不静默丢弃）
        self.evict(&mut map);
    }

    /// 追加文本增量（rag:delta / llm:delta）。
    pub fn append_text(&self, request_id: &str, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Ok(mut map) = self.inner.lock() {
            if let Some(t) = map.get_mut(request_id) {
                t.content.push_str(delta);
                t.updated_at = now_ms();
                t.last_active = Instant::now();
            }
        }
    }

    /// 覆盖检索来源快照（rag:done 前调用，含完整来源列表）。
    pub fn set_sources(&self, request_id: &str, sources: Vec<serde_json::Value>) {
        if let Ok(mut map) = self.inner.lock() {
            if let Some(t) = map.get_mut(request_id) {
                t.sources = sources;
                t.updated_at = now_ms();
            }
        }
    }

    /// 更新运行状态文本（rag:status）。
    pub fn set_status_message(&self, request_id: &str, message: &str) {
        if let Ok(mut map) = self.inner.lock() {
            if let Some(t) = map.get_mut(request_id) {
                t.status_message = message.to_string();
                t.updated_at = now_ms();
            }
        }
    }

    /// 记录工具调用开始（agent:tool_call）。
    pub fn add_tool_call(&self, request_id: &str, seq: u64, tool: &str, args_preview: &str, skill_id: Option<String>) {
        if let Ok(mut map) = self.inner.lock() {
            if let Some(t) = map.get_mut(request_id) {
                t.tool_traces.push(AgentToolTrace {
                    seq,
                    tool: tool.to_string(),
                    args_preview: args_preview.to_string(),
                    ok: None,
                    summary: String::new(),
                    skill_id,
                });
                t.updated_at = now_ms();
                t.last_active = Instant::now();
            }
        }
    }

    /// 记录工具调用结果（agent:tool_result）：按 seq 回填 ok/summary。
    pub fn update_tool_result(&self, request_id: &str, seq: u64, ok: bool, summary: &str) {
        if let Ok(mut map) = self.inner.lock() {
            if let Some(t) = map.get_mut(request_id) {
                if let Some(tr) = t.tool_traces.iter_mut().find(|tr| tr.seq == seq) {
                    tr.ok = Some(ok);
                    tr.summary = summary.to_string();
                }
                t.updated_at = now_ms();
                t.last_active = Instant::now();
            }
        }
    }

    /// 追加 trace 阶段事件快照（trace:event 内容，独立存储便于恢复阶段面板）。
    pub fn add_trace_event(&self, request_id: &str, event: serde_json::Value) {
        if let Ok(mut map) = self.inner.lock() {
            if let Some(t) = map.get_mut(request_id) {
                t.trace_events.push(event);
                t.updated_at = now_ms();
            }
        }
    }

    /// 更新 token 用量（rag:done / llm:usage）。
    pub fn set_usage(&self, request_id: &str, prompt_tokens: u32, completion_tokens: u32) {
        if let Ok(mut map) = self.inner.lock() {
            if let Some(t) = map.get_mut(request_id) {
                t.prompt_tokens = prompt_tokens;
                t.completion_tokens = completion_tokens;
                t.updated_at = now_ms();
            }
        }
    }

    /// 任务收尾：置终态（done/failed/cancelled），保留快照供恢复查看。
    pub fn finish(&self, request_id: &str, status: AgentTaskStatus) {
        let ts = now_ms();
        if let Ok(mut map) = self.inner.lock() {
            if let Some(t) = map.get_mut(request_id) {
                t.status = status;
                t.updated_at = ts;
                t.finished_at = Some(ts);
                t.last_active = Instant::now();
            }
        }
    }

    /// 移除任务（显式清理；终态保留期内通常不调用）。
    pub fn remove(&self, request_id: &str) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(request_id);
        }
    }

    /// 查询单个任务快照（切回页面恢复视图用）。
    pub fn get(&self, request_id: &str) -> Option<BackgroundAgentTask> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(request_id)
            .cloned()
    }

    /// 列出全部任务快照（按更新时间倒序，活动任务在前）。
    pub fn list(&self) -> Vec<BackgroundAgentTask> {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut tasks: Vec<BackgroundAgentTask> = map.values().cloned().collect();
        tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        tasks
    }

    /// 惰性容量治理：淘汰超期终态任务；快照数超上限时淘汰最旧终态。
    fn evict(&self, map: &mut HashMap<String, BackgroundAgentTask>) {
        // 1. 淘汰超过保留期的终态任务
        let now = Instant::now();
        let expired: Vec<String> = map
            .iter()
            .filter(|(_, t)| {
                t.status != AgentTaskStatus::Running && now.duration_since(t.last_active) > RETAIN_DONE
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired {
            map.remove(&k);
        }
        // 2. 超总上限时淘汰最旧的终态任务（running 不淘汰）
        if map.len() > MAX_TRACKED_TASKS {
            let mut finished: Vec<(String, u64)> = map
                .iter()
                .filter(|(_, t)| t.status != AgentTaskStatus::Running)
                .map(|(k, t)| (k.clone(), t.updated_at))
                .collect();
            finished.sort_by(|a, b| a.1.cmp(&b.1));
            // 注意：不能用 finished.drain(..).next()——drain 会移除并清空整个范围，
            // 取首元素后其余被丢弃，导致只淘汰一个。用 remove(0) 逐旧淘汰。
            while map.len() > MAX_TRACKED_TASKS && !finished.is_empty() {
                let (oldest, _) = finished.remove(0);
                map.remove(&oldest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_get_and_append() {
        let s = AgentTaskStore::new();
        s.register("r1", Some("s1".into()), "/tmp", "rag", "用户消息");
        assert_eq!(s.get("r1").unwrap().status, AgentTaskStatus::Running);
        s.append_text("r1", "你好");
        s.append_text("r1", "，世界");
        assert_eq!(s.get("r1").unwrap().content, "你好，世界");
        assert_eq!(s.get("r1").unwrap().mode, "rag");
        assert_eq!(s.get("r1").unwrap().session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn tool_trace_call_then_result() {
        let s = AgentTaskStore::new();
        s.register("r1", None, "/tmp", "rag", "用户消息");
        s.add_tool_call("r1", 1, "read", "docs/a.md", None);
        assert!(s.get("r1").unwrap().tool_traces[0].ok.is_none());
        s.update_tool_result("r1", 1, true, "2 字符");
        let tr = &s.get("r1").unwrap().tool_traces[0];
        assert_eq!(tr.ok, Some(true));
        assert_eq!(tr.summary, "2 字符");
        assert_eq!(tr.tool, "read");
    }

    #[test]
    fn finish_marks_terminal_and_keeps_snapshot() {
        let s = AgentTaskStore::new();
        s.register("r1", None, "/tmp", "rag", "用户消息");
        s.append_text("r1", "部分内容");
        s.finish("r1", AgentTaskStatus::Cancelled);
        let t = s.get("r1").unwrap();
        assert_eq!(t.status, AgentTaskStatus::Cancelled);
        assert!(t.finished_at.is_some());
        assert_eq!(t.content, "部分内容"); // 终态保留累积文本
        assert_eq!(t.status.as_str(), "cancelled");
    }

    #[test]
    fn remove_drops_task() {
        let s = AgentTaskStore::new();
        s.register("r1", None, "/tmp", "chat", "用户消息");
        assert!(s.get("r1").is_some());
        s.remove("r1");
        assert!(s.get("r1").is_none());
    }

    #[test]
    fn list_sorts_by_updated_desc() {
        let s = AgentTaskStore::new();
        s.register("older", None, "/tmp", "rag", "用户消息");
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.register("newer", None, "/tmp", "rag", "用户消息");
        let list = s.list();
        assert_eq!(list[0].request_id, "newer");
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn evicts_expired_finished_but_keeps_running() {
        let s = AgentTaskStore::new();
        s.register("run", None, "/tmp", "rag", "用户消息");
        s.register("done", None, "/tmp", "rag", "用户消息");
        s.finish("done", AgentTaskStatus::Done);
        s.register("fail", None, "/tmp", "rag", "用户消息");
        s.finish("fail", AgentTaskStatus::Failed);
        let map = s.inner.lock().unwrap_or_else(|e| e.into_inner());
        assert!(map.contains_key("run"));
        assert!(map.contains_key("done"));
        assert!(map.contains_key("fail"));
        drop(map);
        // 通过大量注册触发淘汰：只淘汰最旧终态，running 保留
        for i in 0..MAX_TRACKED_TASKS + 10 {
            s.register(&format!("t{}", i), None, "/tmp", "rag", "用户消息");
            s.finish(&format!("t{}", i), AgentTaskStatus::Done);
        }
        let map = s.inner.lock().unwrap_or_else(|e| e.into_inner());
        assert!(map.len() <= MAX_TRACKED_TASKS);
        assert!(map.contains_key("run"), "running 任务不应被淘汰");
    }
}
