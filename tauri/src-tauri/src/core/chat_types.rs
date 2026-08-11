use serde::{Deserialize, Serialize};

/// 一次模型发起的工具调用（OpenAI 协议视图）。
///
/// 与 rig 的 `ToolCall` 解耦（依赖倒置）：本项目会话层只依赖本 DTO，
/// 由转换层（`commands::llm::chat_turns_to_history`）映射为 rig 消息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCallDto {
    /// 工具调用 ID（`call_*`），用于与 tool 结果消息的 `tool_call_id` 配对
    pub id: String,
    /// 工具名
    pub name: String,
    /// 参数 JSON 字符串（模型原始产出，回放时原样解析）
    pub arguments: String,
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
