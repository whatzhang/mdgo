use serde::{Deserialize, Serialize};

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
}
