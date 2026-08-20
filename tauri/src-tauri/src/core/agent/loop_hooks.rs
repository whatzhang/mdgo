//! 业务 Hook 的新内核实现——直接重构 rig `AgentHook` → `core/loop::LoopHook`（"直接重构，不做桥接"）。
//!
//! - [`SkillGateHook`]：工具放行裁决（BASE_TOOLS + 激活技能声明 + allow_extra）+ 重复调用熔断
//!   （对齐 rig `SkillGateHook` 语义；`active_tools` 可见性窄化属 pre_request 职责，Phase 4/5 接入）；
//! - [`ApprovalHook`]：写工具审批门（[`ApprovalGate::check`]，fail-closed + 分类反馈文案
//!   与 rig `approval::hook` 一致，消除模型文本式确认）。
//!
//! 挂载顺序（与 rig 一致）：先技能门禁、后审批——避免对"本就不该调用的工具"弹窗打扰用户。

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::core::approval::{ApprovalDenial, ApprovalGate, DenialCategory};
use crate::core::r#loop::{HookCtx, LlmMessage, LoopHook, RequestPatch, ToolDecision};
use crate::core::skill::activation::ActiveSkillState;
use crate::core::skill::SkillRegistry;

/// 技能指令 Hook（loop 版 `pre_request`）——直接重构 rig `SkillInstructionHook`：
/// - 每轮注入**已激活技能约束摘要**（P1-5，≤800 字符，防长任务/压缩后技能规范漂移；
///   正文一次性注入由请求入口负责，v3 接入后补齐）；
/// - 窄化本轮模型可见工具（`active_tools`）：BASE_TOOLS ∪ 软门禁可见 ∪ 外部工具 ∪ MCP ∪
///   已激活技能声明工具。
///
/// 预算预警（剩余轮次）由 loop 内建（`assemble_request`），不在此重复。
pub struct SkillInstructionHook {
    pub state: Arc<ActiveSkillState>,
    pub registry: Arc<SkillRegistry>,
    pub mcp_tool_names: Vec<String>,
}

impl SkillInstructionHook {
    fn skill_constraint_summary(&self) -> String {
        const MAX_SUMMARY_CHARS: usize = 800;
        const MAX_DESC_CHARS: usize = 120;
        let mut parts: Vec<String> = Vec::new();
        let mut used = 0usize;
        for a in self.state.active_only() {
            let (name, desc) = match self.registry.get(a.scope, &a.skill_id) {
                Some(skill) => {
                    let mut desc = skill.description.trim().to_string();
                    if desc.chars().count() > MAX_DESC_CHARS {
                        let mut d: String = desc.chars().take(MAX_DESC_CHARS).collect();
                        d.push('…');
                        desc = d;
                    }
                    (skill.name.clone(), desc)
                }
                None => (a.skill_id.clone(), String::new()),
            };
            let block = if desc.is_empty() {
                format!("- {}（{} v{}）：已激活（正文一次性注入，请遵循其规范）", name, a.skill_id, a.version)
            } else {
                format!("- {}（{} v{}）：{}", name, a.skill_id, a.version, desc)
            };
            let block_chars = block.chars().count();
            if used + block_chars > MAX_SUMMARY_CHARS {
                break;
            }
            parts.push(block);
            used += block_chars;
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(
                "[已激活技能（约束摘要每轮常驻；正文仅注入一次，需完整内容可用 read 读取对应 SKILL.md）：\n{}\n]",
                parts.join("\n")
            )
        }
    }
}

#[async_trait]
impl LoopHook for SkillInstructionHook {
    fn pre_request(&self, _ctx: &HookCtx, _messages: &[LlmMessage]) -> RequestPatch {
        let mut patch = RequestPatch::default();
        // 技能约束摘要每轮注入（正文一次性注入由请求入口负责）
        let summary = self.skill_constraint_summary();
        if !summary.is_empty() {
            patch.preamble_override = Some(summary);
        }
        // 可见工具窄化：BASE_TOOLS ∪ 软门禁可见 ∪ 外部工具 ∪ MCP ∪ 已激活技能声明
        let mut visible: Vec<String> =
            crate::core::agent::BASE_TOOLS.iter().map(|s| s.to_string()).collect();
        for t in crate::core::agent::SKILL_GATED_VISIBLE_TOOLS {
            if !visible.iter().any(|v| v == t) {
                visible.push(t.to_string());
            }
        }
        for def in crate::core::agent::external_tools::load_external_tools_or_default() {
            if !visible.iter().any(|v| v == &def.name) {
                visible.push(def.name.clone());
            }
        }
        for n in &self.mcp_tool_names {
            if !visible.iter().any(|v| v == n) {
                visible.push(n.clone());
            }
        }
        if let Some(declared) = self.state.allowed_tools() {
            for t in declared {
                if !visible.iter().any(|v| v == &t) {
                    visible.push(t);
                }
            }
        }
        patch.active_tools = Some(visible);
        patch
    }
}

