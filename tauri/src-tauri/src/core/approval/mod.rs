//! 工具调用审批门:破坏性/不可逆操作在执行前须经用户确认。
//!
//! # 设计(SOLID)
//!
//! - [`ApprovalPolicy`]:判定「是否需要审批」(单一职责,开闭——新规则 = 新实现)
//! - [`ApprovalTransport`]:向用户请求决定的通道抽象(依赖倒置,实现可替换)
//! - [`ApprovalGate`]:组合策略 + 会话内已决缓存(单一职责:编排)
//! - [`hook::ApprovalGateHook`]:rig 拦截点,只负责决定 Run 还是 Skip
//!
//! 新增需要审批的工具类型 = 实现 [`ApprovalPolicy`] 并注册,无需改动门/Hook 本身。
//!
//! # 拒绝原因分类
//!
//! [`DenialCategory`] 让上层(Agent Hook)能按「用户拒绝 / 通道不可用 / 超时」生成
//! 不同的反馈语义,避免模型把审批失败误读为"等待用户文字输入确认"。

pub mod hook;
pub mod policy;
pub mod transport;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

/// 一次需要审批的工具调用
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool: String,
    /// 模型原始参数(JSON 对象)
    pub args: Value,
    /// 给用户看的人类可读摘要(不暴露 old_string 全文等大块内容)
    pub summary: String,
    /// 建议展示给用户的详细说明(如替换规模),可为空
    pub detail: String,
}

/// 拒绝原因类别(供 Hook 生成差异化的模型反馈)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialCategory {
    /// 用户主动拒绝
    UserRejected,
    /// 审批通道不可用(前端未监听 / 事件发送失败 / 响应通道异常)
    ChannelUnavailable,
    /// 等待用户确认超时,默认拒绝
    Timeout,
    /// 策略级拒绝(P2-19:配置规则直接禁止,不弹窗)
    PolicyDenied,
}

/// 一次审批的拒绝详情
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalDenial {
    pub category: DenialCategory,
    /// 人类可读原因(用户输入或通道错误信息)
    pub reason: String,
}

/// 用户(或通道)给出的审批结果
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalOutcome {
    Approved,
    Denied(ApprovalDenial),
}

impl ApprovalOutcome {
    pub fn is_approved(&self) -> bool {
        matches!(self, ApprovalOutcome::Approved)
    }
}

/// 审批策略:判定某次调用是否需要人类审批(单一职责)。
///
/// 返回 `None` = 放行;`Some(request)` = 需要审批。
pub trait ApprovalPolicy: Send + Sync {
    /// 判定是否需要审批(返回请求则走用户确认通道;None = 放行)
    fn evaluate(&self, tool: &str, args: &Value) -> Option<ApprovalRequest>;

    /// 策略级放行(P2-19):命中直接放行并短路其余策略(如配置 allow 规则覆盖默认 ask)。
    fn allow(&self, _tool: &str, _args: &Value) -> bool {
        false
    }

    /// 策略级拒绝(P2-19):返回拒绝原因则直接禁止该工具调用(不弹窗)。
    /// 默认不拒绝;配置策略(`ConfigApprovalPolicy`)按规则覆盖。
    fn deny(&self, _tool: &str, _args: &Value) -> Option<String> {
        None
    }
}

/// 审批通道抽象:向用户展示请求并等待决定(依赖倒置)。
///
/// 生产实现走 Tauri IPC(`emit("approval:request")` + 前端 `invoke("approval_respond")`);
/// 测试实现为 mock,无需任何真实通道。超时语义由通道保证:超时视为拒绝(fail-closed)。
#[async_trait]
pub trait ApprovalTransport: Send + Sync {
    async fn request_approval(
        &self,
        req: &ApprovalRequest,
        timeout: Duration,
    ) -> ApprovalOutcome;
}

/// 审批门:组合多个策略 + 同一 run 内已决缓存(避免多轮弹窗)。
pub struct ApprovalGate {
    policies: Vec<Box<dyn ApprovalPolicy>>,
    transport: Box<dyn ApprovalTransport>,
    /// 默认超时
    timeout: Duration,
    /// 缓存 key = (run_id, tool, canonical_args)
    cache: Mutex<HashMap<(String, String, String), ApprovalOutcome>>,
}

