//! 查询 embedding 缓存（B1）：进程内 LRU，重复/近似查询零推理。
//!
//! 键 = **模型作用域（model|dim）+** 原始查询文本的 FNV-1a 128 哈希
//! （不缓存 BGE instruction 前缀后的文本，保证同一用户查询命中）。
//! 🟠 M19 修复：键纳入模型名与维度——模型原地替换（同名同维不同 checkpoint）
//! 时查询向量与文档向量来自不同模型，键变化避免检索语义错位；与磁盘缓存
//! `model|dim|content_hash` 的策略对齐。容量 [`CACHE_CAPACITY`]，满时按 FIFO 淘汰
//! （查询侧调用频率低，Mutex 足够；FIFO 是 LRU 的轻量近似）。

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use crate::core::db::utils::fnv1a_128;

/// 缓存容量（条）
const CACHE_CAPACITY: usize = 512;

/// 模型作用域（`model|dim`）：模型/维度变化 → 键自然失效。
/// 注意：`get_model_name()` 在模型未初始化时返回回退常量，与磁盘缓存同为
/// 惰性初始化语义——put 发生在推理成功后（真实名），get 若先于初始化会 miss
/// （无害，仅失去一次命中）。
fn model_scope() -> &'static str {
    static SCOPE: OnceLock<String> = OnceLock::new();
    SCOPE.get_or_init(|| {
        format!(
            "{}|{}",
            crate::core::embedding::get_model_name(),
            crate::core::embedding::get_embedding_dimension()
        )
    })
}

fn cache_key(query: &str) -> u128 {
    fnv1a_128(format!("{}|{}", model_scope(), query).as_bytes())
}

struct Inner {
    map: HashMap<u128, Vec<f32>>,
    order: VecDeque<u128>,
}

pub struct QueryEmbeddingCache {
    inner: Mutex<Inner>,
}

impl QueryEmbeddingCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// 查询缓存（键 = 模型作用域 + 查询文本哈希）；未命中返回 None
    pub fn get(&self, query: &str) -> Option<Vec<f32>> {
        let key = cache_key(query);
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.map.get(&key).cloned()
    }

    /// 写入缓存（FIFO 淘汰）
    pub fn put(&self, query: &str, vector: Vec<f32>) {
        let key = cache_key(query);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.map.contains_key(&key) {
            return;
        }
        inner.map.insert(key, vector);
        inner.order.push_back(key);
        while inner.order.len() > CACHE_CAPACITY {
            if let Some(oldest) = inner.order.pop_front() {
                inner.map.remove(&oldest);
            }
        }
    }

    /// 清空缓存（模型切换时调用；当前无调用点——模型进程内固定，保留供未来模型热切换）
    #[allow(dead_code)]
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.map.clear();
        inner.order.clear();
    }
}

impl Default for QueryEmbeddingCache {
    fn default() -> Self {
        Self::new()
    }
}

/// 全局查询 embedding 缓存（懒初始化）
pub fn global_query_embedding_cache() -> &'static QueryEmbeddingCache {
    static CACHE: std::sync::OnceLock<QueryEmbeddingCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(QueryEmbeddingCache::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_put_roundtrip() {
        let c = QueryEmbeddingCache::new();
        assert!(c.get("Redis 连接池").is_none());
        c.put("Redis 连接池", vec![1.0, 2.0, 3.0]);
        assert_eq!(c.get("Redis 连接池").unwrap(), vec![1.0, 2.0, 3.0]);
        assert!(c.get("不同查询").is_none());
    }

    #[test]
    fn fifo_eviction() {
        let c = QueryEmbeddingCache {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
        };
        // 塞满容量 + 1
        for i in 0..CACHE_CAPACITY + 1 {
            c.put(&format!("查询{}", i), vec![i as f32]);
        }
        // 最旧的被淘汰
        assert!(c.get("查询0").is_none());
        assert!(c.get(&format!("查询{}", CACHE_CAPACITY)).is_some());
    }
}
