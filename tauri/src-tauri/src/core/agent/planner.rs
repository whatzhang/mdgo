//! 轻量任务规划器：规则路由判定 + 结构化计划解析（单模型版 plan-then-execute）。
//!
//! 对齐 Reasonix 的 plan-then-execute 思想（单模型版）：复杂任务先产出计划
//! （目标/步骤/验收/风险）供用户确认，批准后注入执行阶段；简单/原子请求
//! 直执行，零规划开销。
//!
//! 职责划分（单一职责）：
//! - [`should_plan`]：纯规则路由，零 LLM 开销，判定"要不要规划"
//! - [`Plan`] + [`parse_plan`]：结构化计划模型与宽松 JSON 解析
//! - LLM 调用本体在 `services/llm.rs::generate_plan_json`（返回 JSON 文本），
//!   本模块不依赖任何 LLM 客户端类型（依赖倒置）

use serde::{Deserialize, Serialize};

/// 任务性动词/意图：命中即视为复杂任务候选（规则路由信号之一）。
const PLAN_VERBS: &[&str] = &[
    "重构", "设计", "分析", "总结", "迁移", "调研", "实现", "搭建",
    "规划", "计划", "优化", "评估", "对比", "架构", "方案", "步骤",
    "分步", "详细说明", "研究", "改进",
];

/// 多意图连接词：中等长度问题含此信号也视为复杂任务。
const MULTI_INTENT_MARKERS: &[&str] = &["并且", "同时", "以及", "然后", "还要", "先", "再"];

/// 长问题判定阈值（字符）：超过即直接视为复杂任务。
const LONG_QUERY_CHARS: usize = 120;

/// 短问题判定阈值（字符）：低于且无任务动词时不规划。
const SHORT_QUERY_CHARS: usize = 40;

/// 判定是否为需要规划的复杂任务（纯规则，零 LLM 开销）。
///
/// 触发信号（满足任一即规划）：
/// - 问题长度 ≥ `LONG_QUERY_CHARS`
/// - 含任务性动词/意图（`PLAN_VERBS`）
/// - 中等长度且含多意图连接词（`MULTI_INTENT_MARKERS`）
pub fn should_plan(query: &str) -> bool {
    let q = query.trim();
    let chars = q.chars().count();
    if chars >= LONG_QUERY_CHARS {
        return true;
    }
    let has_verb = PLAN_VERBS.iter().any(|v| q.contains(v));
    if chars <= SHORT_QUERY_CHARS {
        return has_verb;
    }
    if has_verb {
        return true;
    }
    MULTI_INTENT_MARKERS.iter().any(|m| q.contains(m))
}

/// 用户对任务计划的决定（plan:request 确认通道的回传值）。
#[derive(Debug, Clone, PartialEq)]
pub enum PlanDecision {
    /// 批准计划，继续执行
    Approved,
    /// 拒绝计划（带原因）
    Denied(String),
}

/// 结构化任务计划。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    /// 一句话目标
    pub goal: String,
    /// 有序执行步骤
    pub steps: Vec<String>,
    /// 可验证的验收标准
    pub acceptance: Vec<String>,
    /// 风险与注意点
    #[serde(default)]
    pub risks: Vec<String>,
}

impl Plan {
    /// 序列化为注入执行阶段 preamble 的文本（每轮可见，约束最强）。
    pub fn to_preamble_text(&self) -> String {
        let mut out = String::from("【已确认的任务计划，请严格按此执行】\n");
        out.push_str(&format!("目标：{}\n", self.goal));
        out.push_str("步骤：\n");
        for (i, step) in self.steps.iter().enumerate() {
            out.push_str(&format!("  {}. {}\n", i + 1, step));
        }
        if !self.acceptance.is_empty() {
            out.push_str("验收标准：\n");
            for acc in &self.acceptance {
                out.push_str(&format!("  - {}\n", acc));
            }
        }
        if !self.risks.is_empty() {
            out.push_str("风险注意：\n");
            for risk in &self.risks {
                out.push_str(&format!("  - {}\n", risk));
            }
        }
        out
    }
}

/// 宽松解析 LLM 返回的计划 JSON（容忍 ```json 围栏、前后杂质、缺字段）。
///
/// 解析失败返回 `None`，由调用方降级为"不规划"继续原流程（fail-open）。
pub fn parse_plan(raw: &str) -> Option<Plan> {
    let trimmed = raw.trim();
    // 剥离 ```json ... ``` 围栏
    let body = strip_code_fence(trimmed);
    // 从第一个 '{' 到最后一个 '}' 截取 JSON 主体
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    if end <= start {
        return None;
    }
    let json_text = &body[start..=end];
    let mut plan: Plan = serde_json::from_str(json_text).ok()?;
    // 兜底清洗：goal 为空则视为无效；steps 至少 1 步
    if plan.goal.trim().is_empty() || plan.steps.is_empty() {
        return None;
    }
    plan.steps.retain(|s| !s.trim().is_empty());
    plan.acceptance.retain(|s| !s.trim().is_empty());
    plan.risks.retain(|s| !s.trim().is_empty());
    if plan.steps.is_empty() {
        return None;
    }
    Some(plan)
}

/// 剥离 Markdown 代码围栏（```json ... ``` 或 ``` ... ```）。
fn strip_code_fence(s: &str) -> &str {
    if s.starts_with("```") {
        let body = &s[3..];
        if let Some(idx) = body.find("```") {
            return body[..idx].trim();
        }
        return body.trim();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_plan_routes_simple_and_complex() {
        // 简单/原子请求不规划
        assert!(!should_plan("什么是 Rust 的所有权？"));
        assert!(!should_plan("把 README 第一段改成两句话"));
        // 任务动词触发
        assert!(should_plan("帮我重构这个模块的代码结构"));
        assert!(should_plan("分析一下这个项目的性能瓶颈"));
        // 长问题触发
        let long = "请详细说明如何把当前的 Markdown 文档管理系统迁移到新的架构，包括数据库选型、索引策略、迁移步骤和回滚方案，同时要考虑多端同步和离线场景";
        assert!(should_plan(long));
        // 多意图连接词（中等长度）
        assert!(should_plan("重构登录模块并且同时优化数据库索引"));
    }

    #[test]
    fn parse_plan_accepts_json_and_fenced_json() {
        let raw = r#"```json
{"goal": "重构模块A", "steps": ["梳理现状", "拆分接口", "迁移调用"], "acceptance": ["编译通过", "测试全绿"], "risks": ["影响现有调用"]}
```"#;
        let plan = parse_plan(raw).expect("应解析成功");
        assert_eq!(plan.goal, "重构模块A");
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.acceptance.len(), 2);
        assert_eq!(plan.risks.len(), 1);
    }

    #[test]
    fn parse_plan_rejects_invalid() {
        assert!(parse_plan("不是 JSON").is_none());
        assert!(parse_plan(r#"{"goal": "", "steps": []}"#).is_none());
        assert!(parse_plan(r#"{"goal": "x"}"#).is_none(), "缺 steps 应无效");
    }

    #[test]
    fn to_preamble_text_is_structured() {
        let plan = Plan {
            goal: "重构".into(),
            steps: vec!["步骤1".into(), "步骤2".into()],
            acceptance: vec!["通过".into()],
            risks: vec![],
        };
        let text = plan.to_preamble_text();
        assert!(text.contains("【已确认的任务计划"));
        assert!(text.contains("1. 步骤1"));
        assert!(text.contains("验收标准"));
    }
}