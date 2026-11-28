//! VPN 管理器：状态机、生命周期、流式推送、订阅、系统代理、诊断、网络监听。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

pub mod api;
pub mod config;
pub mod diagnostics;
pub mod paths;
pub mod process;
pub mod profiles;
#[path = "settings.rs"]
pub mod settings_store;
pub mod system_proxy;
pub mod types;

use api::ApiClient;
use process::CoreSupervisor;
use types::*;

const LOG_RING_MAX: usize = 1000;

struct Inner {
    settings: Settings,
    state: VpnState,
    profiles: ProfilesFile,
    logs: VecDeque<LogEntry>,
    active_ports: Ports,
    totals: (u64, u64),
}

pub struct VpnManager {
    app: AppHandle,
    inner: Mutex<Inner>,
    lifecycle: tokio::sync::Mutex<()>,
    supervisor: CoreSupervisor,
    stream_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    monitors_started: AtomicBool,
    generation: AtomicU64,
    /// 退出流程标记：置位后不再刷新托盘菜单（避免主线程 block_on 与托盘调度互等）
    shutting_down: AtomicBool,
    /// 连续自动重启次数（成功后清零，避免无限自愈）
    crash_restarts: AtomicU32,
    /// 配置热重载串行化（避免并发写同一候选文件）
    reload_lock: tokio::sync::Mutex<()>,
}

