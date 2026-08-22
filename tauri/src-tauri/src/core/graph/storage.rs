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

use rusqlite::{params, Connection, OptionalExtension};

use super::model::{
    GraphEdge, GraphNeighborhood, GraphNode, GraphStats, GraphStatus, NodeType, Relation,
};

/// 图 schema 版本（升级时自动重建，与 BM25 `.schema_v4` 同模式）
pub const GRAPH_SCHEMA_VERSION: u32 = 1;
const SCHEMA_MARKER: &str = "graph_schema_version";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 邻域查询默认上限（与前端契约 graph-model QUERY_LIMITS 一致）
pub const DEFAULT_MAX_NODES: u32 = 200;
pub const DEFAULT_MAX_EDGES: u32 = 400;
pub const DEFAULT_DEPTH: u32 = 2;
pub const DEFAULT_WEIGHT_MIN: f32 = 0.3;

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
                "INSERT INTO graph_nodes (id, type, name, path, meta, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                    type=excluded.type, name=excluded.name, path=excluded.path, meta=excluded.meta",
                params![
                    node.id,
                    node.node_type.as_str(),
                    node.name,
                    node.path,
                    node.meta,
                    now_ms()
                ],
            )
            .map_err(|e| format!("写入图节点失败: {}", e))?;
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
                "SELECT id, type, name, path, meta FROM graph_nodes WHERE id = ?1",
                params![id],
                |row| {
                    Ok(GraphNode {
                        id: row.get(0)?,
                        node_type: NodeType::from_str(&row.get::<_, String>(1)?),
                        name: row.get(2)?,
                        path: row.get(3)?,
                        meta: row.get(4)?,
                        degree: None,
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
                        // 扇出截断：每跳邻居过多时只取一部分（degree 降序，取 max_nodes 余量）
                        if next.len() >= (max_nodes as usize).saturating_sub(nodes.len()).min(50) {
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

    // ─── 搜索 / 统计 / 状态 ───

    /// 节点搜索（name 模糊匹配，LIKE 不区分大小写）
    pub fn search_nodes(&self, keyword: &str, limit: u32) -> Result<Vec<GraphNode>, String> {
        let kw = format!("%{}%", keyword.replace('%', "%%").replace('_', "\\_"));
        let mut stmt = self
            .conn
            .prepare("SELECT id, type, name, path, meta FROM graph_nodes WHERE name LIKE ?1 ESCAPE '\\' ORDER BY length(name) LIMIT ?2")
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
        Ok(GraphStatus {
            schema_version: GRAPH_SCHEMA_VERSION,
            node_count: node_count as u64,
            edge_count: edge_count as u64,
            building: false,
            progress_pct: None,
        })
    }

    /// 清空全部图数据（重建用）
    pub fn clear(&self) -> Result<(), String> {
        self.conn
            .execute_batch("DELETE FROM graph_edges; DELETE FROM graph_nodes;")
            .map_err(|e| format!("清空图数据失败: {}", e))
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

    // ─── 概览（L0 数据源） ───    /// 全量节点（limit 截断；概览用）
    pub fn all_nodes(&self, limit: u32) -> Result<Vec<GraphNode>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, type, name, path, meta FROM graph_nodes ORDER BY id LIMIT ?1")
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

    /// 临时目录中的 GraphStore（独立 db 文件，避免污染真实知识库）
    fn temp_store(name: &str) -> (GraphStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("mdgo_graph_test_{}_{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("mdgo.db");
        let store = GraphStore::open_for_dir(dir.to_string_lossy().as_ref(), &db).unwrap();
        (store, dir)
    }

    #[test]
    fn test_init_schema_reads_version_from_text_column() {
        // 回归：graph_properties.value 是 TEXT 列，版本读取必须用 Option<String>，
        // 隐式 i64 会抛 "Invalid column type Text"
        let (store, dir) = temp_store("schema_version");
        let status = store.status().unwrap();
        assert_eq!(status.schema_version, GRAPH_SCHEMA_VERSION);
        assert_eq!(status.node_count, 0);
        assert_eq!(status.edge_count, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_stats_on_empty_graph_returns_null_last_built() {
        // 回归：空表 MAX(updated_at) 返回 NULL，须以 Option<i64> 读取
        let (store, dir) = temp_store("stats_empty");
        let stats = store.stats().unwrap();
        assert!(stats.by_type.is_empty());
        assert!(stats.top_degree.is_empty());
        assert_eq!(stats.last_built_at, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_full_lifecycle_node_edge_neighborhood() {
        let (store, dir) = temp_store("lifecycle");

        // 节点
        let doc_a = GraphNode {
            id: "doc:a.md".into(),
            node_type: NodeType::Doc,
            name: "a.md".into(),
            path: Some("a.md".into()),
            meta: None,
            degree: None,
        };
        let doc_b = GraphNode {
            id: "doc:b.md".into(),
            node_type: NodeType::Doc,
            name: "b.md".into(),
            path: Some("b.md".into()),
            meta: None,
            degree: None,
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

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_stats_with_data_reports_counts() {
        let (store, dir) = temp_store("stats_data");
        store
            .upsert_node(&GraphNode {
                id: "doc:x.md".into(),
                node_type: NodeType::Doc,
                name: "x.md".into(),
                path: Some("x.md".into()),
                meta: None,
                degree: None,
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
        std::fs::remove_dir_all(&dir).ok();
    }
}
