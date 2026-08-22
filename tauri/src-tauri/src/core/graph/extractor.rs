//! 实体抽取器（Entity Graph，Phase 3）——三级策略。
//!
//! 三级成本递增、覆盖面递增：
//! - **Level 1 规则抽取（免费）**：从文档文本中按确定性规则提取候选实体
//!   （`[[链接]]` 目标、`[text](url)` 链接文字、高亮关键词），零 LLM 成本；
//! - **Level 2 Embedding 聚类（低）**：对文档/知识库做向量聚类，簇心名作为主题实体
//!   （预留：调用方注入 embedding 能力；骨架提供接口与空实现降级）；
//! - **Level 3 LLM 精抽取（高）**：只处理高价值节点（访问频次高 / 核心文档），
//!   产出 {name, aliases, type} 结构化实体（注入 `EntityLlmExtractor` trait）。
//!
//! 全部产出经 [`crate::core::graph::merger::EntityMerger`] 消歧合并入库。
//! 抽取为**后台批任务**：不阻塞索引/检索（与 bookmark Enrichment Worker 同模式）。

use std::collections::HashMap;

use super::builder::{extract_link_targets, is_linkable};
use super::merger::EntityMerger;
use super::model::{GraphNode, NodeType};
use super::storage::GraphStore;

/// Level 3 LLM 抽取器抽象（依赖倒置：GraphEngine 不感知具体 LLM 实现）。
/// 由 lib.rs 用 AppState 的 LLM 配置注入；未注入时 Level 3 降级跳过。
/// （当前全量抽取走同步 Level 1；本 trait 为 Phase 3.2 LLM 精抽取预留）
#[allow(dead_code)]
pub trait EntityLlmExtractor: Send + Sync {
    /// 从一段文本抽取实体，返回 (name, aliases, type)。
    fn extract_entities(
        &self,
        text: &str,
        source_doc: &str,
    ) -> Box<dyn std::future::Future<Output = Result<Vec<(String, Vec<String>, String)>, String>> + Send + '_>;
}

/// 三级抽取器（组合 Level 1 规则 + 可选 Level 2/3）。
pub struct EntityExtractor<'a> {
    store: &'a GraphStore,
    /// Level 3 LLM 抽取器（None = 降级跳过；当前仅保留注入点，
    /// 全量抽取走同步 `extract_all_docs`（Level 1），LLM 精抽取由后续
    /// 后台任务编排注入——字段预留，避免接口丢失）
    #[allow(dead_code)]
    llm: Option<&'a dyn EntityLlmExtractor>,
}

impl<'a> EntityExtractor<'a> {
    pub fn new(store: &'a GraphStore, llm: Option<&'a dyn EntityLlmExtractor>) -> Self {
        Self { store, llm }
    }

    /// 规则抽取候选实体（Level 1）：
    /// - Markdown 链接目标（已解析为相对路径的跳过——那是 doc 节点，不是实体）；
    /// - 链接文字（alias）；外部链接 host（如 redis.io → redis）。
    /// 返回 (候选名, 别名列表)。
    pub fn rule_candidates(&self, rel_path: &str, content: &str) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        if is_linkable(rel_path) {
            for target in extract_link_targets(content) {
                // 外部链接 → host 作为实体（redis.io → redis；github.com/xx → github）
                if target.starts_with("http://") || target.starts_with("https://") {
                    let host = target
                        .split("://")
                        .nth(1)
                        .unwrap_or("")
                        .split('/')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    let name = host.split('.').next().unwrap_or(&host).to_string();
                    if !name.is_empty() && name.len() >= 2 && seen.insert(name.clone()) {
                        out.push((name, vec![host.clone()]));
                    }
                }
            }
        }
        out
    }

    /// 从文档节点扫描（增量/全量批处理入口）：遍历 doc 节点，读取内容做 Level1 抽取。
    /// 全量场景由 GraphEngine 调度；单文件由 index_file 联动后调用。
    /// 同步实现（无跨 await 借用）；Level 3 LLM 抽取请调用方在注入前自行编排。
    pub fn extract_all_docs(&self, base_dir: &str, limit: usize) -> Result<usize, String> {
        let docs = self.store.all_nodes(limit as u32)?;
        let mut count = 0usize;
        for doc in docs {
            if doc.node_type != NodeType::Doc {
                continue;
            }
            let path = match doc.path {
                Some(p) => p,
                None => continue,
            };
            if !is_linkable(&path) {
                continue;
            }
            let abs = std::path::Path::new(base_dir).join(&path);
            let content = match std::fs::read_to_string(&abs) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let mut merger = EntityMerger::new(self.store);
            let source_id = crate::core::graph::storage::node_id_for(NodeType::Doc, &path);
            for (name, aliases) in self.rule_candidates(&path, &content) {
                if let Ok(_id) = merger.upsert_entity(&name, &aliases, Some(&source_id)) {
                    count += 1;
                }
            }
        }
        Ok(count)
    }
}

/// Level 2 聚类预留：对文本集合做 embedding 聚类 → 簇主题实体。
/// 接口化（依赖注入 embedding 闭包）；未注入时返回空（降级）。
#[allow(dead_code)]
pub trait ClusterEntitySource: Send + Sync {
    /// 返回聚类主题实体候选 (name, members: Vec<doc_id>)
    fn cluster_entities(&self, docs: &[GraphNode]) -> Vec<(String, Vec<String>)>;
}

/// 简单启发式实现：按文档「目录前缀」聚类（无需 embedding 的零成本聚类）。
/// 真正的向量聚类由后续注入实现替换（O：策略可替换）。
#[allow(dead_code)]
pub struct DirPrefixCluster;

impl ClusterEntitySource for DirPrefixCluster {
    fn cluster_entities(&self, docs: &[GraphNode]) -> Vec<(String, Vec<String>)> {
        let mut groups: HashMap<String, Vec<String>> = HashMap::new();
        for d in docs {
            if d.node_type != NodeType::Doc {
                continue;
            }
            let path = d.path.clone().unwrap_or_default();
            let top = path.split('/').next().unwrap_or("").to_string();
            if !top.is_empty() {
                groups.entry(top).or_default().push(d.id.clone());
            }
        }
        groups
            .into_iter()
            .map(|(name, members)| (name, members))
            .collect()
    }
}
