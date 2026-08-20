//! 记忆向量检索（O1）：语义召回增强。
//!
//! # 设计（SOLID）
//!
//! - [`MemoryEmbedder`]：embedding 抽象（依赖倒置）。生产实现 [`LocalEmbedder`]
//!   走本地 BGE 模型（`core::db::utils::call_embedding`，同步 ONNX 批处理）；
//!   测试用确定性 mock。
//! - [`MemoryVectorIndex`]：内存向量索引（记忆规模小，几百条量级，内存余弦
//!   检索完全够用且无 LanceDB 集成成本）；惰性增量同步（`sync` 对比记忆全量
//!   id 与已索引集，仅为新增项补 embedding）。
//! - [`rrf_fuse_memory`]：按记忆 id 的轻量 RRF 融合（关键词 ∪ 向量），
//!   权重偏置对齐文档检索（向量 0.6 / 关键词 0.4，k=60）。
//!
//! 降级：embedding 模型不可用 / 同步失败 → 上层回退纯关键词检索（现状）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// embedding 抽象（依赖倒置，测试可注入 mock）。
pub trait MemoryEmbedder: Send + Sync {
    /// 批量文本 embedding；失败返回 Err（上层降级纯关键词）。
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String>;
}

/// 生产实现：本地 BGE 模型（同步 ONNX 批处理）。
pub struct LocalEmbedder;

impl MemoryEmbedder for LocalEmbedder {
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        crate::core::db::utils::call_embedding(texts, None)
    }
}

/// 余弦相似度。
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// 内存记忆向量索引（id → embedding），惰性增量同步。
pub struct MemoryVectorIndex {
    vectors: Mutex<HashMap<String, Vec<f32>>>,
    embedder: Arc<dyn MemoryEmbedder>,
}

impl MemoryVectorIndex {
    pub fn new(embedder: Arc<dyn MemoryEmbedder>) -> Self {
        Self {
            vectors: Mutex::new(HashMap::new()),
            embedder,
        }
    }

    /// 惰性同步：对比「记忆全量 id 集」与「已索引集」，为新增记忆补 embedding。
    ///
    /// `get_memory(id)` 由调用方注入（memory 模块取条目内容），避免本类型依赖
    /// MemoryStore（依赖倒置）。返回本次新增索引条数。
    pub fn sync<F>(&self, all_ids: &[String], mut get_memory: F) -> Result<usize, String>
    where
        F: FnMut(&str) -> Option<(String, String)>, // id -> (title, body)
    {
        let mut vectors = self.vectors.lock().map_err(|e| e.to_string())?;
        let missing: Vec<String> = all_ids
            .iter()
            .filter(|id| !vectors.contains_key(*id))
            .cloned()
            .collect();
        if missing.is_empty() {
            return Ok(0);
        }
        // 批量取文本并 embedding
        let mut texts: Vec<String> = Vec::with_capacity(missing.len());
        let mut by_id: Vec<(String, String)> = Vec::with_capacity(missing.len());
        for id in &missing {
            if let Some((title, body)) = get_memory(id) {
                texts.push(format!("{title} {body}"));
                by_id.push((id.clone(), format!("{title} {body}")));
            }
        }
        if texts.is_empty() {
            return Ok(0);
        }
        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let embs = self.embedder.embed(&refs).map_err(|e| format!("记忆向量化失败: {e}"))?;
        let mut count = 0usize;
        for ((id, _), emb) in by_id.into_iter().zip(embs.into_iter()) {
            vectors.insert(id, emb);
            count += 1;
        }
        Ok(count)
    }

    /// 移除已删除记忆的向量（P1-7：删除路径）。
    ///
    /// 原实现 `sync` 只增不删——记忆删除后陈旧向量累积（召回命中已删除内容、
    /// 内存随增删 churn 增长）。`live_ids` 为当前仍存在的记忆 id 集；
    /// 返回被移除的向量条数。
    pub fn prune(&self, live_ids: &std::collections::HashSet<String>) -> usize {
        let mut vectors = self.vectors.lock().unwrap_or_else(|e| e.into_inner());
        let before = vectors.len();
        vectors.retain(|id, _| live_ids.contains(id));
        before - vectors.len()
    }