impl VpnManager {
    pub fn new(app: AppHandle) -> Arc<Self> {
        let settings = settings_store::load();
        let _ = paths::ensure_dirs();
        let initial_ports = Ports {
            mixed: settings.mixed_port,
            controller: settings.controller_port,
        };
        let state = VpnState {
            core: CoreState::stopped(),
            system_proxy: SystemProxyState::disabled(),
            connection: ConnectionState::disconnected(),
            network: NetworkState::online("启动中"),
            ports: Ports {
                mixed: settings.mixed_port,
                controller: settings.controller_port,
            },
            active_profile: None,
            mode: settings.mode.clone(),
            diag: None,
            intent: "disconnected".into(),
            connected_at: None,
        };
        Arc::new(Self {
            app,
            inner: Mutex::new(Inner {
                settings,
                state,
                profiles: profiles::load_index(),
                logs: VecDeque::new(),
                active_ports: initial_ports,
                totals: (0, 0),
            }),
            lifecycle: tokio::sync::Mutex::new(()),
            supervisor: CoreSupervisor::new(),
            stream_tasks: Mutex::new(Vec::new()),
            monitors_started: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            shutting_down: AtomicBool::new(false),
            crash_restarts: AtomicU32::new(0),
            reload_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// 清理上次异常退出（Ctrl+C / 崩溃 / 强杀）遗留的内核进程。
    ///
    /// 应用启动时与每次连接前调用；只清理指向本数据目录的 mihomo，不影响其它实例。
    pub fn cleanup_stale_cores(&self) -> usize {
        let killed = kill_stale_cores(&paths::data_dir());
        if killed > 0 {
            self.push_log("warn", &format!("已清理 {killed} 个上次遗留的内核进程"));
        }
        paths::clear_pid_record();
        killed
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, String> {
        self.inner
            .lock()
            .map_err(|_| "VPN 状态锁已损坏".to_string())
    }

    // ───────────── 状态 ─────────────

    pub fn state(&self) -> VpnState {
        match self.lock() {
            Ok(g) => g.state.clone(),
            Err(_) => VpnState::default(),
        }
    }

    pub fn publish(&self) {
        let state = self.state();
        // 退出阶段跳过托盘刷新：主线程此时可能正 block_on 等待收尾任务，
        // 跨线程改菜单会与主线程互等。
        if !self.shutting_down.load(Ordering::SeqCst) {
            if let Some(tray) = self.app.try_state::<crate::tray::VpnTrayItems>() {
                tray.update(&state);
            }
        }
        let _ = self.app.emit("vpn://state", state);
    }

    fn publish_profiles(&self) {
        let (profiles, active) = match self.lock() {
            Ok(g) => (g.profiles.profiles.clone(), g.profiles.active_id.clone()),
            Err(_) => (vec![], String::new()),
        };
        let _ = self.app.emit(
            "vpn://profiles",
            serde_json::json!({ "profiles": profiles, "active_id": active }),
        );
    }

    fn emit_progress(&self, step: &str, label: &str, ok: bool, error: Option<String>) {
        let _ = self.app.emit(
            "vpn://progress",
            serde_json::json!({ "step": step, "label": label, "ok": ok, "error": error }),
        );
    }

    fn refresh_derived(&self) {
        let mut g = match self.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        g.state.mode = g.settings.mode.clone();
        let core_running = g.state.core.state == "running";
        let starting = g.state.core.state == "starting";
        let sys_enabled = g.state.system_proxy.state == "enabled";
        g.state.ports = if g.state.core.state == "running" {
            g.active_ports.clone()
        } else {
            Ports {
                mixed: g.settings.mixed_port,
                controller: g.settings.controller_port,
            }
        };
        g.state.connection = if starting {
            ConnectionState::connecting(if g.state.core.message.is_empty() {
                "正在启动内核…".into()
            } else {
                g.state.core.message.clone()
            })
        } else if core_running && sys_enabled {
            ConnectionState::connected()
        } else if core_running {
            let mut reasons = vec!["系统代理未开启：浏览器不会经过代理".to_string()];
            if g.state.system_proxy.state == "error" {
                reasons.push(g.state.system_proxy.message.clone());
            }
            ConnectionState::degraded(reasons)
        } else {
            ConnectionState::disconnected()
        };
        if core_running && sys_enabled {
            g.state.connected_at = g.state.connected_at.or_else(|| Some(paths::now_secs()));
        }
    }

    fn set_system_proxy_state(&self, state: SystemProxyState) {
        if let Ok(mut g) = self.lock() {
            g.state.system_proxy = state;
        }
        self.refresh_derived();
    }

    fn set_connection_intent(&self, intent: &str) {
        if let Ok(mut g) = self.lock() {
            g.state.intent = intent.to_string();
        }
    }

    // ───────────── 端口 / 客户端 ─────────────

    /// 探测实际监听端口：自动模式下避开对方端口，手动模式下检查占用并给出明确错误。
    fn probe_ports(&self, settings: &Settings) -> Result<Ports, String> {
        if !settings.auto_ports {
            if settings.mixed_port == settings.controller_port {
                return Err("混合端口与控制器端口不能相同".into());
            }
            if !paths::port_available(settings.mixed_port) {
                return Err(format!("混合端口 {} 已被占用", settings.mixed_port));
            }
            if !paths::port_available(settings.controller_port) {
                return Err(format!("控制器端口 {} 已被占用", settings.controller_port));
            }
            return Ok(Ports {
                mixed: settings.mixed_port,
                controller: settings.controller_port,
            });
        }
        let mixed = (0u16..20)
            .map(|offset| settings.mixed_port.saturating_add(offset))
            .find(|p| *p != settings.controller_port && paths::port_available(*p))
            .ok_or_else(|| {
                format!(
                    "未找到可用混合端口（{}~{} 均被占用）",
                    settings.mixed_port,
                    settings.mixed_port.saturating_add(19)
                )
            })?;
        let controller = (0u16..20)
            .map(|offset| settings.controller_port.saturating_add(offset))
            .find(|p| *p != mixed && paths::port_available(*p))
            .ok_or_else(|| {
                format!(
                    "未找到可用控制器端口（{}~{} 均被占用）",
                    settings.controller_port,
                    settings.controller_port.saturating_add(19)
                )
            })?;
        Ok(Ports { mixed, controller })
    }

    fn api(&self) -> Result<ApiClient, String> {
        let g = self.lock()?;
        Ok(ApiClient::new(
            &g.active_ports,
            &g.settings.secret,
            Duration::from_secs(8),
        ))
    }

    fn timeout_settings(&self) -> Settings {
        self.lock().map(|g| g.settings.clone()).unwrap_or_default()
    }

    // ───────────── 日志 ─────────────

    pub fn push_log(&self, level: &str, message: &str) {
        // mihomo SSE 用 "warning"，内核 stderr 分类用 "warn"：统一成前端认识的级别
        let level = match level {
            "warning" => "warn",
            "err" => "error",
            other => other,
        };
        let mut masked = mask_secrets(message);
        // 单条日志过长（内核偶发超大行）时截断，避免事件负载与环形缓冲膨胀
        if masked.chars().count() > 4000 {
            masked = masked.chars().take(4000).collect::<String>();
            masked.push_str(" …");
        }
        let entry = LogEntry {
            time: paths::clock_now(),
            level: level.to_string(),
            message: masked,
        };
        if let Ok(mut g) = self.lock() {
            if g.logs.len() >= LOG_RING_MAX {
                g.logs.pop_front();
            }
            g.logs.push_back(entry.clone());
        }
        let _ = self.app.emit("vpn://log", entry);
    }

    fn logs_snapshot(&self, level: &str, limit: usize) -> Vec<LogEntry> {
        let g = match self.lock() {
            Ok(g) => g,
            Err(_) => return vec![],
        };
        g.logs
            .iter()
            .rev()
            .filter(|e| level.is_empty() || level == "all" || e.level == level)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    pub fn clear_logs(&self) {
        if let Ok(mut g) = self.lock() {
            g.logs.clear();
        }
    }

    // ───────────── 连接 / 断开 ─────────────

    fn active_profile_data(&self) -> Result<(String, String), String> {
        let g = self.lock()?;
        let active = g
            .profiles
            .profiles
            .iter()
            .find(|p| p.id == g.profiles.active_id)
            .or_else(|| g.profiles.profiles.first())
            .ok_or_else(|| "还没有导入任何订阅，请先在「订阅」页导入".to_string())?;
        Ok((active.id.clone(), active.name.clone()))
    }

    pub async fn connect(self: &Arc<Self>) -> Result<VpnState, String> {
        let result = self.connect_inner().await;
        if result.is_err() {
            // 连接失败：清掉「用户想连」的意图，避免崩溃监控反复自愈
            self.set_connection_intent("disconnected");
            self.refresh_derived();
            self.publish();
        }
        result
    }

    async fn connect_inner(self: &Arc<Self>) -> Result<VpnState, String> {
        let _guard = self.lifecycle.lock().await;
        if self.supervisor.pid().is_some() {
            return Ok(self.state());
        }
        self.set_connection_intent("connected");
        // 先清理上次异常退出遗留的孤儿内核（Ctrl+C / 崩溃 / 强杀），
        // 在端口探测之前执行，避免旧内核占着端口导致新内核换端口。
        let stale = self.cleanup_stale_cores();
        if stale > 0 {
            log::warn!("[vpn] 连接前清理了 {stale} 个遗留内核进程");
        }
        let settings = self.timeout_settings();
        let bin = paths::resolve_core_bin(&self.app, &settings.bin_path)
            .ok_or_else(|| "未找到 mihomo 内核（请检查安装包资源或设置里的内核路径）".to_string())?;

        let (profile_id, _profile_name) = self.active_profile_data()?;
        let raw = profiles::read_raw(&profile_id).map_err(|e| format!("读取配置失败: {e}"))?;
        config::validate_structure(&raw)?;

        let ports = self.probe_ports(&settings)?;
        if let Ok(mut g) = self.lock() {
            g.state.ports = ports.clone();
            g.active_ports = ports.clone();
            g.state.core = CoreState::starting();
            g.state.core.message = "正在生成配置…".into();
        }
        self.refresh_derived();
        self.publish();
        self.emit_progress("config", "正在生成配置", true, None);

        let config_text = match config::build_runtime(&raw, &settings, &ports) {
            Ok(text) => text,
            Err(e) => {
                self.fail_core("生成配置失败", &e);
                return Err(e);
            }
        };

        self.emit_progress("test", "正在校验配置", true, None);
        let cfg_path = match self.prepare_config(&config_text, &bin).await {
            Ok(path) => path,
            Err(e) => {
                self.fail_core("配置校验失败", &e);
                return Err(e);
            }
        };

        // 启动内核
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.start_log_pump(rx);
        self.emit_progress("spawn", "正在启动内核", true, None);
        let pid = match self.supervisor.spawn(
            &bin,
            &cfg_path,
            &paths::cache_dir(),
            paths::log_path(),
            tx,
        ) {
            Ok(pid) => pid,
            Err(e) => {
                self.fail_core("内核启动失败", &e);
                return Err(e);
            }
        };
        paths::write_pid_record(pid, &bin, &cfg_path);
        self.push_log("info", &format!("内核已启动 pid={pid}，端口 {}", ports.mixed));

        // 等待就绪
        self.emit_progress("ready", "等待内核就绪", true, None);
        let api = ApiClient::new(&ports, &settings.secret, Duration::from_millis(1200));
        let mut version = String::new();
        for _ in 0..40 {
            match api.version().await {
                Ok(v) => {
                    version = v;
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
        if version.is_empty() {
            let _ = self.supervisor.stop().await;
            self.supervisor.forget();
        paths::clear_pid_record();
            let msg = "内核启动超时（8s 内未就绪），请查看日志".to_string();
            self.fail_core("启动超时", &msg);
            return Err(msg);
        }

        if let Ok(mut g) = self.lock() {
            g.state.core = CoreState::running(pid, version.clone());
            g.state.active_profile = Some(profile_id.clone());
            // 记住最近成功使用的订阅，供「启动时自动连接上次节点」使用
            g.settings.last_profile_id = profile_id.clone();
            let _ = settings_store::save(&g.settings);
        }
        self.push_log("info", &format!("内核就绪：{version}"));
        self.emit_progress("started", "内核已就绪", true, None);

        self.start_streams(api.clone());

        // 系统代理
        if settings.auto_system_proxy {
            self.emit_progress("sysproxy", "正在开启系统代理", true, None);
            match self.enable_sysproxy_internal().await {
                Ok(()) => self.push_log("info", "系统代理已开启"),
                Err(e) => {
                    self.push_log("error", &format!("系统代理开启失败: {e}"));
                    self.set_system_proxy_state(SystemProxyState::error(e));
                }
            }
        } else {
            self.set_system_proxy_state(SystemProxyState::disabled());
        }

        self.refresh_derived();
        self.publish();
        self.publish_profiles();

        // 异步诊断一次（不阻塞连接）
        let manager = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let diag = manager.run_diagnostics().await;
            if let Ok(mut g) = manager.lock() {
                g.state.diag = Some(diag.clone());
            }
            let _ = manager.app.emit("vpn://diag", diag);
        });

        Ok(self.state())
    }

    fn fail_core(&self, title: &str, detail: &str) {
        self.emit_progress("error", title, false, Some(detail.to_string()));
        if let Ok(mut g) = self.lock() {
            g.state.core = CoreState::error(format!("{title}：{detail}"));
        }
        self.push_log("error", &format!("{title}: {detail}"));
        self.refresh_derived();
        self.publish();
    }

    pub async fn disconnect(self: &Arc<Self>) -> Result<VpnState, String> {
        let _guard = self.lifecycle.lock().await;
        self.set_connection_intent("disconnected");
        // 先恢复系统代理，再停内核（避免指向死端口）
        if let Err(e) = self.disable_sysproxy_internal().await {
            self.push_log("warn", &format!("恢复系统代理失败: {e}"));
        }
        self.stop_streams();
        if let Err(e) = self.supervisor.stop().await {
            self.push_log("warn", &format!("停止内核失败: {e}"));
        }
        self.supervisor.forget();
        paths::clear_pid_record();
        self.crash_restarts.store(0, Ordering::SeqCst);
        if let Ok(mut g) = self.lock() {
            g.state.core = CoreState::stopped();
            g.state.connected_at = None;
            g.state.diag = None;
            g.totals = (0, 0);
        }
        self.refresh_derived();
        self.publish();
        Ok(self.state())
    }

    pub async fn restart_core(self: &Arc<Self>) -> Result<VpnState, String> {
        let _ = self.disconnect().await;
        self.connect().await
    }

    // ───────────── 流式任务 ─────────────

    fn start_log_pump(self: &Arc<Self>, mut rx: tokio::sync::mpsc::UnboundedReceiver<(String, String)>) {
        let manager = self.clone();
        tokio::spawn(async move {
            while let Some((level, message)) = rx.recv().await {
                manager.push_log(&level, &message);
            }
        });
    }

    fn start_streams(self: &Arc<Self>, api: ApiClient) {
        self.stop_streams();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let mut handles = Vec::new();

        // 流量：SSE 断开（内核重启/网络抖动）后自动重连，直到代际变化
        let manager = self.clone();
        let traffic_api = api.clone();
        handles.push(tokio::spawn(async move {
            let mut up_total = 0u64;
            let mut down_total = 0u64;
            loop {
                if manager.generation.load(Ordering::SeqCst) != generation {
                    break;
                }
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let stream = {
                    let api = traffic_api.clone();
                    tokio::spawn(async move {
                        let _ = api.stream_traffic(tx).await;
                    })
                };
                while let Some((up, down)) = rx.recv().await {
                    if manager.generation.load(Ordering::SeqCst) != generation {
                        break;
                    }
                    up_total += up;
                    down_total += down;
                    if let Ok(mut g) = manager.lock() {
                        g.totals = (up_total, down_total);
                    }
                    let _ = manager.app.emit(
                        "vpn://traffic",
                        serde_json::json!({
                            "up": up,
                            "down": down,
                            "up_total": up_total,
                            "down_total": down_total
                        }),
                    );
                }
                stream.abort();
                if manager.generation.load(Ordering::SeqCst) != generation {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }));

        // 日志流：同样自动重连；级别变化在下一次重连时生效
        let manager = self.clone();
        let log_api = api;
        handles.push(tokio::spawn(async move {
            loop {
                if manager.generation.load(Ordering::SeqCst) != generation {
                    break;
                }
                let level = manager
                    .lock()
                    .map(|g| g.settings.log_level.clone())
                    .unwrap_or_else(|_| "info".into());
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let stream = {
                    let api = log_api.clone();
                    let level = level.clone();
                    tokio::spawn(async move {
                        let _ = api.stream_logs(&level, tx).await;
                    })
                };
                while let Some((lv, payload)) = rx.recv().await {
                    if manager.generation.load(Ordering::SeqCst) != generation {
                        break;
                    }
                    manager.push_log(&lv, &payload);
                }
                stream.abort();
                if manager.generation.load(Ordering::SeqCst) != generation {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }));

        if let Ok(mut g) = self.stream_tasks.lock() {
            g.extend(handles);
        }
    }

    fn stop_streams(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut g) = self.stream_tasks.lock() {
            for handle in g.drain(..) {
                handle.abort();
            }
        }
    }

    // ───────────── 系统代理 ─────────────

    async fn enable_sysproxy_internal(&self) -> Result<(), String> {
        let (settings, ports, need_snapshot) = {
            let g = self.lock()?;
            (
                g.settings.clone(),
                g.active_ports.clone(),
                g.settings.sys_proxy_snapshot.is_none(),
            )
        };
        self.set_system_proxy_state(SystemProxyState {
            state: "enabling".into(),
            ..Default::default()
        });
        if need_snapshot {
            let snap = system_proxy::snapshot(&settings).await?;
            if let Ok(mut g) = self.lock() {
                g.settings.sys_proxy_snapshot = Some(snap);
                let _ = settings_store::save(&g.settings);
            }
        }
        let snapshot_for_enable = self
            .lock()?
            .settings
            .sys_proxy_snapshot
            .clone()
            .ok_or_else(|| "缺少系统代理快照".to_string())?;
        let applied = match system_proxy::enable(ports.mixed, &settings, &snapshot_for_enable).await {
            Ok(services) => {
                if system_proxy::verify(ports.mixed, &services)
                    .await
                    .unwrap_or(false)
                {
                    Ok(services)
                } else {
                    Err("系统代理写入后校验失败".to_string())
                }
            }
            Err(e) => Err(e),
        };
        let services = match applied {
            Ok(services) => services,
            Err(e) => {
                // 开启失败：尽量按快照回滚，避免系统指向一个不可用端口
                let snap = self.lock().ok().and_then(|g| g.settings.sys_proxy_snapshot.clone());
                if let Some(snap) = snap {
                    match system_proxy::restore(&snap).await {
                        Ok(()) => {
                            if let Ok(mut g) = self.lock() {
                                g.settings.sys_proxy_snapshot = None;
                                let _ = settings_store::save(&g.settings);
                            }
                            self.set_system_proxy_state(SystemProxyState::disabled());
                        }
                        Err(restore_err) => {
                            // 回滚也失败：保留快照，留给下次启动/手动恢复
                            self.set_system_proxy_state(SystemProxyState::error(format!(
                                "{e}；自动回滚也失败: {restore_err}"
                            )));
                            return Err(e);
                        }
                    }
                } else {
                    self.set_system_proxy_state(SystemProxyState::error(e.clone()));
                }
                return Err(e);
            }
        };
        let detail = services.join(", ");
        self.set_system_proxy_state(SystemProxyState::enabled(services, detail));
        Ok(())
    }

    async fn disable_sysproxy_internal(&self) -> Result<(), String> {
        let snap = {
            let g = self.lock()?;
            g.settings.sys_proxy_snapshot.clone()
        };
        let Some(snap) = snap else {
            self.set_system_proxy_state(SystemProxyState::disabled());
            return Ok(());
        };
        self.set_system_proxy_state(SystemProxyState {
            state: "restoring".into(),
            ..Default::default()
        });
        if let Err(e) = system_proxy::restore(&snap).await {
            // 恢复失败保留快照，并把状态置为 error（设置页可再次「立即恢复」）
            self.set_system_proxy_state(SystemProxyState::error(format!(
                "恢复系统代理失败: {e}"
            )));
            return Err(e);
        }
        if let Ok(mut g) = self.lock() {
            g.settings.sys_proxy_snapshot = None;
            let _ = settings_store::save(&g.settings);
        }
        self.set_system_proxy_state(SystemProxyState::disabled());
        Ok(())
    }

    /// 应用启动时的崩溃恢复：快照存在说明上次没有正常退出。
    pub async fn recover_after_crash(self: &Arc<Self>) {
        let (snapshot, enabled) = {
            let g = match self.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            (
                g.settings.sys_proxy_snapshot.clone(),
                g.settings.recover_on_crash,
            )
        };
        let Some(snap) = snapshot else { return };
        if !snap.owned {
            return;
        }
        // 正常退出且用户选择「退出不恢复」：清掉快照即可，不要冒充崩溃恢复
        if snap.clean_exit {
            if let Ok(mut g) = self.lock() {
                g.settings.sys_proxy_snapshot = None;
                let _ = settings_store::save(&g.settings);
            }
            return;
        }
        if !enabled {
            // 用户关闭了异常恢复：保留快照，让「立即恢复」仍可用
            self.set_system_proxy_state(SystemProxyState {
                state: "error".into(),
                message: "上次未正常恢复系统代理，可在设置中立即恢复".into(),
                ..Default::default()
            });
            self.publish();
            return;
        }
        log::warn!("[vpn] 检测到上次异常退出，正在恢复系统代理");
        match system_proxy::restore(&snap).await {
            Ok(()) => {
                self.push_log("warn", "检测到上次异常退出，已恢复系统代理");
                if let Ok(mut g) = self.lock() {
                    g.settings.sys_proxy_snapshot = None;
                    let _ = settings_store::save(&g.settings);
                }
                self.set_system_proxy_state(SystemProxyState::disabled());
            }
            Err(e) => {
                // 恢复失败必须保留快照，否则原始配置永久丢失、全机断网
                log::warn!("[vpn] 恢复系统代理失败: {e}");
                self.push_log("error", &format!("上次异常退出后恢复系统代理失败: {e}"));
                self.set_system_proxy_state(SystemProxyState::error(format!(
                    "上次未正常恢复系统代理: {e}"
                )));
            }
        }
        self.publish();
    }

    pub async fn set_system_proxy(self: &Arc<Self>, on: bool) -> Result<VpnState, String> {
        let _guard = self.lifecycle.lock().await;
        let result = if on {
            // 内核没起来时不要把系统指向死端口
            if self.supervisor.pid().is_none() {
                Err("请先开启代理（内核未运行）".to_string())
            } else {
                self.enable_sysproxy_internal().await
            }
        } else {
            self.disable_sysproxy_internal().await
        };
        if let Err(e) = &result {
            self.set_system_proxy_state(SystemProxyState::error(e.clone()));
        }
        self.refresh_derived();
        self.publish();
        result.map(|_| self.state())
    }

    // ───────────── 诊断 ─────────────

    pub async fn run_diagnostics(self: &Arc<Self>) -> Diagnostics {
        let (settings, ports, core_running, services) = {
            match self.lock() {
                Ok(g) => (
                    g.settings.clone(),
                    g.active_ports.clone(),
                    g.state.core.state == "running" && self.supervisor.pid().is_some(),
                    g.state.system_proxy.services.clone(),
                ),
                Err(_) => (Settings::default(), Ports::default(), false, vec![]),
            }
        };
        let api = ApiClient::new(&ports, &settings.secret, Duration::from_secs(5));
        let diag = diagnostics::run(diagnostics::DiagContext {
            api: &api,
            ports: &ports,
            settings: &settings,
            core_running,
            proxy_services: &services,
        })
        .await;
        if let Ok(mut g) = self.lock() {
            g.state.diag = Some(diag.clone());
        }
        let _ = self.app.emit("vpn://diag", diag.clone());
        diag
    }

    // ───────────── 代理 / 规则 / 连接 ─────────────

    pub async fn proxies(&self) -> Result<Vec<ProxyGroup>, String> {
        self.api()?.proxies().await
    }

    pub async fn select_proxy(&self, group: &str, name: &str) -> Result<(), String> {
        self.api()?.select(group, name).await?;
        self.publish();
        Ok(())
    }

    pub async fn delay(&self, name: &str) -> Result<DelayResult, String> {
        let settings = self.timeout_settings();
        Ok(self
            .api()?
            .delay(name, &settings.test_url, settings.test_timeout_ms)
            .await)
    }

    pub async fn group_delay(&self, group: &str) -> Result<std::collections::HashMap<String, u64>, String> {
        let settings = self.timeout_settings();
        self.api()?
            .group_delay(group, &settings.test_url, settings.test_timeout_ms)
            .await
    }

    pub async fn set_mode(self: &Arc<Self>, mode: &str) -> Result<VpnState, String> {
        let normalized = config::normalize_mode(mode);
        if let Ok(mut g) = self.lock() {
            g.settings.mode = normalized.to_string();
            let _ = settings_store::save(&g.settings);
        }
        let api = self.api()?;
        if api.version().await.is_ok() {
            // 内核未运行时只持久化设置；运行中切换失败也不阻塞（下次重载会带上新模式）
            if let Err(e) = api.set_mode(normalized).await {
                self.push_log("warn", &format!("切换路由模式失败: {e}"));
            }
        }
        self.push_log("info", &format!("路由模式切换为 {normalized}"));
        self.publish();
        Ok(self.state())
    }

    pub async fn rules(&self) -> Result<Vec<RuleItem>, String> {
        self.api()?.rules().await
    }

    pub async fn connections(&self) -> Result<ConnectionsSnapshot, String> {
        self.api()?.connections().await
    }

    pub async fn kill_connection(&self, id: &str) -> Result<(), String> {
        self.api()?.kill_connection(id).await
    }

    pub async fn kill_all_connections(&self) -> Result<(), String> {
        self.api()?.kill_all().await
    }

    // ───────────── 订阅 ─────────────

    /// 启动自动连接前，把激活项切回「上次成功连接的订阅」。
    pub fn prepare_last_profile(&self) {
        let (last, exists) = match self.lock() {
            Ok(g) => {
                if !g.settings.auto_connect_last || g.settings.last_profile_id.is_empty() {
                    return;
                }
                (
                    g.settings.last_profile_id.clone(),
                    g.profiles.profiles.iter().any(|p| p.id == g.settings.last_profile_id),
                )
            }
            Err(_) => return,
        };
        if !exists {
            return;
        }
        if let Ok(mut g) = self.lock() {
            g.profiles.active_id = last;
            let _ = profiles::save_index(&g.profiles);
        }
    }

    pub fn profiles_list(&self) -> (Vec<Profile>, String) {
        match self.lock() {
            Ok(g) => (g.profiles.profiles.clone(), g.profiles.active_id.clone()),
            Err(_) => (vec![], String::new()),
        }
    }

    /// 导入/更新前的统一校验：结构 → 生成运行配置 →（有内核时）`mihomo -t`。
    ///
    /// 目的是避免把「YAML 合法但 mihomo 不认」的订阅存进去，导致下次启动直接失败。
    async fn validate_import(&self, raw: &str) -> Result<(), String> {
        config::validate_structure(raw)?;
        let settings = self.timeout_settings();
        let ports = Ports {
            mixed: settings.mixed_port,
            controller: settings.controller_port,
        };
        let built = config::build_runtime(raw, &settings, &ports)?;
        let Some(bin) = paths::resolve_core_bin(&self.app, &settings.bin_path) else {
            // 没有内核时无法做 mihomo -t；结构校验已通过，连接阶段会给出明确错误
            return Ok(());
        };
        let candidate = paths::data_dir()
            .join(format!("import.candidate.{}-{}.yaml", std::process::id(), paths::now_ms()));
        paths::atomic_write(&candidate, built.as_bytes())
            .map_err(|e| format!("写入临时配置失败: {e}"))?;
        let test = config::config_test(&bin, &candidate, &paths::cache_dir()).await;
        let _ = std::fs::remove_file(&candidate);
        match test {
            Ok(None) => Ok(()),
            Ok(Some(err)) => Err(format!("mihomo -t 配置测试失败：{err}")),
            Err(e) => Err(e),
        }
    }

    /// 校验订阅 id 是否属于当前索引，且只含安全字符。
    ///
    /// 所有会拼路径的入口（删除目录 / 读版本 / 回滚）都必须先过这里，
    /// 否则 `id` 传入绝对路径或 `..` 会穿越到 VPN 数据目录甚至任意目录。
    fn checked_profile_id(&self, id: &str) -> Result<(), String> {
        if !is_safe_profile_id(id) {
            return Err("非法的订阅 id".into());
        }
        let g = self.lock()?;
        if !g.profiles.profiles.iter().any(|p| p.id == id) {
            return Err("订阅不存在".into());
        }
        Ok(())
    }

    pub async fn add_profile(self: &Arc<Self>, input: ProfileAddInput) -> Result<Profile, String> {
        let id = profiles::new_id();
        let name = if input.name.trim().is_empty() {
            "未命名订阅".to_string()
        } else {
            input.name.trim().to_string()
        };
        let interval = match input.interval_hours {
            Some(n) => n,
            None => self.timeout_settings().default_interval_hours,
        };

        let (raw, sub) = if !input.url.trim().is_empty() {
            profiles::fetch_subscription(input.url.trim()).await?
        } else if !input.path.trim().is_empty() {
            (profiles::read_local(input.path.trim())?, profiles::SubInfo::default())
        } else {
            return Err("请提供订阅 URL 或本地 YAML 路径".into());
        };
        self.validate_import(&raw).await?;

        let mut profile = Profile {
            id: id.clone(),
            name,
            kind: if input.url.trim().is_empty() { "file".into() } else { "url".into() },
            url: input.url.trim().to_string(),
            path: input.path.trim().to_string(),
            interval_hours: interval,
            updated_at: paths::now_secs(),
            upload: sub.upload,
            download: sub.download,
            total: sub.total,
            expire_at: sub.expire,
            ..Default::default()
        };
        profiles::apply_stats(&mut profile, &raw);
        profiles::save_raw(&id, &raw)?;

        if let Ok(mut g) = self.lock() {
            if g.profiles.active_id.is_empty() {
                g.profiles.active_id = id.clone();
            }
            g.profiles.profiles.push(profile.clone());
            let _ = profiles::save_index(&g.profiles);
        }
        self.publish_profiles();
        self.push_log("info", &format!("已导入订阅：{}", profile.name));
        Ok(profile)
    }

    pub async fn update_profile(self: &Arc<Self>, id: &str) -> Result<Profile, String> {
        self.checked_profile_id(id)?;
        let profile = {
            let g = self.lock()?;
            g.profiles
                .profiles
                .iter()
                .find(|p| p.id == id)
                .cloned()
                .ok_or_else(|| "订阅不存在".to_string())?
        };
        let result = if profile.kind == "url" && !profile.url.is_empty() {
            profiles::fetch_subscription(&profile.url)
                .await
                .map(|(raw, info)| (raw, Some(info)))
        } else if !profile.path.is_empty() {
            profiles::read_local(&profile.path).map(|raw| (raw, None))
        } else {
            Err("该订阅没有可更新的来源".to_string())
        };
        // 先校验再落盘：校验失败时保留旧配置（下方 Err 分支记录 last_error）
        // 0 节点订阅视为异常（机场下线/空配置），不能覆盖掉现有可用节点
        let result = match result {
            Ok((raw, info)) => {
                // 0 节点订阅（机场下线/空配置）不能覆盖掉现有可用节点
                let (new_nodes, _, _) = config::stats(&raw);
                if profile.nodes > 0 && new_nodes == 0 {
                    Err("订阅返回 0 个节点，已保留旧配置".to_string())
                } else {
                    match self.validate_import(&raw).await {
                        Ok(()) => Ok((raw, info)),
                        Err(e) => Err(e),
                    }
                }
            }
            Err(e) => Err(e),
        };

        match result {
            Ok((raw, info)) => {
                profiles::backup_version(id);
                profiles::save_raw(id, &raw)?;
                let mut updated = profile.clone();
                profiles::apply_stats(&mut updated, &raw);
                updated.updated_at = paths::now_secs();
                updated.last_error.clear();
                if let Some(info) = info {
                    updated.upload = info.upload;
                    updated.download = info.download;
                    updated.total = info.total;
                    updated.expire_at = info.expire;
                }
                updated.version_count = profiles::versions(id).len() as u32;
                updated.last_attempt_at = paths::now_secs();
                updated.fail_count = 0;
                if !updated.updated_note.is_empty() {
                    self.push_log("warn", &format!("{}：{}", updated.name, updated.updated_note));
                }
                let is_active = {
                    let g = self.lock()?;
                    g.profiles.active_id == id
                };
                if let Ok(mut g) = self.lock() {
                    if let Some(slot) = g.profiles.profiles.iter_mut().find(|p| p.id == id) {
                        *slot = updated.clone();
                    }
                    let _ = profiles::save_index(&g.profiles);
                }
                if is_active {
                    // 复用统一的热重载路径：内部会重新校验并在失败时回退重启
                    if let Err(e) = self.reload_runtime_config().await {
                        self.push_log("warn", &format!("更新后热重载失败: {e}"));
                    }
                }
                self.publish_profiles();
                Ok(updated)
            }
            Err(e) => {
                if let Ok(mut g) = self.lock() {
                    if let Some(slot) = g.profiles.profiles.iter_mut().find(|p| p.id == id) {
                        slot.last_error = mask_secrets(&e);
                        slot.last_attempt_at = paths::now_secs();
                        slot.fail_count = slot.fail_count.saturating_add(1);
                    }
                    let _ = profiles::save_index(&g.profiles);
                }
                self.publish_profiles();
                // 更新失败通知（设置项 notify_on_update_fail 的真实接线）
                let notify = self
                    .lock()
                    .map(|g| g.settings.notify_on_update_fail)
                    .unwrap_or(true);
                if notify {
                    let _ = self.app.emit(
                        "vpn://notice",
                        serde_json::json!({
                            "level": "error",
                            "message": format!("{} 更新失败：{}", profile.name, mask_secrets(&e))
                        }),
                    );
                }
                Err(e)
            }
        }
    }

    pub fn remove_profile(self: &Arc<Self>, id: &str) -> Result<(), String> {
        self.checked_profile_id(id)?;
        if let Ok(mut g) = self.lock() {
            g.profiles.profiles.retain(|p| p.id != id);
            if g.profiles.active_id == id {
                g.profiles.active_id = g
                    .profiles
                    .profiles
                    .first()
                    .map(|p| p.id.clone())
                    .unwrap_or_default();
            }
            let _ = profiles::save_index(&g.profiles);
        }
        let _ = std::fs::remove_dir_all(paths::profile_dir(id));
        self.publish_profiles();
        Ok(())
    }

    pub async fn activate_profile(self: &Arc<Self>, id: &str) -> Result<(), String> {
        self.checked_profile_id(id)?;
        let raw = profiles::read_raw(id)?;
        config::validate_structure(&raw)?;
        let previous_id = {
            let mut g = self.lock()?;
            if !g.profiles.profiles.iter().any(|p| p.id == id) {
                return Err("订阅不存在".into());
            }
            let previous = g.profiles.active_id.clone();
            g.profiles.active_id = id.to_string();
            let _ = profiles::save_index(&g.profiles);
            g.state.active_profile = Some(id.to_string());
            previous
        };
        // 已连接时热重载；未连接时只切换激活项
        if self.supervisor.pid().is_some() {
            if let Err(e) = self.reload_runtime_config().await {
                // 热重载失败：回滚激活项，避免下次启动用上未通过的配置
                if let Ok(mut g) = self.lock() {
                    g.profiles.active_id = previous_id.clone();
                    let _ = profiles::save_index(&g.profiles);
                    g.state.active_profile = if previous_id.is_empty() {
                        None
                    } else {
                        Some(previous_id.clone())
                    };
                }
                self.publish_profiles();
                self.publish();
                return Err(e);
            }
        }
        self.publish_profiles();
        self.publish();
        Ok(())
    }

    pub fn versions_for(&self, id: &str) -> Vec<ProfileVersion> {
        if self.checked_profile_id(id).is_err() {
            return Vec::new();
        }
        profiles::versions(id)
    }

    pub async fn rollback_profile(self: &Arc<Self>, id: &str, ts: i64) -> Result<(), String> {
        self.checked_profile_id(id)?;
        profiles::rollback(id, ts)?;
        if self.supervisor.pid().is_some() {
            let _ = self.activate_profile(id).await;
        }
        Ok(())
    }

    /// 定时更新：由后台任务调用。
    pub async fn run_due_updates(self: &Arc<Self>) {
        let index = match self.lock() {
            Ok(g) => g.profiles.clone(),
            Err(_) => return,
        };
        let due = profiles::due_ids(&index, paths::now_secs());
        for id in due {
            if let Err(e) = self.update_profile(&id).await {
                log::warn!("[vpn] 自动更新订阅 {id} 失败: {e}");
            }
        }
    }

    // ───────────── 设置 ─────────────

    pub fn settings(&self) -> Settings {
        self.lock().map(|g| g.settings.clone()).unwrap_or_default()
    }

    pub async fn apply_settings(self: &Arc<Self>, patch: SettingsPatch) -> Result<Settings, String> {
        let (autostart_changed, needs_reload, needs_restart) = {
            let mut g = self.lock()?;
            let before = (
                g.settings.autostart,
                g.settings.mixed_port,
                g.settings.controller_port,
                g.settings.auto_ports,
                g.settings.dns_enable,
                g.settings.dns_override,
                g.settings.dns_nameservers.clone(),
                g.settings.find_process,
                g.settings.log_level.clone(),
            );
            settings_store::apply_patch(&mut g.settings, patch);
            settings_store::save(&g.settings)?;
            let after = (
                g.settings.autostart,
                g.settings.mixed_port,
                g.settings.controller_port,
                g.settings.auto_ports,
                g.settings.dns_enable,
                g.settings.dns_override,
                g.settings.dns_nameservers.clone(),
                g.settings.find_process,
                g.settings.log_level.clone(),
            );
            let port_changed =
                before.1 != after.1 || before.2 != after.2 || before.3 != after.3;
            // find-process-mode 在部分内核版本热重载不生效，改为一律重启内核
            let find_process_changed = before.7 != after.7;
            let reload_changed = before.4 != after.4
                || before.5 != after.5
                || before.6 != after.6
                || before.8 != after.8;
            (
                before.0 != after.0,
                reload_changed,
                port_changed || find_process_changed,
            )
        };
        if autostart_changed {
            let enabled = self.settings().autostart;
            settings_store::set_autostart(enabled)?;
        }
        if self.supervisor.pid().is_some() {
            if needs_restart {
                // 端口变更必须重启内核才能生效
                if let Err(e) = self.restart_core().await {
                    self.push_log("warn", &format!("端口变更后重启内核失败: {e}"));
                }
            } else if needs_reload {
                if let Err(e) = self.reload_runtime_config().await {
                    self.push_log("warn", &format!("设置变更后热重载失败: {e}"));
                }
            } else if let Ok(api) = self.api() {
                let _ = api.set_mode(config::normalize_mode(&self.settings().mode)).await;
            }
        }
        self.publish();
        Ok(self.settings())
    }

    /// 配置流水线：写候选文件 → `mihomo -t` → 原子替换正式配置。
    ///
    /// 只有校验通过才会覆盖 `config.yaml`，避免把坏配置留给下次启动。
    async fn prepare_config(
        &self,
        config_text: &str,
        bin: &std::path::Path,
    ) -> Result<std::path::PathBuf, String> {
        // 唯一候选文件名：并发热重载/连接时不会互相 rename 走对方正在写的文件
        let candidate = paths::data_dir().join(format!("config.candidate.{}-{}.yaml", std::process::id(), paths::now_ms()));
        paths::atomic_write(&candidate, config_text.as_bytes())
            .map_err(|e| format!("写入配置失败: {e}"))?;
        if let Some(err) = config::config_test(bin, &candidate, &paths::cache_dir()).await? {
            let _ = std::fs::remove_file(&candidate);
            return Err(format!("配置校验失败：{err}"));
        }
        let final_path = paths::config_path();
        #[cfg(target_os = "windows")]
        let _ = std::fs::remove_file(&final_path);
        std::fs::rename(&candidate, &final_path).map_err(|e| format!("替换配置失败: {e}"))?;
        Ok(final_path)
    }

    /// 按当前设置重建运行配置并热重载（DNS / 进程匹配 / 日志级别等）。
    async fn reload_runtime_config(self: &Arc<Self>) -> Result<(), String> {
        let _guard = self.reload_lock.lock().await;
        self.reload_runtime_config_inner().await
    }

    async fn reload_runtime_config_inner(self: &Arc<Self>) -> Result<(), String> {
        let (settings, ports, active_id) = {
            let g = self.lock()?;
            (
                g.settings.clone(),
                g.active_ports.clone(),
                g.profiles.active_id.clone(),
            )
        };
        if active_id.is_empty() {
            return Err("没有激活的订阅".into());
        }
        let raw = profiles::read_raw(&active_id)?;
        let config_text = config::build_runtime(&raw, &settings, &ports)?;
        let bin = paths::resolve_core_bin(&self.app, &settings.bin_path)
            .ok_or_else(|| "未找到 mihomo 内核".to_string())?;
        self.prepare_config(&config_text, &bin).await?;
        let api = self.api()?;
        if api.reload_config().await.is_err() {
            self.push_log("warn", "配置热重载失败，正在重启内核");
            self.restart_core().await?;
        } else {
            self.push_log("info", "配置已热重载");
        }
        Ok(())
    }

    pub fn ensure_monitors(self: &Arc<Self>) {
        if self.monitors_started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.spawn_crash_monitor();
        self.spawn_network_monitor();
        self.spawn_update_scheduler();
    }

    fn spawn_crash_monitor(self: &Arc<Self>) {
        let manager = self.clone();
        tokio::spawn(async move {
            // 连续崩溃计数只在「稳定运行一段时间」后才清零，否则启动即崩会无限自愈
            let mut healthy_since: Option<std::time::Instant> = None;
            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;
                if manager.shutting_down.load(Ordering::SeqCst) {
                    return;
                }
                let Some(pid) = manager.supervisor.pid() else {
                    healthy_since = None;
                    continue;
                };
                if pid_alive(pid) {
                    let since = *healthy_since.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed() >= Duration::from_secs(120) {
                        manager.crash_restarts.store(0, Ordering::SeqCst);
                    }
                    continue;
                }
                healthy_since = None;
                manager.supervisor.forget();
                paths::clear_pid_record();
                manager.push_log("error", &format!("内核进程已退出（pid={pid}）"));
                // ① 先恢复系统代理，避免把系统指向已死端口
                let _ = manager.disable_sysproxy_internal().await;
                let (intent, recover) = match manager.lock() {
                    Ok(g) => (g.state.intent.clone(), g.settings.recover_on_crash),
                    Err(_) => ("disconnected".into(), false),
                };
                if intent != "connected" {
                    continue;
                }
                let attempts = manager.crash_restarts.fetch_add(1, Ordering::SeqCst);
                if recover && attempts < 3 {
                    manager.push_log(
                        "warn",
                        &format!("3 秒后尝试重启内核（第 {} 次）", attempts + 1),
                    );
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    match manager.connect().await {
                        Ok(_) => { /* 计数保留：稳定运行 120s 后由监控清零 */ }
                        Err(e) => manager.push_log("error", &format!("自动重启失败: {e}")),
                    }
                } else {
                    if let Ok(mut g) = manager.lock() {
                        g.state.core = CoreState::error("内核异常退出（已停止自动重启）");
                    }
                    manager.refresh_derived();
                    manager.publish();
                }
            }
        });
    }

    fn spawn_network_monitor(self: &Arc<Self>) {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut last_fingerprint = String::new();
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if manager.shutting_down.load(Ordering::SeqCst) {
                    return;
                }
                let fingerprint = network_fingerprint();
                if fingerprint == last_fingerprint {
                    continue;
                }
                let first = last_fingerprint.is_empty();
                last_fingerprint = fingerprint.clone();
                if first {
                    continue;
                }
                manager.push_log("info", &format!("网络环境变化：{fingerprint}"));
                let network = NetworkState {
                    state: "changed".into(),
                    detail: fingerprint,
                };
                if let Ok(mut g) = manager.lock() {
                    g.state.network = network.clone();
                }
                let _ = manager.app.emit("vpn://network", network);
                manager.repair_after_network_change().await;
            }
        });
    }

    async fn repair_after_network_change(self: &Arc<Self>) {
        let (intent, auto_sys) = match self.lock() {
            Ok(g) => (g.state.intent.clone(), g.settings.auto_system_proxy),
            Err(_) => return,
        };
        if intent != "connected" {
            return;
        }
        let core_ok = match self.api() {
            Ok(api) => api.version().await.is_ok(),
            Err(_) => false,
        };
        if !core_ok {
            self.push_log("warn", "网络变化后内核无响应，正在重启");
            let _ = self.restart_core().await;
            return;
        }
        if auto_sys {
            let (ports, services) = match self.lock() {
                Ok(g) => (g.active_ports.clone(), g.state.system_proxy.services.clone()),
                Err(_) => return,
            };
            let ok = system_proxy::verify(ports.mixed, &services)
                .await
                .unwrap_or(false);
            if !ok {
                self.push_log("warn", "网络变化后系统代理失效，正在重新开启");
                if let Err(e) = self.enable_sysproxy_internal().await {
                    self.push_log("error", &format!("网络变化后重新开启系统代理失败: {e}"));
                    self.set_system_proxy_state(SystemProxyState::error(e));
                }
                self.refresh_derived();
                self.publish();
            }
        }
    }

    fn spawn_update_scheduler(self: &Arc<Self>) {
        let manager = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(300)).await;
                manager.run_due_updates().await;
            }
        });
    }

    /// 应用退出前的收尾：恢复系统代理 + 停内核。
    pub async fn shutdown(self: &Arc<Self>) {
        // 幂等：托盘退出与 RunEvent::ExitRequested 都会调用一次；
        // 第二个调用必须立刻返回，避免主线程 block_on 与第一次收尾互等。
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            return;
        }
        // 有界等待生命周期锁：连接流程最长可能 ~20s，不能把退出卡死
        let _guard = match tokio::time::timeout(Duration::from_secs(3), self.lifecycle.lock()).await
        {
            Ok(guard) => Some(guard),
            Err(_) => {
                log::warn!("[vpn] 退出时等待生命周期锁超时，跳过系统代理恢复（快照保留，下次启动恢复）");
                None
            }
        };
        let restore = self
            .lock()
            .map(|g| g.settings.restore_on_exit)
            .unwrap_or(true);
        if restore {
            if let Err(e) = self.disable_sysproxy_internal().await {
                log::warn!("[vpn] 退出时恢复系统代理失败: {e}");
            }
        } else if let Ok(mut g) = self.lock() {
            // 用户选择退出不恢复：标记为正常退出，避免下次启动误判崩溃
            if let Some(snap) = g.settings.sys_proxy_snapshot.as_mut() {
                snap.clean_exit = true;
                let _ = settings_store::save(&g.settings);
            }
        }
        self.stop_streams();
        let _ = tokio::time::timeout(Duration::from_secs(3), self.supervisor.stop()).await;
        self.supervisor.forget();
        paths::clear_pid_record();
    }

