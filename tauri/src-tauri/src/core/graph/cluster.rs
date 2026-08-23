//! Cluster Engine（L0 聚合单元；PRD §11/§13/§39）。
//!
//! 第一级实现（Level 1：目录结构聚类，零 LLM 成本）：
//! - 有 path 的节点（doc/folder）：按 `path` 顶层目录归簇；
//!   根目录文件（path 不含 `/`）归入 `cluster:__root__`；
//! - 无 path 的节点（entity/chunk/experience）：邻域多数投票归簇；
//! - 簇间聚合边（`ClusterLink`）：跨簇边的计数（L0 聚合图数据源）。
//!
//! 后续 Level 2（文档主题）/ Level 3（Embedding）/ Level 4（LLM）在同一接口上扩展
//! （`algorithm` 字段区分），前端与命令层零改动。

use std::collections::HashMap;

use super::model::{ClusterLink, GraphCluster, GraphEdge, GraphNode, NodeType};
use super::storage::{node_id_for, GraphStore};

/// 根目录（顶层文件）的簇 id
pub const ROOT_CLUSTER_ID: &str = "cluster:__root__";
/// 无法归簇节点（无 path 且无邻居）的兜底簇 id
const GLOBAL_CLUSTER_ID: &str = "cluster:__global__";

/// 归一化簇 id：`cluster:<顶层目录名>`；根目录文件 → ROOT_CLUSTER_ID。
pub fn cluster_id_for_path(path: &str) -> String {
    let norm = path.replace('\\', "/");
    let top = norm.split('/').next().unwrap_or("");
    if top.is_empty() || top == norm {
        // 空 path 或根目录文件（不含 '/'）
        ROOT_CLUSTER_ID.to_string()
    } else {
        format!("cluster:{}", top)
    }
}

/// 簇 id → 展示名（cluster:docs → docs；__root__ → 根目录）
pub fn cluster_display_name(id: &str) -> String {
    let top = id.strip_prefix("cluster:").unwrap_or(id);
    if top == "__root__" {
        "根目录".to_string()
    } else if top == "__global__" {
        "全局（未归类）".to_string()
    } else {
        top.to_string()
    }
}

/// Cluster Engine：目录级聚类的发现与持久化。
pub struct ClusterEngine<'a> {
    store: &'a GraphStore,
}

impl<'a> ClusterEngine<'a> {
    pub fn new(store: &'a GraphStore) -> Self {
        Self { store }
    }