/// 技能工具白名单 Hook（loop 版）：兜底拦截未授权工具 + 重复调用熔断。
pub struct SkillGateHook {
    /// 始终放行的基础工具名
    pub base_tools: &'static [&'static str],
    /// 当前激活技能状态（allowed_tools 决定技能声明工具是否放行）
    pub state: Arc<ActiveSkillState>,
    /// 额外放行名（外部 HTTP 工具 / MCP 工具）
    pub allow_extra: Arc<HashSet<String>>,
}

#[async_trait]
impl LoopHook for SkillGateHook {
    async fn on_tool_call(&self, ctx: &HookCtx, name: &str, args: &Value) -> ToolDecision {
        // 防重复调用熔断：同一请求内「连续相同 (工具, 参数)」≥2 次后，第 3 次起跳过引导
        if let Some(warning) =
            crate::core::agent::tools::guard_duplicate_call(&ctx.request_id, name, &args.to_string())
        {
            log::warn!("[loop_guard] 熔断重复工具调用: {}", warning);
            return ToolDecision::Skip(warning);
        }
        if self.base_tools.contains(&name) || self.allow_extra.contains(name) {
            return ToolDecision::Run;
        }
        let declared = self.state.allowed_tools().unwrap_or_default();
        if declared.iter().any(|t| t == name) {
            return ToolDecision::Run;
        }
        ToolDecision::Skip(format!(
            "工具 '{}' 当前不可用（未由任何已激活技能声明）。可先声明该工具的技能，或改用其他工具。",
            name
        ))
    }
}

/// 审批门 Hook（loop 版）：破坏性写操作须经用户确认，fail-closed。
pub struct ApprovalHook {
    pub gate: Arc<ApprovalGate>,
}

#[async_trait]
impl LoopHook for ApprovalHook {
    async fn on_tool_call(&self, ctx: &HookCtx, name: &str, args: &Value) -> ToolDecision {
        match self.gate.check(&ctx.request_id, name, args).await {
            Ok(()) => ToolDecision::Run,
            Err(denial) => {
                log::warn!(
                    "[approval] 工具调用被拒绝: tool={} category={:?} reason={}",
                    name,
                    denial.category,
                    denial.reason
                );
                ToolDecision::Skip(skip_message(name, &denial))
            }
        }
    }
}

