//! Experience Brain（经验大脑，Phase 4）——统一事件源 + Problem/Solution/Result 图。
//!
//! 目标：让 AI 能回答「我以前遇到过类似问题吗？当时怎么解决的？」。
//!
//! 数据流：
//! ```text
//! 事件源（Git 提交 / AI 操作 / 对话历史）
//!         │  统一采集入口 ExperienceRecorder.record()
//!         ▼
//! graph_experience_events（append-only 事件表）
//!         │  结构化：problem 片段（提取自 commit message / AI 操作结果）
//!         ▼
//! Problem ──SOLVED_BY──► Solution ──IMPLEMENTED_IN──► doc / commit
//!         └──────────────VALIDATED_BY────────────► Experience
//! ```
//!
//! 节点类型：`experience`（Problem/Solution 统一落 experience 类节点，meta.type 区分）；
//! 关系：`SOLVED_BY`（problem→solution）、`IMPLEMENTED_IN`（solution→doc/commit）、
//! `VALIDATED_BY`（solution→experience）。
//!
//! 本模块为**骨架实现**（结构化规则提取 + 事件存储 + 图写入）：
//! - Level 1 规则：commit message / AI 操作 label 按关键词拆出 problem/solution；
//! - LLM 精抽取接口预留（`ExperienceLlmExtractor` trait，注入后替换规则实现）。

use serde::{Deserialize, Serialize};

use super::model::{GraphNode, NodeType, Relation};
use super::storage::GraphStore;

/// 事件来源类型（serde 兼容小写 snake_case：git_commit / ai_operation / chat_message）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    GitCommit,
    AiOperation,
    ChatMessage,
}

impl EventSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventSource::GitCommit => "git_commit",
            EventSource::AiOperation => "ai_operation",
            EventSource::ChatMessage => "chat_message",
        }
    }

    pub fn from_str(s: &str) -> EventSource {
        match s {
            "git_commit" => EventSource::GitCommit,
            "ai_operation" => EventSource::AiOperation,
            _ => EventSource::ChatMessage,
        }
    }
}

/// 一条经验事件（统一事件源记录）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperienceEvent {
    pub id: String,
    pub source: EventSource,
    /// 原始标题（commit message / AI 操作 label / 对话主题）
    pub title: String,
    /// 原始正文（commit 详情 / AI 结果摘要 / 对话片段）
    pub body: String,
    /// 关联文件相对路径（可选）
    pub file_path: Option<String>,
    /// 时间戳（毫秒）
    pub created_at: i64,
}

/// 经验图查询结果（「类似问题」检索）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperienceHit {
    pub event: ExperienceEvent,
    pub problem: String,
    pub solution: String,
    /// 关联文档路径
    pub doc_path: Option<String>,
    /// 相似度得分（0~1，当前为规则关键词命中度）
    pub score: f32,
}

/// LLM 精抽取器抽象（Phase 4.2 已接线：`ai::LlmExperienceExtractor` 为
/// `GraphLlm` 适配实现，命令层经 `graph_experience_record` 注入；
/// 未注入时 record 保持规则降级）。
pub trait ExperienceLlmExtractor: Send + Sync {
    /// 从 (title, body) 拆出 (problem, solution)
    fn extract_problem_solution(
        &self,
        title: &str,
        body: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(String, String), String>> + Send + '_>,
    >;
}

/// Experience Brain：事件采集 + P/S/R 图写入 + 查询。
pub struct ExperienceBrain<'a> {
    store: &'a GraphStore,
    /// 可选 LLM 精抽取（None = 规则实现；命令层按配置注入）
    #[allow(dead_code)]
    llm: Option<&'a dyn ExperienceLlmExtractor>,
}

impl<'a> ExperienceBrain<'a> {
    pub fn new(store: &'a GraphStore, llm: Option<&'a dyn ExperienceLlmExtractor>) -> Self {
        Self { store, llm }
    }

    /// 记录一条事件并写入图（problem→solution→doc 骨架）。
    /// 事件表为 append-only（id 主键幂等）。
    ///
    /// 同步规则实现：拆解走 `rule_extract`；LLM 富化路径由调用方
    /// 先抽取再经 [`Self::record_extracted`] 写图（见 `GraphEngine::experience_record_ai`）。
    /// 多步写入（事件+节点+边）包在单个事务中（R5 修复：原子性）。
    pub fn record(&self, event: &ExperienceEvent) -> Result<(), String> {
        let (problem, solution) = Self::rule_extract(&event.title, &event.body);
        self.record_extracted(event, &problem, &solution)
    }

