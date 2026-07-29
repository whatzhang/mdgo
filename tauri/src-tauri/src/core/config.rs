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
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            dir_blacklist: Vec::new(),
            file_blacklist: Vec::new(),
            chunk_size: 1500,
            chunk_overlap: 300,
            top_k: 10,
            min_score: 0.3,
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
