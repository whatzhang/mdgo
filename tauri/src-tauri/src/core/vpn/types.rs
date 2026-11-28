//! VPN 模块的对外数据结构（Serde 序列化给前端，字段统一 snake_case）。

use serde::{Deserialize, Serialize};

/// 代理端口。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ports {
    pub mixed: u16,
    pub controller: u16,
}

impl Default for Ports {
    fn default() -> Self {
        Self {
            mixed: 7890,
            controller: 9090,
        }
    }
}

/// 内核状态（扁平结构，便于前端直接渲染）。
#[derive(Debug, Clone, Serialize, Default)]
pub struct CoreState {
    /// stopped | starting | running | stopping | error
    pub state: String,
    pub pid: Option<u32>,
    pub version: String,
    pub message: String,
}

impl CoreState {
    pub fn stopped() -> Self {
        Self {
            state: "stopped".into(),
            ..Default::default()
        }
    }
    pub fn starting() -> Self {
        Self {
            state: "starting".into(),
            ..Default::default()
        }
    }
    pub fn running(pid: u32, version: String) -> Self {
        Self {
            state: "running".into(),
            pid: Some(pid),
            version,
            message: String::new(),
        }
    }
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            state: "error".into(),
            message: message.into(),
            ..Default::default()
        }
    }
}

/// 系统代理状态。
#[derive(Debug, Clone, Serialize, Default)]
pub struct SystemProxyState {
    /// disabled | enabling | enabled | restoring | error
    pub state: String,
    /// 已启用的网络服务（macOS）或 `Default`（Windows）。
    pub services: Vec<String>,
    /// 附加说明（例如服务名、PAC 提示）。
    pub detail: String,
    pub message: String,
}

impl SystemProxyState {
    pub fn disabled() -> Self {
        Self {
            state: "disabled".into(),
            ..Default::default()
        }
    }
    pub fn enabled(services: Vec<String>, detail: impl Into<String>) -> Self {
        Self {
            state: "enabled".into(),
            services,
            detail: detail.into(),
            message: String::new(),
        }
    }
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            state: "error".into(),
            message: message.into(),
            ..Default::default()
        }
    }
}

/// 连接状态（由内核 + 系统代理 + 诊断推导）。
#[derive(Debug, Clone, Serialize, Default)]
pub struct ConnectionState {
    /// disconnected | connecting | connected | degraded
    pub state: String,
    /// 连接步骤文案（connecting 时）。
    pub step: String,
    /// degraded 的原因列表。
    pub reasons: Vec<String>,
}

impl ConnectionState {
    pub fn disconnected() -> Self {
        Self {
            state: "disconnected".into(),
            ..Default::default()
        }
    }
    pub fn connecting(step: impl Into<String>) -> Self {
        Self {
            state: "connecting".into(),
            step: step.into(),
            reasons: vec![],
        }
    }
    pub fn connected() -> Self {
        Self {
            state: "connected".into(),
            ..Default::default()
        }
    }
    pub fn degraded(reasons: Vec<String>) -> Self {
        Self {
            state: "degraded".into(),
            step: String::new(),
            reasons,
        }
    }
}

/// 网络状态。
#[derive(Debug, Clone, Serialize, Default)]
pub struct NetworkState {
    /// online | offline | changed
    pub state: String,
    pub detail: String,
}

impl NetworkState {
    pub fn online(detail: impl Into<String>) -> Self {
        Self {
            state: "online".into(),
            detail: detail.into(),
        }
    }

}

/// 诊断单项。
#[derive(Debug, Clone, Serialize)]
pub struct DiagCheck {
    pub id: String,
    pub name: String,
    pub ok: bool,
    pub message: String,
    pub latency_ms: Option<u64>,
}

/// 诊断结果。
#[derive(Debug, Clone, Serialize, Default)]
pub struct Diagnostics {
    pub ok: bool,
    pub checks: Vec<DiagCheck>,
    pub conclusion: String,
    pub suggestions: Vec<String>,
    pub at: i64,
}

/// 聚合状态（`vpn_state` 返回值 / `vpn://state` 事件负载）。
#[derive(Debug, Clone, Serialize, Default)]
pub struct VpnState {
    pub core: CoreState,
    pub system_proxy: SystemProxyState,
    pub connection: ConnectionState,
    pub network: NetworkState,
    pub ports: Ports,
    pub active_profile: Option<String>,
    /// 当前路由模式（rule/global/direct），由 settings 同步，供前端高亮
    #[serde(default)]
    pub mode: String,
    pub diag: Option<Diagnostics>,
    /// 用户意图：connected / disconnected（崩溃后是否自愈的依据）
    pub intent: String,
    pub connected_at: Option<i64>,
}