    /// 全量重算聚类（重建 graph_clusters 表；build_all/build_incremental 后调用）。
    /// 返回簇数量。O(n+e) 单遍扫描，884 节点毫秒级。
    pub fn rebuild(&self) -> Result<usize, String> {
        // 1) 节点 → 簇（第一遍：有 path 的按顶层目录；无 path 的先挂 global）
        let all_nodes = self.store.all_nodes(500_000)?;
        let mut node_cluster: HashMap<String, String> = HashMap::new();
        for n in &all_nodes {
            match &n.path {
                Some(p) => {
                    node_cluster.insert(n.id.clone(), cluster_id_for_path(p));
                }
                None => {
                    node_cluster.insert(n.id.clone(), GLOBAL_CLUSTER_ID.to_string());
                }
            }
        }

        // 2) 边统计：簇内边数 / 跨簇计数（links）/ 节点簇内度数
        let mut cluster_internal: HashMap<String, u32> = HashMap::new();
        let mut cluster_degree: HashMap<String, HashMap<String, u32>> = HashMap::new(); // cluster → node → degree
        let mut cross: HashMap<(String, String), u32> = HashMap::new();
        let all_edges = self.store.all_edges(1_000_000)?;
        for e in &all_edges {
            let ca = node_cluster.get(&e.source).cloned().unwrap_or_else(|| GLOBAL_CLUSTER_ID.to_string());
            let cb = node_cluster.get(&e.target).cloned().unwrap_or_else(|| GLOBAL_CLUSTER_ID.to_string());
            *cluster_degree.entry(ca.clone()).or_default().entry(e.source.clone()).or_insert(0) += 1;
            *cluster_degree.entry(cb.clone()).or_default().entry(e.target.clone()).or_insert(0) += 1;
            if ca == cb {
                *cluster_internal.entry(ca).or_insert(0) += 1;
            } else {
                let key = if ca < cb { (ca.clone(), cb.clone()) } else { (cb.clone(), ca.clone()) };
                *cross.entry(key).or_insert(0) += 1;
            }
        }

        // 3) 无 path 节点邻域多数投票（第二遍：用簇内度数判断归属）
        for n in &all_nodes {
            if n.path.is_some() {
                continue;
            }
            let mut votes: HashMap<String, u32> = HashMap::new();
            for nb in self.store.neighbors_of(&n.id)? {
                if let Some(c) = node_cluster.get(&nb) {
                    *votes.entry(c.clone()).or_insert(0) += 1;
                }
            }
            let best = votes.into_iter().max_by_key(|(_, v)| *v);
            if let Some((best_cluster, _)) = best {
                // 移除旧 global 归属，改投多数簇
                node_cluster.insert(n.id.clone(), best_cluster.clone());
                if let Some(deg) = cluster_degree.get_mut(&GLOBAL_CLUSTER_ID.to_string()) {
                    deg.remove(&n.id);
                }
                *cluster_degree.entry(best_cluster).or_default().entry(n.id.clone()).or_insert(0) += 1;
            }
        }

        // 4) 组装 GraphCluster（centroid = 簇内度数最高节点；全孤立簇回退最小 id 成员）
        let now = crate::core::graph::storage::now_ms_public();
        let mut clusters: Vec<GraphCluster> = Vec::new();
        for (cid, mut members) in node_cluster_group(&node_cluster) {
            let centroid = cluster_degree
                .get(&cid)
                .and_then(|m| {
                    // 确定性选心：度数降序，同度数取 id 最小（避免 HashMap 迭代序抖动）
                    m.iter()
                        .max_by(|(ida, da), (idb, db)| da.cmp(db).then_with(|| idb.cmp(ida)))
                        .map(|(id, _)| id.clone())
                })
                .or_else(|| {
                    members.sort();
                    members.first().cloned()
                });
            let node_count = members.len() as u32;
            let edge_count = cluster_internal.get(&cid).copied().unwrap_or(0);
            let name = cluster_display_name(&cid);
            let description = Some(format!(
                "基于目录结构自动聚类的知识簇，包含 {} 个节点、{} 条内部关系。",
                node_count, edge_count
            ));
            clusters.push(GraphCluster {
                id: cid,
                name,
                description,
                algorithm: "directory".to_string(),
                centroid,
                node_count,
                edge_count,
                confidence: 1.0,
                created_at: now,
                updated_at: now,
                links: Vec::new(),
                top_files: Vec::new(),
            });
            // 显式成员表（统一成员解析路径）
            self.store.replace_cluster_members(&clusters.last().unwrap().id, &members)?;
        }
        // 排序：节点数降序（大簇在前）
        clusters.sort_by(|a, b| b.node_count.cmp(&a.node_count));

        // 5) 簇间链接（跨簇边聚合；排除 global 兜底簇）
        let mut links: Vec<ClusterLink> = Vec::new();
        for ((a, b), count) in cross {
            if a == GLOBAL_CLUSTER_ID || b == GLOBAL_CLUSTER_ID {
                continue;
            }
            links.push(ClusterLink {
                source: a,
                target: b,
                count,
            });
        }
        for c in &mut clusters {
            c.links = links
                .iter()
                .filter(|l| l.source == c.id || l.target == c.id)
                .cloned()
                .collect();
        }

        self.store.replace_clusters(&clusters)?;
        Ok(clusters.len())
    }

