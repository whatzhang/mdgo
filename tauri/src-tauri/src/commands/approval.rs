//! 审批结果回传命令:前端确认框点击后经 Tauri IPC 回传用户决定。
//!
//! 与 [`crate::core::approval::transport::IpcApprovalTransport`] 共享
//! AppState 中的挂起表(单一数据源):transport 侧注册并 emit `approval:request`
//! 事件,本命令消费前端的 `invoke("approval_respond", ...)` 回传并完成 oneshot。
//!
//! 另提供权限模式查询/切换命令(`approval_get_mode` / `approval_set_mode`),
//! 对齐 DeepSeek Harness 的 permission preset 语义:
//! - `ask`(默认):破坏性操作弹窗询问用户
//! - `read-only`:写类工具直接拒绝(不弹窗)
//! - `allow-all`:全部工具放行(无人值守/自动化)

use tauri::State;

use crate::core::approval::{ApprovalDenial, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, DenialCategory};
use crate::AppState;

/// 前端回传审批结果。
///
/// - `request_id`:transport 随 `approval:request` 事件下发的请求 ID
/// - `approved`:`true` 允许执行;`false` 拒绝(归为 [`DenialCategory::UserRejected`])
/// - `reason`:拒绝时的人类可读原因(可空)
#[tauri::command]
pub async fn approval_respond(
    state: State<'_, AppState>,
    request_id: String,
    approved: bool,
    reason: Option<String>,
) -> Result<(), String> {
    let outcome = if approved {
        ApprovalOutcome::Approved
    } else {
        ApprovalOutcome::Denied(ApprovalDenial {
            category: DenialCategory::UserRejected,
            reason: reason.unwrap_or_else(|| "用户拒绝了此操作".to_string()),
        })
    };
    let sender = state
        .approval_pending
        .lock()
        .map_err(|e| format!("审批挂起表锁异常: {}", e))?
        .remove(&request_id);
    match sender {
        Some(tx) => tx
            .send(outcome)
            .map_err(|_| "审批请求已超时或已被处理".to_string()),
        None => Err(format!("未知的审批请求: {}", request_id)),
    }
}

// ─── 权限模式（对齐 DSH permission preset） ───

/// 当前权限模式标识（持久化文件内容，取其一）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    /// 询问：破坏性操作弹窗确认（默认）
    Ask,
    /// 只读：写类工具直接拒绝，不弹窗
    ReadOnly,
    /// 全放行：所有工具直接放行（无人值守）
    AllowAll,
}

impl ApprovalMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalMode::Ask => "ask",
            ApprovalMode::ReadOnly => "read-only",
            ApprovalMode::AllowAll => "allow-all",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "ask" => Some(ApprovalMode::Ask),
            "read-only" => Some(ApprovalMode::ReadOnly),
            "allow-all" => Some(ApprovalMode::AllowAll),
            _ => None,
        }
    }
}

/// 模式持久化文件路径：`%APPDATA%/com.mdgo/approval_mode`（内容为模式标识）
fn mode_file_path() -> std::path::PathBuf {
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
    base.join("com.mdgo").join("approval_mode")
}

/// 读取持久化的权限模式（文件缺失/非法 → 默认 Ask）
pub fn load_approval_mode() -> ApprovalMode {
    match std::fs::read_to_string(mode_file_path()) {
        Ok(raw) => ApprovalMode::from_str(raw.trim()).unwrap_or(ApprovalMode::Ask),
        Err(_) => ApprovalMode::Ask,
    }
}

/// 持久化权限模式（写文件失败仅记日志，不阻断模式切换——内存策略已生效）
fn save_approval_mode(mode: ApprovalMode) {
    let path = mode_file_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::warn!("[approval] 创建配置目录失败: {}", e);
            return;
        }
    }
    if let Err(e) = std::fs::write(&path, mode.as_str()) {
        log::warn!("[approval] 持久化权限模式失败 ({}): {}", mode.as_str(), e);
    }
}

/// 写类工具前缀（read-only 模式拒绝的集合）
const WRITE_TOOL_NAMES: &[&str] = &[
    "edit", "write", "delete", "multi_edit", "git_commit", "git_checkout", "git_reset",
];