    /// 查询向量 top-k 检索（余弦相似度），返回 (id, score)。
    pub fn search(&self, query_emb: &[f32], limit: usize) -> Vec<(String, f32)> {
        let vectors = self.vectors.lock().map(|m| m.clone()).unwrap_or_default();
        let mut scored: Vec<(String, f32)> = vectors
            .iter()
            .map(|(id, emb)| (id.clone(), cosine(query_emb, emb)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().take(limit.max(1)).collect()
    }

    /// 已索引条数（观测/测试）。
    pub fn len(&self) -> usize {
        self.vectors.lock().map(|m| m.len()).unwrap_or(0)
    }
}

/// 按记忆 id 的轻量 RRF 融合：关键词排名 ∪ 向量排名。
///
/// `keyword_hits` / `vec_hits` 均为 (id, score) 有序列表（score 仅用于同路内
/// 参考，融合只依赖排名）。返回融合后的 id 列表（按 RRF 分数降序）。
pub fn rrf_fuse_memory(
    keyword_hits: &[(String, f32)],
    vec_hits: &[(String, f32)],
    limit: usize,
) -> Vec<String> {
    const K: f32 = 60.0;
    const W_KW: f32 = 0.4;
    const W_VEC: f32 = 0.6;
    let mut scores: HashMap<String, f32> = HashMap::new();
    for (rank, (id, _)) in keyword_hits.iter().enumerate() {
        *scores.entry(id.clone()).or_insert(0.0) += W_KW / (K + rank as f32 + 1.0);
    }
    for (rank, (id, _)) in vec_hits.iter().enumerate() {
        *scores.entry(id.clone()).or_insert(0.0) += W_VEC / (K + rank as f32 + 1.0);
    }
    let mut ranked: Vec<(String, f32)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.into_iter().take(limit.max(1)).map(|(id, _)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确定性 mock embedder：按文本长度编码为可区分向量
    struct FakeEmbedder;

    impl MemoryEmbedder for FakeEmbedder {
        fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
            Ok(texts
                .iter()
                .map(|t| {
                    let len = t.len() as f32;
                    vec![len, (t.chars().count() % 7) as f32, 1.0]
                })
                .collect())
        }
    }

    #[test]
    fn vector_index_sync_and_search() {
        let idx = MemoryVectorIndex::new(Arc::new(FakeEmbedder));
        // 首次 sync 索引 2 条
        let ids = vec!["a".to_string(), "b".to_string()];
        let added = idx
            .sync(&ids, |id| match id {
                "a" => Some(("标题A".into(), "内容A内容".into())),
                "b" => Some(("标题B".into(), "内容B".into())),
                _ => None,
            })
            .unwrap();
        assert_eq!(added, 2);
        assert_eq!(idx.len(), 2);
        // 再次 sync 无新增
        assert_eq!(idx.sync(&ids, |_| None).unwrap(), 0);
        // 查询：与 a 文本相近的 query 应优先召回 a
        let q = FakeEmbedder.embed(&["标题A内容A内容"]).unwrap().pop().unwrap();
        let hits = idx.search(&q, 2);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0, "a");
    }

    #[test]
    fn rrf_fuse_prefers_multi_route_hits() {
        // id "x" 双路命中（关键词+向量）→ 融合分最高
        let kw = vec![("x".to_string(), 0.9), ("y".to_string(), 0.5)];
        let vec = vec![("z".to_string(), 0.8), ("x".to_string(), 0.7)];
        let fused = rrf_fuse_memory(&kw, &vec, 3);
        assert_eq!(fused[0], "x");
        // limit 生效
        let limited = rrf_fuse_memory(&kw, &vec, 1);
        assert_eq!(limited.len(), 1);
    }

    #[test]
    fn rrf_fuse_empty_inputs() {
        assert!(rrf_fuse_memory(&[], &[], 5).is_empty());
        assert_eq!(rrf_fuse_memory(&[("a".into(), 1.0)], &[], 5), vec!["a".to_string()]);
    }
}
