//! 工具结果缓存（P1-12）：只读工具（read）的 LRU 结果缓存，文件 mtime 变化即失效。
//!
//! # 设计（SOLID）
//!
//! - [`ToolResultCache`]：单一职责的 LRU 缓存（容量上限 + 访问序淘汰，仿
//!   [`crate::core::subagent::LruResultStore`] 的纯内存实现，可单测）。
//! - 失效策略：缓存条目携带文件 `mtime_ns`，读取前 stat 一次；mtime 变化
//!   （文件被编辑/替换）即视为失效——只读工具结果与文件内容强相关，
//!   mtime 是廉价且可靠的指纹。
//! - 只对纯函数只读工具启用（当前仅 `read`）；edit/delete/审批类绝不缓存
//!   （副作用工具缓存会掩盖真实状态）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// 单条缓存条目：`(mtime_ns, access_seq, text)`
type CacheEntry = (u64, u64, String);

/// 有界 LRU 工具结果缓存。
pub struct ToolResultCache {
    map: Mutex<HashMap<String, CacheEntry>>,
    seq: AtomicU64,
    max: usize,
}

impl ToolResultCache {
    pub fn new(max: usize) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
            max,
        }
    }

    /// 读取缓存：仅当条目的 mtime 与当前一致时命中（并刷新访问序）。
    pub fn get(&self, key: &str, mtime_ns: u64) -> Option<String> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut map) = self.map.lock() {
            match map.get_mut(key) {
                Some((stored_mtime, access, text)) if *stored_mtime == mtime_ns => {
                    *access = seq;
                    Some(text.clone())
                }
                // mtime 不一致：条目已失效，顺手清理
                Some((_, _, _)) => {
                    map.remove(key);
                    None
                }
                None => None,
            }
        } else {
            None
        }
    }

    /// 写入缓存：新 key 且已达上限时淘汰最久未访问的一条。
    pub fn put(&self, key: &str, mtime_ns: u64, text: &str) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut map) = self.map.lock() {
            if !map.contains_key(key) && map.len() >= self.max {
                if let Some(oldest) = map
                    .iter()
                    .min_by_key(|(_, (_, access, _))| *access)
                    .map(|(k, _)| k.clone())
                {
                    map.remove(&oldest);
                }
            }
            map.insert(key.to_string(), (mtime_ns, seq, text.to_string()));
        }
    }

    /// 当前条目数（观测/测试用；lib 构建不调用，标记避免 dead_code 告警）
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.map.lock().map(|m| m.len()).unwrap_or(0)
    }
}

static TOOL_RESULT_CACHE: OnceLock<ToolResultCache> = OnceLock::new();

/// 全局工具结果缓存（单例，容量 256）。
pub fn tool_result_cache() -> &'static ToolResultCache {
    TOOL_RESULT_CACHE.get_or_init(|| ToolResultCache::new(256))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hits_on_same_mtime_and_evicts_on_change() {
        let c = ToolResultCache::new(2);
        // 相同 mtime 命中
        c.put("read|a.md|0", 100, "内容A");
        assert_eq!(c.get("read|a.md|0", 100), Some("内容A".to_string()));
        // mtime 变化（文件被编辑）→ 失效
        assert_eq!(c.get("read|a.md|0", 101), None);
        // 重新写入后可命中
        c.put("read|a.md|0", 101, "内容A2");
        assert_eq!(c.get("read|a.md|0", 101), Some("内容A2".to_string()));
    }

    #[test]
    fn cache_evicts_least_recently_used() {
        let c = ToolResultCache::new(2);
        c.put("k1", 1, "v1");
        c.put("k2", 1, "v2");
        // 刷新 k1 访问序
        assert_eq!(c.get("k1", 1), Some("v1".to_string()));
        // 插入 k3 → 淘汰 k2（最久未访问）
        c.put("k3", 1, "v3");
        assert_eq!(c.len(), 2);
        assert_eq!(c.get("k2", 1), None, "应淘汰最久未访问的 k2");
        assert_eq!(c.get("k1", 1), Some("v1".to_string()));
        assert_eq!(c.get("k3", 1), Some("v3".to_string()));
    }
}
