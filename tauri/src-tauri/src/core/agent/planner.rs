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
use serde_json::Value;

use crate::core::validation::JsonSchemaValidator;

/// 计划 JSON 的 JSON Schema（P0-3：结构化输出校验）。
///
/// 与 [`Plan`] 字段对齐；`risks`/`touchpoints`/`non_goals`/`rollback` 可选
/// （缺省容忍），其余字段必填且类型受约束。
pub fn plan_json_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "required": ["goal", "steps", "acceptance"],
        "properties": {
            "goal": { "type": "string", "minLength": 1 },
            "steps": { "type": "array", "minItems": 1, "items": { "type": "string", "minLength": 1 } },
            "acceptance": { "type": "array", "items": { "type": "string" } },
            "risks": { "type": "array", "items": { "type": "string" } },
            "touchpoints": { "type": "array", "items": { "type": "string" } },
            "non_goals": { "type": "array", "items": { "type": "string" } },
            "rollback": { "type": "array", "items": { "type": "string" } }
        },
        "additionalProperties": true
    })
}

/// 校验 LLM 返回的计划 JSON 文本（容忍 ```json 围栏与前后杂质）。
///
/// 返回解析后的 JSON 值，或可读错误列表（用于构造修正提示引导模型重发）。
/// 校验不通过时调用方可用 [`build_fix_prompt`] 生成修正指令再问一次。
pub fn validate_plan_json(text: &str) -> Result<Value, Vec<String>> {
    let trimmed = text.trim();
    let body = strip_code_fence(trimmed);
    let (start, end) = match (body.find('{'), body.rfind('}')) {
        (Some(s), Some(e)) if e > s => (s, e),
        _ => return Err(vec!["未找到 JSON 对象".into()]),
    };
    let validator = JsonSchemaValidator::new(plan_json_schema()).map_err(|e| vec![e])?;
    validator.validate_json_text(&body[start..=end])
}

/// 任务性动词/意图：命中即视为复杂任务候选（规则路由信号之一）。
///
/// P1-9：从「先/再」等高频连词中剥离——「先……再……」是常见叙述结构，
/// 与"需要规划"相关性弱（原表把它们当多意图信号导致误报）。本表只保留
/// 明确的「任务/交付型」动词。
const PLAN_VERBS: &[&str] = &[
    "重构", "设计", "分析", "总结", "迁移", "调研", "实现", "搭建",
    "规划", "计划", "优化", "评估", "对比", "架构", "方案", "步骤",
    "分步", "详细说明", "研究", "改进",
];

