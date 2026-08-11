//! 内置审批策略(开闭原则:新增工具类型 = 新增实现,不改 Hook/门)。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
