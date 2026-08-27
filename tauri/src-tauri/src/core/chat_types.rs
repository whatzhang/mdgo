use serde::{Deserialize, Serialize};

use crate::core::context::ChatTurn;

/// 一次模型发起的工具调用（OpenAI 协议视图）。
///
/// 与 rig 的 `ToolCall` 解耦（依赖倒置）：本项目会话层只依赖本 DTO，
/// 由 core/loop 会话层（`crate::core::loop::session::Session::derive_history`）映射为模型消息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCallDto {
    /// 工具调用 ID（`call_*`），用于与 tool 结果消息的 `tool_call_id` 配对
    pub id: String,
    /// 工具名
    pub name: String,
    /// 参数 JSON 字符串（模型原始产出，回放时原样解析）
    pub arguments: String,
}

/// 将历史按「工具调用单元」分组：一条 assistant（带 `tool_calls`）与其
/// 紧随其后的连续 tool 结果消息同组，其余消息各自成组。
///
/// 这是「工具调用配对」语义的**唯一后端来源**（P1-1，rig 时代的
/// `core/context::group_turns` 与 `commands/llm::chat_turns_to_history` 各有近似
/// 实现，改一处漏一处；rig 移除后收敛到本函数）。
/// ⚠️ 前端 `chat-history.js` 的 `groupToolUnits` 是同一语义的镜像副本（历史裁剪用）；
/// 修改本函数时须同步前端，避免配对语义漂移（B8）。
/// - 压缩切分（`core::context` 的滑窗/摘要策略）必须以单元为单位——否则会把
///   assistant 的 tool_call 与 tool 结果切到不同侧，产生「孤儿 tool 消息」
///   导致 OpenAI 协议拒绝请求；
/// - 历史 → 模型消息转换（`crate::core::loop::session::derive_history`）用同一分组
///   判定孤儿 tool_call（无配对结果的调用不重放）。
///
/// 返回 `(单元起始下标, 单元)`，供压缩器计算保留起点（`kept_from`）。
pub fn group_tool_units(history: &[ChatTurn]) -> Vec<(usize, Vec<ChatTurn>)> {
    let mut units: Vec<(usize, Vec<ChatTurn>)> = Vec::new();
    for (idx, turn) in history.iter().enumerate() {
        if turn.is_tool_message() {
            // 并入当前组（防御：若没有当前组（孤儿 tool 消息）则自成一组）
            match units.last_mut() {
                Some((_, last)) => last.push(turn.clone()),
                None => units.push((idx, vec![turn.clone()])),
            }
        } else {
            units.push((idx, vec![turn.clone()]));
        }
    }
    units
}

#[derive(Debug, Serialize, Clone)]
pub struct ChatSession {
    pub id: String,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub favorite: bool,
    pub message_count: u32,
    pub token_usage: u32,
    pub month_group: String,
    pub r#type: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChatMessage {
    pub id: String,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub token_count: i32,
    pub created_at: u64,
    /// 助手消息关联的工具调用轨迹（JSON 数组字符串），历史回放时重渲染用
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<String>,
    /// 助手消息的推理过程（thinking 增量拼接；历史回放恢复 thinking 时间线）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChatSessionSearchResult {
    pub session: ChatSession,
    pub score: f32,
    pub matched_content: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatMessageSource {
    pub id: String,
    pub message_id: String,
    pub doc_name: String,
    pub score: f32,
    pub snippet: String,
    /// OPML 节点路径 JSON 数组（仅 OPML 文件有值）
    pub path_json: Option<String>,
}
