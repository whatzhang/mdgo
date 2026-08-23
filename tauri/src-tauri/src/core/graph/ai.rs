//! Graph AI 服务（PRD P1/P2：AI 抽取 / 簇摘要 / GraphRAG / 缺口 / 冲突 / 重复 / 推荐）。
//!
//! 设计（对齐竞品成熟做法，如 Notion AI / NotebookLM / Microsoft GraphRAG）：
//! - LLM 依赖经 [`GraphLlm`] trait 注入（services 层实现，core 不反向依赖）；
//! - 未配置 LLM 时全部能力**规则降级**（fail-open，绝不阻断图谱核心功能）；
//! - AI 产出走 `graph_ai_candidates` 状态机（PRD §27-28）：confidence ≥ 0.9 自动确认，
//!   否则进入待确认队列，由用户确认后落正式边；AI 永不直接覆盖用户事实（PRD §49）；
//! - 所有 AI 结论携带来源证据（source_doc + evidence 摘录，PRD §47）；
//! - 成本控制（PRD §75-76）：只对高价值文档（度数高/被引用多）做 LLM 抽取。
//!
//! 【并发约定】MutexGuard 不得跨越 await —— 所有方法「同步读 → 释放锁 → await → 同步写」。

use std::sync::Arc;

use async_trait::async_trait;

use super::merger::EntityMerger;
use super::model::{
    GraphAiCandidate, GraphConflict, GraphDuplicate, GraphEvidence, GraphGap, GraphNode,
    GraphQueryResult, GraphRecommendation, NodeType, Relation,
};
use super::storage::{node_id_for, GraphStore};
use super::GraphEngine;

/// LLM 门面（core 层接口；services 层用 LLMClient 实现）。
#[async_trait]
pub trait GraphLlm: Send + Sync {
    /// 结构化 JSON 补全（失败返回 None）
    async fn json(&self, system: &str, user: &str) -> Option<serde_json::Value>;
    /// 文本补全（失败返回 None）
    async fn text(&self, system: &str, user: &str, max_tokens: u32) -> Option<String>;
    /// 是否为降级空实现（LLM 未配置）。调用方据此区分
    /// 「未配置 → 规则降级（正常）」与「已配置但调用失败（需重试/告警）」（D1 修复）。
    fn is_null(&self) -> bool {
        false
    }
}

/// 空实现（LLM 未配置时注入；全部操作降级为规则/跳过，fail-open）
pub struct NullGraphLlm;

#[async_trait]
impl GraphLlm for NullGraphLlm {
    async fn json(&self, _system: &str, _user: &str) -> Option<serde_json::Value> {
        None
    }
    async fn text(&self, _system: &str, _user: &str, _max_tokens: u32) -> Option<String> {
        None
    }
    fn is_null(&self) -> bool {
        true
    }
}

/// Graph AI 服务：所有操作以 dir_path 为入口（内部经 GraphEngine 获取 store）。
pub struct GraphAiService {
    engine: Arc<GraphEngine>,
}

/// LLM 关系中文标签 → Relation（PRD §9 关系类型全集）
pub fn relation_from_label(label: &str) -> Option<Relation> {
    let l = label.trim().to_lowercase();
    match l.as_str() {
        "包含" | "contains" => Some(Relation::Contains),
        "引用" | "references" | "refer" | "referenced" => Some(Relation::References),
        "依赖" | "导入" | "imports" | "import" | "depends" => Some(Relation::Imports),
        "实现" | "implemented_in" | "implements" | "implemented" => Some(Relation::ImplementedIn),
        "相关" | "same_topic" | "related" | "related_to" => Some(Relation::SameTopic),
        "派生" | "产生" | "derived_from" | "derived" | "produced_by" => Some(Relation::DerivedFrom),
        "解决" | "solved_by" | "solves" | "solved" => Some(Relation::SolvedBy),
        "替代" | "replaced_by" | "replaces" | "replaced" => Some(Relation::ReplacedBy),
        "使用" | "uses" | "use" | "used_by" => Some(Relation::Uses),
        "属于" | "belongs_to" | "belongs" | "part_of" => Some(Relation::BelongsTo),
        "验证" | "validated_by" | "validates" => Some(Relation::ValidatedBy),
        _ => None,
    }
}

impl GraphAiService {
    pub fn new(engine: Arc<GraphEngine>) -> Self {
        Self { engine }
    }

    /// 在 store 锁内执行同步闭包（Arc 生命周期由本函数管理，避免守卫跨 await）。
    fn with_store<T>(
        &self,
        dir_path: &str,
        f: impl FnOnce(&GraphStore) -> Result<T, String>,
    ) -> Result<T, String> {
        let store = self.engine.store(dir_path)?;
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        f(&guard)
    }

    // ─── AI 实体关系抽取（PRD §26 Level 3 / §27-28） ───