    /// Level 3 Embedding 语义聚类（PRD §11.1 Level 3；算法='embedding'）。
    ///
    /// `samples`：(doc 节点 id, 文档 embedding)。贪婪在线聚类：
    /// 与最近簇心余弦相似度 ≥ threshold 归入该簇（簇心在线均值更新），否则开新簇。
    /// 替换现有目录簇（前端可切换聚类模式）。
    pub fn rebuild_from_embeddings(&self, samples: &[(String, Vec<f32>)]) -> Result<usize, String> {
        const THRESHOLD: f32 = 0.60;
        if samples.is_empty() {
            return Ok(0);
        }
        let dim = samples[0].1.len();
        // (簇心, 成员 id 列表)
        let mut clusters: Vec<(Vec<f32>, Vec<String>)> = Vec::new();
        for (doc_id, emb) in samples {
            if emb.len() != dim {
                continue;
            }
            let mut best: Option<(usize, f32)> = None;
            for (i, (centroid, _)) in clusters.iter().enumerate() {
                let sim = cosine_similarity(centroid, emb);
                if sim >= THRESHOLD && best.map(|(_, s)| sim > s).unwrap_or(true) {
                    best = Some((i, sim));
                }
            }
            match best {
                Some((i, _)) => {
                    let (centroid, members) = &mut clusters[i];
                    let n = members.len() as f32;
                    for k in 0..centroid.len() {
                        centroid[k] = (centroid[k] * n + emb[k]) / (n + 1.0);
                    }
                    members.push(doc_id.clone());
                }
                None => clusters.push((emb.clone(), vec![doc_id.clone()])),
            }
        }

        // 组装 GraphCluster（名称取簇内度数最高节点；centroid 同规则）
        let now = crate::core::graph::storage::now_ms_public();
        let all_edges = self.store.all_edges(1_000_000)?;
        let mut out: Vec<GraphCluster> = Vec::new();
        for (i, (_centroid, members)) in clusters.into_iter().enumerate() {
            let member_set: std::collections::HashSet<&str> = members.iter().map(|s| s.as_str()).collect();
            let edge_count = all_edges
                .iter()
                .filter(|e| member_set.contains(e.source.as_str()) && member_set.contains(e.target.as_str()))
                .count() as u32;
            // 度数最高成员（确定性）
            let mut best_member: Option<&str> = None;
            let mut best_deg = 0u32;
            let mut name = format!("主题簇 {}", i + 1);
            for m in &members {
                if let Ok(deg) = self.store.degree(m) {
                    if deg > best_deg {
                        best_deg = deg;
                        best_member = Some(m);
                    }
                }
                // 名称优先用 doc 名
                if let Ok(Some(n)) = self.store.get_node(m) {
                    if n.node_type == NodeType::Doc && n.name.len() > name.len() && n.name.len() < 24 {
                        name = n.name.trim_end_matches(".md").to_string();
                    }
                }
            }
            let cid = format!("cluster:emb:{}", i + 1);
            let node_count = members.len() as u32;
            out.push(GraphCluster {
                id: cid.clone(),
                name,
                description: Some(format!(
                    "基于 Embedding 语义相似度自动聚类的知识簇，包含 {} 个节点、{} 条内部关系。",
                    node_count, edge_count
                )),
                algorithm: "embedding".to_string(),
                centroid: best_member.map(String::from),
                node_count,
                edge_count,
                confidence: 1.0,
                created_at: now,
                updated_at: now,
                links: Vec::new(),
                top_files: Vec::new(),
            });
            self.store.replace_cluster_members(&cid, &members)?;
        }
        out.sort_by(|a, b| b.node_count.cmp(&a.node_count));
        self.store.replace_clusters(&out)?;
        Ok(out.len())
    }

    /// 全部聚类（按节点数降序；命令层填充 top_files）。
    /// 簇间链接（links）不在库中持久化（无独立列），读取时按跨簇边实时聚合。
    pub fn list(&self, limit: u32) -> Result<Vec<GraphCluster>, String> {
        let mut clusters = self.store.list_clusters(limit)?;
        self.attach_links(&mut clusters)?;
        Ok(clusters)
    }

    /// 单聚类（links 实时聚合）
    pub fn get(&self, id: &str) -> Result<Option<GraphCluster>, String> {
        let mut cluster = match self.store.get_cluster(id)? {
            Some(c) => c,
            None => return Ok(None),
        };
        let mut one = [cluster.clone()];
        self.attach_links(&mut one)?;
        cluster = one.into_iter().next().unwrap_or(cluster);
        Ok(Some(cluster))
    }

