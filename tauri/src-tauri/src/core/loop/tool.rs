//! 工具系统契约——对齐 DeepSeek Harness `ToolDefinition`（`docs/.../architecture-report.md` §3.1）。
//!
//! 替代 rig `DynamicTool`。设计原则（SOLID）：
//! - **接口隔离**：schema（[`ToolSpec`]）、执行（[`Tool`]）、事件出口（[`ToolEventSink`]）、
//!   注册表（[`ToolRegistry`]）分离为独立接口，调用方只依赖最小接口；
//! - **依赖倒置**：工具不感知 LLM/循环/审批，只依赖 [`ToolRunContext`] 注入的运行信息与
//!   [`ToolEventSink`]（轨迹/前端出口）；审批/技能门禁由 loop 的 Hook 层在调用前裁决；
//! - **开闭原则**：新增工具 = 实现 [`Tool`] 并注册，不改循环/调度器。
//!
//! `concurrency_safe` 语义（对齐 DSH `isConcurrencySafe`）：**只有显式声明 true 才可并行**，
//! 缺省/异常一律按 exclusive 串行（写工具副作用不可重叠）。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// 工具 schema 级契约（模型可见 + 调度元数据；对齐 DSH `ToolSchema` 白名单投影：
/// output/timeout/concurrency 永不上模型）。
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema（模型可见 `parameters`）
    pub parameters: Value,
    /// 输出契约（可选；结构化结果校验/前端类型化渲染用，永不上模型）
    pub output_schema: Option<Value>,
    /// 单次执行超时（毫秒；超过取消并记为失败，永不上模型）
    pub timeout_ms: Option<u64>,
    /// 是否可与同轮其他工具并行执行（**默认 false = exclusive 串行**）
    pub concurrency_safe: bool,
}

impl ToolSpec {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            output_schema: None,
            timeout_ms: None,
            concurrency_safe: false,
        }
    }
}

/// 工具执行结果（回填模型历史；`is_error=true` 时 `content` 为错误信息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}

/// 工具执行错误。
#[derive(Debug, Clone)]
pub enum ToolError {
    /// 工具未注册
    NotFound(String),
    /// 参数非法
    InvalidArgs(String),
    /// 执行失败（业务错误）
    Failed(String),
    /// 超时
    Timeout { timeout_ms: u64 },
    /// 已取消
    Cancelled,
    /// 内部错误
    Internal(String),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::NotFound(n) => write!(f, "工具不存在: {n}"),
            ToolError::InvalidArgs(m) => write!(f, "参数无效: {m}"),
            ToolError::Failed(m) => write!(f, "{m}"),
            ToolError::Timeout { timeout_ms } => write!(f, "执行超时（{timeout_ms}ms）"),
            ToolError::Cancelled => write!(f, "已取消"),
            ToolError::Internal(m) => write!(f, "内部错误: {m}"),
        }
    }
}

impl std::error::Error for ToolError {}

/// 工具运行上下文（只读注入；生命周期为单次调用）。
pub struct ToolRunContext<'a> {
    pub request_id: &'a str,
    pub call_id: &'a str,
    pub cancel: &'a CancellationToken,
    /// 轨迹/前端事件出口
    pub sink: &'a dyn ToolEventSink,
}

/// 工具事件出口（轨迹/前端转发；命令层实现为转发到前端协议，测试用记录实现）。
pub trait ToolEventSink: Send + Sync {
    fn on_call(&self, call_id: &str, tool: &str, args_preview: &str, args: &Value);
    fn on_result(&self, call_id: &str, tool: &str, ok: bool, summary: &str, result: Option<&str>);
}

/// 空实现（默认；未接命令层时事件丢弃）。
pub struct NullSink;

impl ToolEventSink for NullSink {
    fn on_call(&self, _call_id: &str, _tool: &str, _args_preview: &str, _args: &Value) {}
    fn on_result(&self, _call_id: &str, _tool: &str, _ok: bool, _summary: &str, _result: Option<&str>) {}
}

/// 工具抽象（对齐 DSH `ToolDefinition.execute`）。
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    /// 执行一次调用；返回**规范化的无损 JSON 值**（对齐 DSH canonical output）。
    /// 必须尊重 `ctx.cancel`（取消后尽快返回 [`ToolError::Cancelled`]）。
    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError>;
}

/// 工具注册表抽象（loop/调度器只依赖此接口）。
pub trait ToolRegistry: Send + Sync {
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>>;
    fn names(&self) -> Vec<String>;
}

/// 简单 HashMap 注册表实现。
#[derive(Default)]
pub struct HashMapToolRegistry {
    map: HashMap<String, Arc<dyn Tool>>,
}

impl HashMapToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.spec().name.clone();
        self.map.insert(name, tool);
    }
}

impl ToolRegistry for HashMapToolRegistry {
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.map.get(name).cloned()
    }

    fn names(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> &ToolSpec {
            static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
            SPEC.get_or_init(|| {
                let mut s = ToolSpec::new("echo", "回显参数", serde_json::json!({"type":"object"}));
                s.concurrency_safe = true;
                s
            })
        }

        async fn execute(&self, args: Value, _ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
            Ok(args)
        }
    }

    #[test]
    fn registry_round_trip() {
        let mut reg = HashMapToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        assert!(reg.get("echo").is_some());
        assert!(reg.get("missing").is_none());
        assert_eq!(reg.names(), vec!["echo".to_string()]);
    }
}