/// 订阅/配置条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    /// url | file
    pub kind: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub interval_hours: u32,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub nodes: u32,
    #[serde(default)]
    pub groups: u32,
    #[serde(default)]
    pub rules: u32,
    #[serde(default)]
    pub upload: u64,
    #[serde(default)]
    pub download: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub expire_at: i64,
    #[serde(default)]
    pub last_error: String,
    #[serde(default)]
    pub updated_note: String,
    #[serde(default)]
    pub version_count: u32,
    /// 最近一次尝试更新的时间（失败退避用）
    #[serde(default)]
    pub last_attempt_at: i64,
    /// 连续失败次数（成功后清零）
    #[serde(default)]
    pub fail_count: u32,
}

/// `profiles.json`
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProfilesFile {
    #[serde(default)]
    pub active_id: String,
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

/// 订阅历史版本。
#[derive(Debug, Clone, Serialize)]
pub struct ProfileVersion {
    pub ts: i64,
    pub nodes: u32,
    pub groups: u32,
    pub rules: u32,
}

/// 日志条目。
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub time: String,
    pub level: String,
    pub message: String,
}

/// 代理节点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyNode {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub delay: Option<u64>,
    /// ok | slow | timeout | untested | testing
    pub delay_state: String,
    pub tested_at: Option<i64>,
}

/// 策略组。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyGroup {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub now: String,
    pub nodes: Vec<ProxyNode>,
}

/// 规则。
#[derive(Debug, Clone, Serialize)]
pub struct RuleItem {
    pub index: usize,
    #[serde(rename = "type")]
    pub kind: String,
    pub payload: String,
    pub proxy: String,
    pub provider: String,
    pub is_final: bool,
}

/// 连接。
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionItem {
    pub id: String,
    pub host: String,
    pub protocol: String,
    pub chains: Vec<String>,
    pub process: String,
    pub upload: u64,
    pub download: u64,
    pub start: String,
    pub rule: String,
}

/// 连接列表快照。
#[derive(Debug, Clone, Serialize, Default)]
pub struct ConnectionsSnapshot {
    pub connections: Vec<ConnectionItem>,
    pub upload: u64,
    pub download: u64,
}

/// 代理节点测速结果。
#[derive(Debug, Clone, Serialize, Default)]
pub struct DelayResult {
    pub ok: bool,
    pub delay_ms: Option<u64>,
    pub message: String,
}

// ───────────────────────── 设置 ─────────────────────────

/// Windows 系统代理快照。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WinProxySnapshot {
    pub proxy_enable: Option<u32>,
    pub proxy_server: Option<String>,
    pub proxy_override: Option<String>,
    pub auto_config_url: Option<String>,
    /// AutoDetect 为二进制值，直接用 REG_BINARY 原始字节保存
    pub auto_detect: Option<Vec<u8>>,
}

/// 单个代理协议的快照。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MacProxyEntry {
    pub enabled: bool,
    pub server: String,
    pub port: u16,
}

/// macOS 单个网络服务的快照。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MacServiceSnapshot {
    pub name: String,
    pub web: MacProxyEntry,
    pub secure_web: MacProxyEntry,
    pub socks: MacProxyEntry,
}

/// 系统代理快照（落盘到 settings.json）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SysProxySnapshot {
    pub owned: bool,
    pub session: String,
    pub created_at: i64,
    /// 正常退出且用户选择「退出时不恢复」时置 true：下次启动不再当作崩溃恢复
    #[serde(default)]
    pub clean_exit: bool,
    pub platform: String,
    pub windows: Option<WinProxySnapshot>,
    pub macos: Option<Vec<MacServiceSnapshot>>,
}