    /// 实时聚合簇间链接（跨簇边计数；O(n+e) 单遍）。
    /// 依赖成员表 + path 前缀回退；排除 global 兜底簇。
    fn attach_links(&self, clusters: &mut [GraphCluster]) -> Result<(), String> {
        if clusters.is_empty() {
            return Ok(());
        }
        let node_cluster = self.store.node_cluster_map()?;
        let mut tally: HashMap<(String, String), u32> = HashMap::new();
        for e in self.store.all_edges(1_000_000)? {
            let ca = node_cluster
                .get(&e.source)
                .cloned()
                .unwrap_or_else(|| GLOBAL_CLUSTER_ID.to_string());
            let cb = node_cluster
                .get(&e.target)
                .cloned()
                .unwrap_or_else(|| GLOBAL_CLUSTER_ID.to_string());
            if ca == cb || ca == GLOBAL_CLUSTER_ID || cb == GLOBAL_CLUSTER_ID {
                continue;
            }
            let key = if ca < cb { (ca, cb) } else { (cb, ca) };
            *tally.entry(key).or_insert(0) += 1;
        }
        for c in clusters {
            c.links = tally
                .iter()
                .filter(|((a, b), _)| a == &c.id || b == &c.id)
                .map(|((a, b), count)| ClusterLink {
                    source: a.clone(),
                    target: b.clone(),
                    count: *count,
                })
                .collect();
        }
        Ok(())
    }

    /// 聚类成员 + 簇内边（Cluster 子图；前端 Cluster 展开数据源）。
    pub fn subgraph(&self, id: &str, max_nodes: u32) -> Result<(Vec<GraphNode>, Vec<GraphEdge>), String> {
        let members = self.store.cluster_members(id)?;
        let members: Vec<GraphNode> = members.into_iter().take(max_nodes.max(1).min(2000) as usize).collect();
        let ids: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
        let edges = self.store.cluster_edges(&ids)?;
        Ok((members, edges))
    }

    /// 簇关键文件（按度数 Top N；供 Cluster 节点详情「关键文件」）
    pub fn top_files(&self, id: &str, n: usize) -> Result<Vec<GraphNode>, String> {
        let members = self.store.cluster_members(id)?;
        let mut docs: Vec<GraphNode> = members
            .into_iter()
            .filter(|m| m.node_type == NodeType::Doc && m.path.is_some())
            .collect();
        docs.sort_by(|a, b| b.degree.unwrap_or(0).cmp(&a.degree.unwrap_or(0)));
        docs.truncate(n);
        Ok(docs)
    }
}

/// 按簇分组节点 id（保持 node_cluster map 的所有权）
fn node_cluster_group(node_cluster: &HashMap<String, String>) -> HashMap<String, Vec<String>> {
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for (id, cid) in node_cluster {
        groups.entry(cid.clone()).or_default().push(id.clone());
    }
    groups
}

/// 构造 folder 节点 id（复用 storage 约定；供外部测试/调试）
#[allow(dead_code)]
pub fn folder_node_id(dir: &str) -> String {
    node_id_for(NodeType::Folder, dir)
}

