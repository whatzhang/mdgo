use std::sync::{Arc, RwLock};
use serde::Serialize;

/// 索引器配置，定义索引时需要排除的目录和文件黑名单，以及分块/检索参数
#[derive(Clone, Debug, Serialize)]
pub struct IndexerConfig {
    pub dir_blacklist: Vec<String>,
    pub file_blacklist: Vec<String>,
    pub chunk_size: usize,
    pub chunk_overlap: usize,
    pub top_k: u32,
    pub min_score: f32,
    /// 向量/BM25 融合权重（0~1，越高越偏向语义向量，越低越偏向关键词）
    pub fusion_alpha: f32,
    /// 送入 LLM 上下文的最大文档数
    pub max_context_docs: usize,
    /// 单文档最多保留并送入上下文的 chunk 数
    pub max_chunks_per_doc: usize,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        // 分块默认值动态对齐本地嵌入模型的 token 窗口（如 bge-small-zh 512 token）
        let chunk_size = crate::core::embedding::recommended_chunk_size();
        Self {
            dir_blacklist: Vec::new(),
            file_blacklist: Vec::new(),
            chunk_size,
            chunk_overlap: crate::core::embedding::recommended_chunk_overlap(),
            top_k: 10,
            min_score: 0.3,
            fusion_alpha: 0.6,
            max_context_docs: 4,
            max_chunks_per_doc: 3,
        }
    }
}

/// 线程安全的可热更新配置容器
pub struct ConfigStore {
    inner: Arc<RwLock<IndexerConfig>>,
}

impl ConfigStore {
    pub fn new(initial: IndexerConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(initial)),
        }
    }

    pub fn update(&self, config: IndexerConfig) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        *guard = config;
    }

    pub fn read(&self) -> IndexerConfig {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn clone_inner(&self) -> Arc<RwLock<IndexerConfig>> {
        self.inner.clone()
    }
}
