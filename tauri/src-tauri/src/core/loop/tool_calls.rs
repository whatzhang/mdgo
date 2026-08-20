//! 工具并行调度器——对齐 DeepSeek Harness `tool-calls.ts`（exclusive barrier + 有界滚动池 +
//! 模型序提交）。
//!
//! 两阶段：
//! 1. **ordered pre**：按模型序依次跑 Hook 的 `on_tool_call` 裁决（技能门禁/审批/熔断），
//!    任一返回 Skip 即短路，结果以错误形式回填（不执行）；
//! 2. **concurrent execute**：通过裁决的调用按 `concurrency_safe` 分组——exclusive 单独成
//!    屏障串行；concurrency_safe 调用走 `chunks(max_parallel)` 有界并行——结果**严格按模型序**
//!    提交（`out[i]` 按下标填充，顺序天然保持）。
//!
//! 取消：取消后未启动的调用产出 `ToolError::Cancelled` 结果（保回放完整），已启动的
//! drain 到 quiescence（工具尊重 `ctx.cancel`）。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::hooks::{HookCtx, LoopHook, ToolDecision};
use super::tool::{ToolError, ToolEventSink, ToolRegistry, ToolResult, ToolRunContext};
use super::types::ToolCall;

/// 一次工具执行的完整记录（调用 + 结果，模型序）。
#[derive(Debug, Clone)]
pub struct ToolExecution {
    pub call: ToolCall,
    pub result: ToolResult,
}

/// 有界并行的默认上限。
pub const DEFAULT_MAX_PARALLEL: usize = 4;

/// 执行一批工具调用（`calls` 为模型序）。
pub async fn execute_tool_calls(
    tools: &dyn ToolRegistry,
    calls: Vec<ToolCall>,
    hooks: &[Arc<dyn LoopHook>],
    hook_ctx: &HookCtx,
    request_id: &str,
    cancel: CancellationToken,
    sink: &dyn ToolEventSink,
    max_parallel: usize,
) -> Vec<ToolExecution> {
    let n = calls.len();
    let mut out: Vec<Option<ToolExecution>> = vec![None; n];

    // ── Phase 1：ordered pre（Hook 裁决，模型序短路）──
    let mut decisions: Vec<ToolDecision> = Vec::with_capacity(n);
    for call in &calls {
        let args = parse_args(&call.arguments);
        let mut decision = ToolDecision::Run;
        for h in hooks {
            match h.on_tool_call(hook_ctx, &call.name, &args).await {
                ToolDecision::Skip(reason) => {
                    decision = ToolDecision::Skip(reason);
                    break;
                }
                ToolDecision::Run => {}
            }
        }
        decisions.push(decision);
    }

    // ── Phase 2a：Skip 调用立即回填（模型序）──
    for (i, call) in calls.iter().enumerate() {
        if let ToolDecision::Skip(reason) = &decisions[i] {
            let result = ToolResult {
                call_id: call.id.clone(),
                content: format!("（被拦截）{reason}"),
                is_error: true,
            };
            sink.on_result(&call.id, &call.name, false, &result.content, Some(&result.content));
            out[i] = Some(ToolExecution { call: call.clone(), result });
        }
    }

    // ── Phase 2b：通过裁决的调用按并发安全分组 → 有界并行 → 模型序提交 ──
    let run_indices: Vec<usize> = (0..n).filter(|&i| matches!(decisions[i], ToolDecision::Run)).collect();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for &i in &run_indices {
        let safe = tool_concurrency_safe(tools, &calls[i].name);
        let can_join = groups
            .last()
            .is_some_and(|g| tool_concurrency_safe(tools, &calls[*g.first().unwrap_or(&i)].name));
        if safe && can_join {
            groups.last_mut().expect("last 存在").push(i);
        } else {
            groups.push(vec![i]);
        }
    }
    for group in groups {
        // 有界滚动池：按 max_parallel 分窗，窗内并行，窗口串行（结果按模型序提交）
        for chunk in group.chunks(max_parallel.max(1)) {
            let futs: Vec<_> = chunk
                .iter()
                .map(|&i| {
                    let call = calls[i].clone();
                    let cancel = cancel.clone();
                    run_one(tools, call, request_id, cancel, sink)
                })
                .collect();
            let results = futures::future::join_all(futs).await;
            for (k, &i) in chunk.iter().enumerate() {
                out[i] = Some(results[k].clone());
            }
        }
    }

    out.into_iter().map(|o| o.expect("每个调用必已填充")).collect()
}

