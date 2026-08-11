//! Agent 能力评测框架（P2-17）。
//!
//! # 设计（SOLID）
//!
//! - [`EvalScenario`]：场景定义（query + 期望工具序列 + 输出正则），可 YAML 加载
//!   （`load_scenarios_yaml`）或内置（[`builtin_scenarios`]）。
//! - [`evaluate_scenario`]：纯断言逻辑（工具序列匹配 + 输出正则），零 LLM 依赖，
//!   完全可单测（单一职责）。
//! - [`evaluate_all`]：依赖倒置——执行由调用方闭包注入（`FnMut(&EvalScenario) ->
//!   (tool_calls, output)`），本模块只做断言汇总与 [`EvalReport`] 生成；
//!   真实 LLM 执行器由未来 CLI/集成层提供（当前以单测覆盖断言与报告）。
//!
//! 对齐商用 Agent 的 eval 实践：场景集回归（工具行为 + 输出质量），
//! 报告落库可由上层（skill_metrics 同款机制）扩展。

use serde::{Deserialize, Serialize};

/// 单个评测场景。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalScenario {
    /// 场景名（报告/日志标识）
    pub name: String,
    /// 注入 Agent 的用户问题
    pub query: String,
    /// 期望调用的工具（按序出现即可，不要求连续）
    #[serde(default)]
    pub expected_tools: Vec<String>,
    /// 期望输出匹配的正则（可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_outcome_regex: Option<String>,
}

/// 单场景评测结果。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EvalOutcome {
    pub name: String,
    pub passed: bool,
    /// 失败原因 / 通过时的摘要（工具序列、输出长度）
    pub detail: String,
}

/// 评测汇总报告。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EvalReport {
    pub total: usize,
    pub passed: usize,
    pub outcomes: Vec<EvalOutcome>,
}

impl EvalReport {
    pub fn passed_rate(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.passed as f32 / self.total as f32
        }
    }
}

/// 断言实际工具调用序列满足期望（期望中的每个工具都按序出现；实际可含额外调用）。
pub fn assert_tool_sequence(actual: &[String], expected: &[String]) -> Result<(), String> {
    let mut idx = 0usize;
    for want in expected {
        let found = actual[idx..]
            .iter()
            .position(|got| got == want)
            .ok_or_else(|| {
                format!(
                    "期望工具「{want}」未在调用序列中出现（已匹配 {} 个；实际调用：{:?}）",
                    idx, actual
                )
            })?;
        idx += found + 1;
    }
    Ok(())
}

/// 断言输出匹配正则（可选；未配置视为通过）。
pub fn assert_outcome(text: &str, regex: Option<&str>) -> Result<(), String> {
    let Some(pattern) = regex else {
        return Ok(());
    };
    let re = regex::Regex::new(pattern).map_err(|e| format!("评测正则非法: {}", e))?;
    if re.is_match(text) {
        Ok(())
    } else {
        Err(format!(
            "输出未匹配正则「{pattern}」。输出前 200 字符：{}",
            text.chars().take(200).collect::<String>()
        ))
    }
}

/// 评估单个场景（纯断言逻辑，可单测）。
pub fn evaluate_scenario(
    scenario: &EvalScenario,
    tool_calls: &[String],
    output: &str,
) -> EvalOutcome {
    let mut detail = String::new();
    let mut ok = true;
    if let Err(e) = assert_tool_sequence(tool_calls, &scenario.expected_tools) {
        ok = false;
        detail.push_str(&format!("[工具] {e}"));
    }
    if let Err(e) = assert_outcome(output, scenario.expected_outcome_regex.as_deref()) {
        if !detail.is_empty() {
            detail.push_str("；");
        }
        detail.push_str(&format!("[输出] {e}"));
        ok = false;
    }
    if ok {
        detail = format!("工具调用 {} 次；输出 {} 字符", tool_calls.len(), output.chars().count());
    }
    EvalOutcome {
        name: scenario.name.clone(),
        passed: ok,
        detail,
    }
}

/// 运行一组场景并汇总报告（执行由调用方闭包注入，依赖倒置）。
pub async fn evaluate_all<F, Fut>(
    scenarios: &[EvalScenario],
    mut run: F,
) -> EvalReport
where
    F: FnMut(&EvalScenario) -> Fut,
    Fut: std::future::Future<Output = (Vec<String>, String)>,
{
    let mut outcomes = Vec::with_capacity(scenarios.len());
    for scenario in scenarios {
        let (tool_calls, output) = run(scenario).await;
        outcomes.push(evaluate_scenario(scenario, &tool_calls, &output));
    }
    let passed = outcomes.iter().filter(|o| o.passed).count();
    EvalReport {
        total: scenarios.len(),
        passed,
        outcomes,
    }
}

