//! 结构化 trace：request_id 贯穿的调试事件总线。
//!
//! 与 `ToolCallBus` 同构：按 request_id 分桶、容量治理、消费式 `drain`。
//! agent_query / kb_llm_query / subagent 各阶段写入结构化事件，
//! 命令层经 `trace:event` 转发前端按 request_id 渲染（可折叠阶段列表）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// 全局总线跟踪的并发请求桶上限：超过后清空最旧（trace 是辅助展示，丢失无害）。
const MAX_TRACKED_TRACES: usize = 64;

/// 单条 trace 事件。
#[derive(Debug, Clone, Serialize)]
pub struct TraceEvent {
    /// 单调递增序号（事件顺序）
    pub seq: u64,
    /// 阶段名：planning / expanding / searching / aggregating / generating / llm
    pub stage: String,
    /// 状态：start / ok / error / cancelled / denied
    pub status: String,
    /// 阶段耗时（毫秒；start 事件为 0）
    pub duration_ms: u64,
    /// 补充信息（如 token 数、拒绝原因、命中数）
    pub detail: String,
    /// 事件时间戳（Unix 毫秒）
    pub ts_ms: u64,
}

/// 按 `request_id` 记录结构化事件的全局总线。
///
/// 工具/阶段闭包在 Rig 流式内部执行，无法直接访问 Tauri 事件发射器，
/// 先写入本总线，由命令层按请求 drain 并经 `trace:event` 转发。
pub struct TraceBus {
    seq: AtomicU64,
    map: Mutex<HashMap<String, Vec<TraceEvent>>>,
}

impl TraceBus {
    fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            map: Mutex::new(HashMap::new()),
        }
    }

    /// 写入一条事件（容量治理：超过上限清空最旧桶）。
    pub fn record(
        &self,
        request_id: &str,
        stage: &str,
        status: &str,
        duration_ms: u64,
        detail: &str,
    ) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut map) = self.map.lock() {
            if map.len() >= MAX_TRACKED_TRACES {
                map.clear();
            }
            let ts_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            map.entry(request_id.to_string())
                .or_default()
                .push(TraceEvent {
                    seq,
                    stage: stage.into(),
                    status: status.into(),
                    duration_ms,
                    detail: detail.into(),
                    ts_ms,
                });
        }
    }

    /// 消费式取出该请求尚未转发的事件，并清理空桶。
    pub fn drain(&self, request_id: &str) -> Vec<TraceEvent> {
        let mut out = Vec::new();
        if let Ok(mut map) = self.map.lock() {
            if let Some(events) = map.get_mut(request_id) {
                out = std::mem::take(events);
            }
            if map.get(request_id).map(|v| v.is_empty()).unwrap_or(true) {
                map.remove(request_id);
            }
        }
        out
    }

    /// 清空该请求的残留事件（请求结束兜底，防内存累积）。
    pub fn clear(&self, request_id: &str) {
        if let Ok(mut map) = self.map.lock() {
            map.remove(request_id);
        }
    }
}

static TRACE_BUS: OnceLock<TraceBus> = OnceLock::new();

/// 获取全局 trace 总线单例。
pub fn trace_bus() -> &'static TraceBus {
    TRACE_BUS.get_or_init(TraceBus::new)
}

/// 便捷：记录"阶段开始"事件。
pub fn stage_start(request_id: &str, stage: &str, detail: &str) {
    trace_bus().record(request_id, stage, "start", 0, detail);
}

/// 便捷：记录"阶段结束"事件。
pub fn stage_end(request_id: &str, stage: &str, status: &str, duration_ms: u64, detail: &str) {
    trace_bus().record(request_id, stage, status, duration_ms, detail);
}