/// 多意图连接词：中等长度问题含此信号也视为复杂任务。
///
/// P1-9：移除 "先"/"再"（误报源）——"先看 A 再看 B" 这类一次性指令不需要
/// 规划确认；保留真正表示「多任务并行/串行编排」的连接词。
const MULTI_INTENT_MARKERS: &[&str] = &["并且", "同时", "以及", "然后", "还要"];

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
///
/// P1-9 反例守卫（显式抑制误报）：
/// - 纯提问型（疑问句结尾「?/？/吗/呢」）且长度中等 → 不规划（用户要答案，不是要计划）
/// - 单文件/单主题的轻量操作（"读取/查看/翻译/解释 X"）→ 不规划
pub fn should_plan(query: &str) -> bool {
    let q = query.trim();
    let chars = q.chars().count();
    if chars >= LONG_QUERY_CHARS {
        return true;
    }
    // P1-9：疑问句抑制——"这个文件是干什么的？" 不是规划任务
    let is_question = q.ends_with('?')
        || q.ends_with('？')
        || q.ends_with("吗")
        || q.ends_with("呢")
        || q.ends_with("什么")
        || q.ends_with("哪些");
    if chars <= SHORT_QUERY_CHARS {
        return !is_question && PLAN_VERBS.iter().any(|v| q.contains(v));
    }
    // P1-9：轻量单动作动词（查看类）不触发规划——"解释/翻译/朗读/查看 X" 是原子操作
    const LIGHT_ACTIONS: &[&str] = &["查看", "解释", "翻译", "朗读", "读取", "打开", "转换"];
    if LIGHT_ACTIONS.iter().any(|a| q.contains(a)) && !q.contains("并且") && !q.contains("同时") {
        return false;
    }
    if PLAN_VERBS.iter().any(|v| q.contains(v)) {
        return !is_question;
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

/// 结构化任务计划（P1-10：full plan 结构，对齐 Reasonix light/full plan）。
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
    /// 涉及的文件/模块/知识域（touchpoints）
    #[serde(default)]
    pub touchpoints: Vec<String>,
    /// 明确不做的事（非目标）
    #[serde(default)]
    pub non_goals: Vec<String>,
    /// 失败时的回滚步骤
    #[serde(default)]
    pub rollback: Vec<String>,
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
        if !self.touchpoints.is_empty() {
            out.push_str("涉及范围：\n");
            for t in &self.touchpoints {
                out.push_str(&format!("  - {}\n", t));
            }
        }
        if !self.risks.is_empty() {
            out.push_str("风险注意：\n");
            for risk in &self.risks {
                out.push_str(&format!("  - {}\n", risk));
            }
        }
        if !self.non_goals.is_empty() {
            out.push_str("非目标（明确不做）：\n");
            for ng in &self.non_goals {
                out.push_str(&format!("  - {}\n", ng));
            }
        }
        if !self.rollback.is_empty() {
            out.push_str("失败回滚：\n");
            for rb in &self.rollback {
                out.push_str(&format!("  - {}\n", rb));
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
    plan.touchpoints.retain(|s| !s.trim().is_empty());
    plan.non_goals.retain(|s| !s.trim().is_empty());
    plan.rollback.retain(|s| !s.trim().is_empty());
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

    /// P1-9 回归：抑制「先/再」叙述结构与疑问句的误报
    #[test]
    fn should_plan_suppresses_common_false_positives() {
        // "先……再……" 是普通叙述，不是多任务规划信号（原表含"先/再"会误报）
        assert!(!should_plan("先读取文件 A，再读取文件 B"));
        // 疑问句（即使含任务动词）不规划——用户要的是答案不是计划
        assert!(!should_plan("这个项目应该怎么重构？"));
        assert!(!should_plan("帮我分析一下这个项目的性能瓶颈在哪里？"));
        // 中等长度纯查看/解释类操作不规划
        assert!(!should_plan("请解释一下这份文档的目录结构，说明各章节的用途"));
        // 真正的中等长度多任务仍规划
        assert!(should_plan("请设计一个完整的迁移方案，同时评估数据库选型与索引策略"));
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
            touchpoints: vec!["core/mod.rs".into()],
            non_goals: vec!["不引入新依赖".into()],
            rollback: vec!["git revert".into()],
        };
        let text = plan.to_preamble_text();
        assert!(text.contains("【已确认的任务计划"));
        assert!(text.contains("1. 步骤1"));
        assert!(text.contains("验收标准"));
        assert!(text.contains("涉及范围"));
        assert!(text.contains("非目标"));
        assert!(text.contains("失败回滚"));
    }

    #[test]
    fn plan_full_structure_parses_and_cleans() {
        let raw = r#"{"goal": "重构", "steps": ["a"], "acceptance": ["ok"], "risks": ["r"], "touchpoints": ["t1", ""], "non_goals": ["ng"], "rollback": ["rb"]}"#;
        let plan = parse_plan(raw).expect("full plan 应解析成功");
        assert_eq!(plan.touchpoints, vec!["t1".to_string()], "空 touchpoints 应被清洗");
        assert_eq!(plan.non_goals, vec!["ng".to_string()]);
        assert_eq!(plan.rollback, vec!["rb".to_string()]);
        // 旧格式（无新字段）仍可解析（serde default）
        let old = parse_plan(r#"{"goal": "x", "steps": ["s"], "acceptance": []}"#).expect("旧格式应兼容");
        assert!(old.touchpoints.is_empty());
        assert!(old.rollback.is_empty());
    }

    #[test]
    fn validate_plan_json_accepts_valid_and_rejects_invalid() {
        // 合法计划（含围栏容忍）
        assert!(
            validate_plan_json(
                r#"{"goal": "重构", "steps": ["梳理现状", "拆分接口"], "acceptance": ["编译通过"]}"#
            )
            .is_ok()
        );
        let fenced = "```json\n{\"goal\": \"x\", \"steps\": [\"a\"], \"acceptance\": []}\n```";
        assert!(validate_plan_json(fenced).is_ok());
        // 缺必填 / 空 goal / 空 steps / 非法 JSON：均拒绝且错误可读
        assert!(validate_plan_json(r#"{"goal": "x"}"#).is_err());
        assert!(validate_plan_json(r#"{"goal": "", "steps": ["a"], "acceptance": []}"#).is_err());
        assert!(validate_plan_json(r#"{"goal": "x", "steps": [], "acceptance": []}"#).is_err());
        assert!(validate_plan_json("不是 JSON").is_err());
        // 类型错误：goal 非字符串
        let errs = validate_plan_json(r#"{"goal": 42, "steps": ["a"], "acceptance": []}"#)
            .expect_err("goal 类型错误应拒绝");
        assert!(!errs.is_empty());
    }
}