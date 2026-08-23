//! 图谱数据模型（Graph Intelligence Layer 基础类型）。
//!
//! 纯数据结构，无 IO：节点/边/属性/邻域查询结果。
//! 与前端 `graph-model.js` 的契约对齐（`docs/graph-os-frontend-design.md` §七）。

use serde::{Deserialize, Serialize};

/// 节点类型（六类图演进：doc/folder/chunk 先行，entity/experience/memory 预留）。
/// serde 序列化变体名为小写（`Doc` → "doc"），与前端 NODE_TYPES 契约对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeType {
    /// 文档（Markdown/代码/文本等可索引文件）
    Doc,
    /// 目录（Folder）
    Folder,
    /// 语义块（Chunk，由索引管线产出，知识图谱底座 Layer 1）
    Chunk,
    /// 文档结构小节（Section，Markdown 标题层级；Layer 1）
    Section,
    /// 实体（Entity，Phase 3 LLM 抽取）
    Entity,
    /// 经验（Experience，Phase 4）
    Experience,
    /// 记忆（Memory，Phase 4）
    Memory,
    /// 聚合簇（Cluster，L0 概览聚合）
    Cluster,
}

impl NodeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeType::Doc => "doc",
            NodeType::Folder => "folder",
            NodeType::Chunk => "chunk",
            NodeType::Section => "section",
            NodeType::Entity => "entity",
            NodeType::Experience => "experience",
            NodeType::Memory => "memory",
            NodeType::Cluster => "cluster",
        }
    }

    pub fn from_str(s: &str) -> NodeType {
        match s {
            "doc" => NodeType::Doc,
            "folder" => NodeType::Folder,
            "chunk" => NodeType::Chunk,
            "section" => NodeType::Section,
            "entity" => NodeType::Entity,
            "experience" => NodeType::Experience,
            "memory" => NodeType::Memory,
            _ => NodeType::Cluster,
        }
    }
}

/// 关系类型（Document Graph 阶段；后随六类图扩展）。
/// serde 序列化变体名为大写（`Contains` → "CONTAINS"），与前端 RELATIONS 枚举对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Relation {
    /// 包含（folder→doc / doc→chunk）
    Contains,
    /// 引用（doc→doc：wikilink / markdown link）
    References,
    /// 导入（代码 import / module）
    Imports,
    /// 派生（chunk→entity 等）
    DerivedFrom,
    /// 同主题（语义相似）
    SameTopic,
    /// 解决（problem→solution，Phase 4）
    SolvedBy,
    /// 实现于（solution→project/doc）
    ImplementedIn,
    /// 经验验证（solution→experience，Phase 4）
    ValidatedBy,
    /// 偏好（user→topic，Phase 4 memory）
    Prefers,
    /// 回避（user→topic，Phase 4 memory）
    Avoids,
    /// 替代（old→new，知识演进）
    ReplacedBy,
    /// 废弃（old→deprecated）
    Deprecated,
    /// 使用（entity→technology/project，PRD §9「使用」）
    Uses,
    /// 属于（entity→concept/domain，PRD §9「属于」）
    BelongsTo,
    /// 语义相似（chunk↔chunk，embedding 相似边；Layer 1 语义关系）
    SimilarTo,
    /// 依赖（代码/模块依赖，Layer 0→1 结构关系）
    DependsOn,
}

