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
    /// 语义块（Chunk，Phase 3+ 由索引管线写入）
    Chunk,
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