    /// 用外部抽取的 (problem, solution) 写图（LLM 富化结果入口；同步、事务内）。
    /// problem/solution 为空串（如纯 chore commit）→ 仅存事件，不建图。
    pub fn record_extracted(
        &self,
        event: &ExperienceEvent,
        problem: &str,
        solution: &str,
    ) -> Result<(), String> {
        // 事务内写入：事件落表 + 建 P/S 节点 + 边（全部成功或全部回滚）
        self.store.with_transaction(|_conn| {
            // 1. 事件落表（骨架：graph_properties 中以 `exp:{id}` 键存储 JSON；
            //    规模化后可迁移独立表，这里保持单表图存储的简洁性）
            let key = format!("exp:{}", event.id);
            let value = serde_json::to_string(event)
                .map_err(|e| format!("序列化经验事件失败: {}", e))?;
            self.store.set_property(&key, &value)?;

            if problem.trim().is_empty() || solution.trim().is_empty() {
                // 无有效 problem/solution（如纯 chore commit）→ 仅存事件，不建图
                return Ok(());
            }

            // 2. 建 P/S 节点 + 边
            let pid = format!("experience:problem:{}", Self::hash(problem));
            let sid = format!("experience:solution:{}", Self::hash(solution));
            let now = event.created_at.max(1);

            self.store.upsert_node(&GraphNode {
                id: pid.clone(),
                node_type: NodeType::Experience,
                name: truncate(problem, 80),
                path: None,
                meta: Some(format!(
                    "{{\"kind\":\"problem\",\"text\":{}}}",
                    serde_json::to_string(problem).unwrap_or_else(|_| "\"\"".into())
                )),
                degree: None,
            created_at: None,
            content: None,
            })?;
            self.store.upsert_node(&GraphNode {
                id: sid.clone(),
                node_type: NodeType::Experience,
                name: truncate(solution, 80),
                path: None,
                meta: Some(format!(
                    "{{\"kind\":\"solution\",\"text\":{}}}",
                    serde_json::to_string(solution).unwrap_or_else(|_| "\"\"".into())
                )),
                degree: None,
            created_at: None,
            content: None,
            })?;

            // problem --SOLVED_BY--> solution
            self.store.upsert_edge(
                &super::model::GraphEdge {
                    source: pid.clone(),
                    target: sid.clone(),
                    relation: Relation::SolvedBy,
                    weight: Some(1.0),
                    confidence: Some(1.0),
                },
                Some(&event.id),
            )?;

            // solution --IMPLEMENTED_IN--> doc（关联文件）
            if let Some(fp) = &event.file_path {
                let doc_id = super::storage::node_id_for(NodeType::Doc, fp);
                self.store.upsert_edge(
                    &super::model::GraphEdge {
                        source: sid.clone(),
                        target: doc_id,
                        relation: Relation::ImplementedIn,
                        weight: Some(1.0),
                        confidence: Some(1.0),
                    },
                    Some(&event.id),
                )?;
            }

            // solution --VALIDATED_BY--> experience 事件节点（时间线回溯）
            let exp_id = format!("experience:event:{}", event.id);
            self.store.upsert_node(&GraphNode {
                id: exp_id.clone(),
                node_type: NodeType::Experience,
                name: truncate(&event.title, 60),
                path: event.file_path.clone(),
                meta: Some(format!(
                    "{{\"kind\":\"event\",\"source\":\"{}\",\"created_at\":{}}}",
                    event.source.as_str(),
                    now
                )),
                degree: None,
            created_at: None,
            content: None,
            })?;
            self.store.upsert_edge(
                &super::model::GraphEdge {
                    source: sid,
                    target: exp_id,
                    relation: Relation::ValidatedBy,
                    weight: Some(1.0),
                    confidence: Some(1.0),
                },
                Some(&event.id),
            )?;

            Ok(())
        })
    }

    /// 「类似问题」检索：按 problem 关键词匹配历史 solution（规则打分）。
    /// 返回按命中度降序的经验命中。
    pub fn search_similar(&self, problem: &str, limit: usize) -> Result<Vec<ExperienceHit>, String> {
        let keywords = Self::keywords(problem);
        if keywords.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.max(1).min(50);

        // 从事件表扫描（骨架：全量扫描 + 内存打分；规模化后可加 FTS 索引）
        let events = self.all_events()?;
        let mut hits: Vec<ExperienceHit> = Vec::new();
        for ev in &events {
            let (p, s) = Self::rule_extract(&ev.title, &ev.body);
            if p.is_empty() || s.is_empty() {
                continue;
            }
            let score = Self::score(&keywords, &format!("{} {}", p, s));
            if score > 0.0 {
                hits.push(ExperienceHit {
                    event: ev.clone(),
                    problem: p,
                    solution: s,
                    doc_path: ev.file_path.clone(),
                    score,
                });
            }
        }
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(limit);
        Ok(hits)
    }

    /// 读取全部经验事件
    pub fn all_events(&self) -> Result<Vec<ExperienceEvent>, String> {
        let keys = self.store.list_properties_with_prefix("exp:")?;
        let mut out = Vec::new();
        for k in keys {
            if let Some(v) = self.store.get_property(&k)? {
                if let Ok(ev) = serde_json::from_str::<ExperienceEvent>(&v) {
                    out.push(ev);
                }
            }
        }
        Ok(out)
    }

