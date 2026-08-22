//! 实体消歧与别名合并（Entity Graph 工程质量核心）。
//!
//! LLM/规则抽取会产出近似重复实体（`Redis`/`redis`/`Redis 缓存`），直接入库会让
//! 边散落在近似节点上。本模块提供：
//! - [`canonicalize`]：实体规范化键（小写 + 去停用词 + 空白归一），用于判重；
//! - [`EntityMerger`]：插入时查别名 → 命中则合并（边重定向 + 别名追加），
//!   未命中则新建节点并注册别名。
//!
//! 别名存储：`graph_nodes.meta` 的 `aliases` 字段（JSON 数组）；canonical 键即为
//! 节点 id 后缀（`entity:{canonical}`），保证同一实体唯一 id。

use std::collections::HashMap;

use super::model::{GraphNode, NodeType, Relation};
use super::storage::GraphStore;

/// 停用词（中文 + 英文常见修饰词，消歧时剔除，避免 `Redis 缓存` vs `Redis` 分裂）
const STOPWORDS: &[&str] = &[
    "缓存", "系统", "框架", "技术", "工具", "平台", "方案", "组件", "模块",
    "the", "a", "an", "of", "for", "in", "on", "with", "and", "or",
];

/// 实体规范化键：小写 → 剔除停用词 → 空白归一 → 去首尾分隔符。
///
/// 例：`Redis 缓存` → `redis`；`  Redis  ` → `redis`；`Spring Boot` → `spring boot`。
pub fn canonicalize(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    let words: Vec<String> = lower
        .split(|c: char| c.is_whitespace() || c == '，' || c == ',' || c == '：' || c == ':' || c == '（' || c == '）' || c == '(' || c == ')' || c == '-' || c == '_' || c == '/' || c == '\\')
        .map(|w| w.trim().to_string())
        .filter(|w| !w.is_empty())
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .collect();
    if words.is_empty() {
        // 全停用词 → 保留原文小写（避免空键）
        return lower.replace(char::is_whitespace, "");
    }
    words.join(" ")
}

/// 实体消歧合并器。
pub struct EntityMerger<'a> {
    store: &'a GraphStore,
    /// 本次会话的别名索引缓存（{alias_lower → canonical}，避免重复查库）
    alias_index: HashMap<String, String>,
}

impl<'a> EntityMerger<'a> {
    pub fn new(store: &'a GraphStore) -> Self {
        Self {
            store,
            alias_index: HashMap::new(),
        }
    }

    /// 插入一个实体（带别名列表），自动消歧合并。
    ///
    /// 返回最终实体节点 id（新建或已存在的 canonical id）。
    /// - 任一别名（含规范名）命中已有实体 → 合并：更新 meta.aliases 并返回其 id；
    /// - 未命中 → 新建 `entity:{canonical}` 节点。
    pub fn upsert_entity(
        &mut self,
        name: &str,
        aliases: &[String],
        source_doc: Option<&str>,
    ) -> Result<String, String> {
        let canon = canonicalize(name);
        if canon.is_empty() {
            return Err("实体名为空".to_string());
        }
        let id = format!("entity:{}", canon);

        // 候选别名：规范名 + 传入别名（小写归一）
        let mut candidates: Vec<String> = vec![canon.clone()];
        candidates.extend(aliases.iter().map(|a| canonicalize(a)).filter(|a| !a.is_empty()));

        // 1. 先查本地索引缓存
        let mut existing: Option<String> = None;
        for c in &candidates {
            if let Some(hit) = self.alias_index.get(c) {
                existing = Some(hit.clone());
                break;
            }
        }
        // 2. 缓存未命中 → 查库（按 id 精确 + name 模糊）
        if existing.is_none() {
            for c in &candidates {
                let cid = format!("entity:{}", c);
                if self.store.get_node(&cid)?.is_some() {
                    existing = Some(cid);
                    break;
                }
            }
        }
        if existing.is_none() {
            // 精确 id 未命中 → 尝试 name 匹配（旧数据可能未按 canonical id）
            if let Ok(hits) = self.store.search_nodes(&canon, 5) {
                for h in hits {
                    if h.node_type == NodeType::Entity && canonicalize(&h.name) == canon {
                        existing = Some(h.id.clone());
                        break;
                    }
                }
            }
        }

        match existing {
            Some(existing_id) => {
                // 合并：追加新别名到 meta（取一次节点，避免重复查询，R6 修复）
                let existing_node = self.store.get_node(&existing_id)?;
                let mut meta = existing_node
                    .as_ref()
                    .and_then(|n| n.meta.clone())
                    .and_then(|m| serde_json::from_str::<serde_json::Value>(&m).ok())
                    .unwrap_or_else(|| serde_json::json!({}));
                let mut alias_list: Vec<String> = meta
                    .get("aliases")
                    .and_then(|a| a.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                for c in &candidates {
                    if !alias_list.contains(c) {
                        alias_list.push(c.clone());
                    }
                }
                meta["aliases"] = serde_json::json!(alias_list);
                let node = GraphNode {
                    id: existing_id.clone(),
                    node_type: NodeType::Entity,
                    name: existing_node
                        .map(|n| n.name)
                        .unwrap_or_else(|| name.to_string()),
                    path: None,
                    meta: Some(meta.to_string()),
                    degree: None,
                };
                self.store.upsert_node(&node)?;
                // 边重定向：把指向旧名（若存在独立节点）的边并入 canonical id
                for c in &candidates {
                    let cid = format!("entity:{}", c);
                    if cid != existing_id && self.store.get_node(&cid)?.is_some() {
                        self.store.merge_node_edges(&cid, &existing_id)?;
                    }
                }
                // 更新别名索引
                for c in &candidates {
                    self.alias_index.insert(c.clone(), existing_id.clone());
                }
                // 来源文档边（DERIVED_FROM）
                if let Some(doc) = source_doc {
                    self.store.upsert_edge(
                        &super::model::GraphEdge {
                            source: existing_id.clone(),
                            target: doc.to_string(),
                            relation: Relation::DerivedFrom,
                            weight: Some(1.0),
                            confidence: Some(1.0),
                        },
                        Some(doc),
                    )?;
                }
                Ok(existing_id)
            }
            None => {
                // 新建实体节点
                let meta = serde_json::json!({ "aliases": &candidates });
                let node = GraphNode {
                    id: id.clone(),
                    node_type: NodeType::Entity,
                    name: name.trim().to_string(),
                    path: None,
                    meta: Some(meta.to_string()),
                    degree: None,
                };
                self.store.upsert_node(&node)?;
                for c in &candidates {
                    self.alias_index.insert(c.clone(), id.clone());
                }
                if let Some(doc) = source_doc {
                    self.store.upsert_edge(
                        &super::model::GraphEdge {
                            source: id.clone(),
                            target: doc.to_string(),
                            relation: Relation::DerivedFrom,
                            weight: Some(1.0),
                            confidence: Some(1.0),
                        },
                        Some(doc),
                    )?;
                }
                Ok(id)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canonicalize() {
        assert_eq!(canonicalize("Redis"), "redis");
        assert_eq!(canonicalize("Redis 缓存"), "redis");
        assert_eq!(canonicalize("  Redis  "), "redis");
        assert_eq!(canonicalize("Spring Boot"), "spring boot");
        assert_eq!(canonicalize("RAG 框架"), "rag");
    }
}