impl std::fmt::Debug for ApprovalGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalGate")
            .field("policy_count", &self.policies.len())
            .field("timeout", &self.timeout)
            .field(
                "cache_size",
                &self.cache.lock().map(|c| c.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl ApprovalGate {
    pub fn new(
        policies: Vec<Box<dyn ApprovalPolicy>>,
        transport: Box<dyn ApprovalTransport>,
        timeout: Duration,
    ) -> Self {
        Self {
            policies,
            transport,
            timeout,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// 检查一次工具调用。`Ok(())` = 放行;`Err(denial)` = 拒绝,携带原因类别。
    pub async fn check(&self, run_id: &str, tool: &str, args: &Value) -> Result<(), ApprovalDenial> {
        // 0a. 策略级放行(配置 allow 规则直接放行并短路其余策略)
        if self.policies.iter().any(|p| p.allow(tool, args)) {
            return Ok(());
        }
        // 0b. 策略级拒绝(配置规则直接禁止,不弹窗、不缓存——每次调用都被拒)
        if let Some(reason) = self.policies.iter().find_map(|p| p.deny(tool, args)) {
            return Err(ApprovalDenial {
                category: DenialCategory::PolicyDenied,
                reason,
            });
        }

        // 1. 策略判定:首个命中者决定是否需要审批(无命中 → 放行)
        let Some(req) = self.policies.iter().find_map(|p| p.evaluate(tool, args)) else {
            return Ok(());
        };

        // 2. 会话内已决缓存:同一 run 相同参数只询问一次
        let key = (run_id.to_string(), tool.to_string(), canonical(args));
        {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(outcome) = cache.get(&key) {
                return match outcome {
                    ApprovalOutcome::Approved => Ok(()),
                    ApprovalOutcome::Denied(denial) => Err(denial.clone()),
                };
            }
        }

        // 3. 走通道请求用户决定(超时/通道异常默认拒绝,安全优先)
        let outcome = self.transport.request_approval(&req, self.timeout).await;

        // 4. 记录结果到缓存(有界:超过上限清空,避免长期运行缓存无限增长)
        {
            const MAX_CACHE_ENTRIES: usize = 256;
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache.len() >= MAX_CACHE_ENTRIES {
                cache.clear();
            }
            cache.insert(key, outcome.clone());
        }
        match outcome {
            ApprovalOutcome::Approved => Ok(()),
            ApprovalOutcome::Denied(denial) => Err(denial),
        }
    }
}

/// 参数规范化(与 `tools::canonical_args` 同语义:键排序扁平化),用于缓存 key
fn canonical(args: &Value) -> String {
    fn canon(v: &Value) -> String {
        match v {
            Value::Object(map) => {
                let mut entries: Vec<(String, String)> = map
                    .iter()
                    .map(|(k, val)| (k.clone(), canon(val)))
                    .collect();
                entries.sort();
                entries
                    .iter()
                    .map(|(k, val)| format!("{k}={val}"))
                    .collect::<Vec<_>>()
                    .join("&")
            }
            _ => v.to_string(),
        }
    }
    canon(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct AlwaysAskPolicy;

    impl ApprovalPolicy for AlwaysAskPolicy {
        fn evaluate(&self, tool: &str, _args: &Value) -> Option<ApprovalRequest> {
            Some(ApprovalRequest {
                tool: tool.to_string(),
                args: Value::Null,
                summary: format!("需要确认 {tool}"),
                detail: String::new(),
            })
        }
    }

    struct MockTransport {
        /// 共享计数:clone 到 gate 后仍指向同一原子,断言才可信
        calls: Arc<AtomicUsize>,
        outcome: ApprovalOutcome,
    }

    impl Clone for MockTransport {
        fn clone(&self) -> Self {
            Self {
                calls: self.calls.clone(),
                outcome: self.outcome.clone(),
            }
        }
    }

    #[async_trait]
    impl ApprovalTransport for MockTransport {
        async fn request_approval(
            &self,
            _req: &ApprovalRequest,
            _timeout: Duration,
        ) -> ApprovalOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.outcome.clone()
        }
    }

    fn args(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    fn user_denial(reason: &str) -> ApprovalOutcome {
        ApprovalOutcome::Denied(ApprovalDenial {
            category: DenialCategory::UserRejected,
            reason: reason.to_string(),
        })
    }

    #[tokio::test]
    async fn gate_denies_when_transport_denies() {
        let t = MockTransport {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: user_denial("用户拒绝"),
        };
        let gate = ApprovalGate::new(
            vec![Box::new(AlwaysAskPolicy)],
            Box::new(t),
            Duration::from_secs(5),
        );
        let denial = gate
            .check("run1", "delete", &args(r#"{"rel_path":"a.md"}"#))
            .await
            .unwrap_err();
        assert_eq!(denial.category, DenialCategory::UserRejected);
        assert!(denial.reason.contains("用户拒绝"));
    }

    #[tokio::test]
    async fn gate_allows_when_transport_approves() {
        let t = MockTransport {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: ApprovalOutcome::Approved,
        };
        let gate = ApprovalGate::new(
            vec![Box::new(AlwaysAskPolicy)],
            Box::new(t),
            Duration::from_secs(5),
        );
        assert!(gate
            .check("run1", "edit", &args(r#"{"rel_path":"a.md"}"#))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn gate_caches_same_args_per_run() {
        let t = MockTransport {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: ApprovalOutcome::Approved,
        };
        let gate = ApprovalGate::new(
            vec![Box::new(AlwaysAskPolicy)],
            Box::new(t.clone()),
            Duration::from_secs(5),
        );
        let a = args(r#"{"rel_path":"a.md","old_string":"x","new_string":"y"}"#);
        assert!(gate.check("run1", "edit", &a).await.is_ok());
        // 同 run 同参数(键序不同)第二次不再询问
        let b = args(r#"{"new_string":"y","old_string":"x","rel_path":"a.md"}"#);
        assert!(gate.check("run1", "edit", &b).await.is_ok());
        assert_eq!(t.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn gate_skips_untracked_tools_when_no_policy_hits() {
        let gate = ApprovalGate::new(
            Vec::new(),
            Box::new(MockTransport {
                calls: Arc::new(AtomicUsize::new(0)),
                outcome: ApprovalOutcome::Approved,
            }),
            Duration::from_secs(5),
        );
        assert!(gate
            .check("run1", "read", &args(r#"{"rel_path":"a.md"}"#))
            .await
            .is_ok());
    }
}
