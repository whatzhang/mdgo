//! 图谱 SQLite 存储（Graph Store）。
//!
//! 数据落在知识库级统一数据库 `{dir}/.mdgo/mdgo.db` 的三张图专用表
//! （graph_nodes / graph_edges / graph_properties），与 schedule/bookmark 等
//! 共用同一文件但独立连接（WAL 多连接并发，写竞争由 busy_timeout 兜底）。
//!
//! 设计要点（对齐 `docs/graph-os-frontend-design.md` §三）：
//! - `graph_edges.source_id`：边的来源标识（doc/chunk/commit/event id），
//!   生命周期级联删除（`delete_by_source`）的根本——文件删除时按 source_id 清边，
//!   不残留孤儿边。
//! - 复合索引 `(source, relation)` / `(target, relation)`：邻域 BFS 每次查询
//!   都带 relation 过滤，单列索引不够。
//! - `UNIQUE(source, target, relation)`：LLM 重复抽取/重复构建去重，重复边合并增重。
//! - WAL + busy_timeout：与 mdgo.db 其它连接兼容。

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, params_from_iter, Connection, OptionalExtension};

use super::model::{
    AiQueueItem, GraphAiCandidate, GraphCluster, GraphEdge, GraphEvolution, GraphNeighborhood,
    GraphNode, GraphPath, GraphStats, GraphStatus, NodeType, Relation,
};

/// 图 schema 版本（升级时自动重建，与 BM25 `.schema_v4` 同模式）
/// V2：新增 graph_clusters 表 + graph_version 属性
/// V3：新增 graph_ai_candidates / graph_favorites / graph_metrics（PRD §32/§50/§74）
/// V4：graph_nodes 新增 content 列（chunk/section 文本；知识图谱底座 Layer 1）
pub const GRAPH_SCHEMA_VERSION: u32 = 4;
const SCHEMA_MARKER: &str = "graph_schema_version";
/// 图版本属性键（PRD §42/§73：每次图变更 +1，前端缓存失效依据）
const VERSION_MARKER: &str = "graph_version";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 公开的毫秒时间戳（供 cluster/experience 等模块写入时间用）
pub fn now_ms_public() -> i64 {
    now_ms()
}

/// 邻域查询默认上限（与前端契约 graph-model QUERY_LIMITS 一致）
pub const DEFAULT_MAX_NODES: u32 = 200;
pub const DEFAULT_MAX_EDGES: u32 = 400;
pub const DEFAULT_DEPTH: u32 = 2;
pub const DEFAULT_WEIGHT_MIN: f32 = 0.3;

/// AI 队列项「processing 卡死」判定阈值（worker 轮询周期的 3 倍；
/// 超过该时长未更新的 processing 项视为上次运行崩溃残留，重置回 pending）。
const AI_QUEUE_STALE_MS: i64 = 90_000;

pub struct GraphStore {
    conn: Connection,
    dir_path: String,
}

