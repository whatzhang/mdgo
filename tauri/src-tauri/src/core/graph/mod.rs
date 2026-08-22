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

pub mod builder;
pub mod experience;
pub mod extractor;
pub mod merger;
pub mod model;
pub mod storage;

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
    pub fn build_all(&self, dir_path: &str, ignore: &IgnoreMatcher) -> Result<(), String> {
        if self.is_building(dir_path) {
            return Ok(()); // 已在构建，跳过（幂等）
        }
        self.set_building(dir_path, true);
        let result = (|| {
            let store = self.store(dir_path)?;
            let guard = store.lock().unwrap_or_else(|e| e.into_inner());
            let builder = crate::core::graph::builder::GraphBuilder::new(&guard);
            builder.build_all(dir_path, ignore)
        })();
        self.set_building(dir_path, false);
        result
    }

    /// 增量构建（目录树 + 全文件扫描；全量重建的内部实现，供启动同步/手动刷新）。
    pub fn build_incremental(&self, dir_path: &str, ignore: &IgnoreMatcher) -> Result<(), String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let builder = crate::core::graph::builder::GraphBuilder::new(&guard);
        builder.build_incremental(dir_path, ignore)
    }

    /// 单文件增量（新增/修改）：更新 doc 节点 + 重写出边。watcher 增量链路挂点。
    pub fn build_file(&self, dir_path: &str, rel_path: &str, ignore: &IgnoreMatcher) -> Result<(), String> {
        let store = self.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let builder = crate::core::graph::builder::GraphBuilder::new(&guard);
        builder.build_file(dir_path, rel_path, ignore)
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