impl Relation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Relation::Contains => "CONTAINS",
            Relation::References => "REFERENCES",
            Relation::Imports => "IMPORTS",
            Relation::DerivedFrom => "DERIVED_FROM",
            Relation::SameTopic => "SAME_TOPIC",
            Relation::SolvedBy => "SOLVED_BY",
            Relation::ImplementedIn => "IMPLEMENTED_IN",
            Relation::ValidatedBy => "VALIDATED_BY",
            Relation::Prefers => "PREFERS",
            Relation::Avoids => "AVOIDS",
            Relation::ReplacedBy => "REPLACED_BY",
            Relation::Deprecated => "DEPRECATED",
            Relation::Uses => "USES",
            Relation::BelongsTo => "BELONGS_TO",
            Relation::SimilarTo => "SIMILAR_TO",
            Relation::DependsOn => "DEPENDS_ON",
        }
    }

    pub fn from_str(s: &str) -> Relation {
        match s {
            "CONTAINS" => Relation::Contains,
            "REFERENCES" => Relation::References,
            "IMPORTS" => Relation::Imports,
            "DERIVED_FROM" => Relation::DerivedFrom,
            "SAME_TOPIC" => Relation::SameTopic,
            "SOLVED_BY" => Relation::SolvedBy,
            "IMPLEMENTED_IN" => Relation::ImplementedIn,
            "VALIDATED_BY" => Relation::ValidatedBy,
            "PREFERS" => Relation::Prefers,
            "AVOIDS" => Relation::Avoids,
            "REPLACED_BY" => Relation::ReplacedBy,
            "DEPRECATED" => Relation::Deprecated,
            "USES" => Relation::Uses,
            "BELONGS_TO" => Relation::BelongsTo,
            "SIMILAR_TO" => Relation::SimilarTo,
            "DEPENDS_ON" => Relation::DependsOn,
            _ => Relation::References,
        }
    }
}

/// 图节点（对应前端 GraphNode 契约；`type` 字段名与前端一致）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    /// 序列化为 `type`（前端契约：`input.type`）；Rust 字段名保持 node_type
    #[serde(rename = "type")]
    pub node_type: NodeType,
    pub name: String,
    /// 关联文件相对路径（doc/folder 类节点）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 扩展元数据（JSON 字符串，如 `{"ext":"md","symbols":[...]}`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<String>,
    /// 度数（查询时填充）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degree: Option<u32>,
    /// 首次入库时间（ms；时间轴/知识演化数据源，PRD §30）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    /// 内容（chunk/section 节点存文本；doc 节点可为空摘要。L4 细粒度/证据用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// 图边（对应前端 GraphEdge 契约；relation 大写枚举值由 Relation 的 serde 配置保证）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub relation: Relation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

/// 邻域查询结果（graph_related / graph_expand 返回）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNeighborhood {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// 是否因截断而缺失（扇出/总数上限）
    pub truncated: bool,
}

/// 图统计（graph_stats 返回）
#[derive(Debug, Clone, Default, Serialize)]
pub struct GraphStats {
    pub by_type: std::collections::HashMap<String, u64>,
    pub top_degree: Vec<(String, u32)>,
    pub last_built_at: Option<i64>,
    /// 图版本（每次图变更 +1；前端缓存失效依据，PRD §42/§72/§73）
    pub graph_version: u64,
    /// 聚类数量
    pub cluster_count: u64,
    /// 当前聚类模式（directory=目录结构 / embedding=语义聚类；Phase 2 默认化）
    pub cluster_mode: String,
}

/// 图构建状态（graph_status 返回）
#[derive(Debug, Clone, Serialize)]
pub struct GraphStatus {
    pub schema_version: u32,
    pub node_count: u64,
    pub edge_count: u64,
    pub building: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_pct: Option<u32>,
    /// 图版本（PRD §73：所有图谱查询携带版本，前端据此失效缓存）
    pub graph_version: u64,
    /// 聚类数量
    pub cluster_count: u64,
    /// 当前聚类模式（directory=目录结构 / embedding=语义聚类；Phase 2 默认化）
    pub cluster_mode: String,
}

