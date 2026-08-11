//! 内置审批策略(开闭原则:新增工具类型 = 新增实现,不改 Hook/门)。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::{ApprovalPolicy, ApprovalRequest};

/// 策略:edit / delete 等破坏性写操作需要审批。
///
/// `enabled` 开关便于自动化测试与降级(如无人值守模式)。
pub struct DestructiveWritePolicy {
    enabled: Arc<AtomicBool>,
}

impl DestructiveWritePolicy {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(enabled)),
        }
    }

    /// 动态开关（保留给未来的自动化测试/降级场景，当前无调用方）
    #[allow(dead_code)]
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }
}

impl ApprovalPolicy for DestructiveWritePolicy {
    fn evaluate(&self, tool: &str, args: &Value) -> Option<ApprovalRequest> {
        if !self.enabled.load(Ordering::Relaxed) {
            return None;
        }
        let rel_path = args.get("rel_path").and_then(|v| v.as_str()).unwrap_or("?");
        match tool {
            "edit" => {
                let old_len = args
                    .get("old_string")
                    .and_then(|v| v.as_str())
                    .map(|s| s.chars().count())
                    .unwrap_or(0);
                let new_len = args
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .map(|s| s.chars().count())
                    .unwrap_or(0);
                Some(ApprovalRequest {
                    tool: tool.to_string(),
                    args: args.clone(),
                    summary: format!(
                        "编辑文件 {rel_path}(替换 {old_len} 字符 → {new_len} 字符)"
                    ),
                    detail: "内容替换不可撤销,请确认这是预期修改".to_string(),
                })
            }
            "delete" => Some(ApprovalRequest {
                tool: tool.to_string(),
                args: args.clone(),
                summary: format!("删除文件 {rel_path}(不可恢复)"),
                detail: "删除操作不可恢复,已删除内容无法通过本应用找回".to_string(),
            }),
            _ => None,
        }
    }
}

// ─────────────────────────── 配置驱动策略（P2-19） ───────────────────────────

/// 一条审批规则（YAML 配置）。
///
/// ```yaml
/// - tool: edit
///   action: deny      # allow 直接放行 | ask 需确认 | deny 直接禁止
/// - tool: "*"
///   action: ask
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct ApprovalRule {
    /// 工具名；`*` 匹配全部工具
    pub tool: String,
    /// `allow` 直接放行（短路默认策略）| `ask` 走用户确认 | `deny` 直接禁止
    pub action: String,
}

/// 默认审批策略配置文件：`%APPDATA%/com.mdgo/approval.yaml`。
pub fn default_rules_path() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    let base = std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    #[cfg(target_os = "macos")]
    let base = std::path::PathBuf::from(
        std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
    )
    .join("Library")
    .join("Application Support");
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let base = std::path::PathBuf::from(
        std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
    )
    .join(".local")
    .join("share");
    base.join("com.mdgo").join("approval.yaml")
}

/// 从 YAML 加载审批规则（文件不存在 → 空集；解析失败 → Err）。
pub fn load_approval_rules(path: &std::path::Path) -> Result<Vec<ApprovalRule>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("读取审批规则配置失败: {}", e))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let rules: Vec<ApprovalRule> = serde_yaml::from_str(&raw)
        .map_err(|e| format!("审批规则配置 YAML 解析失败: {}", e))?;
    Ok(rules
        .into_iter()
        .filter(|r| !r.tool.trim().is_empty())
        .collect())
}

/// 配置驱动审批策略：按规则表给出 allow / ask / deny 判定。
///
/// - `allow` 规则：`allow()` 命中并短路其余策略（覆盖默认 DestructiveWritePolicy）
/// - `deny` 规则：`deny()` 返回原因，gate 直接拒绝（不弹窗）
/// - `ask` 规则：`evaluate()` 返回通用确认请求
/// 规则按表顺序首个匹配者生效。
#[derive(Debug, Clone)]
pub struct ConfigApprovalPolicy {
    rules: Vec<ApprovalRule>,
}

impl ConfigApprovalPolicy {
    pub fn new(rules: Vec<ApprovalRule>) -> Self {
        Self { rules }
    }

    fn first_match<'a>(&'a self, tool: &str) -> Option<&'a ApprovalRule> {
        self.rules
            .iter()
            .find(|r| r.tool == "*" || r.tool == tool)
    }
}

impl ApprovalPolicy for ConfigApprovalPolicy {
    fn evaluate(&self, tool: &str, args: &Value) -> Option<ApprovalRequest> {
        match self.first_match(tool) {
            Some(rule) if rule.action == "ask" => Some(ApprovalRequest {
                tool: tool.to_string(),
                args: args.clone(),
                summary: format!("工具「{tool}」需要确认后执行"),
                detail: "该操作由审批规则配置为需用户确认".to_string(),
            }),
            _ => None,
        }
    }

    fn allow(&self, tool: &str, _args: &Value) -> bool {
        matches!(
            self.first_match(tool),
            Some(rule) if rule.action == "allow"
        )
    }

    fn deny(&self, tool: &str, _args: &Value) -> Option<String> {
        match self.first_match(tool) {
            Some(rule) if rule.action == "deny" => {
                Some(format!("工具「{tool}」已被审批策略禁止（action=deny）"))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::approval::ApprovalPolicy;
    use serde_json::json;

    fn rules_yaml() -> &'static str {
        r#"
- tool: edit
  action: allow
- tool: delete
  action: deny
- tool: pomodoro
  action: ask
"#
    }

    #[test]
    fn load_rules_parses_yaml() {
        use std::io::Write;
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("approval_test_{}.yaml", uuid::Uuid::new_v4()));
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(rules_yaml().as_bytes()).unwrap();
        let rules = load_approval_rules(&tmp).expect("YAML 应可解析");
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[1].action, "deny");
        assert!(load_approval_rules(&std::path::Path::new("C:/nonexistent_approval.yaml")).unwrap().is_empty());
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn config_policy_routes_by_action() {
        let rules = load_approval_rules_for_test();
        let policy = ConfigApprovalPolicy::new(rules);
        let args = json!({"rel_path": "a.md"});
        // allow：短路放行
        assert!(policy.allow("edit", &args));
        assert!(policy.deny("edit", &args).is_none());
        assert!(policy.evaluate("edit", &args).is_none());
        // deny：直接拒绝（不弹窗）
        assert!(policy.deny("delete", &args).is_some());
        // ask：走用户确认
        assert!(policy.evaluate("pomodoro", &json!({})).is_some());
        // 未配置工具：全部 None（交给默认策略）
        assert!(!policy.allow("read", &args));
        assert!(policy.deny("read", &args).is_none());
        assert!(policy.evaluate("read", &args).is_none());
    }

    fn load_approval_rules_for_test() -> Vec<ApprovalRule> {
        use std::io::Write;
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("approval_test2_{}.yaml", uuid::Uuid::new_v4()));
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(rules_yaml().as_bytes()).unwrap();
        let rules = load_approval_rules(&tmp).unwrap();
        std::fs::remove_file(&tmp).ok();
        rules
    }
}