    /// 规则拆解：title/body 中按关键词切出 problem/solution。
    fn rule_extract(title: &str, body: &str) -> (String, String) {
        // problem 关键词：标题含"修复/解决/优化/重构/问题/避免"等
        let problem_kws = ["修复", "解决", "优化", "重构", "问题", "避免", "fix", "resolve", "fixes", "optimize", "refactor", "bug", "error", "穿透", "失败"];
        let mut problem = String::new();
        for kw in problem_kws {
            if let Some(idx) = title.find(kw) {
                // 取标题从关键词开始的整句
                problem = title[idx..].trim().to_string();
                break;
            }
        }
        if problem.is_empty() {
            // 退化为整个标题
            problem = title.trim().to_string();
        }
        // solution：body 首行（去除 problem 重复部分）
        let body_first = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        let solution = if body_first.is_empty() {
            title.trim().to_string()
        } else {
            body_first.to_string()
        };
        (problem, solution)
    }

    /// 关键词切分（中英文；按常用分隔符）
    fn keywords(text: &str) -> Vec<String> {
        text.split(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|w| w.trim().to_lowercase())
            .filter(|w| w.len() >= 2)
            .collect()
    }

    /// 关键词命中度：命中关键词数 / 关键词总数（含部分匹配权重）
    fn score(keywords: &[String], text: &str) -> f32 {
        let lower = text.to_lowercase();
        let hit = keywords.iter().filter(|k| lower.contains(k.as_str())).count();
        if keywords.is_empty() {
            0.0
        } else {
            hit as f32 / keywords.len() as f32
        }
    }

    /// 简单字符串哈希（FNV-1a，稳定 id）
    fn hash(s: &str) -> String {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in s.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{:x}", h)
    }
}

/// 截断字符串（按字符）
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rule_extract() {
        let (p, s) = ExperienceBrain::rule_extract("fix: 修复缓存穿透问题", "通过布隆过滤器避免击穿");
        assert!(p.contains("修复缓存穿透问题"), "problem={}", p);
        assert!(s.contains("布隆过滤器"), "solution={}", s);
    }

    #[test]
    fn test_keywords_score() {
        let kws = ExperienceBrain::keywords("缓存穿透");
        assert_eq!(kws, vec!["缓存穿透".to_string()]);
        let s = ExperienceBrain::score(&kws, "缓存穿透问题解决");
        assert_eq!(s, 1.0);
    }

    /// 临时 store（对齐 storage 测试模式）
    fn temp_store(name: &str) -> (GraphStore, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("mdgo_graph_exp_{}_", name))
            .tempdir()
            .unwrap();
        let db = dir.path().join("mdgo.db");
        let store = GraphStore::open_for_dir(dir.path().to_string_lossy().as_ref(), &db).unwrap();
        (store, dir)
    }

    fn sample_event(id: &str) -> ExperienceEvent {
        ExperienceEvent {
            id: id.to_string(),
            source: EventSource::GitCommit,
            title: "fix: 修复缓存穿透".to_string(),
            body: "使用布隆过滤器避免缓存击穿".to_string(),
            file_path: Some("src/cache.rs".to_string()),
            created_at: 1_700_000_000_000,
        }
    }

    #[test]
    fn test_record_extracted_writes_ps_graph() {
        let (store, _dir) = temp_store("ps_graph");
        let brain = ExperienceBrain::new(&store, None::<&dyn ExperienceLlmExtractor>);
        let event = sample_event("evt-1");
        // LLM 富化结果（外部抽取的 P/S）写图
        brain
            .record_extracted(&event, "缓存穿透导致数据库压力", "布隆过滤器拦截无效请求")
            .unwrap();
        // 事件落表（append-only）
        assert!(store.get_property("exp:evt-1").unwrap().is_some());
        // P/S 节点 + 事件节点存在（SOLVED_BY / VALIDATED_BY / IMPLEMENTED_IN 边已写入）
        let pid = "experience:problem:".to_string()
            + &ExperienceBrain::hash("缓存穿透导致数据库压力");
        let sid = "experience:solution:".to_string()
            + &ExperienceBrain::hash("布隆过滤器拦截无效请求");
        assert!(store.get_node(&pid).unwrap().is_some(), "problem 节点缺失");
        assert!(store.get_node(&sid).unwrap().is_some(), "solution 节点缺失");
        assert!(store.get_node("experience:event:evt-1").unwrap().is_some(), "事件节点缺失");
        // 图版本因图变更已 bump
        assert!(store.graph_version().unwrap() > 0);
    }

    #[test]
    fn test_record_extracted_empty_ps_stores_event_only() {
        let (store, _dir) = temp_store("ps_empty");
        let brain = ExperienceBrain::new(&store, None::<&dyn ExperienceLlmExtractor>);
        let event = sample_event("evt-2");
        // 空 P/S（如纯 chore commit）：仅存事件，不建图
        brain.record_extracted(&event, "", "").unwrap();
        assert!(store.get_property("exp:evt-2").unwrap().is_some());
        assert!(store.get_node("experience:event:evt-2").unwrap().is_none());
    }
}