    pub fn open_data_dir(&self) -> Result<(), String> {
        let dir = paths::data_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        #[cfg(target_os = "windows")]
        {
            std::process::Command::new("explorer")
                .arg(&dir)
                .spawn()
                .map_err(|e| e.to_string())?;
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open")
                .arg(&dir)
                .spawn()
                .map_err(|e| e.to_string())?;
            return Ok(());
        }
        #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
        {
            std::process::Command::new("xdg-open")
                .arg(&dir)
                .spawn()
                .map_err(|e| e.to_string())?;
            Ok(())
        }
    }

    pub fn logs(&self, level: &str, limit: usize) -> Vec<LogEntry> {
        self.logs_snapshot(level, limit)
    }

    pub fn network_state(&self) -> NetworkState {
        self.lock()
            .map(|g| g.state.network.clone())
            .unwrap_or_default()
    }
}

/// 清理指向本数据目录的遗留内核进程（Ctrl+C / 崩溃后未回收的孤儿）。
///
/// 只匹配「进程名含 mihomo 且命令行包含本数据目录的 cache/config 路径」，
/// 避免误杀用户其它 mihomo 实例。
pub(crate) fn kill_stale_cores(data_dir: &std::path::Path) -> usize {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let cache = data_dir.join("cache").to_string_lossy().to_string();
    let config = data_dir.join("config.yaml").to_string_lossy().to_string();
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always),
    );
    let mut killed = 0;
    for (pid, process) in sys.processes() {
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        if !name.contains("mihomo") {
            continue;
        }
        let cmd = process
            .cmd()
            .iter()
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        if !cmd.contains(&cache) && !cmd.contains(&config) {
            continue;
        }
        log::warn!("[vpn] 清理遗留内核进程 pid={pid} cmd={cmd}");
        if process.kill() {
            killed += 1;
        }
    }
    killed
}

