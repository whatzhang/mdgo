use serde::{Deserialize, Serialize};

/// 文件类型计数（用于文档类型分布）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTypeCount {
    pub file_type: String,
    pub count: u32,
    pub percentage: f32,
}

#[derive(Debug, Serialize)]
pub struct KbIndexResult {
    pub file_count: u32,
    pub chunk_count: u32,
    pub vector_count: u32,
    pub indexed_at: u64,
}

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
    /// 已索引文件的类型分布（由 index_all/index_file 写入，chart 消费）
    #[serde(default)]
    pub type_distribution: Vec<FileTypeCount>,
}

/// 嵌入模型信息（动态获取，非硬编码）
#[derive(Debug, Serialize)]
pub struct KbEmbeddingInfo {
    pub model_name: String,
    pub dimension: u32,
    pub status: String,
}
