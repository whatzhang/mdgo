//! LLM 结构化输出校验（P0-3）。
//!
//! # 设计（SOLID）
//!
//! - [`JsonSchemaValidator`]：JSON Schema 本地校验器（单一职责，纯校验无副作用）。
//!   不依赖任何 LLM 客户端类型（依赖倒置），校验失败返回可读错误列表，
//!   供上层构造"修正提示"引导模型重发。
//! - [`build_fix_prompt`]：把校验错误整理成模型可读的修正指令（开闭：错误
//!   来源与格式变化不影响校验器本身）。
//!
//! 使用位置：规划 JSON（`core/agent/planner.rs`）等结构化输出链路；
//! 网关侧 `output_schema` 透传保持关闭（Ollama/llama.cpp 等本地网关对
//! `response_format.json_schema` 支持参差，本地校验 + 重试更兼容，设计取舍见
//! `docs/agent_gap_plan.md` P0-3）。

use jsonschema::Validator;
use serde_json::Value;

/// JSON Schema 校验器：编译一次 schema，多次校验实例。
pub struct JsonSchemaValidator {
    validator: Validator,
}

impl JsonSchemaValidator {
    /// 从 JSON Schema 编译校验器；schema 非法时返回 Err（调用方应视为配置错误）。
    pub fn new(schema: Value) -> Result<Self, String> {
        let validator = Validator::new(&schema)
            .map_err(|e| format!("JSON Schema 编译失败: {}", e))?;
        Ok(Self { validator })
    }

    /// 校验已解析的 JSON 值；成功返回 `Ok(())`，失败返回可读错误列表。
    pub fn validate(&self, value: &Value) -> Result<(), Vec<String>> {
        let errors: Vec<String> = self
            .validator
            .iter_errors(value)
            .map(|e| e.to_string())
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// 解析 JSON 文本并校验；返回解析后的值或错误列表（含解析错误）。
    pub fn validate_json_text(&self, text: &str) -> Result<Value, Vec<String>> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| vec![format!("JSON 解析失败: {}", e)])?;
        self.validate(&value)?;
        Ok(value)
    }
}

/// 把校验错误整理成模型可读的修正提示（追加到重试 prompt）。
///
/// 输入为 [`JsonSchemaValidator::validate`] 返回的错误列表；列表为空时
/// 返回通用提示（适用于"解析失败但无具体 schema 错误"的场景）。
pub fn build_fix_prompt(errors: &[String], fallback: &str) -> String {
    if errors.is_empty() {
        format!("{fallback}")
    } else {
        let joined = errors.join("；");
        format!("{fallback}具体错误：{joined}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan_schema() -> Value {
        json!({
            "type": "object",
            "required": ["goal", "steps", "acceptance"],
            "properties": {
                "goal": { "type": "string", "minLength": 1 },
                "steps": { "type": "array", "minItems": 1, "items": { "type": "string" } },
                "acceptance": { "type": "array", "items": { "type": "string" } },
                "risks": { "type": "array", "items": { "type": "string" } }
            },
            "additionalProperties": true
        })
    }

    #[test]
    fn validator_accepts_valid_plan() {
        let v = JsonSchemaValidator::new(plan_schema()).expect("schema 应可编译");
        let text = r#"{"goal": "重构", "steps": ["a", "b"], "acceptance": ["ok"], "risks": []}"#;
        let value = v.validate_json_text(text).expect("合法计划应通过");
        assert_eq!(value["goal"], "重构");
    }

    #[test]
    fn validator_rejects_missing_required() {
        let v = JsonSchemaValidator::new(plan_schema()).unwrap();
        let errs = v
            .validate_json_text(r#"{"goal": "x"}"#)
            .expect_err("缺 steps/acceptance 应被拒绝");
        assert!(!errs.is_empty());
    }

    #[test]
    fn validator_rejects_wrong_type() {
        let v = JsonSchemaValidator::new(plan_schema()).unwrap();
        let errs = v
            .validate_json_text(r#"{"goal": 42, "steps": ["a"], "acceptance": []}"#)
            .expect_err("goal 非字符串应被拒绝");
        assert!(errs.iter().any(|e| e.contains("goal") || e.contains("string")));
    }

    #[test]
    fn validator_rejects_invalid_json_text() {
        let v = JsonSchemaValidator::new(plan_schema()).unwrap();
        let errs = v
            .validate_json_text("不是 JSON")
            .expect_err("非法 JSON 应返回解析错误");
        assert!(errs[0].contains("解析失败"));
    }

    #[test]
    fn fix_prompt_carries_errors_or_fallback() {
        let with_errors = build_fix_prompt(&["字段 goal 类型错误".into()], "请重新输出合法 JSON。");
        assert!(with_errors.contains("字段 goal 类型错误"));
        let fallback = build_fix_prompt(&[], "请重新输出合法 JSON。");
        assert_eq!(fallback, "请重新输出合法 JSON。");
    }
}