fn pid_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys.process(Pid::from_u32(pid)).is_some()
}

/// 网络指纹：网卡名 + 本地地址列表（用于检测换网/断网）。
fn network_fingerprint() -> String {
    use std::net::UdpSocket;
    let local = UdpSocket::bind("0.0.0.0:0")
        .ok()
        .and_then(|s| {
            s.connect("223.5.5.5:53").ok()?;
            s.local_addr().ok()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "offline".into());

    let mut names: Vec<String> = vec![];
    let mut networks = sysinfo::Networks::new_with_refreshed_list();
    networks.refresh(true);
    for (name, _) in networks.list() {
        names.push(name.clone());
    }
    names.sort();
    format!("{local}|{}", names.join(","))
}

/// 订阅 id 只允许短横线/下划线/字母数字，避免 ..、绝对路径等穿越数据目录。
pub(crate) fn is_safe_profile_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 日志脱敏：token/password/secret/key 等参数值打码。
///
/// 允许前缀（access_token、apikey、auth_key…）一起命中，只替换值，保留键名可读性。
pub(crate) fn mask_secrets(text: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new("(?i)([\\w-]*(?:token|password|passwd|uuid|secret|auth|key))[=:]([^&\\s\"']+)")
            .expect("mask regex")
    });
    re.replace_all(text, "$1=••••").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：模拟调试 Ctrl+C 后遗留的内核，启动前清理必须能杀掉它。
    #[tokio::test]
    #[ignore = "需要本机 mihomo 二进制"]
    async fn stale_core_process_is_killed() {
        let name = if cfg!(windows) {
            "mihomo-windows-x64.exe"
        } else {
            "mihomo-darwin-arm64"
        };
        let bin = paths::bin_dir().join(name);
        if !bin.is_file() {
            eprintln!("跳过：未找到 mihomo 二进制");
            return;
        }
        let dir = std::env::temp_dir().join("mdgo-vpn-orphan-test");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let config = dir.join("config.yaml");
        std::fs::write(
            &config,
            "mixed-port: 17891\nexternal-controller: 127.0.0.1:19091\nproxies: []\nproxy-groups:\n  - {name: T, type: select, proxies: [DIRECT]}\nrules:\n  - MATCH,T\n",
        )
        .expect("写测试配置");
        let mut child = std::process::Command::new(&bin)
            .arg("-d")
            .arg(&dir)
            .arg("-f")
            .arg(&config)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("启动测试内核");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            child.try_wait().expect("查询测试进程").is_none(),
            "测试内核未存活，无法验证孤儿清理"
        );
        let killed = kill_stale_cores(&dir);
        assert!(killed >= 1, "应清理至少 1 个遗留内核，实际 {killed}");
        // 被清理的进程应已退出（kill 后给系统一点回收时间）
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            child.try_wait().expect("查询测试进程").is_some(),
            "遗留内核未被真正杀掉"
        );
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profile_id_rejects_path_traversal() {
        // 合法 id（new_id 生成形如 p_1a2b3c4d）
        assert!(is_safe_profile_id("p_1a2b3c4d"));
        assert!(is_safe_profile_id("a-b_c9"));
        // 路径穿越/非法字符/空串全部拒绝
        for bad in ["", "..", "../x", "a/b", "C:\\Users\\x", "a b", "/etc/passwd", "p_1/../../x"] {
            assert!(!is_safe_profile_id(bad), "应拒绝: {bad}");
        }
    }

    #[test]
    fn mask_hides_credentials() {
        let masked = mask_secrets("GET https://ex.com/sub?token=abcd1234&x=1");
        assert!(masked.contains("token=••••"));
        assert!(!masked.contains("abcd1234"));
        assert!(masked.contains("x=1"));
        // 带前缀的键（access_token / apikey）同样要脱敏，且不能破坏键名
        let masked2 = mask_secrets("apikey=zzz&access_token=yyy&ok=1");
        assert!(masked2.contains("apikey=••••"), "{masked2}");
        assert!(masked2.contains("access_token=••••"), "{masked2}");
        assert!(!masked2.contains("zzz") && !masked2.contains("yyy"));
        assert!(masked2.contains("ok=1"));
    }

    #[test]
    fn network_fingerprint_has_separator() {
        let fp = network_fingerprint();
        assert!(fp.contains('|'));
    }

    /// 真实内核联调（默认忽略）：验证 spawn + API 客户端 + 规则/连接解析。
    ///
    /// 运行方式：
    /// `cargo test --lib real_core_api_smoke -- --ignored --nocapture`
    /// 需要本机存在 mihomo 二进制（数据目录 `bin/` 或 `MDGO_MIHOMO_BIN`）。
    #[tokio::test]
    #[ignore = "需要本机 mihomo 二进制"]
    async fn real_core_api_smoke() {
        let bin = std::env::var("MDGO_MIHOMO_BIN")
            .ok()
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_file())
            .or_else(|| {
                let name = if cfg!(windows) {
                    "mihomo-windows-x64.exe"
                } else {
                    "mihomo-darwin-arm64"
                };
                let p = paths::bin_dir().join(name);
                p.is_file().then_some(p)
            });
        let Some(bin) = bin else {
            eprintln!("跳过：未找到 mihomo 二进制");
            return;
        };

        let ports = Ports {
            mixed: 17890,
            controller: 19090,
        };
        let secret = "test-secret";
        let config = format!(
            "mixed-port: {}\nexternal-controller: 127.0.0.1:{}\nsecret: {}\nmode: rule\nlog-level: info\nproxies: []\nproxy-groups:\n  - {{name: TEST, type: select, proxies: [DIRECT]}}\nrules:\n  - MATCH,TEST\n",
            ports.mixed, ports.controller, secret
        );
        let dir = std::env::temp_dir().join("mdgo-vpn-it");
        let _ = std::fs::create_dir_all(&dir);
        let cfg_path = dir.join("config.yaml");
        std::fs::write(&cfg_path, config).expect("写测试配置");

        let supervisor = CoreSupervisor::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        supervisor
            .spawn(&bin, &cfg_path, &dir, dir.join("mihomo.log"), tx)
            .expect("spawn mihomo");

        let api = ApiClient::new(&ports, secret, Duration::from_secs(3));
        // 先跑完所有检查并收集结果，**无论成败都先停内核**再断言：
        // 否则 panicked 时 tokio 运行时会卡在子进程回收上（实测挂起 9 分钟）。
        let outcome: Result<(), String> = async {
            let mut version = String::new();
            let mut last_error = String::new();
            for attempt in 0..20 {
                match tokio::time::timeout(Duration::from_secs(5), api.version()).await {
                    Ok(Ok(v)) => {
                        version = v;
                        break;
                    }
                    Ok(Err(e)) => last_error = e,
                    Err(_) => last_error = "请求 5s 超时".to_string(),
                }
                eprintln!("[it] 第 {attempt} 次探测失败: {last_error}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            if !version.starts_with('v') {
                return Err(format!("内核未就绪，version={version:?}，最后错误: {last_error}"));
            }

            let groups = api.proxies().await?;
            if !groups
                .iter()
                .any(|g| g.name == "TEST" && g.nodes.iter().any(|n| n.name == "DIRECT"))
            {
                return Err(format!("未解析到 TEST 组 / DIRECT 节点: {groups:?}"));
            }

            let rules = api.rules().await?;
            if !rules
                .iter()
                .any(|r| r.is_final && r.kind.eq_ignore_ascii_case("MATCH"))
            {
                return Err(format!("未解析到 MATCH 兜底规则: {rules:?}"));
            }

            // 空连接列表在 mihomo 里返回 `connections: null`，这里顺带回归该场景
            let conns = api.connections().await?;
            if !conns.connections.is_empty() {
                return Err(format!("连接列表应为空: {:?}", conns.connections));
            }

            api.select("TEST", "DIRECT").await?;
            api.set_mode("global").await?;
            api.set_mode("rule").await?;
            Ok(())
        }
        .await;

        let stop_result = supervisor.stop().await;
        let pid_after = supervisor.pid();
        stop_result.expect("stop mihomo");
        assert!(pid_after.is_none());

        // 配置流水线：带 tun/listeners 的订阅应被转换后通过 `mihomo -t`
        let raw = "mixed-port: 7890\nexternal-controller: 127.0.0.1:9090\nsecret: x\ntun:\n  enable: true\n  stack: gvisor\nlisteners:\n  - {name: in, type: socks, port: 1080}\ndns:\n  enable: true\n  enhanced-mode: fake-ip\n  nameserver: [https://223.5.5.5/dns-query]\nproxies: []\nproxy-groups:\n  - {name: TEST, type: select, proxies: [DIRECT]}\nrules:\n  - MATCH,TEST\n";
        let settings = Settings::default();
        let built = config::build_runtime(&raw, &settings, &ports).expect("生成运行时配置");
        assert!(!built.contains("stack: gvisor"), "tun 应被移除");
        let candidate = dir.join("candidate.yaml");
        std::fs::write(&candidate, built).expect("写候选配置");
        let test_result = config::config_test(&bin, &candidate, &dir).await;
        assert!(
            matches!(test_result, Ok(None)),
            "mihomo -t 未通过: {test_result:?}"
        );

        assert!(outcome.is_ok(), "{}", outcome.err().unwrap_or_default());
    }
}

// 让 `Profile` 默认构造可用（订阅导入时字段较多）。
impl Default for Profile {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            kind: "file".into(),
            url: String::new(),
            path: String::new(),
            interval_hours: 0,
            updated_at: 0,
            nodes: 0,
            groups: 0,
            rules: 0,
            upload: 0,
            download: 0,
            total: 0,
            expire_at: 0,
            last_error: String::new(),
            updated_note: String::new(),
            version_count: 0,
            last_attempt_at: 0,
            fail_count: 0,
        }
    }
}


