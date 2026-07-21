use std::collections::{HashMap, HashSet};
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use serde::Serialize;
use sysinfo::{Disks, Networks, System};
use tauri::{AppHandle, Emitter};

// ============== 常量定义 ==============

/// 单个监控采集周期总时长
const MONITOR_INTERVAL: Duration = Duration::from_secs(3);
/// 采集周期内分段休眠的每段时长
const MONITOR_SLEEP_SEGMENT: Duration = Duration::from_millis(500);
/// 采集周期内分段休眠的段数
const MONITOR_SLEEP_SEGMENTS: u32 = MONITOR_INTERVAL.as_millis() as u32 / MONITOR_SLEEP_SEGMENT.as_millis() as u32;
/// CPU 预热等待时长（确保首次 CPU 使用率采样准确）
const CPU_WARMUP_DELAY: Duration = Duration::from_millis(200);
/// 网络速率计算的最小有效间隔（秒），低于此值跳过计算以防止时钟抖动
const NET_MIN_INTERVAL_SECS: f64 = 0.001;

// ============== 监控线程状态管理 ==============

/// 后台监控线程的控制状态
pub struct SystemMonitorState {
    pub running: Arc<AtomicBool>,
    /// 进程启动时间戳（在 Tauri app 初始化时记录）
    pub process_start: Instant,
}

impl SystemMonitorState {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            process_start: Instant::now(),
        }
    }
}

// ============== 指标数据结构（与前端 _handleWsMetrics 接收格式对齐） ==============