/// 知识簇（Cluster，L0 聚合单元；PRD §11/§13/§16）。
/// 对应 `graph_clusters` 表；成员关系由 `node.path` 顶层目录推导（Level 1 目录聚类）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphCluster {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 聚类算法（当前：directory；后续 theme/embedding/llm）
    pub algorithm: String,
    /// 核心节点 id（簇内度数最高）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub centroid: Option<String>,
    pub node_count: u32,
    pub edge_count: u32,
    pub confidence: f32,
    pub created_at: i64,
    pub updated_at: i64,
    /// 簇间关系（L0 聚合边；由命令层查询时计算填充）
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub links: Vec<ClusterLink>,
    /// 关键文件（按度数 Top 5；概览 Tab 用）
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub top_files: Vec<GraphNode>,
}

/// 簇间聚合关系（source/target 为簇 id，count = 跨簇边数）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterLink {
    pub source: String,
    pub target: String,
    pub count: u32,
}

/// 路径查询结果（graph_path 返回；PRD §24/§36）
#[derive(Debug, Clone, Default, Serialize)]
pub struct GraphPath {
    /// 是否找到可达路径
    pub found: bool,
    /// 路径节点 id 序列（source → ... → target）
    pub path_ids: Vec<String>,
    /// 路径节点（含 degree）
    pub nodes: Vec<GraphNode>,
    /// 路径相邻边
    pub edges: Vec<GraphEdge>,
}

// ─── AI 层（P1/P2：候选关系 / GraphRAG / 缺口 / 冲突 / 重复 / 演化 / 推荐） ───

/// AI 抽取的候选关系（PRD §27-28：confidence + status 状态机）。
/// 对应 `graph_ai_candidates` 表；confirm 后落正式边，AI 不直接覆盖用户事实。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphAiCandidate {
    pub id: String,
    pub source: String,
    pub target: String,
    pub relation: Relation,
    pub confidence: f32,
    /// candidate / confirmed / rejected / auto_confirmed
    pub status: String,
    /// 来源文档相对路径（来源证据，PRD §47）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_doc: Option<String>,
    /// 证据原文摘录
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    pub created_at: i64,
}

/// 后台 AI 工作队列项（`graph_ai_queue` 表；Phase 3 完整形态：构建后异步抽取）。
/// 每条对应一个文档（rel_path），worker 按 importance 降序处理；
/// 处理成功 → done，重试耗尽 → failed，失败可再次入队。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiQueueItem {
    pub id: i64,
    pub dir_path: String,
    pub rel_path: String,
    /// 文档重要度（0..=1，degree + 新鲜度 + 文件名启发式综合）
    pub importance: f64,
    /// pending / processing / done / failed
    pub status: String,
    pub attempts: u32,
    pub created_at: i64,
    pub updated_at: i64,
}

/// GraphRAG 问答结果（graph_query 返回；PRD §22-23/§47 来源证据）
#[derive(Debug, Clone, Default, Serialize)]
pub struct GraphQueryResult {
    /// LLM 生成的回答
    pub answer: String,
    /// 问题中识别出的图谱实体
    pub entities: Vec<GraphNode>,
    /// 证据（来源文档 + 摘录；可追溯）
    pub evidence: Vec<GraphEvidence>,
    /// 相关节点（图扩展产物）
    pub related: Vec<GraphNode>,
    /// 是否命中图谱证据（无 LLM 时的降级回答标记）
    pub used_llm: bool,
}

/// 单条证据（PRD §47：任何 AI 结论必须可解释、可跳转原文）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEvidence {
    /// 来源文档相对路径
    pub source_doc: String,
    /// 证据摘录
    pub snippet: String,
    /// 关联关系描述（如「doc:a 引用 doc:b」）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    /// 证据对应的 chunk 节点 id（证据下沉到语义块；前端可定位到段落）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_id: Option<String>,
}

/// 知识缺口项（PRD §52）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphGap {
    pub cluster_id: String,
    pub cluster_name: String,
    /// 已覆盖概念
    pub covered: Vec<String>,
    /// 缺失概念（AI 建议）
    pub missing: Vec<String>,
}