/// 执行单个调用（Hook 已裁决为 Run）。
async fn run_one(
    tools: &dyn ToolRegistry,
    call: ToolCall,
    request_id: &str,
    cancel: CancellationToken,
    sink: &dyn ToolEventSink,
) -> ToolExecution {
    let args = parse_args(&call.arguments);
    let args_preview = preview(&args);
    sink.on_call(&call.id, &call.name, &args_preview, &args);

    let Some(tool) = tools.get(&call.name) else {
        let result = ToolResult {
            call_id: call.id.clone(),
            content: format!("工具不存在: {}", call.name),
            is_error: true,
        };
        sink.on_result(&call.id, &call.name, false, &result.content, Some(&result.content));
        return ToolExecution { call, result };
    };

    let spec = tool.spec();
    let ctx = ToolRunContext { request_id, call_id: &call.id, cancel: &cancel, sink };
    let outcome = match spec.timeout_ms {
        Some(t) => tokio::select! {
            _ = cancel.cancelled() => Err(ToolError::Cancelled),
            r = tokio::time::timeout(std::time::Duration::from_millis(t), tool.execute(args, &ctx)) => {
                match r {
                    Ok(Ok(v)) => Ok(v),
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err(ToolError::Timeout { timeout_ms: t }),
                }
            }
        },
        None => tokio::select! {
            _ = cancel.cancelled() => Err(ToolError::Cancelled),
            r = tool.execute(args, &ctx) => r,
        },
    };

    match outcome {
        Ok(v) => {
            let call_id = call.id.clone();
            let content = value_to_content(&v);
            let summary = preview(&v);
            sink.on_result(&call_id, &call.name, true, &summary, Some(&content));
            ToolExecution {
                call,
                result: ToolResult { call_id, content, is_error: false },
            }
        }
        Err(e) => {
            let call_id = call.id.clone();
            let msg = e.to_string();
            sink.on_result(&call_id, &call.name, false, &msg, Some(&msg));
            ToolExecution {
                call,
                result: ToolResult { call_id, content: msg, is_error: true },
            }
        }
    }
}

fn parse_args(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| Value::Null)
}

fn preview(v: &Value) -> String {
    let s = if v.is_string() {
        v.as_str().unwrap_or("").to_string()
    } else {
        v.to_string()
    };
    if s.chars().count() > 120 {
        let mut t: String = s.chars().take(120).collect();
        t.push('…');
        t
    } else {
        s
    }
}

/// 规范化的结果内容：字符串值原样，其余 JSON 序列化。
fn value_to_content(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        s.to_string()
    } else {
        v.to_string()
    }
}

fn tool_concurrency_safe(tools: &dyn ToolRegistry, name: &str) -> bool {
    tools.get(name).is_some_and(|t| t.spec().concurrency_safe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::r#loop::tool::{HashMapToolRegistry, Tool, ToolSpec};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockTool {
        spec: ToolSpec,
        delay_ms: u64,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        async fn execute(&self, args: Value, _ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            Ok(args)
        }
    }

    fn mock_tool(name: &'static str, safe: bool, delay_ms: u64) -> (Arc<MockTool>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut spec = ToolSpec::new(name, "mock", serde_json::json!({"type": "object"}));
        spec.concurrency_safe = safe;
        (
            Arc::new(MockTool { spec, delay_ms, calls: calls.clone() }),
            calls,
        )
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall { id: id.into(), name: name.into(), arguments: "{\"path\":\"a\"}".into() }
    }

    fn ctx() -> HookCtx {
        HookCtx::new(1, 1, "m", "r", 10)
    }

    #[tokio::test]
    async fn exclusive_tools_serial_and_ordered() {
        let (t1, c1) = mock_tool("read_a", true, 0);
        let (t2, c2) = mock_tool("read_b", true, 0);
        let mut reg = HashMapToolRegistry::new();
        reg.register(t1);
        reg.register(t2);
        let calls = vec![call("c1", "read_a"), call("c2", "read_b")];
        let hook_ctx = ctx();
        let sink = super::super::tool::NullSink;
        let out = execute_tool_calls(
            &reg, calls, &[], &hook_ctx, "r", CancellationToken::new(), &sink, 4,
        )
        .await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].call.id, "c1");
        assert_eq!(out[1].call.id, "c2");
        assert!(!out[0].result.is_error);
        assert_eq!(c1.load(Ordering::SeqCst), 1);
        assert_eq!(c2.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_result() {
        let reg = HashMapToolRegistry::new();
        let hook_ctx = ctx();
        let out = execute_tool_calls(
            &reg,
            vec![call("c1", "nope")],
            &[],
            &hook_ctx,
            "r",
            CancellationToken::new(),
            &super::super::tool::NullSink,
            4,
        )
        .await;
        assert!(out[0].result.is_error);
        assert!(out[0].result.content.contains("不存在"));
    }

    #[tokio::test]
    async fn hook_skip_short_circuits() {
        struct DenyHook;
        #[async_trait]
        impl LoopHook for DenyHook {
            async fn on_tool_call(&self, _ctx: &HookCtx, name: &str, _args: &Value) -> ToolDecision {
                if name == "secret" {
                    ToolDecision::Skip("该工具不可用".into())
                } else {
                    ToolDecision::Run
                }
            }
        }
        let (t1, calls) = mock_tool("secret", false, 0);
        let mut reg = HashMapToolRegistry::new();
        reg.register(t1);
        let hook_ctx = ctx();
        let out = execute_tool_calls(
            &reg,
            vec![call("c1", "secret")],
            &[Arc::new(DenyHook)],
            &hook_ctx,
            "r",
            CancellationToken::new(),
            &super::super::tool::NullSink,
            4,
        )
        .await;
        assert!(out[0].result.is_error);
        assert!(out[0].result.content.contains("被拦截"));
        assert_eq!(calls.load(Ordering::SeqCst), 0); // 未执行
    }
}