/// 按拒绝类别生成返回给模型的指令消息（语义与 rig `approval::hook::skip_message` 一致：
/// 消除模型文本式确认——审批由系统弹窗处理，不在对话中进行）。
fn skip_message(tool: &str, denial: &ApprovalDenial) -> String {
    let op = if tool.starts_with("mcp_") {
        format!("工具「{tool}」")
    } else {
        match tool {
            "delete" => "删除文件".to_string(),
            "edit" => "编辑文件".to_string(),
            "open-ui" => "打开文件".to_string(),
            _ => tool.to_string(),
        }
    };
    match denial.category {
        DenialCategory::UserRejected => format!(
            "用户拒绝了{op}操作({reason})。请勿重试相同操作;如需继续,请向用户说明理由或提供替代方案。",
            op = op,
            reason = denial.reason
        ),
        DenialCategory::ChannelUnavailable => format!(
            "{op}操作需要在桌面应用中通过系统确认弹窗完成,当前系统弹窗不可用,该操作无法自动执行。请告知用户:请在桌面应用中使用本功能。注意:不要请求用户输入'确认'或'取消'等文字,审批由系统弹窗处理,不在对话中进行。",
            op = op
        ),
        DenialCategory::Timeout => format!(
            "{op}操作的确认已超时,系统已按拒绝处理。请勿重试相同操作;如需执行,请让用户重新发起请求。注意:不要请求用户输入'确认'或'取消'等文字,审批由系统弹窗处理,不在对话中进行。",
            op = op
        ),
        DenialCategory::PolicyDenied => format!(
            "{op}操作已被审批策略禁止,无法执行({reason})。请勿尝试该操作或请求用户确认——这是系统级策略限制,不是用户可解除的审批。请改用其他方式完成任务。",
            op = op,
            reason = denial.reason
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::r#loop::HookCtx;
    use std::time::Duration;

    fn ctx() -> HookCtx {
        HookCtx::new(1, 1, "m", "r1", 10)
    }

    #[tokio::test]
    async fn skill_gate_allows_base_and_extra_skips_undeclared() {
        let state = Arc::new(ActiveSkillState::new());
        let extra: Arc<HashSet<String>> = Arc::new(["mcp_fs".into()].into_iter().collect());
        let hook = SkillGateHook {
            base_tools: &["read", "write"],
            state,
            allow_extra: extra,
        };
        assert_eq!(
            hook.on_tool_call(&ctx(), "read", &serde_json::json!({})).await,
            ToolDecision::Run
        );
        assert_eq!(
            hook.on_tool_call(&ctx(), "mcp_fs", &serde_json::json!({})).await,
            ToolDecision::Run
        );
        match hook.on_tool_call(&ctx(), "kb_search", &serde_json::json!({})).await {
            ToolDecision::Skip(reason) => assert!(reason.contains("不可用")),
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn approval_hook_denied_returns_skip_feedback() {
        use crate::core::approval::{
            ApprovalOutcome, ApprovalPolicy, ApprovalRequest, ApprovalTransport,
        };

        struct AskAll;
        impl ApprovalPolicy for AskAll {
            fn evaluate(&self, tool: &str, _args: &Value) -> Option<ApprovalRequest> {
                Some(ApprovalRequest {
                    tool: tool.to_string(),
                    args: Value::Null,
                    summary: "确认".into(),
                    detail: String::new(),
                })
            }
        }
        struct DenyTransport;
        #[async_trait]
        impl ApprovalTransport for DenyTransport {
            async fn request_approval(
                &self,
                _req: &ApprovalRequest,
                _timeout: Duration,
            ) -> ApprovalOutcome {
                ApprovalOutcome::Denied(ApprovalDenial {
                    category: DenialCategory::UserRejected,
                    reason: "用户点击了拒绝".into(),
                })
            }
        }
        let gate = Arc::new(ApprovalGate::new(
            vec![Box::new(AskAll)],
            Box::new(DenyTransport),
            Duration::from_secs(60),
        ));
        let hook = ApprovalHook { gate };
        match hook.on_tool_call(&ctx(), "delete", &serde_json::json!({"rel_path": "a.md"})).await {
            ToolDecision::Skip(reason) => {
                assert!(reason.contains("用户拒绝了删除文件操作"), "reason: {reason}");
                // UserRejected 分支：明确不可重试 + 提供替代方案（与 rig 版文案一致）
                assert!(reason.contains("请勿重试相同操作"), "reason: {reason}");
            }
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn skill_instruction_narrows_active_tools_and_empty_summary() {
        use crate::core::skill::SkillRegistry;
        let state = Arc::new(ActiveSkillState::new());
        let hook = SkillInstructionHook {
            state,
            registry: Arc::new(SkillRegistry::new()),
            mcp_tool_names: vec!["mcp_fs".into()],
        };
        let patch = hook.pre_request(&ctx(), &[]);
        // 无激活技能 → 无摘要注入
        assert!(patch.preamble_override.is_none());
        // 可见工具 = BASE_TOOLS ∪ 软门禁 ∪ MCP（无技能声明）
        let visible = patch.active_tools.expect("active_tools 必有值");
        assert!(visible.contains(&"read".to_string()), "BASE_TOOLS 应含 read");
        assert!(visible.contains(&"kb_search".to_string()), "软门禁应含 kb_search");
        assert!(visible.contains(&"mcp_fs".to_string()), "MCP 工具应可见");
        assert!(visible.contains(&"write".to_string()));
    }
}
