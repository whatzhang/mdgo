use std::sync::{Arc, RwLock};

/// 索引器配置（持目录/文件黑名单）
#[derive(Clone, Debug)]
pub struct IndexerConfig {
    pub dir_blacklist: Vec<String>,
    pub file_blacklist: Vec<String>,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            dir_blacklist: Vec::new(),
            file_blacklist: Vec::new(),
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