/// 从 YAML 文本加载场景集（name/query/expected_tools/expected_outcome_regex）。
pub fn load_scenarios_yaml(text: &str) -> Result<Vec<EvalScenario>, String> {
    let scenarios: Vec<EvalScenario> =
        serde_yaml::from_str(text).map_err(|e| format!("评测场景 YAML 解析失败: {}", e))?;
    if scenarios.is_empty() {
        return Err("评测场景集为空".into());
    }
    Ok(scenarios)
}

/// 内置评测场景集（对齐核心 Agent 能力：检索/工具调用/规划/反思）。
pub fn builtin_scenarios() -> Vec<EvalScenario> {
    vec![
        EvalScenario {
            name: "multi_tool_task".into(),
            query: "先检索知识库中关于异步的资料，再读取第一个命中文档，最后总结。".into(),
            expected_tools: vec!["kb_search".into(), "read".into()],
            expected_outcome_regex: Some(r"(?s)总结|小结|要点".into()),
        },
        EvalScenario {
            name: "plan_then_execute".into(),
            query: "分析项目性能瓶颈并给出优化步骤与验收标准。".into(),
            expected_tools: vec!["grep".into()],
            expected_outcome_regex: Some(r"步骤|优化|分析".into()),
        },
        EvalScenario {
            name: "memory_recall".into(),
            query: "我之前的偏好是什么？".into(),
            expected_tools: vec!["search_memory".into()],
            expected_outcome_regex: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_sequence_matches_in_order() {
        let actual: Vec<String> = ["kb_search", "read", "grep", "edit"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(assert_tool_sequence(&actual, &["kb_search".into(), "read".into()]).is_ok());
        assert!(assert_tool_sequence(&actual, &["read".into(), "edit".into()]).is_ok());
        // 顺序错误：edit 在 read 前出现
        assert!(assert_tool_sequence(&actual, &["edit".into(), "read".into()]).is_err());
        // 缺失工具
        assert!(assert_tool_sequence(&actual, &["deep_research".into()]).is_err());
        // 空期望恒通过
        assert!(assert_tool_sequence(&actual, &[]).is_ok());
    }

    #[test]
    fn outcome_regex_matches_or_passes_when_unset() {
        assert!(assert_outcome("本方案包含三个优化步骤", Some(r"步骤")).is_ok());
        assert!(assert_outcome("无匹配内容", Some(r"步骤")).is_err());
        assert!(assert_outcome("任意内容", None).is_ok());
    }

    #[test]
    fn evaluate_scenario_reports_failure_details() {
        let s = EvalScenario {
            name: "t".into(),
            query: "q".into(),
            expected_tools: vec!["kb_search".into()],
            expected_outcome_regex: Some(r"结论".into()),
        };
        let pass = evaluate_scenario(&s, &["kb_search".into()], "最终结论：X");
        assert!(pass.passed);
        let fail_tool = evaluate_scenario(&s, &["read".into()], "最终结论：X");
        assert!(!fail_tool.passed);
        assert!(fail_tool.detail.contains("kb_search"));
        let fail_out = evaluate_scenario(&s, &["kb_search".into()], "本段没有匹配文本");
        assert!(!fail_out.passed);
        assert!(fail_out.detail.contains("结论"));
    }

    #[test]
    fn yaml_roundtrip_and_builtin() {
        let yaml = r#"
- name: demo
  query: 测试问题
  expected_tools: [read, grep]
  expected_outcome_regex: "OK"
"#;
        let scenarios = load_scenarios_yaml(yaml).expect("YAML 应可解析");
        assert_eq!(scenarios.len(), 1);
        assert_eq!(scenarios[0].expected_tools, vec!["read".to_string(), "grep".to_string()]);
        assert!(load_scenarios_yaml("").is_err(), "空场景集应报错");
        assert!(!builtin_scenarios().is_empty());
    }

    #[tokio::test]
    async fn evaluate_all_aggregates_report() {
        // 用内置场景 + mock 执行闭包验证报告聚合（依赖倒置：执行注入）
        let scenarios = builtin_scenarios();
        let report = evaluate_all(&scenarios[..2], |s| {
            let tools = s.expected_tools.clone();
            async move {
                // 模拟：调用全部期望工具，输出同时含"总结"与"步骤"（覆盖两个场景的正则）
                (tools, "分析完成，包含总结要点、具体步骤与验收标准".to_string())
            }
        })
        .await;
        assert_eq!(report.total, 2);
        assert_eq!(report.passed, 2);
        assert!((report.passed_rate() - 1.0).abs() < f32::EPSILON);
        assert!(report.outcomes.iter().all(|o| o.passed));
        // 失败场景进入报告
        let bad = evaluate_all(&scenarios[..1], |s| {
            let tools = s.expected_tools.clone();
            async move { (tools, "这是最终答复，无额外标记内容".to_string()) }
        })
        .await;
        assert_eq!(bad.passed, 0);
    }
}
