//! Graph Intelligence Layer（知识智能层）——SQLite Graph Engine + Document Graph。
//!
//! 分层（对齐 `docs/graph-os-frontend-design.md`）：
//! - [`model`]：纯数据模型（节点/边/关系/查询结果）
//! - [`storage`]：SQLite 存储（三张图表 + 邻域 BFS + 生命周期级联）
//! - [`builder`]：Document Graph 构建器（规则抽取，Level 1 零 LLM）
//! - [`merger`]：实体消歧与别名合并（Entity Graph 工程质量核心）
//! - [`extractor`]：实体抽取器（三级策略：规则/聚类/LLM）
//! - [`experience`]：Experience Brain（统一事件源 + Problem/Solution/Result 图）
//! - [`engine`]：目录级门面（store 缓存 + 构建调度 + 生命周期钩子），供命令层/索引器/watcher 调用

pub mod ai;
pub mod builder;
pub mod chunk;
pub mod cluster;
pub mod experience;
pub mod extractor;
pub mod merger;
pub mod model;
pub mod storage;
pub mod worker;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::core::db::global::kb_db_path;
use crate::core::db::utils::IgnoreMatcher;
use crate::core::graph::model::{
    GraphNeighborhood, GraphNode, GraphStats, GraphStatus, Relation,
};
use crate::core::graph::storage::GraphStore;

/// 目录级 GraphEngine 门面：store 惰性缓存 + Document Graph 构建调度。
///
/// 与 AppState 共享：`Arc<GraphEngine>` 注入命令层与索引器/watcher，
/// 所有读写路径共用同一把 store 锁（与 schedule/bookmark 模式一致）。
pub struct GraphEngine {
    /// 目录路径（规范键）→ GraphStore
    stores: Mutex<HashMap<String, Arc<Mutex<GraphStore>>>>,
    /// 构建进行中标记（全量重建与 watcher 增量互斥）
    building: Mutex<HashMap<String, bool>>,
}

impl GraphEngine {
    pub fn new() -> Self {
        Self {
            stores: Mutex::new(HashMap::new()),
            building: Mutex::new(HashMap::new()),
        }
    }

    /// 获取（或惰性创建）某知识库目录的 GraphStore（共享 Arc<Mutex>）。
    /// dir_path 先经 `sanitize_kb_dir` 规范化（防穿越 + 同目录多写法命中同一实例）。
    pub fn store(&self, dir_path: &str) -> Result<Arc<Mutex<GraphStore>>, String> {
        let key = crate::core::db::global::sanitize_kb_dir(dir_path)?;
        let key_str = key.to_string_lossy().to_string();
        let db_path = kb_db_path(dir_path)?;
        let mut map = self.stores.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = map.get(&key_str) {
            return Ok(Arc::clone(s));
        }
        let store = Arc::new(Mutex::new(GraphStore::open_for_dir(&key_str, db_path)?));
        map.insert(key_str, store.clone());
        Ok(store)
    }

    fn is_building(&self, dir_path: &str) -> bool {
        self.building
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(dir_path)
            .copied()
            .unwrap_or(false)
    }

    fn set_building(&self, dir_path: &str, v: bool) {
        self.building
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(dir_path.to_string(), v);
    }

    // ─── 构建 ───