/// 余弦相似度（向量内积 / 模长积；零向量返回 0）
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for k in 0..a.len() {
        dot += a[k] * b[k];
        na += a[k] * a[k];
        nb += b[k] * b[k];
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

// ─── 单元测试 ───

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::graph::model::{GraphNode, Relation};

    fn temp_store(name: &str) -> (GraphStore, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("mdgo_graph_cluster_test_{}_", name))
            .tempdir()
            .unwrap();
        let db = dir.path().join("mdgo.db");
        let store = GraphStore::open_for_dir(dir.path().to_string_lossy().as_ref(), &db).unwrap();
        (store, dir)
    }

    fn doc(id: &str, name: &str, path: &str) -> GraphNode {
        GraphNode {
            id: id.into(),
            node_type: NodeType::Doc,
            name: name.into(),
            path: Some(path.into()),
            meta: None,
            degree: None,
        created_at: None,
        content: None,
        }
    }

    #[test]
    fn test_cluster_id_for_path() {
        assert_eq!(cluster_id_for_path("docs/a.md"), "cluster:docs");
        assert_eq!(cluster_id_for_path("a.md"), ROOT_CLUSTER_ID);
        assert_eq!(cluster_id_for_path("docs/sub/a.md"), "cluster:docs");
        assert_eq!(cluster_display_name("cluster:docs"), "docs");
        assert_eq!(cluster_display_name(ROOT_CLUSTER_ID), "根目录");
    }

    #[test]
    fn test_rebuild_groups_by_top_dir() {
        let (store, _dir) = temp_store("rebuild");
        store.upsert_node(&doc("doc:a.md", "a.md", "docs/a.md")).unwrap();
        store.upsert_node(&doc("doc:b.md", "b.md", "docs/b.md")).unwrap();
        store.upsert_node(&doc("doc:root.md", "root.md", "root.md")).unwrap();
        store
            .upsert_edge(
                &crate::core::graph::model::GraphEdge {
                    source: "doc:a.md".into(),
                    target: "doc:b.md".into(),
                    relation: Relation::References,
                    weight: Some(1.0),
                    confidence: Some(1.0),
                },
                Some("doc:a.md"),
            )
            .unwrap();

        let engine = ClusterEngine::new(&store);
        let n = engine.rebuild().unwrap();
        assert_eq!(n, 2); // cluster:docs + 根目录
        let clusters = engine.list(100).unwrap();
        let docs = clusters.iter().find(|c| c.id == "cluster:docs").unwrap();
        assert_eq!(docs.node_count, 2);
        assert_eq!(docs.edge_count, 1);
        assert_eq!(docs.centroid.as_deref(), Some("doc:a.md"));
        let root = clusters.iter().find(|c| c.id == ROOT_CLUSTER_ID).unwrap();
        assert_eq!(root.node_count, 1);
        assert_eq!(root.centroid.as_deref(), Some("doc:root.md"));
    }

    #[test]
    fn test_cluster_subgraph() {
        let (store, _dir) = temp_store("subgraph");
        store.upsert_node(&doc("doc:a.md", "a.md", "docs/a.md")).unwrap();
        store.upsert_node(&doc("doc:b.md", "b.md", "docs/b.md")).unwrap();
        store.upsert_node(&doc("doc:root.md", "root.md", "root.md")).unwrap();
        store
            .upsert_edge(
                &crate::core::graph::model::GraphEdge {
                    source: "doc:a.md".into(),
                    target: "doc:b.md".into(),
                    relation: Relation::References,
                    weight: Some(1.0),
                    confidence: Some(1.0),
                },
                Some("doc:a.md"),
            )
            .unwrap();
        let engine = ClusterEngine::new(&store);
        engine.rebuild().unwrap();
        let (members, edges) = engine.subgraph("cluster:docs", 100).unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(edges.len(), 1);
    }

    #[test]
    fn test_cluster_links_survive_read() {
        // 回归：rebuild 计算的簇间 links 必须在 list/get 读取时重新聚合（不丢失）
        let (store, _dir) = temp_store("links");
        store.upsert_node(&doc("doc:a.md", "a.md", "docs/a.md")).unwrap();
        store.upsert_node(&doc("doc:b.md", "b.md", "src/b.md")).unwrap();
        store
            .upsert_edge(
                &crate::core::graph::model::GraphEdge {
                    source: "doc:a.md".into(),
                    target: "doc:b.md".into(),
                    relation: Relation::References,
                    weight: Some(1.0),
                    confidence: Some(1.0),
                },
                Some("doc:a.md"),
            )
            .unwrap();
        let engine = ClusterEngine::new(&store);
        engine.rebuild().unwrap();
        let clusters = engine.list(100).unwrap();
        let docs = clusters.iter().find(|c| c.id == "cluster:docs").unwrap();
        let src = clusters.iter().find(|c| c.id == "cluster:src").unwrap();
        // docs 簇应含 1 条到 src 簇的跨簇链接
        assert_eq!(docs.links.len(), 1);
        assert_eq!(docs.links[0].count, 1);
        assert!(docs.links[0].target == "cluster:src" || docs.links[0].source == "cluster:src");
        let _ = src;
        // get 单簇同样带 links
        let one = engine.get("cluster:docs").unwrap().unwrap();
        assert_eq!(one.links.len(), 1);
    }

    #[test]
    fn test_node_cluster_map_covers_all() {
        let (store, _dir) = temp_store("ncm");
        store.upsert_node(&doc("doc:a.md", "a.md", "docs/a.md")).unwrap();
        store.upsert_node(&doc("doc:root.md", "root.md", "root.md")).unwrap();
        let engine = ClusterEngine::new(&store);
        engine.rebuild().unwrap();
        let map = store.node_cluster_map().unwrap();
        assert_eq!(map.get("doc:a.md").map(String::as_str), Some("cluster:docs"));
        assert_eq!(map.get("doc:root.md").map(String::as_str), Some(ROOT_CLUSTER_ID));
    }
}