/// 知识冲突候选（PRD §54）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphConflict {
    /// 冲突主题（如 Redis 线程模型）
    pub topic: String,
    /// 说法 A（来源文档）
    pub claim_a: String,
    pub source_a: String,
    /// 说法 B（来源文档）
    pub claim_b: String,
    pub source_b: String,
    /// AI 分析（可能对应不同版本/阶段）
    pub analysis: String,
}

/// 知识重复候选（PRD §53）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphDuplicate {
    pub node_a: String,
    pub node_b: String,
    pub name_a: String,
    pub name_b: String,
    pub similarity: f32,
}

/// 知识演化统计（PRD §30-31/§74 可观测性）
#[derive(Debug, Clone, Default, Serialize)]
pub struct GraphEvolution {
    /// 每月新增节点数（键 YYYY-MM）
    pub monthly_nodes: Vec<(String, u64)>,
    /// 每月新增边数
    pub monthly_edges: Vec<(String, u64)>,
    /// 簇月度增长（cluster_id → [(month, count)]）
    pub cluster_growth: Vec<ClusterGrowth>,
    /// 当前图版本
    pub graph_version: u64,
}

/// 单簇月度增长
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterGrowth {
    pub cluster_id: String,
    pub cluster_name: String,
    pub monthly: Vec<(String, u64)>,
}

/// AI 推荐项（PRD §51：你可能还需要了解）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphRecommendation {
    pub node: GraphNode,
    /// 推荐理由
    pub reason: String,
    /// 推荐分数
    pub score: f32,
}

/// 图可观测性指标（PRD §74：LLM 调用次数 / Token / 失败数等）
#[derive(Debug, Clone, Default, Serialize)]
pub struct GraphMetrics {
    pub llm_calls: u64,
    pub llm_tokens: u64,
    pub llm_failures: u64,
    pub candidates_pending: u64,
    pub candidates_confirmed: u64,
    pub favorites: u64,
    /// 后台 AI worker 累计处理成功 / 失败的队列项数（Phase 3 完整形态）
    pub worker_processed: u64,
    pub worker_failed: u64,
}

// ─── 序列化契约测试（前后端对齐：type 小写 / relation 大写） ───

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_serde_contract() {
        let node = GraphNode {
            id: "doc:readme.md".into(),
            node_type: NodeType::Doc,
            name: "readme.md".into(),
            path: Some("readme.md".into()),
            meta: None,
            degree: Some(2),
        created_at: None,
        content: None,
        };
        let json = serde_json::to_string(&node).unwrap();
        // 前端 createNode 读 input.type（小写），后端必须输出 "type":"doc"
        assert!(json.contains("\"type\":\"doc\""), "node_type 未序列化为 type:doc: {}", json);
        assert!(json.contains("\"path\":\"readme.md\""), "path 缺失: {}", json);
        assert!(json.contains("\"degree\":2"), "degree 缺失: {}", json);
    }

    #[test]
    fn test_edge_serde_contract() {
        let edge = GraphEdge {
            source: "doc:a.md".into(),
            target: "doc:b.md".into(),
            relation: Relation::Contains,
            weight: Some(1.0),
            confidence: None,
        };
        let json = serde_json::to_string(&edge).unwrap();
        // 前端 RELATIONS 枚举为大写，后端必须输出 "relation":"CONTAINS"
        assert!(json.contains("\"relation\":\"CONTAINS\""), "relation 未大写: {}", json);
        let back: GraphEdge = serde_json::from_str(&json).unwrap();
        assert_eq!(back.relation, Relation::Contains);
    }

    #[test]
    fn test_node_type_roundtrip() {
        assert_eq!(NodeType::from_str("doc"), NodeType::Doc);
        assert_eq!(NodeType::from_str("entity"), NodeType::Entity);
        assert_eq!(Relation::from_str("CONTAINS"), Relation::Contains);
        assert_eq!(Relation::from_str("SOLVED_BY"), Relation::SolvedBy);
    }
}
