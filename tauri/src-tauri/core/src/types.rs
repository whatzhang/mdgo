use serde::{Deserialize, Serialize};

/// 知识库索引结果
#[derive(Debug, Serialize)]
pub struct KbIndexResult {
    pub file_count: u32,
    pub chunk_count: u32,
    pub vector_count: u32,
    pub indexed_at: u64,
}

/// 知识库状态
#[derive(Debug, Serialize)]
pub struct KbStatus {
    pub file_count: u32,
    pub chunk_count: u32,
    pub vector_count: u32,
    pub indexed_at: u64,
    pub status: String,
}

/// 索引元数据（持久化到 JSON）
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexMeta {
    pub file_count: u32,
    pub chunk_count: u32,
    pub vector_count: u32,
    pub indexed_at: u64,
}