    /// 全量重建 Document Graph（先 clear 再扫描；由 kb_index 全量索引后调用，阻塞构建标记）。
    /// 构建完成后自动重算 Cluster（L0 聚合单元）。
    pub fn build_all(&self, dir_path: &str, ignore: &IgnoreMatcher) -> Result<(), String> {
        if self.is_building(dir_path) {
            return Ok(()); // 已在构建，跳过（幂等）
        }
        self.set_building(dir_path, true);
        let result = (|| {
            let store = self.store(dir_path)?;
            // guard 生命周期限定在内部块：无论语义聚类分支是否触发，
            // 出块即释放锁，随后 enqueue_after_build 重新取锁不会死锁。
            {
                let guard = store.lock().unwrap_or_else(|e| e.into_inner());
                let builder = crate::core::graph::builder::GraphBuilder::new(&guard);
                builder.build_all(dir_path, ignore)?;
                // Chunk 图（知识图谱底座 Layer 1）：文档结构层
                crate::core::graph::chunk::ChunkGraphBuilder::new(&guard).build_all(
                    dir_path,
                    crate::core::graph::chunk::CHUNK_MAX_CHARS,
                    crate::core::graph::chunk::CHUNK_OVERLAP,
                )?;
                // 自动规则抽取（Phase 3 起步：Level 1 免费，外部链接 host → 实体）
                {
                    let extractor = crate::core::graph::extractor::EntityExtractor::new(
                        &guard,
                        None::<&dyn crate::core::graph::extractor::EntityLlmExtractor>,
                    );
                    if let Err(e) = extractor.extract_all_docs(dir_path, 20_000) {
                        log::warn!("[graph] 规则实体抽取失败: {}", e);
                    }
                }
                // Cluster（Level 1 目录聚类；零 LLM）
                crate::core::graph::cluster::ClusterEngine::new(&guard).rebuild()?;
                guard.set_property("graph_cluster_mode", "directory")?;
                // 语义聚类默认化（Phase 2）：本地模型就绪且本知识库未尝试过 → 尝试一次；
                // 成功则模式变为 embedding（前端默认展示语义簇）。
                // D6 修复：仅「成功」置位 graph_auto_semantic_done——失败不置位，
                // 模型就绪后的下次构建自动重试（此前先置位再尝试，失败永不重试）。
                if crate::core::db::utils::is_model_ready()
                    && guard.get_property("graph_auto_semantic_done")?.is_none()
                {
                    drop(guard); // 先释放锁（embed_clusters 内部会重新取锁，避免死锁）
                    match self.embed_clusters(dir_path, 300) {
                        Ok(_) => {
                            if let Ok(store) = self.store(dir_path) {
                                if let Ok(g) = store.lock() {
                                    let _ = g.set_property("graph_auto_semantic_done", "1");
                                }
                            }
                        }
                        Err(e) => log::warn!(
                            "[graph] 语义聚类默认化失败（模型就绪后下次构建将自动重试，当前保持目录聚类）: {}",
                            e
                        ),
                    }
                }
            }
            // AI 工作队列（Phase 3）：全量构建后按重要度入队，后台 worker 异步抽取。
            // 入队失败不影响构建结果（仅告警日志）。
            let _ = self.enqueue_after_build(dir_path);
            Ok(())
        })();
        self.set_building(dir_path, false);
        result
    }

