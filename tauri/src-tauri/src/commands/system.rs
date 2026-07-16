use std::collections::HashSet;
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
/// 磁盘列表刷新间隔（采集周期数）。每 N 次采集才完整刷新一次磁盘列表及用量。
/// 磁盘用量变化缓慢，频繁 I/O 无意义。
const DISK_REFRESH_CYCLES: u32 = 5; // 每 ~15 秒刷新一次

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
pub struct ServiceMetrics {
    pub rss: u64,
    pub vms: u64,
    pub uptime: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsData {
    pub host_ip: String,
    pub cpu: CpuMetrics,
    pub memory: MemoryMetrics,
    pub network: NetworkMetrics,
    pub disk: DiskMetrics,
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

/// 采集一次系统指标（增量刷新，极低开销）
fn collect_metrics(
    sys: &mut System,
    disks: &Disks,
    process_pid: Option<sysinfo::Pid>,
    cached_boot_ts: u64,
    cached_host_ip: &str,
) -> SystemMetricsPayload {
    // CPU 使用率（sysinfo 0.39+ 使用简化 API，仅刷新使用率字段）
    sys.refresh_cpu_usage();
    // 内存 + Swap 信息
    sys.refresh_memory();
    // 磁盘数据由调用方按 TTL 刷新，此处仅读取
    // 当前进程自身的内存/时间信息
    if let Some(pid) = process_pid {
        let pids = [pid];
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&pids), false);
    }

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
    let mut disk_total: u64 = 0;
    let mut disk_used: u64 = 0;
    let mut disk_free: u64 = 0;
    let mut seen_devices = HashSet::new();
    for d in disks.list() {
        let base = extract_base_device_name(&d.name().to_string_lossy());
        if !seen_devices.insert(base) {
            continue; // 同一物理设备只计入一次
        }
        let total = d.total_space();
        let avail = d.available_space();
        let used = total.saturating_sub(avail);
        disk_total = disk_total.saturating_add(total);
        disk_used = disk_used.saturating_add(used);
        disk_free = disk_free.saturating_add(avail);
    }
    // 避免除零
    let disk_total_safe = if disk_total == 0 { 1 } else { disk_total };
    let disk_percent = (disk_used as f32 / disk_total_safe as f32) * 100.0;

    // 系统运行时长（使用缓存值，避免循环内重复系统调用）
    let uptime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .saturating_sub(cached_boot_ts);

    // 当前进程（mdgo Tauri 进程）自身内存
    let (service_rss, service_vms) = if let Some(pid) = process_pid {
        if let Some(process) = sys.process(pid) {
            (process.memory(), process.virtual_memory())
        } else {
            (0, 0)
        }
    } else {
        (0, 0)
    };

    SystemMetricsPayload {
        r#type: "metrics".to_string(),
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
            uptime,
            service: ServiceMetrics {
                rss: service_rss,
                vms: service_vms,
                uptime: 0, // 在循环中填充真实值
            },
        },
    }
}

// ============== Tauri 命令 ==============

/// 启动后台系统监控线程（每 3 秒采集一次，通过 Tauri Event 推送到前端）
#[tauri::command]
pub fn start_monitor(app: AppHandle, state: tauri::State<'_, SystemMonitorState>) {
    if state.running.load(Ordering::SeqCst) {
        return; // 已在运行
    }
    state.running.store(true, Ordering::SeqCst);

    let running = state.running.clone();
    let app_handle: AppHandle = app.clone();
    let process_start = state.process_start;

    // 缓存系统启动时间戳（启动后不会变），避免循环内重复系统调用
    let cached_boot_ts = System::boot_time();
    // 缓存本地 IP（启动后不会变）
    let cached_host_ip = get_local_ip();

    thread::spawn(move || {
        // catch_unwind 兜底：防止线程 panic 导致 running 永久锁死
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut sys = System::new();
            let mut disks = Disks::new();
            let mut networks = Networks::new_with_refreshed_list();
            let pid = sysinfo::Pid::from_u32(std::process::id());

            // === 预热阶段 ===
            // sysinfo 的 CPU 使用率需要两次采样才能算出差值，
            // 先在循环外做一次刷新建立基线，避免第一个 tick 显示 0%
            sys.refresh_cpu_usage();
            thread::sleep(CPU_WARMUP_DELAY);

            // 预热磁盘和进程信息
            disks.refresh(false);
            let pids = [pid];
            sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&pids), false);

            // 网络 IO 基线：记录累计收发字节数，后续循环算差值/时间间隔得到速率
            let mut net_prev_total_sent: u64 =
                networks.list().iter().map(|(_, n)| n.total_transmitted()).sum();
            let mut net_prev_total_recv: u64 =
                networks.list().iter().map(|(_, n)| n.total_received()).sum();
            let mut net_prev_time = Instant::now();

            // 磁盘 TTL 计数器：每 DISK_REFRESH_CYCLES 次才完整刷新一次磁盘
            let mut disk_refresh_cycle = 0u32;

            while running.load(Ordering::SeqCst) {
                // 磁盘：按 TTL 刷新，避免每次循环做 I/O
                if disk_refresh_cycle == 0 {
                    disks.refresh(false);
                }
                disk_refresh_cycle = (disk_refresh_cycle + 1) % DISK_REFRESH_CYCLES;

                let mut payload = collect_metrics(&mut sys, &disks, Some(pid), cached_boot_ts, &cached_host_ip);

                // 网络 IO 速率：累计值差值 / 时间间隔
                // 使用 refresh(true) 及时清理已移除的网络接口（VPN断开、USB网卡拔出等）
                networks.refresh(true);
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

                // 填充进程持续运行时长（秒）
                payload.data.service.uptime = process_start.elapsed().as_secs();

                if let Err(e) = app_handle.emit("system-metrics", &payload) {
                    eprintln!("[SystemMonitor] 发送事件失败: {e}");
                }

                // 分段休眠：支持更及时的退出响应
                for _ in 0..MONITOR_SLEEP_SEGMENTS {
                    if !running.load(Ordering::SeqCst) {
                        break;
                    }
                    thread::sleep(MONITOR_SLEEP_SEGMENT);
                }
            }
        }));

        if result.is_err() {
            eprintln!(
                "[SystemMonitor] 监控线程 panic 退出，已重置运行状态"
            );
            running.store(false, Ordering::SeqCst);
        }
    });
}

/// 停止后台系统监控线程（幂等安全）
#[tauri::command]
pub fn stop_monitor(state: tauri::State<'_, SystemMonitorState>) {
    if !state.running.load(Ordering::SeqCst) {
        return; // 幂等保护：未运行不执行任何操作
    }
    state.running.store(false, Ordering::SeqCst);
}