impl GraphStore {
    /// 打开（或创建）某知识库目录的图存储：`{dir}/.mdgo/mdgo.db` 中三张图表。
    pub fn open_for_dir(dir_path: &str, db_path: impl Into<std::path::PathBuf>) -> Result<Self, String> {
        let db_path = db_path.into();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建图数据目录失败: {}", e))?;
        }
        let conn = Connection::open(&db_path).map_err(|e| format!("打开图数据库失败: {}", e))?;
        crate::core::db::pool::apply_pragmas(&conn)?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn,
            dir_path: dir_path.to_string(),
        })
    }

    pub fn dir_path(&self) -> &str {
        &self.dir_path
    }

    /// 幂等建表 + schema 版本检查（不匹配则重建图数据，破坏性但一致）。
    fn init_schema(conn: &Connection) -> Result<(), String> {
        // 必须先建 graph_properties 表再查版本标记（首次建库时该表不存在，
        // 直接 SELECT 会报 "no such table" 导致 open_for_dir 失败）
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS graph_properties (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )
        .map_err(|e| format!("初始化图属性表失败: {}", e))?;

        // 读取版本标记（TEXT 列）：解析失败/不存在/不匹配一律重建（开发期不做旧数据兼容）。
        // value 列非空（INSERT 恒有值），直接 get::<String>；.optional() 把无行转为 Ok(None)。
        let version: Option<String> = conn
            .query_row(
                "SELECT value FROM graph_properties WHERE key = ?1",
                params![SCHEMA_MARKER],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| format!("读取图 schema 版本失败: {}", e))?;
        let version_num: Option<u32> = version.as_deref().and_then(|v| v.parse().ok());

        if version_num != Some(GRAPH_SCHEMA_VERSION) {
            // 重建：删除图相关表（保留其它域数据）
            conn.execute_batch(
                "DROP TABLE IF EXISTS graph_edges;
                 DROP TABLE IF EXISTS graph_clusters;
                 DROP TABLE IF EXISTS graph_ai_candidates;
                 DROP TABLE IF EXISTS graph_ai_queue;
                 DROP TABLE IF EXISTS graph_favorites;
                 DROP TABLE IF EXISTS graph_metrics;
                 DROP TABLE IF EXISTS graph_properties;
                 DROP TABLE IF EXISTS graph_nodes;",
            )
            .map_err(|e| format!("清除旧图数据失败: {}", e))?;
            log::info!(
                "[graph] schema 版本不匹配（期望 {}，实际 {:?}），已重建图数据",
                GRAPH_SCHEMA_VERSION,
                version
            );
        }

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS graph_nodes (
                id         TEXT PRIMARY KEY,
                type       TEXT NOT NULL,
                name       TEXT NOT NULL,
                path       TEXT,
                meta       TEXT,
                content    TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS graph_edges (
                id         TEXT PRIMARY KEY,
                source     TEXT NOT NULL,
                target     TEXT NOT NULL,
                relation   TEXT NOT NULL,
                weight     REAL NOT NULL DEFAULT 1.0,
                confidence REAL NOT NULL DEFAULT 1.0,
                source_id  TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE(source, target, relation)
            );
            CREATE INDEX IF NOT EXISTS idx_edge_src_rel ON graph_edges(source, relation);
            CREATE INDEX IF NOT EXISTS idx_edge_tgt_rel ON graph_edges(target, relation);
            CREATE INDEX IF NOT EXISTS idx_edge_source_id ON graph_edges(source_id);
            CREATE INDEX IF NOT EXISTS idx_node_type ON graph_nodes(type);
            CREATE TABLE IF NOT EXISTS graph_clusters (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                description TEXT,
                algorithm   TEXT NOT NULL DEFAULT 'directory',
                centroid    TEXT,
                node_count  INTEGER NOT NULL DEFAULT 0,
                edge_count  INTEGER NOT NULL DEFAULT 0,
                confidence  REAL NOT NULL DEFAULT 1.0,
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_cluster_count ON graph_clusters(node_count);
            CREATE TABLE IF NOT EXISTS graph_cluster_members (
                cluster_id TEXT NOT NULL,
                node_id    TEXT NOT NULL,
                PRIMARY KEY (cluster_id, node_id)
            );
            CREATE INDEX IF NOT EXISTS idx_cm_cluster ON graph_cluster_members(cluster_id);
            CREATE TABLE IF NOT EXISTS graph_ai_candidates (
                id          TEXT PRIMARY KEY,
                source      TEXT NOT NULL,
                target      TEXT NOT NULL,
                relation    TEXT NOT NULL,
                confidence  REAL NOT NULL,
                status      TEXT NOT NULL DEFAULT 'candidate',
                source_doc  TEXT,
                evidence    TEXT,
                created_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_candidate_status ON graph_ai_candidates(status);
            CREATE TABLE IF NOT EXISTS graph_ai_queue (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                dir_path   TEXT NOT NULL,
                rel_path   TEXT NOT NULL,
                importance REAL NOT NULL DEFAULT 0,
                status     TEXT NOT NULL DEFAULT 'pending',
                attempts   INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE(dir_path, rel_path)
            );
            CREATE INDEX IF NOT EXISTS idx_aiq_status ON graph_ai_queue(status, importance DESC);
            CREATE TABLE IF NOT EXISTS graph_favorites (
                node_id     TEXT PRIMARY KEY,
                created_at  INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS graph_metrics (
                key     TEXT PRIMARY KEY,
                value   INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS graph_properties (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )
        .map_err(|e| format!("初始化图数据表失败: {}", e))?;

        // 写回 schema 版本标记
        conn.execute(
            "INSERT OR REPLACE INTO graph_properties (key, value) VALUES (?1, ?2)",
            params![SCHEMA_MARKER, GRAPH_SCHEMA_VERSION.to_string()],
        )
        .map_err(|e| format!("写入图 schema 版本失败: {}", e))?;
        Ok(())
    }

    // ─── 节点 CRUD ───

    pub fn upsert_node(&self, node: &GraphNode) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO graph_nodes (id, type, name, path, meta, content, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                    type=excluded.type, name=excluded.name, path=excluded.path, meta=excluded.meta, content=excluded.content",
                params![
                    node.id,
                    node.node_type.as_str(),
                    node.name,
                    node.path,
                    node.meta,
                    node.content,
                    now_ms()
                ],
            )
            .map_err(|e| format!("写入图节点失败: {}", e))?;
        self.bump_version()?;
        Ok(())
    }

    pub fn upsert_nodes(&self, nodes: &[GraphNode]) -> Result<(), String> {
        for n in nodes {
            self.upsert_node(n)?;
        }
        Ok(())
    }

    pub fn get_node(&self, id: &str) -> Result<Option<GraphNode>, String> {
        self.conn
            .query_row(
                "SELECT id, type, name, path, meta, content, created_at FROM graph_nodes WHERE id = ?1",
                params![id],
                |row| {
                    Ok(GraphNode {
                        id: row.get(0)?,
                        node_type: NodeType::from_str(&row.get::<_, String>(1)?),
                        name: row.get(2)?,
                        path: row.get(3)?,
                        meta: row.get(4)?,
                        degree: None,
                        created_at: row.get(6)?,
                        content: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(|e| format!("读取图节点失败: {}", e))
    }

    pub fn delete_node(&self, id: &str) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM graph_nodes WHERE id = ?1", params![id])
            .map_err(|e| format!("删除图节点失败: {}", e))?;
        // 级联清边（node 作为 source 或 target）
        self.conn
            .execute(
                "DELETE FROM graph_edges WHERE source = ?1 OR target = ?1",
                params![id],
            )
            .map_err(|e| format!("级联删除图边失败: {}", e))?;
        self.bump_version()?;
        Ok(())
    }

    /// 按 path 删除文档类节点及其所有边（文件删除/移动/重命名生命周期挂点）。
    /// 目录删除时传目录路径前缀，删除该目录下所有节点与边。
    pub fn delete_by_path(&self, path: &str) -> Result<u64, String> {
        // 收集要删除的节点 id（精确匹配 + 目录前缀匹配）
        let mut ids: Vec<String> = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM graph_nodes WHERE path = ?1 OR path LIKE ?2")
                .map_err(|e| format!("准备删除查询失败: {}", e))?;
            let pattern = format!("{}/%", path.replace('\\', "/"));
            let rows = stmt
                .query_map(params![path.replace('\\', "/"), pattern], |row| row.get::<_, String>(0))
                .map_err(|e| format!("查询待删除图节点失败: {}", e))?;
            for r in rows {
                ids.push(r.map_err(|e| format!("读取待删除节点失败: {}", e))?);
            }
        }
        let n = ids.len() as u64;
        for id in ids {
            self.delete_node(&id)?;
        }
        Ok(n)
    }
    /// 把指向 `from_id` 的所有边重定向到 `to_id`（实体合并用；删除 from_id 节点）。
    /// 冲突边（source/target/relation 已存在）保留原边（UNIQUE 冲突忽略）。
    pub fn merge_node_edges(&self, from_id: &str, to_id: &str) -> Result<(), String> {
        if from_id == to_id {
            return Ok(());
        }
        // 入边：target = from_id → 改为 to_id（行 = source, relation, weight, confidence）
        let in_edges: Vec<(String, String, f32, f32)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT source, relation, weight, confidence FROM graph_edges WHERE target = ?1")
                .map_err(|e| format!("准备入边重定向失败: {}", e))?;
            let rows = stmt
                .query_map(params![from_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, f32>(2)?,
                        row.get::<_, f32>(3)?,
                    ))
                })
                .map_err(|e| format!("查询入边失败: {}", e))?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|e| format!("读取入边失败: {}", e))?
        };
        for (source, relation, weight, confidence) in in_edges {
            self.upsert_edge(
                &GraphEdge {
                    source,
                    target: to_id.to_string(),
                    relation: Relation::from_str(&relation),
                    weight: Some(weight),
                    confidence: Some(confidence),
                },
                None,
            )?;
        }
        // 出边：source = from_id → 改为 to_id（行 = target, relation, weight, confidence）
        let out_edges: Vec<(String, String, f32, f32)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT target, relation, weight, confidence FROM graph_edges WHERE source = ?1")
                .map_err(|e| format!("准备出边重定向失败: {}", e))?;
            let rows = stmt
                .query_map(params![from_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, f32>(2)?,
                        row.get::<_, f32>(3)?,
                    ))
                })
                .map_err(|e| format!("查询出边失败: {}", e))?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|e| format!("读取出边失败: {}", e))?
        };
        for (target, relation, weight, confidence) in out_edges {
            self.upsert_edge(
                &GraphEdge {
                    source: to_id.to_string(),
                    target,
                    relation: Relation::from_str(&relation),
                    weight: Some(weight),
                    confidence: Some(confidence),
                },
                None,
            )?;
        }
        // 删除旧节点（其残留边已被重定向/删除）
        self.conn
            .execute("DELETE FROM graph_nodes WHERE id = ?1", params![from_id])
            .map_err(|e| format!("删除被合并节点失败: {}", e))?;
        self.conn
            .execute(
                "DELETE FROM graph_edges WHERE source = ?1 OR target = ?1",
                params![from_id],
            )
            .map_err(|e| format!("清理被合并节点残留边失败: {}", e))?;
        self.bump_version()?;
        Ok(())
    }

    /// 按 source_id 删除边（生命周期级联：文件/chunk/commit 移除时清其产出的边）。
    pub fn delete_edges_by_source(&self, source_id: &str) -> Result<u64, String> {
        let n = self
            .conn
            .execute(
                "DELETE FROM graph_edges WHERE source_id = ?1",
                params![source_id],
            )
            .map_err(|e| format!("按来源删除图边失败: {}", e))?;
        if n > 0 {
            self.bump_version()?;
        }
        Ok(n as u64)
    }

    /// 按 source_id + 关系类型删除边（增量重建某类边用，如 IMPORTS 重写）。
    pub fn delete_edges_by_relation(&self, source_id: &str, relation: &str) -> Result<u64, String> {
        let n = self
            .conn
            .execute(
                "DELETE FROM graph_edges WHERE source_id = ?1 AND relation = ?2",
                params![source_id, relation],
            )
            .map_err(|e| format!("按来源关系删除图边失败: {}", e))?;
        if n > 0 {
            self.bump_version()?;
        }
        Ok(n as u64)
    }

    // ─── 边 CRUD ───

    pub fn upsert_edge(&self, edge: &GraphEdge, source_id: Option<&str>) -> Result<(), String> {
        let id = format!("{}|{}|{}", edge.source, edge.target, edge.relation.as_str());
        self.conn
            .execute(
                "INSERT INTO graph_edges (id, source, target, relation, weight, confidence, source_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
                 ON CONFLICT(source, target, relation) DO UPDATE SET
                    weight = MIN(graph_edges.weight + excluded.weight, 1.0),
                    confidence = MAX(graph_edges.confidence, excluded.confidence),
                    updated_at = excluded.updated_at",
                params![
                    id,
                    edge.source,
                    edge.target,
                    edge.relation.as_str(),
                    edge.weight.unwrap_or(1.0),
                    edge.confidence.unwrap_or(1.0),
                    source_id,
                    now_ms()
                ],
            )
            .map_err(|e| format!("写入图边失败: {}", e))?;
        self.bump_version()?;
        Ok(())
    }

    pub fn upsert_edges(&self, edges: &[GraphEdge], source_id: Option<&str>) -> Result<(), String> {
        for e in edges {
            self.upsert_edge(e, source_id)?;
        }
        Ok(())
    }

    // ─── 邻域查询（BFS + 截断） ───

    /// 邻域查询：从 node_id 出发 BFS（有界深度），relation 过滤 + 权重阈值 + 扇出截断。
    ///
    /// 实现：Rust 侧循环 BFS（每跳一个 SQL，命中复合索引毫秒级），而非递归 CTE——
    /// SQLite 递归 CTE 不支持「每跳扇出截断」，百万级图上高扇出节点（如 Redis 被千文档引用）
    /// 2 跳会指数爆炸，必须在查询层截断。
    ///
    /// `relations`：仅遍历这些关系（None = 全部）；`weight_min`：边权重下限。
    pub fn neighborhood(
        &self,
        node_id: &str,
        depth: u32,
        max_nodes: u32,
        max_edges: u32,
        relations: Option<&[Relation]>,
        weight_min: f32,
    ) -> Result<GraphNeighborhood, String> {
        let depth = depth.max(1).min(3); // 有界：最多 3 跳
        let max_nodes = max_nodes.max(1).min(1000);
        let max_edges = max_edges.max(1).min(2000);

        let mut nodes: std::collections::HashMap<String, GraphNode> = std::collections::HashMap::new();
        let mut edges: Vec<GraphEdge> = Vec::new();
        let mut edge_keys: std::collections::HashSet<(String, String, String)> =
            std::collections::HashSet::new();
        let mut frontier: Vec<String> = Vec::new();
        let mut truncated = false;

        if let Some(root) = self.get_node(node_id)? {
            let mut root = root;
            root.degree = Some(self.degree(node_id)?);
            nodes.insert(root.id.clone(), root);
            frontier.push(node_id.to_string());
        } else {
            return Ok(GraphNeighborhood {
                nodes: Vec::new(),
                edges: Vec::new(),
                truncated: false,
            });
        }

        for _hop in 0..depth {
            if frontier.is_empty() {
                break;
            }
            // 上一跳已触边数上限 → 停止后续跳（R4 修复：避免内层 break 后继续下一跳超限）
            if edges.len() >= max_edges as usize {
                truncated = true;
                break;
            }
            let mut next: Vec<String> = Vec::new();
            for cur in frontier.iter() {
                let (out_edges, out_neighbors) =
                    self.adjacent(cur, true, relations, weight_min, max_edges)?;
                let (in_edges, in_neighbors) =
                    self.adjacent(cur, false, relations, weight_min, max_edges)?;

                for e in out_edges.into_iter().chain(in_edges) {
                    let key = (e.source.clone(), e.target.clone(), e.relation.as_str().to_string());
                    if edge_keys.insert(key) {
                        edges.push(e);
                    }
                }
                for nb in out_neighbors.into_iter().chain(in_neighbors) {
                    if !nodes.contains_key(&nb) {
                        // 扇出截断：每跳新增节点以「剩余预算」为上限（尊重 max_nodes 契约；
                        // 不做 min(50) 硬顶 —— 否则 max_nodes=200 时总节点恒 ≤101，契约失效）
                        let budget = (max_nodes as usize).saturating_sub(nodes.len());
                        if next.len() >= budget {
                            truncated = true;
                            continue;
                        }
                        if let Some(mut n) = self.get_node(&nb)? {
                            n.degree = Some(self.degree(&nb)?);
                            nodes.insert(n.id.clone(), n);
                            next.push(nb);
                        }
                    }
                }
            }
            if nodes.len() >= max_nodes as usize {
                truncated = true;
                break;
            }
            frontier = next;
        }

        // 边只保留两端都在节点集合内的（截断时避免悬空边）
        edges.retain(|e| nodes.contains_key(&e.source) && nodes.contains_key(&e.target));

        Ok(GraphNeighborhood {
            nodes: nodes.into_values().collect(),
            edges,
            truncated,
        })
    }

    /// 单节点 1 跳邻域（graph_expand 用；不递归，仅直接邻居）
    pub fn expand(
        &self,
        node_id: &str,
        max_nodes: u32,
        relations: Option<&[Relation]>,
        weight_min: f32,
        max_edges: u32,
    ) -> Result<GraphNeighborhood, String> {
        let max_edges = max_edges.max(1).min(1000);
        let max_nodes = max_nodes.max(1).min(1000);
        let mut nodes: std::collections::HashMap<String, GraphNode> = std::collections::HashMap::new();
        let mut edges: Vec<GraphEdge> = Vec::new();
        let mut truncated = false;

        if let Some(mut root) = self.get_node(node_id)? {
            root.degree = Some(self.degree(node_id)?);
            nodes.insert(root.id.clone(), root);
        } else {
            return Ok(GraphNeighborhood {
                nodes: Vec::new(),
                edges: Vec::new(),
                truncated: false,
            });
        }

        let (out_edges, out_neighbors) = self.adjacent(node_id, true, relations, weight_min, max_edges)?;
        let (in_edges, in_neighbors) = self.adjacent(node_id, false, relations, weight_min, max_edges)?;
        edges.extend(out_edges);
        edges.extend(in_edges);
        if edges.len() > max_edges as usize {
            edges.truncate(max_edges as usize);
            truncated = true;
        }
        for nb in out_neighbors.into_iter().chain(in_neighbors) {
            if nodes.len() >= max_nodes as usize {
                truncated = true;
                break;
            }
            if let Some(mut n) = self.get_node(&nb)? {
                n.degree = Some(self.degree(&nb)?);
                nodes.insert(n.id.clone(), n);
            }
        }
        edges.retain(|e| nodes.contains_key(&e.source) && nodes.contains_key(&e.target));
        Ok(GraphNeighborhood {
            nodes: nodes.into_values().collect(),
            edges,
            truncated,
        })
    }

    /// 取某节点相邻边 + 邻居 id（direction=true 出边 source=node，false 入边 target=node）。
    fn adjacent(
        &self,
        node_id: &str,
        outgoing: bool,
        relations: Option<&[Relation]>,
        weight_min: f32,
        limit: u32,
    ) -> Result<(Vec<GraphEdge>, Vec<String>), String> {
        let (sql, arg) = if outgoing {
            (
                "SELECT source, target, relation, weight, confidence FROM graph_edges
                 WHERE source = ?1 AND weight >= ?2 {rel} ORDER BY weight DESC LIMIT ?3",
                node_id,
            )
        } else {
            (
                "SELECT source, target, relation, weight, confidence FROM graph_edges
                 WHERE target = ?1 AND weight >= ?2 {rel} ORDER BY weight DESC LIMIT ?3",
                node_id,
            )
        };
        let rel_clause = match relations {
            Some(rs) if !rs.is_empty() => {
                let in_list: Vec<String> = rs.iter().map(|r| format!("'{}'", r.as_str())).collect();
                format!("AND relation IN ({})", in_list.join(","))
            }
            _ => String::new(),
        };
        let sql = sql.replace("{rel}", &rel_clause);

        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| format!("准备邻域查询失败: {}", e))?;
        let rows = stmt
            .query_map(params![arg, weight_min, limit], |row| {
                Ok(GraphEdge {
                    source: row.get(0)?,
                    target: row.get(1)?,
                    relation: Relation::from_str(&row.get::<_, String>(2)?),
                    weight: Some(row.get::<_, f32>(3)?),
                    confidence: Some(row.get::<_, f32>(4)?),
                })
            })
            .map_err(|e| format!("执行邻域查询失败: {}", e))?;

        let mut edges = Vec::new();
        let mut neighbors = Vec::new();
        for r in rows {
            let e = r.map_err(|e| format!("读取邻域边失败: {}", e))?;
            let nb = if outgoing { e.target.clone() } else { e.source.clone() };
            edges.push(e);
            neighbors.push(nb);
        }
        Ok((edges, neighbors))
    }

    /// 节点度数（边数，双向计数一次）
    pub fn degree(&self, node_id: &str) -> Result<u32, String> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_edges WHERE source = ?1 OR target = ?1",
                params![node_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("统计节点度数失败: {}", e))?;
        Ok(n as u32)
    }

    /// 邻居 id 列表（出边 + 入边，去重；Cluster 投票 / 查询 API 用）
    pub fn neighbors_of(&self, node_id: &str) -> Result<Vec<String>, String> {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out = Vec::new();
        let (_, out_nbs) = self.adjacent(node_id, true, None, 0.0, 500)?;
        for nb in out_nbs {
            if seen.insert(nb.clone()) {
                out.push(nb);
            }
        }
        let (_, in_nbs) = self.adjacent(node_id, false, None, 0.0, 500)?;
        for nb in in_nbs {
            if seen.insert(nb.clone()) {
                out.push(nb);
            }
        }
        Ok(out)
    }

    /// 度数最高的实体节点（AI 冲突检测候选）
    pub fn top_degree_entities(&self, limit: u32) -> Result<Vec<(String, u32)>, String> {        let mut stmt = self
            .conn
            .prepare(
                "SELECT n.id, COUNT(e.id) AS d FROM graph_nodes n
                 LEFT JOIN graph_edges e ON e.source = n.id OR e.target = n.id
                 WHERE n.type = 'entity' GROUP BY n.id ORDER BY d DESC LIMIT ?1",
            )
            .map_err(|e| format!("准备实体度数统计失败: {}", e))?;
        let rows = stmt
            .query_map(params![limit], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u32)))
            .map_err(|e| format!("执行实体度数统计失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取实体度数统计失败: {}", e))?);
        }
        Ok(out)
    }

    /// 实体节点直接关联的文档路径列表（来源证据；通过相邻 doc 节点的 path 推导）
    pub fn source_docs_for_entity(&self, entity_id: &str) -> Result<Vec<String>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT DISTINCT n.path FROM graph_edges e
                 JOIN graph_nodes n ON (n.id = e.source OR n.id = e.target) AND n.id != ?1
                 WHERE (e.source = ?1 OR e.target = ?1) AND n.type = 'doc' AND n.path IS NOT NULL",
            )
            .map_err(|e| format!("准备实体来源文档查询失败: {}", e))?;
        let rows = stmt
            .query_map(params![entity_id, entity_id], |row| row.get::<_, String>(0))
            .map_err(|e| format!("执行实体来源文档查询失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取实体来源文档失败: {}", e))?);
        }
        Ok(out)
    }

    /// 高价值文档路径（按度数降序；AI 抽取优先级，PRD §76 成本控制）
    pub fn priority_doc_paths(&self, limit: u32) -> Result<Vec<String>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT n.path FROM graph_nodes n
                 LEFT JOIN graph_edges e ON e.source = n.id OR e.target = n.id
                 WHERE n.type = 'doc' AND n.path IS NOT NULL
                 GROUP BY n.id ORDER BY COUNT(e.id) DESC LIMIT ?1",
            )
            .map_err(|e| format!("准备优先级文档查询失败: {}", e))?;
        let rows = stmt
            .query_map(params![limit], |row| row.get::<_, String>(0))
            .map_err(|e| format!("执行优先级文档查询失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取优先级文档失败: {}", e))?);
        }
        Ok(out)
    }

    /// 全部 doc 节点的 (path, degree)，供后台 AI worker 计算重要度（Phase 3）。
    pub fn doc_degrees(&self) -> Result<Vec<(String, i64)>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT n.path, COUNT(e.id) FROM graph_nodes n
                 LEFT JOIN graph_edges e ON e.source = n.id OR e.target = n.id
                 WHERE n.type = 'doc' AND n.path IS NOT NULL
                 GROUP BY n.id",
            )
            .map_err(|e| format!("准备文档度数查询失败: {}", e))?;
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
            .map_err(|e| format!("执行文档度数查询失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取文档度数失败: {}", e))?);
        }
        Ok(out)
    }

    /// 更新（或插入）单个队列项的重要度与状态：失败重试后重新入队时刷新重要度用。
    pub fn requeue_ai_item(&self, id: i64, importance: f64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE graph_ai_queue SET importance = ?1, status = 'pending', updated_at = ?2 WHERE id = ?3",
                params![importance, now_ms(), id],
            )
            .map_err(|e| format!("重排队列项失败: {}", e))?;
        Ok(())
    }

    /// 单文档 (degree, max_degree)：build_file 后单条入队用（避免全库扫描）。
    pub fn doc_degree_rank(&self, rel_path: &str) -> Result<(i64, i64), String> {
        let id = node_id_for(NodeType::Doc, rel_path);
        let degree: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_edges WHERE source = ?1 OR target = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(|e| format!("查询单文档度数失败: {}", e))?;
        let max_degree: i64 = self
            .conn
            .query_row(
                "SELECT MAX(d) FROM (
                    SELECT COUNT(e.id) AS d FROM graph_nodes n
                    LEFT JOIN graph_edges e ON e.source = n.id OR e.target = n.id
                    WHERE n.type = 'doc' GROUP BY n.id
                 )",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(|e| format!("查询最大文档度数失败: {}", e))?
            .unwrap_or(0)
            .max(1);
        Ok((degree, max_degree))
    }

    // ─── 内容层节点（chunk/section；知识图谱底座 Layer 1） ───

    /// 删除某文档的全部内容节点（chunk/section；delete_node 级联清边 + bump）。
    /// 单文档增量重建前调用（幂等）。
    pub fn delete_content_nodes_for_doc(&self, rel_path: &str) -> Result<u64, String> {
        let path = rel_path.replace('\\', "/");
        let mut ids: Vec<String> = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM graph_nodes WHERE path = ?1 AND type IN ('chunk','section')")
                .map_err(|e| format!("准备内容节点清理查询失败: {}", e))?;
            let rows = stmt
                .query_map(params![path], |row| row.get::<_, String>(0))
                .map_err(|e| format!("执行内容节点清理查询失败: {}", e))?;
            for r in rows {
                ids.push(r.map_err(|e| format!("读取待清理内容节点失败: {}", e))?);
            }
        }
        let n = ids.len() as u64;
        for id in ids {
            self.delete_node(&id)?;
        }
        Ok(n)
    }

    /// 某文档的内容节点（chunk/section，含 content；前端 L4 细粒度 / 详情数据源）
    pub fn list_content_nodes_for_doc(&self, rel_path: &str, limit: u32) -> Result<Vec<GraphNode>, String> {
        let path = rel_path.replace('\\', "/");
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, type, name, path, meta, content, created_at FROM graph_nodes
                 WHERE path = ?1 AND type IN ('chunk','section') ORDER BY id LIMIT ?2",
            )
            .map_err(|e| format!("准备内容节点查询失败: {}", e))?;
        let rows = stmt
            .query_map(params![path, limit], |row| {
                Ok(GraphNode {
                    id: row.get(0)?,
                    node_type: NodeType::from_str(&row.get::<_, String>(1)?),
                    name: row.get(2)?,
                    path: row.get(3)?,
                    meta: row.get(4)?,
                    degree: None,
                    created_at: row.get(6)?,
                    content: row.get(5)?,
                })
            })
            .map_err(|e| format!("执行内容节点查询失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取内容节点失败: {}", e))?);
        }
        Ok(out)
    }

    // ─── 搜索 / 统计 / 状态 ───

    /// 节点搜索（name 模糊匹配，LIKE 不区分大小写）
    pub fn search_nodes(&self, keyword: &str, limit: u32) -> Result<Vec<GraphNode>, String> {
        let kw = format!("%{}%", keyword.replace('%', "%%").replace('_', "\\_"));
        let mut stmt = self
            .conn
            .prepare("SELECT id, type, name, path, meta, content, created_at FROM graph_nodes WHERE name LIKE ?1 ESCAPE '\\' ORDER BY length(name) LIMIT ?2")
            .map_err(|e| format!("准备节点搜索失败: {}", e))?;
        let rows = stmt
            .query_map(params![kw, limit], |row| {
                Ok(GraphNode {
                    id: row.get(0)?,
                    node_type: NodeType::from_str(&row.get::<_, String>(1)?),
                    name: row.get(2)?,
                    path: row.get(3)?,
                    meta: row.get(4)?,
                    degree: None,
                    created_at: row.get(6)?,
                    content: row.get(5)?,
                })
            })
            .map_err(|e| format!("执行节点搜索失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取搜索结果失败: {}", e))?);
        }
        Ok(out)
    }

    /// 图统计
    pub fn stats(&self) -> Result<GraphStats, String> {
        let mut stats = GraphStats::default();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT type, COUNT(*) FROM graph_nodes GROUP BY type")
                .map_err(|e| format!("准备类型统计失败: {}", e))?;
            let rows = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
                .map_err(|e| format!("执行类型统计失败: {}", e))?;
            for r in rows {
                let (t, c) = r.map_err(|e| format!("读取类型统计失败: {}", e))?;
                stats.by_type.insert(t, c as u64);
            }
        }
        // top degree：按边数排序取前 10
        {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT n.id, COUNT(e.id) AS d FROM graph_nodes n
                     LEFT JOIN graph_edges e ON e.source = n.id OR e.target = n.id
                     GROUP BY n.id ORDER BY d DESC LIMIT 10",
                )
                .map_err(|e| format!("准备度数统计失败: {}", e))?;
            let rows = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
                .map_err(|e| format!("执行度数统计失败: {}", e))?;
            for r in rows {
                let (id, d) = r.map_err(|e| format!("读取度数统计失败: {}", e))?;
                stats.top_degree.push((id, d as u32));
            }
        }
        let last: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(updated_at) FROM graph_edges",
                [],
                // 显式 Option<i64>：MAX() 在空表时返回 NULL，隐式 i64 会抛
                // "Invalid column type Null"（rusqlite 无法把 NULL 读成非 Option 类型）
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(|e| format!("读取图更新时间失败: {}", e))?;
        stats.last_built_at = last;
        stats.graph_version = self.graph_version()?;
        let cluster_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM graph_clusters", [], |row| row.get(0))
            .map_err(|e| format!("统计聚类失败: {}", e))?;
        stats.cluster_count = cluster_count as u64;
        stats.cluster_mode = self.get_property("graph_cluster_mode")?.unwrap_or_else(|| "directory".to_string());
        Ok(stats)
    }

    /// 图状态
    pub fn status(&self) -> Result<GraphStatus, String> {
        let node_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM graph_nodes", [], |row| row.get(0))
            .map_err(|e| format!("统计图节点失败: {}", e))?;
        let edge_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM graph_edges", [], |row| row.get(0))
            .map_err(|e| format!("统计图边失败: {}", e))?;
        let cluster_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM graph_clusters", [], |row| row.get(0))
            .map_err(|e| format!("统计聚类失败: {}", e))?;
        let cluster_mode = self
            .get_property("graph_cluster_mode")?
            .unwrap_or_else(|| "directory".to_string());
        Ok(GraphStatus {
            schema_version: GRAPH_SCHEMA_VERSION,
            node_count: node_count as u64,
            edge_count: edge_count as u64,
            building: false,
            progress_pct: None,
            graph_version: self.graph_version()?,
            cluster_count: cluster_count as u64,
            cluster_mode,
        })
    }

    /// 清空全部图数据（重建用）。级联清理 AI 候选 / 收藏 / 簇成员，避免残留陈旧数据。
    pub fn clear(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "DELETE FROM graph_edges;
                 DELETE FROM graph_nodes;
                 DELETE FROM graph_clusters;
                 DELETE FROM graph_cluster_members;
                 DELETE FROM graph_ai_candidates;
                 DELETE FROM graph_ai_queue;
                 DELETE FROM graph_favorites;",
            )
            .map_err(|e| format!("清空图数据失败: {}", e))?;
        self.bump_version()?;
        Ok(())
    }

    /// 列出全部 doc 节点的 path（链接解析目标集合用；替代全量 walkdir）。
    /// 增量场景下图内已有目标文档（由 build_all/build_file 先前写入），
    /// 未建节点的链接目标暂跳过，待该文件入图/下次全量时补充（R2 修复）。
    pub fn list_doc_paths(&self) -> Result<Vec<String>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM graph_nodes WHERE type = 'doc' AND path IS NOT NULL")
            .map_err(|e| format!("准备 doc 路径列表查询失败: {}", e))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| format!("查询 doc 路径列表失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取 doc 路径失败: {}", e))?);
        }
        Ok(out)
    }

    /// 文件路径是否存在对应节点（增量构建判重）
    pub fn path_exists(&self, path: &str) -> Result<bool, String> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_nodes WHERE path = ?1",
                params![path.replace('\\', "/")],
                |row| row.get(0),
            )
            .map_err(|e| format!("查询图节点存在性失败: {}", e))?;
        Ok(n > 0)
    }

    /// 通用属性（graph_properties；Experience 事件 / schema 版本共用） ───

    /// 在事务中执行多步写入（原子性：全部成功或全部回滚）。
    /// 供多节点/多边联动写入（如 Experience record 的 event+节点+边）使用，
    /// 避免中途失败留下半写入状态。
    pub fn with_transaction<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| format!("开启图事务失败: {}", e))?;
        let result = f(&self.conn);
        match result {
            Ok(v) => {
                self.conn
                    .execute_batch("COMMIT")
                    .map_err(|e| format!("提交图事务失败: {}", e))?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// 写属性（key 主键，upsert）
    pub fn set_property(&self, key: &str, value: &str) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO graph_properties (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(|e| format!("写入图属性失败: {}", e))?;
        Ok(())
    }

    /// 读属性
    pub fn get_property(&self, key: &str) -> Result<Option<String>, String> {
        self.conn
            .query_row(
                "SELECT value FROM graph_properties WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("读取图属性失败: {}", e))
    }

    /// 列出指定前缀的 key（如 `exp:` 列出全部经验事件键）
    pub fn list_properties_with_prefix(&self, prefix: &str) -> Result<Vec<String>, String> {
        let pattern = format!("{}%", prefix);
        let mut stmt = self
            .conn
            .prepare("SELECT key FROM graph_properties WHERE key LIKE ?1")
            .map_err(|e| format!("准备属性前缀查询失败: {}", e))?;
        let rows = stmt
            .query_map(params![pattern], |row| row.get::<_, String>(0))
            .map_err(|e| format!("执行属性前缀查询失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取属性键失败: {}", e))?);
        }
        Ok(out)
    }

    // ─── 图版本（PRD §42/§72/§73：每次图变更 +1，前端缓存失效依据） ───

    /// 当前图版本（无记录 = 0）
    pub fn graph_version(&self) -> Result<u64, String> {
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM graph_properties WHERE key = ?1",
                params![VERSION_MARKER],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("读取图版本失败: {}", e))?;
        Ok(v.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0))
    }

    /// 图变更标记：版本 +1（每次图数据变更后调用）
    fn bump_version(&self) -> Result<(), String> {
        let next = self.graph_version()?.saturating_add(1);
        self.conn
            .execute(
                "INSERT OR REPLACE INTO graph_properties (key, value) VALUES (?1, ?2)",
                params![VERSION_MARKER, next.to_string()],
            )
            .map_err(|e| format!("更新图版本失败: {}", e))?;
        Ok(())
    }

    // ─── Cluster 存储（graph_clusters；L0 聚合单元） ───

    /// 全量替换聚类表（ClusterEngine.rebuild 的持久化步骤；事务内执行）。
    pub fn replace_clusters(&self, clusters: &[GraphCluster]) -> Result<(), String> {
        self.conn
            .execute_batch("DELETE FROM graph_clusters")
            .map_err(|e| format!("清空聚类表失败: {}", e))?;
        for c in clusters {
            self.conn
                .execute(
                    "INSERT INTO graph_clusters
                        (id, name, description, algorithm, centroid, node_count, edge_count, confidence, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        c.id,
                        c.name,
                        c.description,
                        c.algorithm,
                        c.centroid,
                        c.node_count as i64,
                        c.edge_count as i64,
                        c.confidence,
                        c.created_at,
                        c.updated_at
                    ],
                )
                .map_err(|e| format!("写入聚类失败: {}", e))?;
        }
        self.bump_version()?;
        Ok(())
    }

    /// 全部聚类（按节点数降序）
    pub fn list_clusters(&self, limit: u32) -> Result<Vec<GraphCluster>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, name, description, algorithm, centroid, node_count, edge_count, confidence, created_at, updated_at
                 FROM graph_clusters ORDER BY node_count DESC LIMIT ?1",
            )
            .map_err(|e| format!("准备聚类列表查询失败: {}", e))?;
        let rows = stmt
            .query_map(params![limit], |row| {
                Ok(GraphCluster {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    algorithm: row.get(3)?,
                    centroid: row.get(4)?,
                    node_count: row.get::<_, i64>(5)? as u32,
                    edge_count: row.get::<_, i64>(6)? as u32,
                    confidence: row.get::<_, f32>(7)?,
                    created_at: row.get(8)?,
                    updated_at: row.get(9)?,
                    links: Vec::new(),
                    top_files: Vec::new(),
                })
            })
            .map_err(|e| format!("执行聚类列表查询失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取聚类失败: {}", e))?);
        }
        Ok(out)
    }

    /// 单聚类
    pub fn get_cluster(&self, id: &str) -> Result<Option<GraphCluster>, String> {
        self.conn
            .query_row(
                "SELECT id, name, description, algorithm, centroid, node_count, edge_count, confidence, created_at, updated_at
                 FROM graph_clusters WHERE id = ?1",
                params![id],
                |row| {
                    Ok(GraphCluster {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        description: row.get(2)?,
                        algorithm: row.get(3)?,
                        centroid: row.get(4)?,
                        node_count: row.get::<_, i64>(5)? as u32,
                        edge_count: row.get::<_, i64>(6)? as u32,
                        confidence: row.get::<_, f32>(7)?,
                        created_at: row.get(8)?,
                        updated_at: row.get(9)?,
                        links: Vec::new(),
                        top_files: Vec::new(),
                    })
                },
            )
            .optional()
            .map_err(|e| format!("读取聚类失败: {}", e))
    }

    /// 更新聚类描述（AI 摘要写入，PRD §29）
    pub fn update_cluster_description(&self, id: &str, description: &str) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE graph_clusters SET description = ?1, updated_at = ?2 WHERE id = ?3",
                params![description, now_ms(), id],
            )
            .map_err(|e| format!("更新聚类描述失败: {}", e))?;
        Ok(())
    }

    /// 聚类成员节点。优先读显式成员表（graph_cluster_members）；
    /// 无显式成员时回退 doc/folder 按 path 顶层目录匹配 + 无 path 节点邻域多数投票。
    pub fn cluster_members(&self, cluster_id: &str) -> Result<Vec<GraphNode>, String> {
        // 1) 显式成员表（Embedding/目录聚类重建后统一写入）
        if let Some(ids) = self.cluster_member_ids(cluster_id)? {
            let mut out = Vec::new();
            for id in ids {
                if let Some(mut n) = self.get_node(&id)? {
                    n.degree = Some(self.degree(&id)?);
                    out.push(n);
                }
            }
            return Ok(out);
        }
        // 2) 归一化簇名（cluster:docs → docs）
        let top = cluster_id.strip_prefix("cluster:").unwrap_or(cluster_id);
        let is_root = top == "__root__";

        let mut ids: Vec<String> = Vec::new();
        {
            let sql = if is_root {
                "SELECT id FROM graph_nodes WHERE path IS NOT NULL AND path NOT LIKE '%/%'"
            } else {
                "SELECT id FROM graph_nodes WHERE path LIKE ?1 OR path = ?2"
            };
            let arg_vec: Vec<String> = if is_root {
                Vec::new()
            } else {
                vec![format!("{}/%", top.replace('\\', "/")), top.to_string()]
            };
            let mut stmt = self.conn.prepare(sql).map_err(|e| format!("准备聚类成员查询失败: {}", e))?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(arg_vec.iter()), |row| row.get::<_, String>(0))
                .map_err(|e| format!("执行聚类成员查询失败: {}", e))?;
            for r in rows {
                ids.push(r.map_err(|e| format!("读取聚类成员失败: {}", e))?);
            }
        }

        // 无 path 节点：邻域多数投票
        for (nid, _t) in self.nodes_without_path()? {
            if ids.contains(&nid) {
                continue;
            }
            if self.node_belongs_to_cluster(&nid, &ids, cluster_id)? {
                ids.push(nid);
            }
        }

        let mut out = Vec::new();
        for id in ids {
            if let Some(mut n) = self.get_node(&id)? {
                n.degree = Some(self.degree(&id)?);
                out.push(n);
            }
        }
        Ok(out)
    }

    /// 无 path 的节点 id（entity/chunk/experience 等）
    fn nodes_without_path(&self) -> Result<Vec<(String, String)>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, type FROM graph_nodes WHERE path IS NULL")
            .map_err(|e| format!("准备无路径节点查询失败: {}", e))?;
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(|e| format!("执行无路径节点查询失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取无路径节点失败: {}", e))?);
        }
        Ok(out)
    }

    /// 无路径节点是否归属某簇：邻域（1 跳）中该簇成员占比 ≥ 1/3 或最高
    fn node_belongs_to_cluster(
        &self,
        node_id: &str,
        members: &[String],
        _cluster_id: &str,
    ) -> Result<bool, String> {
        let member_set: std::collections::HashSet<&str> = members.iter().map(|s| s.as_str()).collect();
        let mut neighbor_total = 0usize;
        let mut member_hits = 0usize;
        for (target, relation, weight) in self
            .adjacent_raw(node_id, true)?
            .into_iter()
            .chain(self.adjacent_raw(node_id, false)?)
        {
            let _ = relation;
            let _ = weight;
            neighbor_total += 1;
            if member_set.contains(target.as_str()) {
                member_hits += 1;
            }
        }
        Ok(neighbor_total > 0 && member_hits * 3 >= neighbor_total)
    }

    /// 邻域原始边（无过滤；投票用）
    fn adjacent_raw(&self, node_id: &str, outgoing: bool) -> Result<Vec<(String, String, f32)>, String> {
        let sql = if outgoing {
            "SELECT target, relation, weight FROM graph_edges WHERE source = ?1"
        } else {
            "SELECT source, relation, weight FROM graph_edges WHERE target = ?1"
        };
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| format!("准备邻域边查询失败: {}", e))?;
        let rows = stmt
            .query_map(params![node_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f32>(2)?,
                ))
            })
            .map_err(|e| format!("执行邻域边查询失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取邻域边失败: {}", e))?);
        }
        Ok(out)
    }

    /// 聚类内部边（两端都属于该簇成员）
    pub fn cluster_edges(&self, member_ids: &[String]) -> Result<Vec<GraphEdge>, String> {
        if member_ids.is_empty() {
            return Ok(Vec::new());
        }
        let member_set: std::collections::HashSet<&str> = member_ids.iter().map(|s| s.as_str()).collect();
        let mut out = Vec::new();
        for e in self.all_edges(20_000)? {
            if member_set.contains(e.source.as_str()) && member_set.contains(e.target.as_str()) {
                out.push(e);
            }
        }
        Ok(out)
    }

    // ─── 簇成员表（显式成员；目录簇与 Embedding 簇统一走这里） ───

    /// 覆盖写入某簇的成员表（重建聚类时调用）
    pub fn replace_cluster_members(&self, cluster_id: &str, member_ids: &[String]) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM graph_cluster_members WHERE cluster_id = ?1", params![cluster_id])
            .map_err(|e| format!("清空簇成员失败: {}", e))?;
        for id in member_ids {
            self.conn
                .execute(
                    "INSERT OR IGNORE INTO graph_cluster_members (cluster_id, node_id) VALUES (?1, ?2)",
                    params![cluster_id, id],
                )
                .map_err(|e| format!("写入簇成员失败: {}", e))?;
        }
        Ok(())
    }

    /// 显式成员 id 列表（无记录 = None，调用方回退 path 前缀推导）
    pub fn cluster_member_ids(&self, cluster_id: &str) -> Result<Option<Vec<String>>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT node_id FROM graph_cluster_members WHERE cluster_id = ?1")
            .map_err(|e| format!("准备簇成员查询失败: {}", e))?;
        let rows = stmt
            .query_map(params![cluster_id], |row| row.get::<_, String>(0))
            .map_err(|e| format!("执行簇成员查询失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取簇成员失败: {}", e))?);
        }
        if out.is_empty() {
            Ok(None)
        } else {
            Ok(Some(out))
        }
    }

    /// 全量 节点 → 簇 映射（成员表 + path 前缀回退；簇间链接聚合用，单遍构建）
    pub fn node_cluster_map(&self) -> Result<std::collections::HashMap<String, String>, String> {
        let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT cluster_id, node_id FROM graph_cluster_members")
                .map_err(|e| format!("准备节点簇映射查询失败: {}", e))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                    ))
                })
                .map_err(|e| format!("执行节点簇映射查询失败: {}", e))?;
            for r in rows {
                let (cid, nid) = r.map_err(|e| format!("读取节点簇映射失败: {}", e))?;
                map.insert(nid, cid);
            }
        }
        // path 回退（兼容未写成员表的旧数据）
        for n in self.all_nodes(500_000)? {
            if map.contains_key(&n.id) {
                continue;
            }
            if let Some(p) = &n.path {
                map.insert(n.id, crate::core::graph::cluster::cluster_id_for_path(p));
            }
        }
        Ok(map)
    }

    // ─── AI 候选关系（PRD §27-28/§32：graph_ai_candidates） ───

    pub fn upsert_candidate(&self, c: &GraphAiCandidate) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO graph_ai_candidates
                    (id, source, target, relation, confidence, status, source_doc, evidence, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    c.id,
                    c.source,
                    c.target,
                    c.relation.as_str(),
                    c.confidence,
                    c.status,
                    c.source_doc,
                    c.evidence,
                    c.created_at
                ],
            )
            .map_err(|e| format!("写入 AI 候选关系失败: {}", e))?;
        Ok(())
    }

    pub fn upsert_candidates(&self, list: &[GraphAiCandidate]) -> Result<(), String> {
        for c in list {
            self.upsert_candidate(c)?;
        }
        Ok(())
    }

    /// 候选列表（status 为 None = 全部；按置信度降序）
    pub fn list_candidates(&self, status: Option<&str>, limit: u32) -> Result<Vec<GraphAiCandidate>, String> {
        let mut out = Vec::new();
        match status {
            Some(s) => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT id, source, target, relation, confidence, status, source_doc, evidence, created_at
                         FROM graph_ai_candidates WHERE status = ?1 ORDER BY confidence DESC LIMIT ?2",
                    )
                    .map_err(|e| format!("准备候选查询失败: {}", e))?;
                let rows = stmt
                    .query_map(params![s, limit], |row| {
                        Ok(GraphAiCandidate {
                            id: row.get(0)?,
                            source: row.get(1)?,
                            target: row.get(2)?,
                            relation: Relation::from_str(&row.get::<_, String>(3)?),
                            confidence: row.get(4)?,
                            status: row.get(5)?,
                            source_doc: row.get(6)?,
                            evidence: row.get(7)?,
                            created_at: row.get(8)?,
                        
                        })
                    })
                    .map_err(|e| format!("执行候选查询失败: {}", e))?;
                for r in rows {
                    out.push(r.map_err(|e| format!("读取候选失败: {}", e))?);
                }
            }
            None => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT id, source, target, relation, confidence, status, source_doc, evidence, created_at
                         FROM graph_ai_candidates ORDER BY confidence DESC LIMIT ?1",
                    )
                    .map_err(|e| format!("准备候选查询失败: {}", e))?;
                let rows = stmt
                    .query_map(params![limit], |row| {
                        Ok(GraphAiCandidate {
                            id: row.get(0)?,
                            source: row.get(1)?,
                            target: row.get(2)?,
                            relation: Relation::from_str(&row.get::<_, String>(3)?),
                            confidence: row.get(4)?,
                            status: row.get(5)?,
                            source_doc: row.get(6)?,
                            evidence: row.get(7)?,
                            created_at: row.get(8)?,
                        
                        })
                    })
                    .map_err(|e| format!("执行候选查询失败: {}", e))?;
                for r in rows {
                    out.push(r.map_err(|e| format!("读取候选失败: {}", e))?);
                }
            }
        }
        Ok(out)
    }

    /// 更新候选状态（confirm → 落正式边 + auto_confirmed/confirmed；reject → rejected）
    pub fn update_candidate_status(&self, id: &str, status: &str) -> Result<Option<GraphAiCandidate>, String> {
        let found: Option<GraphAiCandidate> = {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT id, source, target, relation, confidence, status, source_doc, evidence, created_at
                     FROM graph_ai_candidates WHERE id = ?1",
                )
                .map_err(|e| format!("准备候选读取失败: {}", e))?;
            let rows = stmt
                .query_map(params![id], |row| {
                    Ok(GraphAiCandidate {
                        id: row.get(0)?,
                        source: row.get(1)?,
                        target: row.get(2)?,
                        relation: Relation::from_str(&row.get::<_, String>(3)?),
                        confidence: row.get(4)?,
                        status: row.get(5)?,
                        source_doc: row.get(6)?,
                        evidence: row.get(7)?,
                        created_at: row.get(8)?,
                    
                    })
                })
                .map_err(|e| format!("执行候选读取失败: {}", e))?;
            let mut out = None;
            for r in rows {
                out = Some(r.map_err(|e| format!("读取候选失败: {}", e))?);
                break;
            }
            out
        };
        let Some(mut candidate) = found else {
            return Ok(None);
        };
        // confirm → 落正式边（AI 关系进入正式图；source_id 记录候选 id 便于溯源/撤销）
        let wrote_edge = status == "confirmed" || status == "auto_confirmed";
        if wrote_edge {
            let edge = GraphEdge {
                source: candidate.source.clone(),
                target: candidate.target.clone(),
                relation: candidate.relation,
                weight: Some(candidate.confidence),
                confidence: Some(candidate.confidence),
            };
            self.upsert_edge(&edge, Some(&candidate.id))?; // 内部 bump 一次
        } else if status == "rejected" {
            // 撤销此前已落边（PRD §49：用户可拒绝 AI 关系；按候选 id 清理，幂等）
            self.conn
                .execute("DELETE FROM graph_edges WHERE source_id = ?1", params![candidate.id])
                .map_err(|e| format!("撤销候选边失败: {}", e))?;
        }
        self.conn
            .execute(
                "UPDATE graph_ai_candidates SET status = ?1 WHERE id = ?2",
                params![status, id],
            )
            .map_err(|e| format!("更新候选状态失败: {}", e))?;
        // 状态变更本身也是一次图变更；确认路径 upsert_edge 已 bump，避免双增
        if !wrote_edge {
            self.bump_version()?;
        }
        candidate.status = status.to_string();
        Ok(Some(candidate))
    }

    /// 候选统计（pending = 待用户确认；confirmed = 已确认含 AI 自动确认落边的）
    pub fn candidate_counts(&self) -> Result<(u64, u64), String> {
        let pending: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_ai_candidates WHERE status = 'candidate'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| format!("统计候选失败: {}", e))?;
        let confirmed: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_ai_candidates WHERE status IN ('confirmed','auto_confirmed')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| format!("统计已确认候选失败: {}", e))?;
        Ok((pending as u64, confirmed as u64))
    }

    // ─── 后台 AI 工作队列（Phase 3 完整形态：构建后异步抽取） ───

    /// 批量入队文档（构建后调用）。ON CONFLICT 语义：
    /// - 已存在项保留原 created_at，更新 importance（重排序 pending）；
    /// - failed 项回退 pending 重试；
    /// - `reset_done=true`（单文件变更）时 done 项也回退 pending 重新抽取；
    ///   全量构建用 `reset_done=false`，done 项不重复处理（队列的幂等核心）。
    pub fn enqueue_ai_docs(
        &self,
        dir_path: &str,
        docs: &[(String, f64)],
        reset_done: bool,
    ) -> Result<usize, String> {
        let now = now_ms();
        let mut inserted = 0usize;
        self.with_transaction(|conn| {
            let mut stmt = conn
                .prepare(
                    "INSERT INTO graph_ai_queue (dir_path, rel_path, importance, status, attempts, created_at, updated_at)
                     VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?4)
                     ON CONFLICT(dir_path, rel_path) DO UPDATE SET
                       importance = excluded.importance,
                       status = CASE
                         WHEN ?5 = 1 THEN 'pending'
                         WHEN graph_ai_queue.status = 'failed' THEN 'pending'
                         ELSE graph_ai_queue.status END,
                       attempts = CASE WHEN ?5 = 1 THEN 0 ELSE graph_ai_queue.attempts END,
                       updated_at = excluded.updated_at",
                )
                .map_err(|e| format!("准备入队语句失败: {}", e))?;
            for (rel_path, importance) in docs {
                let n = stmt
                    .execute(params![dir_path, rel_path, importance, now, reset_done as i64])
                    .map_err(|e| format!("入队文档 {} 失败: {}", rel_path, e))?;
                inserted += n as usize;
            }
            Ok(())
        })?;
        Ok(inserted)
    }

    /// 取下一批待处理项（按 importance 降序，最多 limit 条），原子地标记为 processing。
    /// 同时把上次运行残留的 processing 项（worker 崩溃未收尾）重置回 pending——
    /// 仅重置超过 [`AI_QUEUE_STALE_MS`] 未更新的项，避免打断本轮正在处理的批次。
    pub fn next_ai_batch(&self, dir_path: &str, limit: u32) -> Result<Vec<AiQueueItem>, String> {
        let now = now_ms();
        self.conn
            .execute(
                "UPDATE graph_ai_queue SET status = 'pending', updated_at = ?2
                 WHERE dir_path = ?1 AND status = 'processing' AND updated_at < ?3",
                params![dir_path, now, now - AI_QUEUE_STALE_MS],
            )
            .map_err(|e| format!("重置残留 processing 项失败: {}", e))?;
        // 取前 limit 条并标记 processing
        let ids: Vec<i64> = {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT id FROM graph_ai_queue
                     WHERE dir_path = ?1 AND status = 'pending'
                     ORDER BY importance DESC, created_at ASC LIMIT ?2",
                )
                .map_err(|e| format!("准备取队语句失败: {}", e))?;
            let rows = stmt
                .query_map(params![dir_path, limit], |row| row.get::<_, i64>(0))
                .map_err(|e| format!("查询待处理项失败: {}", e))?;
            let mut ids = Vec::new();
            for r in rows {
                ids.push(r.map_err(|e| format!("读取队首 id 失败: {}", e))?);
            }
            ids
        };
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let now = now_ms();
        self.with_transaction(|conn| {
            let mut stmt = conn
                .prepare("UPDATE graph_ai_queue SET status = 'processing', updated_at = ?1 WHERE id = ?2")
                .map_err(|e| format!("准备标记 processing 语句失败: {}", e))?;
            for id in &ids {
                stmt.execute(params![now, id])
                    .map_err(|e| format!("标记 processing 失败: {}", e))?;
            }
            Ok(())
        })?;
        self.get_ai_items(&ids)
    }

    /// 按 id 读取队列项（worker 处理完后写回状态用）。
    pub fn get_ai_items(&self, ids: &[i64]) -> Result<Vec<AiQueueItem>, String> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, dir_path, rel_path, importance, status, attempts, created_at, updated_at
             FROM graph_ai_queue WHERE id IN ({})",
            placeholders
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| format!("准备队列读取语句失败: {}", e))?;
        let rows = stmt
            .query_map(params_from_iter(ids.iter()), |row| {
                Ok(AiQueueItem {
                    id: row.get(0)?,
                    dir_path: row.get(1)?,
                    rel_path: row.get(2)?,
                    importance: row.get(3)?,
                    status: row.get(4)?,
                    attempts: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            })
            .map_err(|e| format!("查询队列项失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取队列项失败: {}", e))?);
        }
        // 保持 ids 传入顺序（ORDER BY id 不可控时前端/worker 依赖原顺序）
        out.sort_by_key(|i| ids.iter().position(|id| *id == i.id).unwrap_or(usize::MAX));
        Ok(out)
    }

    /// 队列项处理完成（ok=true → done；ok=false → attempts+1，超过 max_attempts → failed，
    /// 否则回退 pending 供下轮重试）。
    pub fn finish_ai_item(&self, id: i64, ok: bool, max_attempts: u32) -> Result<(), String> {
        let cur: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT attempts, status FROM graph_ai_queue WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| format!("查询队列项状态失败: {}", e))?;
        let Some((attempts, status)) = cur else {
            return Ok(()); // 项已被清空（clear 重建），忽略
        };
        if status != "processing" {
            return Ok(()); // 状态已被外部改动，不覆盖
        }
        let (new_status, new_attempts) = if ok {
            ("done".to_string(), attempts)
        } else if attempts + 1 >= max_attempts as i64 {
            ("failed".to_string(), attempts + 1)
        } else {
            ("pending".to_string(), attempts + 1)
        };
        self.conn
            .execute(
                "UPDATE graph_ai_queue SET status = ?1, attempts = ?2, updated_at = ?3 WHERE id = ?4",
                params![new_status, new_attempts, now_ms(), id],
            )
            .map_err(|e| format!("更新队列项状态失败: {}", e))?;
        if ok {
            self.bump_metric("worker_processed", 1)?;
        } else if new_status == "failed" {
            self.bump_metric("worker_failed", 1)?;
        }
        Ok(())
    }

    /// 队列统计（pending / processing / done / failed），供状态命令与可观测性使用。
    pub fn queue_stats(&self, dir_path: &str) -> Result<(u64, u64, u64, u64), String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT status, COUNT(*) FROM graph_ai_queue WHERE dir_path = ?1 GROUP BY status",
            )
            .map_err(|e| format!("准备队列统计失败: {}", e))?;
        let rows = stmt
            .query_map(params![dir_path], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| format!("查询队列统计失败: {}", e))?;
        let mut stats = (0u64, 0u64, 0u64, 0u64);
        for r in rows {
            let (status, c) = r.map_err(|e| format!("读取队列统计失败: {}", e))?;
            match status.as_str() {
                "pending" => stats.0 += c as u64,
                "processing" => stats.1 += c as u64,
                "done" => stats.2 += c as u64,
                "failed" => stats.3 += c as u64,
                _ => {}
            }
        }
        Ok(stats)
    }

    // ─── 收藏（PRD §50：My Knowledge） ───

    pub fn favorite(&self, node_id: &str, on: bool) -> Result<(), String> {
        if on {
            self.conn
                .execute(
                    "INSERT OR IGNORE INTO graph_favorites (node_id, created_at) VALUES (?1, ?2)",
                    params![node_id, now_ms()],
                )
                .map_err(|e| format!("写入收藏失败: {}", e))?;
        } else {
            self.conn
                .execute("DELETE FROM graph_favorites WHERE node_id = ?1", params![node_id])
                .map_err(|e| format!("取消收藏失败: {}", e))?;
        }
        Ok(())
    }

    /// 收藏节点（含节点信息）
    pub fn list_favorites(&self, limit: u32) -> Result<Vec<GraphNode>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT n.id, n.type, n.name, n.path, n.meta, n.content, n.created_at
                 FROM graph_favorites f JOIN graph_nodes n ON n.id = f.node_id
                 ORDER BY f.created_at DESC LIMIT ?1",
            )
            .map_err(|e| format!("准备收藏列表查询失败: {}", e))?;
        let rows = stmt
            .query_map(params![limit], |row| {
                Ok(GraphNode {
                    id: row.get(0)?,
                    node_type: NodeType::from_str(&row.get::<_, String>(1)?),
                    name: row.get(2)?,
                    path: row.get(3)?,
                    meta: row.get(4)?,
                    degree: None,
                    created_at: row.get(6)?,
                    content: row.get(5)?,
                })
            })
            .map_err(|e| format!("执行收藏列表查询失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取收藏节点失败: {}", e))?);
        }
        Ok(out)
    }

    pub fn is_favorite(&self, node_id: &str) -> Result<bool, String> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM graph_favorites WHERE node_id = ?1",
                params![node_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("查询收藏状态失败: {}", e))?;
        Ok(n > 0)
    }

    pub fn favorite_count(&self) -> Result<u64, String> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM graph_favorites", [], |row| row.get(0))
            .map_err(|e| format!("统计收藏失败: {}", e))?;
        Ok(n as u64)
    }

    // ─── 可观测性指标（PRD §74） ───

    pub fn bump_metric(&self, key: &str, delta: i64) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO graph_metrics (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = graph_metrics.value + excluded.value",
                params![key, delta],
            )
            .map_err(|e| format!("写入指标失败: {}", e))?;
        Ok(())
    }

    pub fn get_metric(&self, key: &str) -> Result<u64, String> {
        let v: Option<i64> = self
            .conn
            .query_row(
                "SELECT value FROM graph_metrics WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("读取指标失败: {}", e))?;
        Ok(v.unwrap_or(0).max(0) as u64)
    }

    /// 图指标汇总（LLM 调用 / Token / 失败 / 候选 / 收藏）
    pub fn metrics(&self) -> Result<super::model::GraphMetrics, String> {
        let (pending, confirmed) = self.candidate_counts()?;
        Ok(super::model::GraphMetrics {
            llm_calls: self.get_metric("llm_calls")?,
            llm_tokens: self.get_metric("llm_tokens")?,
            llm_failures: self.get_metric("llm_failures")?,
            candidates_pending: pending,
            candidates_confirmed: confirmed,
            favorites: self.favorite_count()?,
            worker_processed: self.get_metric("worker_processed")?,
            worker_failed: self.get_metric("worker_failed")?,
        })
    }

    // ─── 知识演化统计（PRD §30-31） ───

    /// 按月统计节点/边增长 + 簇月度增长。
    pub fn evolution(&self) -> Result<GraphEvolution, String> {
        let mut out = GraphEvolution {
            graph_version: self.graph_version()?,
            ..Default::default()
        };
        // 节点月度
        {
            let mut stmt = self
                .conn
                .prepare("SELECT strftime('%Y-%m', created_at / 1000, 'unixepoch') AS m, COUNT(*) FROM graph_nodes GROUP BY m ORDER BY m")
                .map_err(|e| format!("准备节点月度统计失败: {}", e))?;
            let rows = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
                .map_err(|e| format!("执行节点月度统计失败: {}", e))?;
            for r in rows {
                let (m, c) = r.map_err(|e| format!("读取节点月度统计失败: {}", e))?;
                out.monthly_nodes.push((m, c as u64));
            }
        }
        // 边月度（按 created_at）
        {
            let mut stmt = self
                .conn
                .prepare("SELECT strftime('%Y-%m', created_at / 1000, 'unixepoch') AS m, COUNT(*) FROM graph_edges GROUP BY m ORDER BY m")
                .map_err(|e| format!("准备边月度统计失败: {}", e))?;
            let rows = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
                .map_err(|e| format!("执行边月度统计失败: {}", e))?;
            for r in rows {
                let (m, c) = r.map_err(|e| format!("读取边月度统计失败: {}", e))?;
                out.monthly_edges.push((m, c as u64));
            }
        }
        // 簇月度增长：按 path 顶层目录分组统计每月新增 doc
        {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT CASE WHEN instr(path, '/') > 0 THEN 'cluster:' || substr(path, 1, instr(path, '/') - 1) ELSE 'cluster:__root__' END AS cid,
                            strftime('%Y-%m', created_at / 1000, 'unixepoch') AS m, COUNT(*)
                     FROM graph_nodes WHERE path IS NOT NULL GROUP BY cid, m ORDER BY cid, m",
                )
                .map_err(|e| format!("准备簇月度统计失败: {}", e))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(|e| format!("执行簇月度统计失败: {}", e))?;
            let mut by_cluster: std::collections::BTreeMap<String, Vec<(String, u64)>> = Default::default();
            for r in rows {
                let (cid, m, c) = r.map_err(|e| format!("读取簇月度统计失败: {}", e))?;
                by_cluster.entry(cid).or_default().push((m, c as u64));
            }
            for (cid, monthly) in by_cluster {
                let name = crate::core::graph::cluster::cluster_display_name(&cid);
                out.cluster_growth.push(super::model::ClusterGrowth {
                    cluster_id: cid,
                    cluster_name: name,
                    monthly,
                });
            }
            out.cluster_growth.sort_by(|a, b| b.cluster_id.cmp(&a.cluster_id));
        }
        Ok(out)
    }

    // ─── 概览（L0 数据源） ───    /// 全量节点（limit 截断；概览用）。
    /// 排除 chunk/section（内容层）：概览只返回文件/目录/实体等「知识结构节点」，
    /// chunk 由邻域展开 / L4 细粒度按需加载（避免概览载荷爆炸）。
    pub fn all_nodes(&self, limit: u32) -> Result<Vec<GraphNode>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, type, name, path, meta, content, created_at FROM graph_nodes WHERE type NOT IN ('chunk','section') ORDER BY id LIMIT ?1")
            .map_err(|e| format!("准备概览节点查询失败: {}", e))?;
        let rows = stmt
            .query_map(params![limit], |row| {
                Ok(GraphNode {
                    id: row.get(0)?,
                    node_type: NodeType::from_str(&row.get::<_, String>(1)?),
                    name: row.get(2)?,
                    path: row.get(3)?,
                    meta: row.get(4)?,
                    degree: None,
                    created_at: row.get(6)?,
                    content: row.get(5)?,
                })
            })
            .map_err(|e| format!("执行概览节点查询失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取概览节点失败: {}", e))?);
        }
        Ok(out)
    }

    /// 全量边（limit 截断；概览用）
    pub fn all_edges(&self, limit: u32) -> Result<Vec<GraphEdge>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT source, target, relation, weight, confidence FROM graph_edges ORDER BY weight DESC LIMIT ?1")
            .map_err(|e| format!("准备概览边查询失败: {}", e))?;
        let rows = stmt
            .query_map(params![limit], |row| {
                Ok(GraphEdge {
                    source: row.get(0)?,
                    target: row.get(1)?,
                    relation: Relation::from_str(&row.get::<_, String>(2)?),
                    weight: Some(row.get::<_, f32>(3)?),
                    confidence: Some(row.get::<_, f32>(4)?),
                })
            })
            .map_err(|e| format!("执行概览边查询失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取概览边失败: {}", e))?);
        }
        Ok(out)
    }

    // ─── Graph Query API（PRD §24/§36：路径 / 共同邻居 / 子图） ───

    /// 两节点间最短路径（无权 BFS；按深度/节点上限保护）。
    /// 返回路径节点序列 + 相邻边（PRD §24 find_path 示例：Redis → Cache → Application → …）。
    pub fn find_path(
        &self,
        source: &str,
        target: &str,
        max_depth: u32,
        max_nodes: u32,
    ) -> Result<GraphPath, String> {
        let max_depth = max_depth.max(1).min(6);
        let max_nodes = max_nodes.max(1).min(500);
        if source == target {
            // 同节点：只返回该节点
            let mut path = GraphPath::default();
            if let Some(mut n) = self.get_node(source)? {
                n.degree = Some(self.degree(source)?);
                path.found = true;
                path.path_ids.push(source.to_string());
                path.nodes.push(n);
            }
            return Ok(path);
        }
        let mut result = GraphPath::default();
        if self.get_node(source)?.is_none() || self.get_node(target)?.is_none() {
            return Ok(result);
        }

        // BFS（parent 回溯）
        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        let mut parent: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        queue.push_back(source.to_string());
        visited.insert(source.to_string());
        let mut found = false;
        let mut hops = 0;

        'outer: while !queue.is_empty() && hops < max_depth {
            hops += 1;
            let level_len = queue.len();
            for _ in 0..level_len {
                let cur = queue.pop_front().unwrap_or_default();
                if visited.len() >= max_nodes as usize {
                    break 'outer;
                }
                let (_, out_nbs) = self.adjacent(&cur, true, None, 0.0, 500)?;
                let (_, in_nbs) = self.adjacent(&cur, false, None, 0.0, 500)?;
                for nb in out_nbs.into_iter().chain(in_nbs) {
                    if visited.insert(nb.clone()) {
                        parent.insert(nb.clone(), cur.clone());
                        if nb == target {
                            found = true;
                            break 'outer;
                        }
                        if visited.len() < max_nodes as usize {
                            queue.push_back(nb);
                        }
                    }
                }
            }
        }

        if !found {
            return Ok(result);
        }
        // 回溯路径
        let mut ids: Vec<String> = Vec::new();
        let mut cur = target.to_string();
        loop {
            ids.push(cur.clone());
            if cur == source {
                break;
            }
            match parent.get(&cur) {
                Some(p) => cur = p.clone(),
                None => break,
            }
        }
        ids.reverse();
        result.found = true;
        result.path_ids = ids.clone();

        // 节点 + 相邻边
        for id in &ids {
            if let Some(mut n) = self.get_node(id)? {
                n.degree = Some(self.degree(id)?);
                result.nodes.push(n);
            }
        }
        for w in ids.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            for e in self
                .all_edges_between(a, b)?
                .into_iter()
                .chain(self.all_edges_between(b, a)?)
            {
                if !result.edges.iter().any(|x| {
                    (x.source == e.source && x.target == e.target && x.relation == e.relation)
                        || (x.source == e.target && x.target == e.source && x.relation == e.relation)
                }) {
                    result.edges.push(e);
                }
            }
        }
        Ok(result)
    }

    /// 两点间全部直连边（双向查询用；Memory/路径证据复用）
    pub fn all_edges_between(&self, a: &str, b: &str) -> Result<Vec<GraphEdge>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT source, target, relation, weight, confidence FROM graph_edges
                 WHERE (source = ?1 AND target = ?2) OR (source = ?2 AND target = ?1)",
            )
            .map_err(|e| format!("准备路径边查询失败: {}", e))?;
        let rows = stmt
            .query_map(params![a, b], |row| {
                Ok(GraphEdge {
                    source: row.get(0)?,
                    target: row.get(1)?,
                    relation: Relation::from_str(&row.get::<_, String>(2)?),
                    weight: Some(row.get::<_, f32>(3)?),
                    confidence: Some(row.get::<_, f32>(4)?),
                })
            })
            .map_err(|e| format!("执行路径边查询失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("读取路径边失败: {}", e))?);
        }
        Ok(out)
    }

    /// 两节点共同邻居（PRD §24 find_common_neighbors）。
    /// 返回：两起点 + 共同邻居 + 连接边（a↔common / b↔common）。
    pub fn common_neighbors(&self, a: &str, b: &str) -> Result<GraphNeighborhood, String> {
        let mut nodes: std::collections::HashMap<String, GraphNode> = std::collections::HashMap::new();
        let mut edges: Vec<GraphEdge> = Vec::new();
        let mut edge_keys: std::collections::HashSet<(String, String, String)> =
            std::collections::HashSet::new();
        for id in [a, b] {
            if let Some(mut n) = self.get_node(id)? {
                n.degree = Some(self.degree(id)?);
                nodes.insert(n.id.clone(), n);
            } else {
                return Ok(GraphNeighborhood {
                    nodes: Vec::new(),
                    edges: Vec::new(),
                    truncated: false,
                });
            }
        }
        // 共同邻居：邻域交集
        let na: std::collections::HashSet<String> = self
            .adjacent(a, true, None, 0.0, 500)?
            .1
            .into_iter()
            .chain(self.adjacent(a, false, None, 0.0, 500)?.1)
            .collect();
        let nb: std::collections::HashSet<String> = self
            .adjacent(b, true, None, 0.0, 500)?
            .1
            .into_iter()
            .chain(self.adjacent(b, false, None, 0.0, 500)?.1)
            .collect();
        let common: Vec<String> = na.intersection(&nb).cloned().collect();
        for id in common {
            if let Some(mut n) = self.get_node(&id)? {
                n.degree = Some(self.degree(&id)?);
                nodes.insert(n.id.clone(), n);
            }
        }
        // 连接边（a↔common、b↔common）
        for center in [a, b] {
            for e in self.adjacent(center, true, None, 0.0, 500)?.0 {
                if nodes.contains_key(&e.target) {
                    let key = (e.source.clone(), e.target.clone(), e.relation.as_str().to_string());
                    if edge_keys.insert(key) {
                        edges.push(e);
                    }
                }
            }
            for e in self.adjacent(center, false, None, 0.0, 500)?.0 {
                if nodes.contains_key(&e.source) {
                    let key = (e.source.clone(), e.target.clone(), e.relation.as_str().to_string());
                    if edge_keys.insert(key) {
                        edges.push(e);
                    }
                }
            }
        }
        Ok(GraphNeighborhood {
            nodes: nodes.into_values().collect(),
            edges,
            truncated: false,
        })
    }

    /// 子图查询（BFS 深度扩展；PRD §24 get_subgraph）。语义同 neighborhood（无关系过滤）。
    pub fn subgraph(
        &self,
        node_id: &str,
        depth: u32,
        max_nodes: u32,
        max_edges: u32,
    ) -> Result<GraphNeighborhood, String> {
        self.neighborhood(node_id, depth, max_nodes, max_edges, None, 0.0)
    }
}