    /// 增量构建（目录树 + 全文件扫描；全量重建的内部实现，供启动同步/手动刷新）。
    /// 构建完成后自动重算 Cluster 与 Chunk 图，并跑 Level 1 规则实体抽取。
    pub fn build_incremental(&self, dir_path: &str, ignore: &IgnoreMatcher) -> Result<(), String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let builder = crate::core::graph::builder::GraphBuilder::new(&guard);
        builder.build_incremental(dir_path, ignore)?;
        crate::core::graph::chunk::ChunkGraphBuilder::new(&guard).build_all(
            dir_path,
            crate::core::graph::chunk::CHUNK_MAX_CHARS,
            crate::core::graph::chunk::CHUNK_OVERLAP,
        )?;
        // 自动规则抽取（Level 1 免费）
        {
            let extractor = crate::core::graph::extractor::EntityExtractor::new(
                &guard,
                None::<&dyn crate::core::graph::extractor::EntityLlmExtractor>,
            );
            if let Err(e) = extractor.extract_all_docs(dir_path, 20_000) {
                log::warn!("[graph] 规则实体抽取失败: {}", e);
            }
        }
        crate::core::graph::cluster::ClusterEngine::new(&guard).rebuild()?;
        guard.set_property("graph_cluster_mode", "directory")?;
        drop(guard); // 释放锁后再入队（enqueue 内部重新取锁，避免死锁）
        let _ = self.enqueue_after_build(dir_path);
        Ok(())
    }

    /// 单文件增量（新增/修改）：更新 doc 节点 + 重写出边 + 重建 chunk/section。watcher 增量链路挂点。
    pub fn build_file(&self, dir_path: &str, rel_path: &str, ignore: &IgnoreMatcher) -> Result<(), String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let builder = crate::core::graph::builder::GraphBuilder::new(&guard);
        builder.build_file(dir_path, rel_path, ignore)?;
        crate::core::graph::chunk::ChunkGraphBuilder::new(&guard).build_file(
            dir_path,
            rel_path,
            crate::core::graph::chunk::CHUNK_MAX_CHARS,
            crate::core::graph::chunk::CHUNK_OVERLAP,
        )?;
        // AI 队列：单文档入队（重要度按 度/最大度 + 新鲜度 + 文件名启发式）
        let (degree, max_degree) = guard.doc_degree_rank(rel_path)?;
        let score = {
            const RECENCY_WINDOW_MS: f64 = 90.0 * 24.0 * 3600.0 * 1000.0;
            let now = crate::core::graph::storage::now_ms_public() as f64;
            let abs = std::path::Path::new(dir_path).join(rel_path);
            let mtime = std::fs::metadata(&abs)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0);
            let recency = if mtime > 0.0 {
                (1.0 - (now - mtime) / RECENCY_WINDOW_MS).clamp(0.0, 1.0)
            } else {
                0.3
            };
            let base = rel_path.rsplit('/').next().unwrap_or(rel_path).to_lowercase();
            let name_bonus = if ["readme", "设计", "方案", "架构", "总结", "指南", "guide", "design"]
                .iter()
                .any(|k| base.contains(k))
            {
                0.2
            } else {
                0.0
            };
            0.5 * (degree as f64 / max_degree as f64) + 0.3 * recency + name_bonus
        };
        drop(guard);
        let _ = self.enqueue_ai_docs(dir_path, &[(rel_path.to_string(), score)], true);
        Ok(())
    }

    /// 文件/目录删除（生命周期级联）：删节点 + 级联边。返回删除的节点数。
    pub fn remove_path(&self, dir_path: &str, rel_path: &str) -> Result<u64, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let builder = crate::core::graph::builder::GraphBuilder::new(&guard);
        builder.remove_path(rel_path)
    }

    /// 全量重建后调用：清空构建标记并释放 store 缓存（下次访问重新打开）。
    pub fn invalidate(&self, dir_path: &str) {
        self.set_building(dir_path, false);
        let key = match crate::core::db::global::sanitize_kb_dir(dir_path) {
            Ok(k) => k.to_string_lossy().to_string(),
            Err(_) => return,
        };
        if let Ok(mut map) = self.stores.lock() {
            map.remove(&key);
        }
    }

    // ─── 后台 AI 工作队列（Phase 3 完整形态） ───

    /// 当前活跃知识库目录（有 store 实例 = 至少打开过一次；worker 轮询这些目录）。
    pub fn active_dirs(&self) -> Vec<String> {
        self.stores
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// 文档重要度评分（0..=1）：度归一化(0.5) + 文件新鲜度(0.3) + 文件名启发式(0.2)。
    /// - 度：边数 / 全库最大度（文档在图中的连通重要性）；
    /// - 新鲜度：mtime 距今 90 天内线性衰减（近期改动的文档更可能是当前关注点）；
    /// - 文件名：README/设计/方案/架构/总结/指南 等 → +0.2。
    /// 返回按重要度降序的 (rel_path, score)。
    pub fn ai_priority_docs(&self, dir_path: &str) -> Result<Vec<(String, f64)>, String> {
        const RECENCY_WINDOW_MS: f64 = 90.0 * 24.0 * 3600.0 * 1000.0;
        const KEYWORDS: [&str; 8] = ["readme", "设计", "方案", "架构", "总结", "指南", "guide", "design"];
        let store = self.store(dir_path)?;
        let now = crate::core::graph::storage::now_ms_public() as f64;
        let (pairs, max_degree) = {
            let guard = store.lock().unwrap_or_else(|e| e.into_inner());
            let degrees = guard.doc_degrees()?;
            let max_degree = degrees.iter().map(|(_, d)| *d).max().unwrap_or(0).max(1) as f64;
            (degrees, max_degree)
        };
        let mut scored: Vec<(String, f64)> = Vec::with_capacity(pairs.len());
        for (path, degree) in pairs {
            let abs = std::path::Path::new(dir_path).join(&path);
            let mtime = std::fs::metadata(&abs)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0);
            let recency = if mtime > 0.0 {
                (1.0 - (now - mtime) / RECENCY_WINDOW_MS).clamp(0.0, 1.0)
            } else {
                0.3 // 无 mtime（非常规文件）给中性分
            };
            let base = path.rsplit('/').next().unwrap_or(&path).to_lowercase();
            let name_bonus = if KEYWORDS.iter().any(|k| base.contains(k)) { 0.2 } else { 0.0 };
            let score = 0.5 * (degree as f64 / max_degree) + 0.3 * recency + name_bonus;
            scored.push((path, score));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored)
    }

    /// 批量入队（构建后调用；`reset_done=true` 用于单文件变更强制重抽取）。
    pub fn enqueue_ai_docs(
        &self,
        dir_path: &str,
        docs: &[(String, f64)],
        reset_done: bool,
    ) -> Result<usize, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.enqueue_ai_docs(dir_path, docs, reset_done)
    }

    /// 取下一批待处理文档（按重要度降序；worker 每轮调用）。
    pub fn next_ai_batch(&self, dir_path: &str, limit: u32) -> Result<Vec<model::AiQueueItem>, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.next_ai_batch(dir_path, limit)
    }

    /// 队列项处理完成回调（ok → done；否则重试/失败）。
    pub fn finish_ai_item(&self, dir_path: &str, id: i64, ok: bool, max_attempts: u32) -> Result<(), String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.finish_ai_item(id, ok, max_attempts)
    }

    /// 队列统计 (pending, processing, done, failed)。
    pub fn queue_stats(&self, dir_path: &str) -> Result<(u64, u64, u64, u64), String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.queue_stats(dir_path)
    }

    /// 构建后入队（内部：重要度评分 → 批量入队）。构建 hook 共用。
    /// D4：公开为「重新分析」入口（命令层 graph_ai_enqueue_all 调用），
    /// 返回参与入队/更新的文档数（done 项不重复处理；failed 项回退重试）。
    pub fn enqueue_after_build(&self, dir_path: &str) -> Result<usize, String> {
        let docs = self.ai_priority_docs(dir_path)?;
        let n = self.enqueue_ai_docs(dir_path, &docs, false)?;
        log::info!("[graph] AI 队列已更新（{} 个文档，其中新入队/重置 {} 条）", docs.len(), n);
        Ok(n)
    }

    /// 全库重新入队（D4）：等价 enqueue_after_build——已 done 文档不重复处理，
    /// 未处理/失败的文档按最新重要度重新排队；供「重新分析」按钮 / 配置变更后调用。
    pub fn requeue_all(&self, dir_path: &str) -> Result<usize, String> {
        self.enqueue_after_build(dir_path)
    }

    // ─── 查询（委托 storage） ───

    pub fn status(&self, dir_path: &str) -> Result<GraphStatus, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.status()
    }

    pub fn stats(&self, dir_path: &str) -> Result<GraphStats, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.stats()
    }

    pub fn neighborhood(
        &self,
        dir_path: &str,
        node_id: &str,
        depth: u32,
        max_nodes: u32,
        max_edges: u32,
        relations: Option<Vec<Relation>>,
        weight_min: f32,
    ) -> Result<GraphNeighborhood, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.neighborhood(
            node_id,
            depth,
            max_nodes,
            max_edges,
            relations.as_deref(),
            weight_min,
        )
    }

    pub fn expand(
        &self,
        dir_path: &str,
        node_id: &str,
        max_nodes: u32,
        relations: Option<Vec<Relation>>,
        weight_min: f32,
        max_edges: u32,
    ) -> Result<GraphNeighborhood, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.expand(node_id, max_nodes, relations.as_deref(), weight_min, max_edges)
    }

    pub fn search(&self, dir_path: &str, keyword: &str, limit: u32) -> Result<Vec<GraphNode>, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.search_nodes(keyword, limit)
    }

    /// 根据文件相对路径查 doc 节点 id（链接解析跳转/前端定位用）。
    pub fn node_id_for_path(&self, _dir_path: &str, path: &str) -> String {
        crate::core::graph::storage::node_id_for(model::NodeType::Doc, path)
    }

    // ─── Cluster（L0 聚合单元；PRD §11/§13/§39） ───

    /// 手动重算 Cluster（build_all/incremental 已自动调用；供前端「重新聚类」）。
    pub fn rebuild_clusters(&self, dir_path: &str) -> Result<usize, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let n = crate::core::graph::cluster::ClusterEngine::new(&guard).rebuild()?;
        let _ = guard.set_property("graph_cluster_mode", "directory");
        Ok(n)
    }

    /// 语义聚类（Embedding，Phase 2）：doc 概览向量 → 贪婪聚类替换目录簇，
    /// 成功后记录 cluster_mode=embedding（模型未就绪/失败返回 Err，调用方降级）。
    /// 同步实现（内部含本地 embedding 推理；调用方用 spawn_blocking 包裹避免阻塞 UI）。
    pub fn embed_clusters(&self, dir_path: &str, doc_limit: u32) -> Result<usize, String> {
        // 1) doc 概览文本（前 300 字符；成本控制 PRD §75）
        let docs: Vec<(String, String)> = {
            let store = self.store(dir_path)?;
            let guard = store.lock().unwrap_or_else(|e| e.into_inner());
            let paths = guard.priority_doc_paths(doc_limit.min(500))?;
            let mut out = Vec::new();
            for p in paths {
                let abs = std::path::Path::new(dir_path).join(&p);
                if let Ok(c) = std::fs::read_to_string(&abs) {
                    let head: String = c.chars().take(300).collect();
                    if !head.trim().is_empty() {
                        out.push((format!("doc:{}", p.replace('\\', "/")), head));
                    }
                }
            }
            out
        };
        if docs.is_empty() {
            return Err("没有可嵌入的文档".to_string());
        }
        // 2) 本地 embedding（同步推理）
        let refs: Vec<&str> = docs.iter().map(|(_, t)| t.as_str()).collect();
        let embeddings = crate::core::db::utils::call_embedding(&refs, None)?;
        // 3) 语义聚类（替换目录簇）
        let samples: Vec<(String, Vec<f32>)> = docs
            .into_iter()
            .zip(embeddings.into_iter())
            .map(|((id, _), emb)| (id, emb))
            .collect();
        let n = self.recluster_embedding(dir_path, &samples)?;
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let _ = guard.set_property("graph_cluster_mode", "embedding");
        Ok(n)
    }

    /// Embedding 语义重聚类（PRD §11.1 Level 3；samples = (doc_id, embedding)）。
    pub fn recluster_embedding(
        &self,
        dir_path: &str,
        samples: &[(String, Vec<f32>)],
    ) -> Result<usize, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        crate::core::graph::cluster::ClusterEngine::new(&guard).rebuild_from_embeddings(samples)
    }

    // ─── Chunk 图（知识图谱底座 Layer 1；build_all/incremental 已自动构建） ───

    /// 手动重建全部 chunk/section 子图（幂等；供「重新构建内容层」入口）。
    pub fn rebuild_chunks(&self, dir_path: &str) -> Result<crate::core::graph::chunk::ChunkBuildStats, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        crate::core::graph::chunk::ChunkGraphBuilder::new(&guard).build_all(
            dir_path,
            crate::core::graph::chunk::CHUNK_MAX_CHARS,
            crate::core::graph::chunk::CHUNK_OVERLAP,
        )
    }

    /// 某文档的内容节点（chunk/section，含 content；L4 细粒度/详情数据源）。
    pub fn content_nodes_for_path(
        &self,
        dir_path: &str,
        rel_path: &str,
    ) -> Result<Vec<model::GraphNode>, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.list_content_nodes_for_doc(rel_path, 1000)
    }

    /// 全部聚类（命令层填充 top_files 与 links 已内嵌）。
    pub fn clusters(&self, dir_path: &str, limit: u32) -> Result<Vec<model::GraphCluster>, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let engine = crate::core::graph::cluster::ClusterEngine::new(&guard);
        let mut list = engine.list(limit.max(1).min(1000))?;
        // top_files（关键文件 Top 5；PRD §17 概览 Tab）
        for c in &mut list {
            c.top_files = engine.top_files(&c.id, 5)?;
        }
        Ok(list)
    }

    /// 单聚类（含 top_files）
    pub fn cluster(&self, dir_path: &str, cluster_id: &str) -> Result<Option<model::GraphCluster>, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let engine = crate::core::graph::cluster::ClusterEngine::new(&guard);
        let mut c = match engine.get(cluster_id)? {
            Some(c) => c,
            None => return Ok(None),
        };
        c.top_files = engine.top_files(cluster_id, 5)?;
        Ok(Some(c))
    }

    /// Cluster 子图（成员 + 簇内边；前端 Cluster 展开数据源）
    pub fn cluster_subgraph(
        &self,
        dir_path: &str,
        cluster_id: &str,
        max_nodes: u32,
    ) -> Result<model::GraphNeighborhood, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let engine = crate::core::graph::cluster::ClusterEngine::new(&guard);
        let (nodes, edges) = engine.subgraph(cluster_id, max_nodes)?;
        Ok(model::GraphNeighborhood {
            nodes,
            edges,
            truncated: false,
        })
    }

    // ─── Graph Query API（PRD §24/§36） ───

    /// 图版本（每次图变更 +1；前端缓存失效依据）
    pub fn graph_version(&self, dir_path: &str) -> Result<u64, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.graph_version()
    }

    /// 两节点最短路径
    pub fn find_path(
        &self,
        dir_path: &str,
        source: &str,
        target: &str,
        max_depth: u32,
    ) -> Result<model::GraphPath, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.find_path(source, target, max_depth, 500)
    }

    /// 共同邻居
    pub fn common_neighbors(
        &self,
        dir_path: &str,
        a: &str,
        b: &str,
    ) -> Result<model::GraphNeighborhood, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.common_neighbors(a, b)
    }

    /// 子图（BFS 深度扩展）
    pub fn subgraph(
        &self,
        dir_path: &str,
        node_id: &str,
        depth: u32,
        max_nodes: u32,
        max_edges: u32,
    ) -> Result<model::GraphNeighborhood, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.subgraph(node_id, depth, max_nodes, max_edges)
    }

    // ─── Phase 3：Entity Graph（实体抽取，Level 1 规则 + 可选 LLM）───

    /// 单文件实体抽取（Level 1 规则）：索引联动后调用，产出 DERIVED_FROM 边。
    /// 同步执行（规则抽取无 IO 阻塞点）；失败仅告警不阻断调用方。
    pub fn extract_entities_file(&self, dir_path: &str, rel_path: &str, content: Option<&str>) -> Result<(), String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let content_owned;
        let content: &str = match content {
            Some(c) => c,
            None => {
                let abs = std::path::Path::new(dir_path).join(rel_path);
                content_owned = match std::fs::read_to_string(&abs) {
                    Ok(c) => c,
                    Err(_) => return Ok(()), // 不可读 → 跳过（降级）
                };
                &content_owned
            }
        };
        let extractor = crate::core::graph::extractor::EntityExtractor::new(
            &guard,
            None::<&dyn crate::core::graph::extractor::EntityLlmExtractor>,
        );
        // Level 1 规则候选直接入库（消歧合并）
        let source_id = crate::core::graph::storage::node_id_for(model::NodeType::Doc, rel_path);
        let mut merger = crate::core::graph::merger::EntityMerger::new(&guard);
        let candidates = extractor.rule_candidates(rel_path, content);
        for (name, aliases) in candidates {
            if let Err(e) = merger.upsert_entity(&name, &aliases, Some(&source_id)) {
                log::warn!("[graph] 实体入库失败 ({}): {}", name, e);
            }
        }
        Ok(())
    }

    /// 全库实体抽取（Level 1 规则，批处理）：遍历 doc 节点执行。
    /// 由命令层手动触发（graph_extract_entities）；成功返回抽取的实体候选数。
    pub fn extract_entities_all(&self, dir_path: &str) -> Result<usize, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let extractor = crate::core::graph::extractor::EntityExtractor::new(
            &guard,
            None::<&dyn crate::core::graph::extractor::EntityLlmExtractor>,
        );
        extractor.extract_all_docs(dir_path, 20_000)
    }

    // ─── Phase 4：Experience Brain（统一事件源 + P/S/R 图）───

    /// 记录一条经验事件并写入图（problem→solution→doc）。
    pub fn experience_record(
        &self,
        dir_path: &str,
        event: &crate::core::graph::experience::ExperienceEvent,
    ) -> Result<(), String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let brain = crate::core::graph::experience::ExperienceBrain::new(
            &guard,
            None::<&dyn crate::core::graph::experience::ExperienceLlmExtractor>,
        );
        brain.record(event)
    }

    /// 记录经验事件并做 LLM 富化（Phase 4.2）：锁外调用 LLM 抽取
    /// (problem, solution)，锁内写图；LLM 未配置/失败 → 规则降级（fail-open）。
    /// LLM 调用计数/失败计数写入 graph_metrics（PRD §74）。
    pub async fn experience_record_ai(
        &self,
        dir_path: &str,
        event: &crate::core::graph::experience::ExperienceEvent,
        llm: Option<&dyn crate::core::graph::experience::ExperienceLlmExtractor>,
    ) -> Result<(), String> {
        // 1) LLM 抽取（锁外，异步）
        let mut extracted: Option<(String, String)> = None;
        if let Some(llm) = llm {
            match llm.extract_problem_solution(&event.title, &event.body).await {
                Ok((p, s)) => {
                    self.bump_metric(dir_path, "llm_calls", 1);
                    if !p.trim().is_empty() || !s.trim().is_empty() {
                        extracted = Some((p, s));
                    }
                }
                Err(e) => {
                    log::warn!("[graph] 经验 LLM 抽取失败（规则降级）: {}", e);
                    self.bump_metric(dir_path, "llm_failures", 1);
                }
            }
        }
        // 2) 锁内写图（同步）
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let brain = crate::core::graph::experience::ExperienceBrain::new(
            &guard,
            None::<&dyn crate::core::graph::experience::ExperienceLlmExtractor>,
        );
        match extracted {
            Some((problem, solution)) => brain.record_extracted(event, &problem, &solution),
            None => brain.record(event),
        }
    }

    /// 指标计数（bump_metric；graph_metrics 表）
    fn bump_metric(&self, dir_path: &str, key: &str, delta: i64) {
        if let Ok(store) = self.store(dir_path) {
            if let Ok(guard) = store.lock() {
                let _ = guard.bump_metric(key, delta);
            }
        }
    }

    /// 「类似问题」检索（规则打分）
    pub fn experience_search(
        &self,
        dir_path: &str,
        problem: &str,
        limit: usize,
    ) -> Result<Vec<crate::core::graph::experience::ExperienceHit>, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let brain = crate::core::graph::experience::ExperienceBrain::new(
            &guard,
            None::<&dyn crate::core::graph::experience::ExperienceLlmExtractor>,
        );
        brain.search_similar(problem, limit)
    }

    /// 全部经验事件
    pub fn experience_events(
        &self,
        dir_path: &str,
    ) -> Result<Vec<crate::core::graph::experience::ExperienceEvent>, String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let brain = crate::core::graph::experience::ExperienceBrain::new(
            &guard,
            None::<&dyn crate::core::graph::experience::ExperienceLlmExtractor>,
        );
        brain.all_events()
    }
}

