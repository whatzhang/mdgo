//! 审批门 Hook:在工具真正执行前拦截破坏性调用,请求用户确认。
//!
//! 职责单一:只做「Run / Skip」决策;策略与通道逻辑全在 [`super::ApprovalGate`]。
//! 挂载顺序由 Agent 组装方决定(建议在技能白名单 Hook 之后,
//! 先技能白名单、后审批,避免对「本就不该调用的工具」弹窗打扰用户)。
//!
//! # 反馈语义(消除模型文本式确认)
//!
//! 审批被拒时,按 [`super::DenialCategory`] 给模型明确指令:
//! - 用户拒绝 → 告知不可重试,可提供替代方案
//! - 通道不可用/超时 → 明确"系统弹窗已处理审批,不要请求用户输入确认文字"

use std::sync::Arc;

use rig_agent::agent::hook::{AgentHook, HookContext, ToolCall, ToolCallAction};

use super::{ApprovalGate, DenialCategory};

#[derive(Clone, Debug)]
pub struct ApprovalGateHook {
    gate: Arc<ApprovalGate>,
}

impl ApprovalGateHook {
    pub fn new(gate: Arc<ApprovalGate>) -> Self {
        Self { gate }
    }
}

impl AgentHook for ApprovalGateHook {
    async fn on_tool_call(
        &self,
        ctx: &HookContext,
        event: ToolCall<'_>,
    ) -> ToolCallAction {
        let args: serde_json::Value =
            serde_json::from_str(event.args).unwrap_or(serde_json::Value::Null);
        match self
            .gate
            .check(ctx.run_id().as_str(), event.tool_name, &args)
            .await
        {
            Ok(()) => ToolCallAction::Run,
            Err(denial) => {
                log::warn!(
                    "[approval] 工具调用被拒绝: tool={} category={:?} reason={}",
                    event.tool_name,
                    denial.category,
                    denial.reason
                );
                ToolCallAction::Skip(skip_message(event.tool_name, &denial))
            }
        }
    }
}

/// 按拒绝类别生成返回给模型的指令消息
fn skip_message(tool: &str, denial: &super::ApprovalDenial) -> String {
    let op = match tool {
        "delete" => "删除文件",
        "edit" => "编辑文件",
        _ => tool,
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
    }
}