    /// 对单文档做 LLM 关系抽取：产出实体节点 + 候选关系（状态机）。
    /// 返回本次新增的候选数。未配置 LLM 时返回 Ok(0)（规则抽取由 extractor.rs 覆盖）。
    pub async fn extract_relations(
        &self,
        dir_path: &str,
        rel_path: &str,
        content: &str,
        llm: &dyn GraphLlm,
    ) -> Result<usize, String> {
        if content.trim().is_empty() {
            return Ok(0);
        }
        // 成本控制（PRD §75-76）：内容截断到 ~4000 字符
        let content_truncated: String = content.chars().take(4000).collect();
        let system = "你是知识图谱关系抽取助手。从文档中抽取实体之间的语义关系，只输出 JSON：\
            {\"relations\":[{\"source\":\"实体A\",\"target\":\"实体B\",\"relation\":\"关系类型\",\"confidence\":0.0-1.0,\"evidence\":\"原文摘录20-60字\"}]}\
            关系类型只能是（中文）：包含/引用/依赖/实现/相关/派生/属于/解决/使用/替代。\
            要求：source/target 必须是文档中出现的实体或概念名；confidence 表示确信度；\
            每条必须给 evidence 原文摘录；最多 10 条；没有明确关系时 relations 为空数组。除 JSON 外不要输出任何内容。";
        let user = format!("文档路径：{}\n\n文档内容：\n{}", rel_path, content_truncated);
        let Some(json) = llm.json(system, &user).await else {
            // D1：区分「未配置（规则降级，正常）」与「已配置但调用失败（Err，可重试）」
            if llm.is_null() {
                return Ok(0);
            }
            self.record_metric(dir_path, "llm_failures", 1).ok();
            return Err(format!("LLM 关系抽取调用失败（{}）", rel_path));
        };
        self.record_metric(dir_path, "llm_calls", 1).ok();

        let mut count = 0usize;
        let relations = json
            .get("relations")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if relations.is_empty() {
            return Ok(0);
        }
        self.with_store(dir_path, |store| {
            let mut merger = EntityMerger::new(store);
            // 来源文档用 doc 节点 id（与规则抽取 extractor.rs 一致），
            // 避免 DERIVED_FROM 边指向裸路径的「幽灵节点」导致来源追溯/级联删除失效
            let doc_node_id = node_id_for(NodeType::Doc, rel_path);
            let mut cand_seq = 0usize;
            for item in relations.iter().take(10) {
                let source_name = item.get("source").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let target_name = item.get("target").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let rel_label = item.get("relation").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let confidence = item.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.5).clamp(0.0, 1.0) as f32;
                let evidence = item.get("evidence").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                if source_name.is_empty() || target_name.is_empty() || source_name == target_name {
                    continue;
                }
                let Some(relation) = relation_from_label(&rel_label) else { continue; };

                // 实体消歧合并（复用规则抽取同款 merger）
                let source_id = match merger.upsert_entity(&source_name, &[], Some(&doc_node_id)) {
                    Ok(id) => id,
                    Err(_) => continue,
                };
                let target_id = match merger.upsert_entity(&target_name, &[], Some(&doc_node_id)) {
                    Ok(id) => id,
                    Err(_) => continue,
                };

                // 候选 id：时间戳 + 递增序号（避免同毫秒同三元组覆盖）
                cand_seq += 1;
                let id = format!(
                    "cand:{}|{}|{}|{}|{}",
                    source_id,
                    target_id,
                    relation.as_str(),
                    super::storage::now_ms_public(),
                    cand_seq
                );
                let candidate = GraphAiCandidate {
                    id,
                    source: source_id,
                    target: target_id,
                    relation,
                    confidence,
                    status: "candidate".to_string(),
                    source_doc: Some(rel_path.to_string()),
                    evidence: if evidence.is_empty() { None } else { Some(evidence) },
                    created_at: super::storage::now_ms_public(),
                };
                store.upsert_candidate(&candidate)?;
                // 自动确认规则（PRD §28）：confidence ≥ 0.9 → auto_confirmed（落正式边）
                if confidence >= 0.9 {
                    store.update_candidate_status(&candidate.id, "auto_confirmed")?;
                }
                count += 1;
            }
            Ok(count)
        })
    }

    // ─── AI 簇摘要（PRD §29/§17） ───

    /// 生成/更新知识簇描述（AI；失败返回 None，保留目录规则描述）。
    pub async fn summarize_cluster(
        &self,
        dir_path: &str,
        cluster_id: &str,
        llm: &dyn GraphLlm,
    ) -> Result<Option<String>, String> {
        // 同步读（锁内）
        let info = self.with_store(
            dir_path,
            |store| -> Result<Option<(String, u32, Vec<String>, Vec<String>)>, String> {
                let Some(cluster) = store.get_cluster(cluster_id)? else { return Ok(None); };
                let members = store.cluster_members(cluster_id)?;
                let entities: Vec<String> = members
                    .iter()
                    .filter(|m| m.node_type == NodeType::Entity)
                    .map(|m| m.name.clone())
                    .take(20)
                    .collect();
                let files: Vec<String> = members
                    .iter()
                    .filter(|m| m.node_type == NodeType::Doc && m.path.is_some())
                    .map(|m| m.name.clone())
                    .take(10)
                    .collect();
                Ok(Some((cluster.name.clone(), cluster.node_count, entities, files)))
            },
        )?;
        let Some((cluster_name, node_count, entities, files)) = info else {
            return Ok(None);
        };
        if entities.is_empty() && files.is_empty() {
            return Ok(None);
        }

        // 异步 LLM（锁外）
        let system = "你是知识图谱摘要助手。根据知识簇的实体与文件清单，输出一段中文簇描述与标签，只输出 JSON：\
            {\"description\":\"60-120字描述\",\"tags\":[\"标签1\",\"标签2\",...]}。描述要概括该知识簇覆盖的主题，标签 3-8 个。除 JSON 外不要输出任何内容。";
        let user = format!(
            "知识簇：{}（{} 节点）\n关键实体：{}\n关键文件：{}\n",
            cluster_name,
            node_count,
            if entities.is_empty() { "（无）".to_string() } else { entities.join("、") },
            if files.is_empty() { "（无）".to_string() } else { files.join("、") }
        );
        let Some(json) = llm.json(system, &user).await else {
            self.record_metric(dir_path, "llm_failures", 1).ok();
            return Ok(None);
        };
        self.record_metric(dir_path, "llm_calls", 1).ok();
        let description = json
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // 同步写（锁内）
        if let Some(desc) = &description {
            let tags = json
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join("、"))
                .unwrap_or_default();
            let final_desc = if tags.is_empty() {
                desc.clone()
            } else {
                format!("{}\n标签：{}", desc, tags)
            };
            self.with_store(dir_path, |store| store.update_cluster_description(cluster_id, &final_desc))?;
        }
        Ok(description)
    }

    // ─── 知识缺口（PRD §52） ───

    /// 检测某簇的知识缺口：LLM 建议缺失概念；未配置 LLM 时用「相邻簇实体差集」规则降级。
    pub async fn detect_gaps(
        &self,
        dir_path: &str,
        cluster_id: &str,
        llm: Option<&dyn GraphLlm>,
    ) -> Result<Vec<GraphGap>, String> {
        // 同步读（锁内）
        let (cluster_name, covered, neighbor_entities) = self.with_store(
            dir_path,
            |store| -> Result<(String, Vec<String>, Vec<String>), String> {
                let Some(cluster) = store.get_cluster(cluster_id)? else {
                    return Ok((String::new(), Vec::new(), Vec::new()));
                };
                let members = store.cluster_members(cluster_id)?;
                let covered: Vec<String> = members
                    .iter()
                    .filter(|m| m.node_type == NodeType::Entity)
                    .map(|m| m.name.clone())
                    .take(20)
                    .collect();
                let neighbor_entities = self.neighbor_cluster_entities(store, cluster_id)?;
                Ok((cluster.name.clone(), covered, neighbor_entities))
            },
        )?;
        if cluster_name.is_empty() {
            return Ok(Vec::new());
        }

        // 异步 LLM（锁外）
        if let Some(llm) = llm {
            let system = "你是知识体系分析师。用户给出一个知识簇已覆盖的概念清单，请判断该知识领域可能缺失的关键概念（如 RAG 领域缺少 Evaluation/Benchmark）。\
                只输出 JSON：{\"missing\":[\"概念1\",...]}，3-8 个，必须是真实存在、与已覆盖概念同领域的概念。除 JSON 外不要输出任何内容。";
            let user = format!(
                "知识簇：{}\n已覆盖概念：{}\n",
                cluster_name,
                if covered.is_empty() { "（无）".to_string() } else { covered.join("、") }
            );
            if let Some(json) = llm.json(system, &user).await {
                self.record_metric(dir_path, "llm_calls", 1).ok();
                let missing: Vec<String> = json
                    .get("missing")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                if !missing.is_empty() {
                    return Ok(vec![GraphGap {
                        cluster_id: cluster_id.to_string(),
                        cluster_name,
                        covered,
                        missing,
                    }]);
                }
            } else {
                self.record_metric(dir_path, "llm_failures", 1).ok();
            }
        }
        // 规则降级结果
        let missing: Vec<String> = neighbor_entities
            .into_iter()
            .filter(|e| !covered.iter().any(|c| super::merger::canonicalize(c) == super::merger::canonicalize(e)))
            .take(8)
            .collect();
        Ok(if missing.is_empty() {
            Vec::new()
        } else {
            vec![GraphGap {
                cluster_id: cluster_id.to_string(),
                cluster_name,
                covered,
                missing,
            }]
        })
    }

    /// 相邻簇（跨簇边）中的实体名集合（规则降级用）
    fn neighbor_cluster_entities(
        &self,
        store: &GraphStore,
        cluster_id: &str,
    ) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        if let Some(c) = store.get_cluster(cluster_id)? {
            for link in c.links {
                let other = if link.source == c.id { &link.target } else { &link.source };
                if let Ok(members) = store.cluster_members(other) {
                    for m in members {
                        if m.node_type == NodeType::Entity && !out.contains(&m.name) {
                            out.push(m.name);
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    // ─── 知识冲突（PRD §54） ───

    /// 冲突检测：取度数最高的若干实体（≥2 个来源文档），比较两个来源的上下文片段。
    /// 未配置 LLM 时返回空（规则无法判断语义冲突）。
    pub async fn detect_conflicts(
        &self,
        dir_path: &str,
        llm: &dyn GraphLlm,
    ) -> Result<Vec<GraphConflict>, String> {
        // 同步收集（锁内：实体 → 来源文档 → 上下文片段；文件读取可接受）
        struct Candidate {
            entity_id: String,
            d0: String,
            d1: String,
            snippet0: String,
            snippet1: String,
        }
        let candidates: Vec<Candidate> = self.with_store(dir_path, |store| {
            let top = store.top_degree_entities(3)?;
            let mut list = Vec::new();
            for (entity_id, _deg) in top {
                let docs = store.source_docs_for_entity(&entity_id)?;
                if docs.len() < 2 {
                    continue;
                }
                let d0 = docs[0].clone();
                let d1 = docs[1].clone();
                let snippet0 = read_context_around(dir_path, &d0, &entity_id, 300);
                let snippet1 = read_context_around(dir_path, &d1, &entity_id, 300);
                if snippet0.is_empty() || snippet1.is_empty() {
                    continue;
                }
                list.push(Candidate {
                    entity_id,
                    d0,
                    d1,
                    snippet0,
                    snippet1,
                });
            }
            Ok(list)
        })?;

        // 异步 LLM（锁外）
        let mut conflicts = Vec::new();
        for cand in candidates {
            let system = "你是知识冲突检测助手。比较两个文档中关于同一主题的论述，判断是否存在冲突（如版本差异、事实矛盾）。\
                只输出 JSON：{\"conflict\":true/false,\"topic\":\"主题\",\"analysis\":\"50-100字分析\"}。\
                若论述一致或只是互补，conflict 必须为 false。除 JSON 外不要输出任何内容。";
            let user = format!(
                "主题实体：{}（节点 {}）\n\n文档 A（{}）：\n{}\n\n文档 B（{}）：\n{}",
                cand.entity_id.split(':').last().unwrap_or(&cand.entity_id),
                cand.entity_id,
                cand.d0,
                cand.snippet0,
                cand.d1,
                cand.snippet1
            );
            let Some(json) = llm.json(system, &user).await else {
                self.record_metric(dir_path, "llm_failures", 1).ok();
                continue;
            };
            self.record_metric(dir_path, "llm_calls", 1).ok();
            if json.get("conflict").and_then(|v| v.as_bool()).unwrap_or(false) {
                conflicts.push(GraphConflict {
                    topic: json.get("topic").and_then(|v| v.as_str()).unwrap_or(&cand.entity_id).to_string(),
                    claim_a: truncate_chars(&cand.snippet0, 200),
                    source_a: cand.d0.clone(),
                    claim_b: truncate_chars(&cand.snippet1, 200),
                    source_b: cand.d1.clone(),
                    analysis: json.get("analysis").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                });
            }
        }
        Ok(conflicts)
    }

    // ─── 知识重复（PRD §53，规则式：规范化名相似度） ───

    /// 重复概念检测：实体节点规范化名两两 Levenshtein 相似度 ≥ 0.85 → 建议合并。
    /// O(n²) 全对比较，实体过多时截断（配合命令层 spawn_blocking 避免阻塞 UI）。
    pub fn detect_duplicates(&self, dir_path: &str, limit: usize) -> Result<Vec<GraphDuplicate>, String> {
        self.with_store(dir_path, |store| {
            let mut entities: Vec<GraphNode> = Vec::new();
            for n in store.all_nodes(200_000)? {
                if n.node_type == NodeType::Entity {
                    entities.push(n);
                }
            }
            // 成本保护（PRD §75）：实体 > 5000 时只比较度数最高的 5000 个（大图降级）
            if entities.len() > 5000 {
                entities.sort_by(|a, b| b.degree.unwrap_or(0).cmp(&a.degree.unwrap_or(0)));
                entities.truncate(5000);
            }
            let mut out = Vec::new();
            for i in 0..entities.len() {
                for j in (i + 1)..entities.len() {
                    let a = &entities[i];
                    let b = &entities[j];
                    let ca = super::merger::canonicalize(&a.name);
                    let cb = super::merger::canonicalize(&b.name);
                    if ca.is_empty() || cb.is_empty() || ca == cb {
                        continue;
                    }
                    let sim = levenshtein_ratio(&ca, &cb);
                    if sim >= 0.85 {
                        out.push(GraphDuplicate {
                            node_a: a.id.clone(),
                            node_b: b.id.clone(),
                            name_a: a.name.clone(),
                            name_b: b.name.clone(),
                            similarity: sim,
                        });
                        if out.len() >= limit {
                            return Ok(out);
                        }
                    }
                }
            }
            Ok(out)
        })
    }

    // ─── GraphRAG（PRD §22-23：意图 → 实体检测 → 图扩展 + 混合检索 → 上下文 → LLM） ───

    /// GraphRAG 问答。`hybrid_hits`：命令层预计算的混合检索命中（doc_name, text, score）。
    pub async fn graph_rag(
        &self,
        dir_path: &str,
        question: &str,
        llm: &dyn GraphLlm,
        hybrid_hits: &[(String, String, f32, u32)],
    ) -> Result<GraphQueryResult, String> {
        let mut result = GraphQueryResult::default();

        // 1) 意图 → 实体检测（LLM；失败则规则：从问题中匹配已有图节点）
        let mut entity_names: Vec<String> = Vec::new();
        let system = "你是知识图谱查询助手。从用户问题中提取要查询的实体/概念名（图谱中的名词），只输出 JSON：\
            {\"entities\":[\"Redis\",\"Kafka\"]}。只提取问题中明确提到的实体，最多 5 个；没有则给空数组。除 JSON 外不要输出任何内容。";
        if let Some(json) = llm.json(system, question).await {
            self.record_metric(dir_path, "llm_calls", 1).ok();
            entity_names = json
                .get("entities")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
        } else {
            // D1：未配置（Null）不计失败——降级为规则实体检测，属正常路径
            if !llm.is_null() {
                self.record_metric(dir_path, "llm_failures", 1).ok();
            }
        }

        // 2) 实体 → 图节点（name 匹配）+ 图扩展（同步，锁内）
        let (entities, related, relation_lines) = self.with_store(
            dir_path,
            |store| -> Result<(Vec<GraphNode>, Vec<GraphNode>, Vec<String>), String> {
                let mut entities: Vec<GraphNode> = Vec::new();
                for name in &entity_names {
                    if let Some(node) = find_best_node(store, name)? {
                        if !entities.iter().any(|e| e.id == node.id) {
                            entities.push(node);
                        }
                    }
                }
                // 规则兜底：问题分词在图中直接搜索
                if entities.is_empty() {
                    for token in question
                        .split(|c: char| !c.is_alphanumeric() && !c.is_ascii_whitespace())
                        .filter(|t| t.chars().count() >= 2)
                    {
                        if let Some(node) = find_best_node(store, token)? {
                            if !entities.iter().any(|e| e.id == node.id) {
                                entities.push(node);
                            }
                        }
                    }
                }
                // 图扩展（1 跳）：用 id→name 映射解析关系行两端（不用字面量 "?"）
                let mut related: Vec<GraphNode> = Vec::new();
                let mut relation_lines: Vec<String> = Vec::new();
                let mut name_of: std::collections::HashMap<String, String> = std::collections::HashMap::new();
                for e in &entities {
                    name_of.insert(e.id.clone(), e.name.clone());
                }
                for r in &related {
                    name_of.insert(r.id.clone(), r.name.clone());
                }
                for e in &entities {
                    let nb = store.neighborhood(&e.id, 1, 40, 80, None, 0.0)?;
                    for n in nb.nodes {
                        if n.id != e.id && !related.iter().any(|r| r.id == n.id) {
                            let (nid, nname) = (n.id.clone(), n.name.clone());
                            related.push(n);
                            name_of.insert(nid, nname);
                        }
                    }
                    for edge in nb.edges {
                        let src = name_of.get(&edge.source).cloned().unwrap_or_else(|| "?".to_string());
                        let dst = name_of.get(&edge.target).cloned().unwrap_or_else(|| "?".to_string());
                        relation_lines.push(format!("{} --{}--> {}", src, edge.relation.as_str(), dst));
                    }
                }
                Ok((entities, related, relation_lines))
            },
        )?;
        result.entities = entities.clone();
        result.related = related;

        // 3) 证据（PRD §47）：混合检索命中 + 图关系。
        // 混合检索命中的 doc_name/text/chunk_index 直接对应图内 chunk 节点（证据下沉到语义块）。
        // 图关系证据无文档路径（实体名≠文件），source_doc 置空 → 前端显示「图关系」且不可点击打开。
        for (doc, text, _score, chunk_index) in hybrid_hits.iter().take(8) {
            result.evidence.push(GraphEvidence {
                source_doc: doc.clone(),
                snippet: truncate_chars(text, 180),
                relation: None,
                chunk_id: Some(crate::core::graph::chunk::chunk_node_id(doc, *chunk_index)),
            });
        }
        let mut seen_rel = std::collections::HashSet::new();
        for line in relation_lines.iter().take(20) {
            if seen_rel.insert(line.clone()) {
                result.evidence.push(GraphEvidence {
                    source_doc: String::new(),
                    snippet: line.clone(),
                    relation: None,
                    chunk_id: None,
                });
            }
        }

        // 4) 上下文组装 + LLM 回答（锁外）
        let graph_context = if relation_lines.is_empty() {
            "（图中未发现相关关系）".to_string()
        } else {
            relation_lines.into_iter().take(30).collect::<Vec<_>>().join("\n")
        };
        let chunks: String = hybrid_hits
            .iter()
            .take(6)
            .map(|(doc, text, score, _chunk_idx)| format!("[{:.2}] {}\n{}", score, doc, truncate_chars(text, 400)))
            .collect::<Vec<_>>()
            .join("\n---\n");

        let entity_list = if entities.is_empty() {
            "（未识别到图实体）".to_string()
        } else {
            entities.iter().map(|e| e.name.clone()).collect::<Vec<_>>().join("、")
        };

        let system = "你是本地知识图谱问答助手（GraphRAG）。基于「图谱关系证据」和「文档检索证据」回答用户问题。\
            要求：1) 先陈述图谱中发现的实体关系；2) 引用证据来源文档；3) 不确定的信息明确说明；4) 回答简洁有条理（200 字内）。\
            证据可能不完整，不要编造图中不存在的知识。";
        let user = format!(
            "问题：{}\n\n识别的图实体：{}\n\n图谱关系证据：\n{}\n\n文档检索证据：\n{}\n",
            question, entity_list, graph_context, chunks
        );
        if let Some(answer) = llm.text(system, &user, 1200).await {
            result.answer = answer;
            result.used_llm = true;
            self.record_metric(dir_path, "llm_calls", 1).ok();
        } else {
            // D1：未配置（Null）不计失败——降级回答属正常路径
            if !llm.is_null() {
                self.record_metric(dir_path, "llm_failures", 1).ok();
            }
            // 降级回答（无 LLM）：列出图证据
            let mut lines = vec![format!("检测到图实体：{}", entity_list)];
            if !graph_context.is_empty() {
                lines.push(format!("发现的关系：\n{}", graph_context));
            }
            if !hybrid_hits.is_empty() {
                lines.push(format!(
                    "相关文档：{}",
                    hybrid_hits.iter().take(5).map(|(d, _, _, _)| d.clone()).collect::<Vec<_>>().join("、")
                ));
            }
            result.answer = lines.join("\n\n");
        }
        Ok(result)
    }

    // ─── 知识推荐（PRD §51，规则式：二跳共邻） ───

    /// 基于图的推荐：当前节点 → 二跳节点（排除自身与直接邻居），
    /// 按「共同邻居数 × 2 + 目标度数」打分（Obsidian/Notion 同类推荐的图论近似）。
    pub fn recommend(
        &self,
        dir_path: &str,
        node_id: &str,
        limit: usize,
    ) -> Result<Vec<GraphRecommendation>, String> {
        self.with_store(dir_path, |store| {
            let Some(node) = store.get_node(node_id)? else { return Ok(Vec::new()); };
            let neighbors: std::collections::HashSet<String> = store.neighbors_of(node_id)?.into_iter().collect();
            let mut common: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
            for nb in &neighbors {
                for nb2 in store.neighbors_of(nb)? {
                    if nb2 == node_id || neighbors.contains(&nb2) {
                        continue;
                    }
                    *common.entry(nb2).or_insert(0) += 1;
                }
            }
            let mut recs: Vec<GraphRecommendation> = Vec::new();
            for (cand_id, common_count) in common {
                if let Some(mut n) = store.get_node(&cand_id)? {
                    let degree = store.degree(&cand_id)?;
                    n.degree = Some(degree);
                    let s = (common_count as f32) * 2.0 + (degree as f32) * 0.5;
                    recs.push(GraphRecommendation {
                        node: n,
                        reason: format!(
                            "与「{}」存在 {} 个共同关联节点，关联强度较高",
                            node.name, common_count
                        ),
                        score: s,
                    });
                }
            }
            recs.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
            recs.truncate(limit);
            Ok(recs)
        })
    }

    // ─── 知识演化分析（PRD §30-31） ───

    /// 演化统计 + AI 洞察（增长/衰减/替代，文本形式；未配置 LLM 时仅返回统计）。
    pub async fn evolution_insights(
        &self,
        dir_path: &str,
        llm: Option<&dyn GraphLlm>,
    ) -> Result<(super::model::GraphEvolution, Option<String>), String> {
        let evolution = self.with_store(dir_path, |store| store.evolution())?;
        if let Some(llm) = llm {
            let summary: String = evolution
                .monthly_nodes
                .iter()
                .rev()
                .take(12)
                .map(|(m, c)| format!("{}: +{} 节点", m, c))
                .collect::<Vec<_>>()
                .join("\n");
            if summary.is_empty() {
                return Ok((evolution, None));
            }
            let growth: Vec<String> = evolution
                .cluster_growth
                .iter()
                .filter(|g| !g.monthly.is_empty())
                .map(|g| {
                    let total: u64 = g.monthly.iter().map(|(_, c)| c).sum();
                    format!("{}: 累计 {} 节点", g.cluster_name, total)
                })
                .take(10)
                .collect();
            let system = "你是知识演化分析师。根据知识库月度增长统计，用中文输出 3-5 句洞察：\
                增长最快的领域、最近变化、可能的趋势。语气客观，不超过 150 字。只输出正文，不要任何前缀。";
            let user = format!("月度节点增长：\n{}\n\n领域累计：\n{}", summary, growth.join("\n"));
            let insight = llm.text(system, &user, 400).await;
            if insight.is_some() {
                self.record_metric(dir_path, "llm_calls", 1).ok();
            } else {
                self.record_metric(dir_path, "llm_failures", 1).ok();
            }
            Ok((evolution, insight))
        } else {
            Ok((evolution, None))
        }
    }

    /// 演化统计（纯数据，无 LLM）
    pub fn evolution(&self, dir_path: &str) -> Result<super::model::GraphEvolution, String> {
        self.with_store(dir_path, |store| store.evolution())
    }

    // ─── Memory Graph（PRD §60 P2：用户偏好记忆，PREFERS/AVOIDS 边） ───

    /// 记录一条用户偏好（topic → 实体/概念节点；relation = PREFERS / AVOIDS）。
    /// 形成「个人知识偏好图」，供 AI 上下文 / 首页推荐使用。
    pub fn memory_set(
        &self,
        dir_path: &str,
        topic: &str,
        preference: bool,
        source: Option<&str>,
    ) -> Result<(), String> {
        self.with_store(dir_path, |store| {
            let mut merger = EntityMerger::new(store);
            // 来源文档同样转 doc 节点 id（避免幽灵节点边）
            let source_doc = source.map(|s| node_id_for(NodeType::Doc, s));
            let topic_id = merger.upsert_entity(topic, &[], source_doc.as_deref())?;
            let user_id = "memory:user".to_string();
            // 用户节点（幂等）
            store.upsert_node(&GraphNode {
                id: user_id.clone(),
                node_type: NodeType::Memory,
                name: "我（我的知识偏好）".to_string(),
                path: None,
                meta: None,
                degree: None,
                created_at: None,
            content: None,
            })?;
            let relation = if preference { Relation::Prefers } else { Relation::Avoids };
            store.upsert_edge(
                &super::model::GraphEdge {
                    source: user_id,
                    target: topic_id,
                    relation,
                    weight: Some(1.0),
                    confidence: Some(1.0),
                },
                Some("memory:user"),
            )?;
            Ok(())
        })
    }

    /// 我的知识偏好列表（Memory Graph 读取；My Knowledge 上下文源）
    pub fn memory_preferences(&self, dir_path: &str) -> Result<Vec<GraphRecommendation>, String> {
        self.with_store(dir_path, |store| {
            let user_id = "memory:user".to_string();
            let mut out = Vec::new();
            if store.get_node(&user_id)?.is_none() {
                return Ok(out);
            }
            for nb in store.neighbors_of(&user_id)? {
                if let Some(mut n) = store.get_node(&nb)? {
                    n.degree = Some(store.degree(&nb)?);
                    // 判断偏好方向（user --PREFERS/AVOIDS--> topic）
                    let prefer = store
                        .all_edges_between(&user_id, &nb)?
                        .iter()
                        .any(|e| e.relation == Relation::Prefers);
                    out.push(GraphRecommendation {
                        node: n,
                        reason: if prefer { "我的偏好（喜欢）".to_string() } else { "我的偏好（回避）".to_string() },
                        score: 1.0,
                    });
                }
            }
            Ok(out)
        })
    }

    // ─── 指标（PRD §74） ───

    fn record_metric(&self, dir_path: &str, key: &str, delta: i64) -> Result<(), String> {
        if let Ok(store) = self.engine.store(dir_path) {
            if let Ok(guard) = store.lock() {
                let _ = guard.bump_metric(key, delta);
            }
        }
        Ok(())
    }
}

// ─── Experience LLM 富化（Phase 4.2：事件描述 → 结构化 P/S 抽取） ───

/// `ExperienceLlmExtractor` 的 LLM 适配实现（包装 [`GraphLlm`]）。
/// 命令层在 LLM 已配置时注入 `graph_experience_record`；未配置时 record 保持规则降级。
/// 成本控制（PRD §75-76）：正文截断到 2000 字符。
pub struct LlmExperienceExtractor {
    llm: Arc<dyn GraphLlm>,
}

impl LlmExperienceExtractor {
    pub fn new(llm: Arc<dyn GraphLlm>) -> Self {
        Self { llm }
    }
}

impl super::experience::ExperienceLlmExtractor for LlmExperienceExtractor {
    fn extract_problem_solution(
        &self,
        title: &str,
        body: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(String, String), String>> + Send + '_>,
    > {
        let system = "你是经验知识结构化助手。从一条经验记录（标题+正文）中抽取：问题描述（problem）与解决方案（solution）。\
            只输出 JSON：{\"problem\":\"...\",\"solution\":\"...\"}。\
            要求：problem 是遇到了什么问题/要解决什么；solution 是怎么解决的（方法/结论）；\
            每条不超过 120 字；从原文提炼，不要编造；无法抽取时给空字符串。除 JSON 外不要输出任何内容。";
        let body_truncated: String = body.chars().take(2000).collect();
        let user = format!("标题：{}\n\n正文：\n{}", title, body_truncated);
        Box::pin(async move {
            let json = self
                .llm
                .json(system, &user)
                .await
                .ok_or_else(|| "LLM 未返回有效 JSON".to_string())?;
            let problem = json
                .get("problem")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let solution = json
                .get("solution")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if problem.is_empty() && solution.is_empty() {
                return Err("LLM 未抽取到有效 P/S".to_string());
            }
            Ok((problem, solution))
        })
    }
}

// ─── 工具函数 ───

/// 名称 → 图节点（精确/规范名匹配优先，其次 LIKE）
fn find_best_node(store: &GraphStore, name: &str) -> Result<Option<GraphNode>, String> {
    let canon = super::merger::canonicalize(name);
    let hits = store.search_nodes(name, 20)?;
    if hits.is_empty() {
        return Ok(None);
    }
    // 1) 规范化名完全一致
    for h in &hits {
        if super::merger::canonicalize(&h.name) == canon {
            return Ok(Some(h.clone()));
        }
    }
    // 2) 名称包含匹配（双向）
    for h in &hits {
        let hc = super::merger::canonicalize(&h.name);
        if (hc.contains(&canon) && !canon.is_empty()) || canon.contains(&hc) {
            return Ok(Some(h.clone()));
        }
    }
    // 3) 首个命中
    Ok(hits.into_iter().next())
}

/// 读取文档中实体附近的上下文片段（证据，PRD §47）。
/// UTF-8 安全：字节索引换算为字符位置后切片（中文文档不会触发 char boundary panic）。
fn read_context_around(dir_path: &str, rel_path: &str, entity_id: &str, radius: usize) -> String {
    let abs = std::path::Path::new(dir_path).join(rel_path);
    let content = match std::fs::read_to_string(&abs) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let name = entity_id.split(':').last().unwrap_or(entity_id);
    match content.find(name) {
        Some(byte_idx) => {
            // find 返回的字节索引必在字符边界；换算为字符位置后按字符切片
            let char_idx = content[..byte_idx].chars().count();
            let chars: Vec<char> = content.chars().collect();
            let start = char_idx.saturating_sub(radius);
            let end = (char_idx + radius).min(chars.len());
            chars[start..end].iter().collect()
        }
        None => truncate_chars(&content, radius * 2),
    }
}

/// 截断到指定字符数（UTF-8 安全）
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Levenshtein 相似度（0~1）
fn levenshtein_ratio(a: &str, b: &str) -> f32 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let dist = prev[b.len()];
    1.0 - (dist as f32) / (a.len().max(b.len()) as f32)
}

// ─── 单元测试 ───

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::experience::ExperienceLlmExtractor;

    #[test]
    fn test_relation_from_label() {
        assert_eq!(relation_from_label("使用"), Some(Relation::Uses));
        assert_eq!(relation_from_label("属于"), Some(Relation::BelongsTo));
        assert_eq!(relation_from_label("引用"), Some(Relation::References));
        assert_eq!(relation_from_label("replaced_by"), Some(Relation::ReplacedBy));
        assert_eq!(relation_from_label("不存在的"), None);
    }

    #[test]
    fn test_levenshtein_ratio() {
        assert!(levenshtein_ratio("kafka streams", "kafka stream") > 0.9);
        assert!(levenshtein_ratio("redis", "redis") > 0.99);
        assert!(levenshtein_ratio("redis", "kafka") < 0.5);
        assert!(levenshtein_ratio("docker compose", "docker-compose") > 0.85);
    }

    #[test]
    fn test_truncate_chars() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert!(truncate_chars("abcdefgh", 4).ends_with('…'));
    }

    /// fake GraphLlm：固定返回结构化 JSON（测试 LLM 富化路径）
    struct FakeLlm;

    #[async_trait]
    impl GraphLlm for FakeLlm {
        async fn json(&self, _system: &str, _user: &str) -> Option<serde_json::Value> {
            serde_json::from_str(r#"{"problem":"缓存穿透问题","solution":"布隆过滤器"}"#).ok()
        }
        async fn text(&self, _s: &str, _u: &str, _m: u32) -> Option<String> {
            None
        }
    }

    #[test]
    fn test_llm_experience_extractor() {
        let llm: Arc<dyn GraphLlm> = Arc::new(FakeLlm);
        let ex = LlmExperienceExtractor::new(llm);
        let fut = ex.extract_problem_solution("fix: 缓存", "正文内容");
        let (problem, solution) = futures::executor::block_on(fut).unwrap();
        assert!(problem.contains("缓存穿透"), "problem={}", problem);
        assert!(solution.contains("布隆过滤器"), "solution={}", solution);
    }

    /// fake GraphLlm：json 恒 None 且 is_null=false —— 模拟「已配置但调用失败」
    struct FailingLlm;

    #[async_trait]
    impl GraphLlm for FailingLlm {
        async fn json(&self, _s: &str, _u: &str) -> Option<serde_json::Value> {
            None
        }
        async fn text(&self, _s: &str, _u: &str, _m: u32) -> Option<String> {
            None
        }
    }

    #[test]
    fn test_extract_relations_failure_semantics() {
        // D1：extract_relations 区分「未配置（Null → Ok(0) 规则降级）」与
        // 「已配置但调用失败（Err → 调用方重试/告警）」——此前失败静默 Ok(0)。
        let dir = tempfile::Builder::new()
            .prefix("mdgo_graph_ai_fail_")
            .tempdir()
            .unwrap();
        let dir_path = dir.path().to_string_lossy().to_string();
        let engine = Arc::new(super::super::GraphEngine::new());
        engine.store(&dir_path).unwrap();
        std::fs::write(dir.path().join("a.md"), "# 标题\n内容 [redis](https://redis.io)").unwrap();
        {
            let store = engine.store(&dir_path).unwrap();
            let guard = store.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .upsert_node(&super::super::model::GraphNode {
                    id: super::super::storage::node_id_for(NodeType::Doc, "a.md"),
                    node_type: NodeType::Doc,
                    name: "a.md".into(),
                    path: Some("a.md".into()),
                    meta: None,
                    degree: None,
                    created_at: None,
                    content: None,
                })
                .unwrap();
        }
        let service = GraphAiService::new(engine.clone());
        // 1) 未配置（NullGraphLlm）→ Ok(0)，且不计 llm_failures
        let n = futures::executor::block_on(service.extract_relations(&dir_path, "a.md", "内容", &NullGraphLlm))
            .unwrap();
        assert_eq!(n, 0, "未配置应规则降级返回 0");
        {
            let store = engine.store(&dir_path).unwrap();
            let guard = store.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(guard.get_metric("llm_failures").unwrap(), 0, "未配置不计失败");
        }
        // 2) 已配置但调用失败 → Err（worker 据此重试）
        let r = futures::executor::block_on(service.extract_relations(&dir_path, "a.md", "内容", &FailingLlm));
        assert!(r.is_err(), "已配置但 LLM 失败应返回 Err: {:?}", r);
        {
            let store = engine.store(&dir_path).unwrap();
            let guard = store.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(guard.get_metric("llm_failures").unwrap(), 1, "真实失败计 llm_failures");
        }
    }
}