#[derive(Debug, Clone, Serialize)]
pub struct CpuMetrics {
    pub percent: f32,
    pub core_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryMetrics {
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub percent: f32,
    /// 总 Swap 大小（字节）
    pub swap_total: u64,
    /// 已用 Swap 大小（字节）
    pub swap_used: u64,
    /// Swap 使用率（百分比）
    pub swap_percent: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkMetrics {
    pub sent_rate: f64,
    pub recv_rate: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskMetrics {
    pub total: u64,
    pub used: u64,
    pub free: u64,
    pub percent: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskIOMetrics {
    pub read_rate: f64,
    pub write_rate: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceMetrics {
    pub rss: u64,
    pub vms: u64,
    pub uptime: u64,
    /// 当前应用所有进程（Rust + WebView）CPU 总使用率（0~100*核数）
    pub app_cpu_percent: f32,
    /// 当前应用所有进程（Rust + WebView）内存占用率（0~100%）
    pub app_mem_percent: f32,
    /// 当前应用所有进程（Rust + WebView）内存占用绝对值（字节）
    pub app_mem_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsData {
    pub host_ip: String,
    pub cpu: CpuMetrics,
    pub memory: MemoryMetrics,
    pub network: NetworkMetrics,
    pub disk: DiskMetrics,
    pub disk_io: DiskIOMetrics,
    pub uptime: u64,
    pub service: ServiceMetrics,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemMetricsPayload {
    pub r#type: String,
    pub data: MetricsData,
}

// ============== 工具函数 ==============

/// 获取本机非回环 IPv4 地址。
/// 通过 UDP 连接到外部 IP（无需真正可达），获取本机出口 IP。
fn get_local_ip() -> String {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return "127.0.0.1".to_string(),
    };
    match socket.connect("10.254.254.254:1") {
        Ok(()) => socket.local_addr().map(|a| a.ip().to_string()).unwrap_or_else(|_| "127.0.0.1".to_string()),
        Err(_) => "127.0.0.1".to_string(),
    }
}

/// 提取磁盘基础设备名称（去除分区编号），用于物理设备去重。
///
/// 处理常见命名模式：
/// - macOS APFS: `disk3s1` / `disk3s5` → `disk3`
/// - Linux NVMe: `/dev/nvme0n1p1` → `/dev/nvme0n1`
/// - Linux SCSI: `/dev/sda1` → `/dev/sda`
fn extract_base_device_name(name: &str) -> String {
    // 先尝试匹配 Linux NVMe 风格（例如 /dev/nvme0n1p1 → /dev/nvme0n1）
    if let Some(idx) = name.rfind(|c: char| c == 'p') {
        let prefix = &name[..idx];
        // 确认 'p' 之后全是数字（分区号）
        let rest = &name[idx + 1..];
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return prefix.to_string();
        }
    }

    // macOS APFS / 通用风格：去掉末尾的 's<N>' 分区后缀（如 disk3s5 → disk3）
    let stripped = name
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_end_matches(|c: char| c == 's');

    // 如果去掉 's' 后有新东西说明匹配到了 APFS 模式
    if stripped != name {
        return stripped.trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
    }

    // Linux SCSI/virtio 风格：去掉末尾数字（如 /dev/sda1 → /dev/sda）
    let stripped_linux = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if stripped_linux != name {
        return stripped_linux.to_string();
    }

    name.to_string()
}

/// 计算网络 IO 速率，兼容 macOS 32-bit 计数器回绕。
///
/// macOS 上 `sysinfo` 通过 `NET_RT_IFLIST2` 获取网络计数器，返回值实际为 32-bit。
/// 当接口累计收发超过 4GB 时计数器会回绕到 0。此函数通过检测 `total < prev`
/// 来识别回绕并正确修正差值。
fn compute_network_rates(
    total_sent: u64,
    prev_total_sent: u64,
    total_recv: u64,
    prev_total_recv: u64,
    elapsed: f64,
) -> (f64, f64) {
    if elapsed <= NET_MIN_INTERVAL_SECS {
        return (0.0, 0.0);
    }

    let sent_bytes = if total_sent >= prev_total_sent {
        total_sent - prev_total_sent
    } else {
        // 计数器回绕：从 prev_total_sent 到 u64::MAX，再到 total_sent
        (u64::MAX - prev_total_sent)
            .saturating_add(total_sent)
            .saturating_add(1)
    };

    let recv_bytes = if total_recv >= prev_total_recv {
        total_recv - prev_total_recv
    } else {
        (u64::MAX - prev_total_recv)
            .saturating_add(total_recv)
            .saturating_add(1)
    };

    (sent_bytes as f64 / elapsed, recv_bytes as f64 / elapsed)
}

// ============== 核心采集函数 ==============

/// 全量扫描进程树，找出 root_pid 的所有后代 PID。
/// 同时刷新所有进程的 CPU 数据，返回完整的应用 PID 列表（含 root）。
/// 使用 HashSet 去重，防止边界情况下的重复 PID。
fn find_app_pids(sys: &mut System, root_pid: sysinfo::Pid) -> Vec<sysinfo::Pid> {
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let mut children_of: HashMap<sysinfo::Pid, Vec<sysinfo::Pid>> = HashMap::new();
    for (pid, process) in sys.processes() {
        if let Some(parent) = process.parent() {
            children_of.entry(parent).or_default().push(*pid);
        }
    }

    let mut seen = HashSet::new();
    let mut result = vec![root_pid];
    seen.insert(root_pid);
    let mut stack: Vec<sysinfo::Pid> = children_of.remove(&root_pid).unwrap_or_default();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        result.push(pid);
        if let Some(grandchildren) = children_of.remove(&pid) {
            stack.extend(grandchildren);
        }
    }
    result
}

/// 从已知 PID 列表汇总 mdgo 应用所有进程的 CPU 和内存。
/// 不依赖 `sys.processes()` 全量遍历，效率 O(k) 其中 k = 应用进程数（通常 ≤5）。
///
/// # CPU 归一化说明
/// `sysinfo::Process::cpu_usage()` 返回的值是以**单核为基准**的百分比。
/// 即一个使用 2% 总系统容量的进程（如 Windows 任务管理器显示），在 20 核机器上
/// 此 API 会返回 ~40%。因此需要除以核心数，归一化为系统总负载百分比，
/// 与任务管理器显示一致。
fn sum_app_resources_by_pids(
    sys: &System,
    pids: &[sysinfo::Pid],
    mem_total: u64,
) -> (f32, u64, f32) {
    let mut total_cpu = 0.0f32;
    let mut total_rss = 0u64;
    let cpu_count = sys.cpus().len() as f32;

    for pid in pids {
        if let Some(process) = sys.process(*pid) {
            total_cpu += process.cpu_usage();
            total_rss += process.memory();
        }
    }

    // sysinfo::Process::cpu_usage() 返回单核基准百分比
    // 除以核心数得到系统总负载百分比（0~100%）
    let normalized_cpu = if cpu_count > 0.0 {
        total_cpu / cpu_count
    } else {
        total_cpu
    };

    let mem_pct = if mem_total > 0 {
        ((total_rss as f32 / mem_total as f32) * 100.0 * 10.0).round() / 10.0
    } else {
        0.0
    };

    ((normalized_cpu * 10.0).round() / 10.0, total_rss, mem_pct)
}

/// 采集一次系统指标。
///
/// # 说明
/// - 进程数据由调用方提前刷新（`sys.refresh_processes`），此函数仅做读取。
/// - `app_pids` 为已知的应用进程 PID 列表（含主进程），由 `find_app_pids` 或缓存提供。
/// - `r#type` 和 `host_ip` 由调用方缓存传入，避免每周期重复分配。
fn collect_metrics(
    sys: &mut System,
    disks: &Disks,
    app_pids: &[sysinfo::Pid],
    cached_host_ip: &str,
    cached_type: &str,
    seen_devices: &mut HashSet<String>,
) -> SystemMetricsPayload {
    sys.refresh_cpu_usage();
    sys.refresh_memory();

    let cpu_percent = sys.global_cpu_usage();
    let core_count = sys.cpus().len();

    let mem_total = sys.total_memory();
    let mem_used = sys.used_memory();
    let mem_available = sys.available_memory();
    let mem_percent = if mem_total > 0 {
        (mem_used as f32 / mem_total as f32) * 100.0
    } else {
        0.0
    };

    let swap_total = sys.total_swap();
    let swap_used = sys.used_swap();
    let swap_percent = if swap_total > 0 {
        (swap_used as f32 / swap_total as f32) * 100.0
    } else {
        0.0
    };

    // 汇总所有磁盘用量（按物理设备去重，防止 APFS/LVM 虚报容量）
    seen_devices.clear();
    let mut disk_total: u64 = 0;
    let mut disk_used: u64 = 0;
    let mut disk_free: u64 = 0;
    for d in disks.list() {
        let base = extract_base_device_name(&d.name().to_string_lossy());
        if !seen_devices.insert(base) {
            continue;
        }
        let total = d.total_space();
        let avail = d.available_space();
        let used = total.saturating_sub(avail);
        disk_total = disk_total.saturating_add(total);
        disk_used = disk_used.saturating_add(used);
        disk_free = disk_free.saturating_add(avail);
    }
    let disk_total_safe = if disk_total == 0 { 1 } else { disk_total };
    let disk_percent = (disk_used as f32 / disk_total_safe as f32) * 100.0;

    // 系统运行时长：用 wall clock 与 boot time 差值。
    // sys.boot_time() 由 sysinfo 内部缓存，调用开销极低。
    let uptime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .saturating_sub(sysinfo::System::boot_time());

    // 应用资源：所有进程累计 CPU/内存（利用调用方已刷新的数据）
    let (app_cpu_percent, app_mem_bytes, app_mem_percent) =
        sum_app_resources_by_pids(sys, app_pids, mem_total);

    // 主进程自身 RSS/VMS（兼容旧字段）
    let root_pid = app_pids.first().copied();
    let (service_rss, service_vms) = if let Some(pid) = root_pid {
        if let Some(process) = sys.process(pid) {
            (process.memory(), process.virtual_memory())
        } else {
            (0, 0)
        }
    } else {
        (0, 0)
    };

    SystemMetricsPayload {
        r#type: cached_type.to_string(),
        data: MetricsData {
            host_ip: cached_host_ip.to_string(),
            cpu: CpuMetrics {
                percent: (cpu_percent * 10.0).round() / 10.0,
                core_count,
            },
            memory: MemoryMetrics {
                total: mem_total,
                used: mem_used,
                available: mem_available,
                percent: (mem_percent * 10.0).round() / 10.0,
                swap_total,
                swap_used,
                swap_percent: (swap_percent * 10.0).round() / 10.0,
            },
            network: NetworkMetrics {
                sent_rate: 0.0,
                recv_rate: 0.0,
            },
            disk: DiskMetrics {
                total: disk_total,
                used: disk_used,
                free: disk_free,
                percent: (disk_percent * 10.0).round() / 10.0,
            },
            disk_io: DiskIOMetrics {
                read_rate: 0.0,
                write_rate: 0.0,
            },
            uptime,
            service: ServiceMetrics {
                rss: service_rss,
                vms: service_vms,
                uptime: 0,
                app_cpu_percent,
                app_mem_percent,
                app_mem_bytes,
            },
        },
    }
}

// ============== Tauri 命令 ==============

/// 全量 PID 重扫间隔（采集周期数）。每 30 次（~90 秒）重新扫描一次进程树，
/// 以捕获新生的 WebView2 子进程。
const FULL_REFRESH_INTERVAL: u32 = 30;

/// 网络接口列表重扫间隔（采集周期数）。每 6 次（~18 秒）重新检测网络接口变化。
const NET_REFRESH_LIST_INTERVAL: u32 = 6;

/// 启动后台系统监控线程（每 3 秒采集一次，通过 Tauri Event 推送到前端）。
///
/// 返回 true 表示成功启动，false 表示监控已在运行中。
///
/// # 性能
/// - 进程刷新使用选择性 PID 刷新（仅刷新已知的应用进程，通常 ≤5 个），
///   避免每 3 秒全量扫描 ~200-400 个系统进程。
/// - 每 `FULL_REFRESH_INTERVAL`（~90 秒）做一次全量扫描以发现新生进程。
/// - 固定字符串（`r#type`、`host_ip`）在线程启动时缓存，避免每周期堆分配。
#[tauri::command]
pub fn start_monitor(app: AppHandle, state: tauri::State<'_, SystemMonitorState>) -> bool {
    // 原子 compare-and-swap：避免 load-then-store 竞态条件
    if state
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return false;
    }

    let running = state.running.clone();
    let app_handle: AppHandle = app.clone();
    let process_start = state.process_start;

    // 缓存启动后不会变化的系统常量（boot_time 改用 sys.boot_time() 每周期读取，无需缓存）
    let cached_host_ip = get_local_ip();
    let cached_type = "metrics".to_string();

    // 为线程命名以便调试 / crash dump
    std::thread::Builder::new()
        .name("system-monitor".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut sys = System::new();
                let mut disks = Disks::new();
                let mut networks = Networks::new_with_refreshed_list();
                let pid = sysinfo::Pid::from_u32(std::process::id());

                // === 预热阶段 ===
                sys.refresh_cpu_usage();
                thread::sleep(CPU_WARMUP_DELAY);
                disks.refresh(false);

                // 初始全量扫描，建立应用 PID 列表（主进程 + WebView 子进程）
                let mut app_pids = find_app_pids(&mut sys, pid);
                // 计数器从 1 开始，使首次循环跳过重扫
                let mut full_refresh_counter = 1u32;
                let mut net_refresh_counter = 1u32;

                // 磁盘去重缓存（复用避免每周期分配）
                let mut seen_devices = HashSet::new();

                // 网络 IO 基线
                let mut net_prev_total_sent: u64 =
                    networks.list().iter().map(|(_, n)| n.total_transmitted()).sum();
                let mut net_prev_total_recv: u64 =
                    networks.list().iter().map(|(_, n)| n.total_received()).sum();
                let mut net_prev_time = Instant::now();

                // 磁盘 IO 基线
                let mut disk_prev_read_bytes: u64 =
                    disks.list().iter().map(|d| d.usage().read_bytes).sum();
                let mut disk_prev_write_bytes: u64 =
                    disks.list().iter().map(|d| d.usage().written_bytes).sum();
                let mut disk_prev_time = Instant::now();

                // 事件背压：上次发送失败时跳过本次，给前端处理时间
                let mut last_emit_ok = true;

                while running.load(Ordering::SeqCst) {
                    disks.refresh(false);

                    // ---------- 进程刷新：选择性刷新 vs 全量扫描 ----------
                    if full_refresh_counter == 0 {
                        // 每 ~90 秒全量扫描一次，更新 PID 列表
                        app_pids = find_app_pids(&mut sys, pid);
                    } else {
                        // 常规周期：仅刷新已知应用进程（通常 ≤5 个 PID）
                        sys.refresh_processes(
                            sysinfo::ProcessesToUpdate::Some(&app_pids),
                            true,
                        );
                    }
                    full_refresh_counter = (full_refresh_counter + 1) % FULL_REFRESH_INTERVAL;

                    let mut payload = collect_metrics(
                        &mut sys,
                        &disks,
                        &app_pids,
                        &cached_host_ip,
                        &cached_type,
                        &mut seen_devices,
                    );

                    // ---------- 网络 IO 速率 ----------
                    // 大部分周期使用 refresh(false) 仅刷新计数器
                    // 每 NET_REFRESH_LIST_INTERVAL 次 refresh(true) 检测接口变化
                    if net_refresh_counter == 0 {
                        networks.refresh(true);
                    } else {
                        networks.refresh(false);
                    }
                    net_refresh_counter = (net_refresh_counter + 1) % NET_REFRESH_LIST_INTERVAL;

                    let net_total_sent: u64 =
                        networks.list().iter().map(|(_, n)| n.total_transmitted()).sum();
                    let net_total_recv: u64 =
                        networks.list().iter().map(|(_, n)| n.total_received()).sum();
                    let net_elapsed = net_prev_time.elapsed().as_secs_f64();

                    let (sent_rate, recv_rate) = compute_network_rates(
                        net_total_sent, net_prev_total_sent,
                        net_total_recv, net_prev_total_recv,
                        net_elapsed,
                    );
                    payload.data.network.sent_rate = sent_rate;
                    payload.data.network.recv_rate = recv_rate;

                    net_prev_total_sent = net_total_sent;
                    net_prev_total_recv = net_total_recv;
                    net_prev_time = Instant::now();

                    // ---------- 磁盘 IO 速率 ----------
                    let disk_read_bytes: u64 =
                        disks.list().iter().map(|d| d.usage().read_bytes).sum();
                    let disk_write_bytes: u64 =
                        disks.list().iter().map(|d| d.usage().written_bytes).sum();
                    let disk_elapsed = disk_prev_time.elapsed().as_secs_f64();
                    if disk_elapsed > NET_MIN_INTERVAL_SECS {
                        payload.data.disk_io.read_rate =
                            (disk_read_bytes.saturating_sub(disk_prev_read_bytes)) as f64 / disk_elapsed;
                        payload.data.disk_io.write_rate =
                            (disk_write_bytes.saturating_sub(disk_prev_write_bytes)) as f64 / disk_elapsed;
                    }
                    disk_prev_read_bytes = disk_read_bytes;
                    disk_prev_write_bytes = disk_write_bytes;
                    disk_prev_time = Instant::now();

                    // 进程持续运行时长（使用单调时钟，不受系统时钟跳变影响）
                    payload.data.service.uptime = process_start.elapsed().as_secs();

                    // ---------- 事件发射：背压保护 ----------
                    // 上次发射失败时跳过本次，给前端处理时间
                    if last_emit_ok {
                        if app_handle.emit("system-metrics", &payload).is_err() {
                            last_emit_ok = false;
                        }
                    } else {
                        // 仅重试一次：如果再次失败保持 false，下次不再跳过
                        if app_handle.emit("system-metrics", &payload).is_ok() {
                            last_emit_ok = true;
                        }
                    }

                    // 分段休眠
                    for _ in 0..MONITOR_SLEEP_SEGMENTS {
                        if !running.load(Ordering::SeqCst) {
                            break;
                        }
                        thread::sleep(MONITOR_SLEEP_SEGMENT);
                    }
                }
            }));

            if result.is_err() {
                // panic 时静默重置运行状态，不输出日志（生产环境无日志依赖）
                running.store(false, Ordering::SeqCst);
            }
        })
        .expect("system-monitor 线程创建失败");

    true
}

/// 停止后台系统监控线程（幂等安全）
#[tauri::command]
pub fn stop_monitor(state: tauri::State<'_, SystemMonitorState>) {
    if !state.running.load(Ordering::SeqCst) {
        return; // 幂等保护：未运行不执行任何操作
    }
    state.running.store(false, Ordering::SeqCst);
}