/// 设置（持久化于 `settings.json`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_mixed")]
    pub mixed_port: u16,
    #[serde(default = "default_controller")]
    pub controller_port: u16,
    /// 连接时是否自动探测可用端口
    #[serde(default = "default_true")]
    pub auto_ports: bool,
    /// rule | global | direct
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// 连接时是否自动开启系统代理
    #[serde(default = "default_true")]
    pub auto_system_proxy: bool,
    #[serde(default = "default_true")]
    pub http_proxy: bool,
    #[serde(default = "default_true")]
    pub https_proxy: bool,
    #[serde(default)]
    pub socks_proxy: bool,
    #[serde(default = "default_true")]
    pub restore_on_exit: bool,
    #[serde(default = "default_true")]
    pub recover_on_crash: bool,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub auto_connect: bool,
    #[serde(default)]
    pub auto_connect_last: bool,
    /// 上次成功连接的订阅 id（auto_connect_last 使用）
    #[serde(default)]
    pub last_profile_id: String,
    #[serde(default = "default_interval")]
    pub default_interval_hours: u32,
    #[serde(default = "default_true")]
    pub notify_on_update_fail: bool,
    #[serde(default = "default_test_url")]
    pub test_url: String,
    #[serde(default = "default_test_timeout")]
    pub test_timeout_ms: u64,
    #[serde(default = "default_test_timeout")]
    pub diag_timeout_ms: u64,
    /// None = 继承订阅；Some(true/false) = 覆盖 dns.enable
    #[serde(default)]
    pub dns_enable: Option<bool>,
    #[serde(default)]
    pub dns_override: bool,
    #[serde(default)]
    pub dns_nameservers: Vec<String>,
    /// 连接页是否显示进程列（需内核 find-process-mode）
    #[serde(default)]
    pub find_process: bool,
    /// macOS 手动指定的网络服务；空 = 自动识别
    #[serde(default)]
    pub mac_service: String,
    /// 手动指定的 mihomo 二进制路径
    #[serde(default)]
    pub bin_path: String,
    #[serde(default)]
    pub secret: String,
    #[serde(default)]
    pub sys_proxy_snapshot: Option<SysProxySnapshot>,
}

fn default_mixed() -> u16 {
    7890
}
fn default_controller() -> u16 {
    9090
}
fn default_mode() -> String {
    "rule".into()
}
fn default_log_level() -> String {
    "info".into()
}
fn default_interval() -> u32 {
    12
}
fn default_test_url() -> String {
    "https://www.gstatic.com/generate_204".into()
}
fn default_test_timeout() -> u64 {
    5000
}
fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mixed_port: 7890,
            controller_port: 9090,
            auto_ports: true,
            mode: "rule".into(),
            log_level: "info".into(),
            auto_system_proxy: true,
            http_proxy: true,
            https_proxy: true,
            socks_proxy: false,
            restore_on_exit: true,
            recover_on_crash: true,
            autostart: false,
            auto_connect: false,
            auto_connect_last: true,
            last_profile_id: String::new(),
            default_interval_hours: 12,
            notify_on_update_fail: true,
            test_url: default_test_url(),
            test_timeout_ms: 5000,
            diag_timeout_ms: 5000,
            dns_enable: None,
            dns_override: false,
            dns_nameservers: vec![],
            find_process: false,
            mac_service: String::new(),
            bin_path: String::new(),
            secret: String::new(),
            sys_proxy_snapshot: None,
        }
    }
}

/// 订阅导入参数。
#[derive(Debug, Clone, Deserialize)]
pub struct ProfileAddInput {
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub path: String,
    /// None = 用全局默认周期；Some(0) = 手动更新；Some(n) = 每 n 小时
    #[serde(default)]
    pub interval_hours: Option<u32>,
}

/// 可增量更新的设置（前端只传需要改的字段）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SettingsPatch {
    pub mixed_port: Option<u16>,
    pub controller_port: Option<u16>,
    pub auto_ports: Option<bool>,
    pub mode: Option<String>,
    pub log_level: Option<String>,
    pub auto_system_proxy: Option<bool>,
    pub http_proxy: Option<bool>,
    pub https_proxy: Option<bool>,
    pub socks_proxy: Option<bool>,
    pub restore_on_exit: Option<bool>,
    pub recover_on_crash: Option<bool>,
    pub autostart: Option<bool>,
    pub auto_connect: Option<bool>,
    pub auto_connect_last: Option<bool>,
    pub default_interval_hours: Option<u32>,
    pub notify_on_update_fail: Option<bool>,
    pub test_url: Option<String>,
    pub test_timeout_ms: Option<u64>,
    pub diag_timeout_ms: Option<u64>,
    pub dns_enable: Option<Option<bool>>,
    /// 置 true 时清空 dns_enable，恢复「完全继承订阅」
    pub dns_enable_clear: Option<bool>,
    pub dns_override: Option<bool>,
    pub dns_nameservers: Option<Vec<String>>,
    pub find_process: Option<bool>,
    pub mac_service: Option<String>,
    pub bin_path: Option<String>,
}