impl Default for GraphEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::graph::model::{GraphEdge, GraphNode, NodeType, Relation};

    /// 临时知识库目录 + 已就绪的 GraphEngine（GraphStore 经 kb_db_path 落 `{dir}/.mdgo/mdgo.db`）
    fn temp_engine(name: &str) -> (GraphEngine, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("mdgo_graph_engine_test_{}_", name))
            .tempdir()
            .unwrap();
        let engine = GraphEngine::new();
        engine.store(dir.path().to_string_lossy().as_ref()).unwrap();
        (engine, dir)
    }

    fn upsert_doc(engine: &GraphEngine, dir: &std::path::Path, path: &str) {
        let store = engine.store(dir.to_string_lossy().as_ref()).unwrap();
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .upsert_node(&GraphNode {
                id: crate::core::graph::storage::node_id_for(NodeType::Doc, path),
                node_type: NodeType::Doc,
                name: path.to_string(),
                path: Some(path.to_string()),
                meta: None,
                degree: None,
                created_at: None,
                content: None,
            })
            .unwrap();
    }

    fn link(engine: &GraphEngine, dir: &std::path::Path, a: &str, b: &str) {
        let store = engine.store(dir.to_string_lossy().as_ref()).unwrap();
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .upsert_edge(
                &GraphEdge {
                    source: crate::core::graph::storage::node_id_for(NodeType::Doc, a),
                    target: crate::core::graph::storage::node_id_for(NodeType::Doc, b),
                    relation: Relation::References,
                    weight: Some(1.0),
                    confidence: Some(1.0),
                },
                Some(a),
            )
            .unwrap();
    }

    #[test]
    fn test_ai_priority_docs_orders_by_degree_recency_and_name() {
        let (engine, dir) = temp_engine("priority");
        let dir_path = dir.path().to_string_lossy().to_string();
        // 3 个文档：README.md（1 条边，README 命名加分）、leaf.md（1 条边）、orphan.md（0 条边）
        for p in ["README.md", "leaf.md", "orphan.md"] {
            std::fs::write(dir.path().join(p), format!("# {}\n内容", p)).unwrap();
            upsert_doc(&engine, dir.path(), p);
        }
        link(&engine, dir.path(), "README.md", "leaf.md");

        let scored = engine.ai_priority_docs(&dir_path).unwrap();
        let order: Vec<&str> = scored.iter().map(|(p, _)| p.as_str()).collect();
        // README（度数与 leaf 相同，但命名 +0.2）应排第一
        assert_eq!(order[0], "README.md");
        // orphan 度数最低且无命名加分 → 最后
        assert_eq!(*order.last().unwrap(), "orphan.md");
        // 分数范围 0..=1（README +0.2 封顶）
        for (_, s) in &scored {
            assert!((0.0..=1.0).contains(s), "重要度越界: {}", s);
        }
        // 入队后队列可见
        let n = engine.enqueue_ai_docs(&dir_path, &scored, false).unwrap();
        assert!(n >= 3);
        let (pending, _p, _d, _f) = engine.queue_stats(&dir_path).unwrap();
        assert_eq!(pending, 3);
    }

    #[test]
    fn test_ai_queue_batch_roundtrip_via_engine() {
        let (engine, dir) = temp_engine("batch");
        let dir_path = dir.path().to_string_lossy().to_string();
        engine
            .enqueue_ai_docs(
                &dir_path,
                &[("a.md".to_string(), 0.9), ("b.md".to_string(), 0.4)],
                false,
            )
            .unwrap();
        let batch = engine.next_ai_batch(&dir_path, 1).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].rel_path, "a.md");
        engine.finish_ai_item(&dir_path, batch[0].id, true, 3).unwrap();
        let (pending, _p, done, _f) = engine.queue_stats(&dir_path).unwrap();
        assert_eq!(pending, 1);
        assert_eq!(done, 1);
        // 取完剩余
        let batch2 = engine.next_ai_batch(&dir_path, 10).unwrap();
        assert_eq!(batch2.len(), 1);
        assert_eq!(batch2[0].rel_path, "b.md");
    }
}