fn is_write_tool(tool: &str) -> bool {
    WRITE_TOOL_NAMES.contains(&tool) || tool.starts_with("mcp_")
}

/// 按模式构建策略集合（对齐 lib.rs 启动时的组装：默认策略 + 配置策略叠加）。
pub fn policies_for_mode(
    mode: ApprovalMode,
    config_rules: Vec<crate::core::approval::policy::ApprovalRule>,
) -> Vec<Box<dyn ApprovalPolicy>> {
    let mut policies: Vec<Box<dyn ApprovalPolicy>> = Vec::new();
    // 配置驱动策略始终前置（allow/deny 规则短路默认策略；lib.rs 启动逻辑同）
    if !config_rules.is_empty() {
        policies.push(Box::new(crate::core::approval::policy::ConfigApprovalPolicy::new(
            config_rules,
        )));
    }
    match mode {
        ApprovalMode::Ask => {
            // 默认策略：edit/delete/write 等需确认
            policies.push(Box::new(
                crate::core::approval::policy::DestructiveWritePolicy::new(true),
            ));
        }
        ApprovalMode::ReadOnly => {
            // 只读：写类工具 deny（不弹窗、直接拒绝）
            policies.push(Box::new(ReadOnlyPolicy));
        }
        ApprovalMode::AllowAll => {
            // 全放行：allow 全部（短路其余策略）
            policies.push(Box::new(AllowAllPolicy));
        }
    }
    policies
}

/// 只读策略：写类工具直接拒绝（PolicyDenied，不弹窗）。
struct ReadOnlyPolicy;

impl ApprovalPolicy for ReadOnlyPolicy {
    fn evaluate(&self, _tool: &str, _args: &serde_json::Value) -> Option<ApprovalRequest> {
        // 只读模式不弹窗（由 deny 处理）
        None
    }

    fn allow(&self, _tool: &str, _args: &serde_json::Value) -> bool {
        false
    }

    fn deny(&self, tool: &str, _args: &serde_json::Value) -> Option<String> {
        if is_write_tool(tool) {
            Some(format!("只读模式已禁止写操作: {tool}"))
        } else {
            None
        }
    }
}

/// 全放行策略：所有工具 allow（短路其余策略）。
struct AllowAllPolicy;

impl ApprovalPolicy for AllowAllPolicy {
    fn evaluate(&self, _tool: &str, _args: &serde_json::Value) -> Option<ApprovalRequest> {
        None
    }

    fn allow(&self, _tool: &str, _args: &serde_json::Value) -> bool {
        true
    }

    fn deny(&self, _tool: &str, _args: &serde_json::Value) -> Option<String> {
        None
    }
}

/// 查询当前权限模式（前端按钮初始化显示）。
#[tauri::command]
pub fn approval_get_mode() -> String {
    load_approval_mode().as_str().to_string()
}

/// 切换权限模式（热生效：替换 gate 策略集 + 持久化）。
///
/// - `mode`: `ask` / `read-only` / `allow-all`
/// - 配置驱动规则（approval.yaml）始终叠加（allow/deny 规则优先于模式默认策略）
#[tauri::command]
pub async fn approval_set_mode(
    state: State<'_, AppState>,
    mode: String,
) -> Result<String, String> {
    let mode = ApprovalMode::from_str(mode.trim())
        .ok_or_else(|| "权限模式非法（应为 ask / read-only / allow-all）".to_string())?;

    // 叠加配置驱动规则（与启动时一致：加载 approval.yaml；失败留默认空）
    let rules = crate::core::approval::policy::load_approval_rules(
        &crate::core::approval::policy::default_rules_path(),
    )
    .unwrap_or_default();

    if let Some(gate) = &state.approval_gate {
        gate.set_policies(policies_for_mode(mode, rules));
        log::info!("[approval] 权限模式切换为 {}", mode.as_str());
    } else {
        log::warn!("[approval] 审批门未启用（approval_gate=None），仅持久化模式");
    }
    save_approval_mode(mode);
    Ok(mode.as_str().to_string())
}