/// 生成边 id 前缀约定：`{source}|{target}|{relation}`（供外部构造，幂等）。
/// （当前边 id 由 upsert_edge 内部生成；本函数为外部直接构造场景预留）
#[allow(dead_code)]
pub fn edge_id(source: &str, target: &str, relation: Relation) -> String {
    format!("{}|{}|{}", source, target, relation.as_str())
}

/// 生成节点 id 约定：`{type}:{path}`（doc/folder）
pub fn node_id_for(node_type: NodeType, path: &str) -> String {
    format!("{}:{}", node_type.as_str(), path.replace('\\', "/"))
}

// ─── 集成测试（覆盖 schema 版本读取 / 空表 stats / 完整生命周期） ───

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时目录中的 GraphStore（tempfile 保证唯一 + 自动清理，消除并行/残留竞争）
    fn temp_store(name: &str) -> (GraphStore, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("mdgo_graph_test_{}_", name))
            .tempdir()
            .unwrap();
        let db = dir.path().join("mdgo.db");
        let store = GraphStore::open_for_dir(dir.path().to_string_lossy().as_ref(), &db).unwrap();
        (store, dir)
    }

    #[test]
    fn test_init_schema_reads_version_from_text_column() {
        // 回归：graph_properties.value 是 TEXT 列，版本读取必须用 Option<String>，
        // 隐式 i64 会抛 "Invalid column type Text"
        let (store, _dir) = temp_store("schema_version");
        let status = store.status().unwrap();
        assert_eq!(status.schema_version, GRAPH_SCHEMA_VERSION);
        assert_eq!(status.node_count, 0);
        assert_eq!(status.edge_count, 0);
    }

    #[test]
    fn test_stats_on_empty_graph_returns_null_last_built() {
        // 回归：空表 MAX(updated_at) 返回 NULL，须以 Option<i64> 读取
        let (store, _dir) = temp_store("stats_empty");
        let stats = store.stats().unwrap();
        assert!(stats.by_type.is_empty());
        assert!(stats.top_degree.is_empty());
        assert_eq!(stats.last_built_at, None);
    }

    #[test]
    fn test_full_lifecycle_node_edge_neighborhood() {
        let (store, _dir) = temp_store("lifecycle");

        // 节点
        let doc_a = GraphNode {
            id: "doc:a.md".into(),
            node_type: NodeType::Doc,
            name: "a.md".into(),
            path: Some("a.md".into()),
            meta: None,
            degree: None,
        created_at: None,
        
        content: None,
        
        };
        let doc_b = GraphNode {
            id: "doc:b.md".into(),
            node_type: NodeType::Doc,
            name: "b.md".into(),
            path: Some("b.md".into()),
            meta: None,
            degree: None,
        created_at: None,
        
        content: None,
        
        };
        store.upsert_node(&doc_a).unwrap();
        store.upsert_node(&doc_b).unwrap();

        // 边 + source_id（生命周期级联依据）
        store
            .upsert_edge(
                &GraphEdge {
                    source: "doc:a.md".into(),
                    target: "doc:b.md".into(),
                    relation: Relation::References,
                    weight: Some(1.0),
                    confidence: Some(1.0),
                },
                Some("doc:a.md"),
            )
            .unwrap();

        // 邻域查询
        let nb = store.neighborhood("doc:a.md", 1, 50, 100, None, 0.0).unwrap();
        assert_eq!(nb.nodes.len(), 2);
        assert_eq!(nb.edges.len(), 1);

        // 度数
        assert_eq!(store.degree("doc:a.md").unwrap(), 1);

        // 按 source_id 级联删边
        assert_eq!(store.delete_edges_by_source("doc:a.md").unwrap(), 1);
        assert_eq!(store.degree("doc:b.md").unwrap(), 0);

        // 删节点
        store.delete_node("doc:a.md").unwrap();
        assert!(store.get_node("doc:a.md").unwrap().is_none());
    }

    #[test]
    fn test_stats_with_data_reports_counts() {
        let (store, _dir) = temp_store("stats_data");
        store
            .upsert_node(&GraphNode {
                id: "doc:x.md".into(),
                node_type: NodeType::Doc,
                name: "x.md".into(),
                path: Some("x.md".into()),
                meta: None,
                degree: None,
            created_at: None,
            
            content: None,
            
            })
            .unwrap();
        // last_built_at 取自 graph_edges 的 MAX(updated_at)：需有边才有值
        store
            .upsert_edge(
                &GraphEdge {
                    source: "doc:x.md".into(),
                    target: "doc:y.md".into(),
                    relation: Relation::References,
                    weight: Some(1.0),
                    confidence: Some(1.0),
                },
                Some("doc:x.md"),
            )
            .unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(stats.by_type.get("doc"), Some(&1));
        assert_eq!(stats.top_degree.len(), 1);
        assert!(stats.last_built_at.is_some());
    }

    #[test]
    fn test_graph_version_bumps_on_mutation() {
        let (store, _dir) = temp_store("graph_version");
        assert_eq!(store.graph_version().unwrap(), 0);
        store
            .upsert_node(&GraphNode {
                id: "doc:v.md".into(),
                node_type: NodeType::Doc,
                name: "v.md".into(),
                path: Some("v.md".into()),
                meta: None,
                degree: None,
                created_at: None,
            
            content: None,
            
            })
            .unwrap();
        assert_eq!(store.graph_version().unwrap(), 1);
        store
            .upsert_edge(
                &GraphEdge {
                    source: "doc:v.md".into(),
                    target: "doc:w.md".into(),
                    relation: Relation::References,
                    weight: Some(1.0),
                    confidence: Some(1.0),
                },
                Some("doc:v.md"),
            )
            .unwrap();
        assert_eq!(store.graph_version().unwrap(), 2);
        let status = store.status().unwrap();
        assert_eq!(status.graph_version, 2);
    }

    #[test]
    fn test_find_path_bfs() {
        let (store, _dir) = temp_store("find_path");
        // a → b → c（以及 a → d → c 的更长路径，BFS 应取最短）
        for (id, name) in [("doc:a.md", "a.md"), ("doc:b.md", "b.md"), ("doc:c.md", "c.md"), ("doc:d.md", "d.md")] {
            store
                .upsert_node(&GraphNode {
                    id: id.into(),
                    node_type: NodeType::Doc,
                    name: name.into(),
                    path: Some(name.into()),
                    meta: None,
                    degree: None,
                    created_at: None,
                
                content: None,
                
                })
                .unwrap();
        }
        for (s, t) in [("doc:a.md", "doc:b.md"), ("doc:b.md", "doc:c.md"), ("doc:a.md", "doc:d.md"), ("doc:d.md", "doc:c.md")] {
            store
                .upsert_edge(
                    &GraphEdge {
                        source: s.into(),
                        target: t.into(),
                        relation: Relation::References,
                        weight: Some(1.0),
                        confidence: Some(1.0),
                    },
                    None,
                )
                .unwrap();
        }
        let path = store.find_path("doc:a.md", "doc:c.md", 6, 500).unwrap();
        assert!(path.found);
        assert_eq!(path.path_ids, vec!["doc:a.md", "doc:b.md", "doc:c.md"]);
        assert_eq!(path.nodes.len(), 3);
        assert_eq!(path.edges.len(), 2);
    }

    #[test]
    fn test_common_neighbors() {
        let (store, _dir) = temp_store("common_neighbors");
        for (id, name) in [("doc:a.md", "a.md"), ("doc:b.md", "b.md"), ("doc:common.md", "common.md")] {
            store
                .upsert_node(&GraphNode {
                    id: id.into(),
                    node_type: NodeType::Doc,
                    name: name.into(),
                    path: Some(name.into()),
                    meta: None,
                    degree: None,
                    created_at: None,
                
                content: None,
                
                })
                .unwrap();
        }
        for (s, t) in [("doc:a.md", "doc:common.md"), ("doc:b.md", "doc:common.md")] {
            store
                .upsert_edge(
                    &GraphEdge {
                        source: s.into(),
                        target: t.into(),
                        relation: Relation::References,
                        weight: Some(1.0),
                        confidence: Some(1.0),
                    },
                    None,
                )
                .unwrap();
        }
        let nb = store.common_neighbors("doc:a.md", "doc:b.md").unwrap();
        assert_eq!(nb.nodes.len(), 3); // a + b + common
        assert!(nb.nodes.iter().any(|n| n.id == "doc:common.md"));
        assert_eq!(nb.edges.len(), 2);
    }

    // ─── 后台 AI 队列（Phase 3 完整形态） ───

    #[test]
    fn test_ai_queue_lifecycle() {
        let (store, _dir) = temp_store("aiq_lifecycle");
        let docs = vec![
            ("a.md".to_string(), 0.9),
            ("b.md".to_string(), 0.6),
            ("c.md".to_string(), 0.3),
        ];
        store.enqueue_ai_docs("kb1", &docs, false).unwrap();
        // 按 importance 降序取 2 条
        let batch = store.next_ai_batch("kb1", 2).unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].rel_path, "a.md");
        assert_eq!(batch[1].rel_path, "b.md");
        assert!(batch.iter().all(|i| i.status == "processing"));
        // 完成第 1 条
        store.finish_ai_item(batch[0].id, true, 3).unwrap();
        // 统计：1 done、1 processing（c.md 仍 pending）
        let (pending, processing, done, failed) = store.queue_stats("kb1").unwrap();
        assert_eq!(pending, 1);
        assert_eq!(processing, 1);
        assert_eq!(done, 1);
        assert_eq!(failed, 0);
        // worker 指标
        assert_eq!(store.get_metric("worker_processed").unwrap(), 1);
        // 再取一批：done 不重复返回
        let batch2 = store.next_ai_batch("kb1", 10).unwrap();
        assert_eq!(batch2.len(), 1);
        assert_eq!(batch2[0].rel_path, "c.md");
    }

    #[test]
    fn test_ai_queue_idempotent_enqueue_and_reset_done() {
        let (store, _dir) = temp_store("aiq_idem");
        store
            .enqueue_ai_docs("kb1", &[("a.md".to_string(), 0.5)], false)
            .unwrap();
        let batch = store.next_ai_batch("kb1", 1).unwrap();
        store.finish_ai_item(batch[0].id, true, 3).unwrap();
        // 全量重建式入队：done 保持 done（不重复处理）
        store
            .enqueue_ai_docs("kb1", &[("a.md".to_string(), 0.8)], false)
            .unwrap();
        let (pending, _p, done, _f) = store.queue_stats("kb1").unwrap();
        assert_eq!(pending, 0);
        assert_eq!(done, 1);
        // 单文件变更入队（reset_done=true）：done → pending 重新抽取
        store
            .enqueue_ai_docs("kb1", &[("a.md".to_string(), 0.8)], true)
            .unwrap();
        let (pending, _p2, done2, _f2) = store.queue_stats("kb1").unwrap();
        assert_eq!(pending, 1);
        assert_eq!(done2, 0);
        let b2 = store.next_ai_batch("kb1", 1).unwrap();
        assert_eq!(b2[0].rel_path, "a.md");
    }

    #[test]
    fn test_ai_queue_bounded_retry_to_failed() {
        let (store, _dir) = temp_store("aiq_retry");
        store
            .enqueue_ai_docs("kb1", &[("a.md".to_string(), 0.5)], false)
            .unwrap();
        let mut id = 0i64;
        for round in 1..=3 {
            let batch = store.next_ai_batch("kb1", 1).unwrap();
            assert_eq!(batch.len(), 1, "第 {} 轮应仍有待处理项", round);
            id = batch[0].id;
            // 第 1、2 次失败 → 回退 pending 重试；第 3 次失败 → failed
            store.finish_ai_item(id, false, 3).unwrap();
        }
        let (pending, _p, _d, failed) = store.queue_stats("kb1").unwrap();
        assert_eq!(pending, 0);
        assert_eq!(failed, 1);
        assert_eq!(store.get_metric("worker_failed").unwrap(), 1);
        // failed 项不再出队
        let batch = store.next_ai_batch("kb1", 10).unwrap();
        assert!(batch.is_empty());
    }

    #[test]
    fn test_ai_queue_stale_processing_reset() {
        let (store, _dir) = temp_store("aiq_stale");
        store
            .enqueue_ai_docs("kb1", &[("a.md".to_string(), 0.5)], false)
            .unwrap();
        // 模拟 worker 崩溃：把项置为 processing 且长时间未更新（超过 stale 阈值）
        store
            .conn
            .execute(
                "UPDATE graph_ai_queue SET status = 'processing', updated_at = ?1",
                params![now_ms() - 200_000],
            )
            .unwrap();
        // 下次取队应重置为 pending 并返回
        let batch = store.next_ai_batch("kb1", 10).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].rel_path, "a.md");
        assert_eq!(batch[0].status, "processing"); // 已被重新标记
    }
}
