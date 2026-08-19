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
pub struct GpuMetrics {
    /// GPU 名称（如 Apple M2 Pro / NVIDIA GeForce RTX 3060）
    pub name: String,
    /// GPU 厂商（如 Apple / NVIDIA / AMD / Intel）
    pub vendor: String,
    /// 显存总量（字节）；共享内存或不可得时为 0
    pub vram: u64,
    /// 已用显存（字节）；不可得时为 null
    pub mem_used: Option<u64>,
    /// GPU 使用率（0~100）；平台不支持时为 -1
    pub usage: f32,
}

/// 系统静态信息（OS / 架构 / 主机名 / 内核 / CPU / GPU），采集一次后基本不变。
#[derive(Debug, Clone, Serialize)]
pub struct SystemInfoMetrics {
    /// 操作系统显示名（如 "macOS 14.5" / "Windows 11 Pro"）
    pub os: String,
    /// CPU 架构（如 aarch64 / x86_64）
    pub arch: String,
    /// 主机名（如 MacBook-Pro）
    pub host_name: String,
    /// 内核版本（如 23.5.0）
    pub kernel: String,
    /// CPU 型号（如 Apple M2 Pro）
    pub cpu_brand: String,
    /// CPU 厂商（如 Apple / GenuineIntel）
    pub cpu_vendor: String,
    /// 物理核心数（部分平台不可得为 None）
    pub cpu_physical_cores: Option<usize>,
    /// 逻辑核心数
    pub cpu_logical_cores: usize,
    /// GPU 列表；无 GPU 时为空数组（前端据此隐藏 GPU 区块）
    pub gpus: Vec<GpuMetrics>,
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
    pub system: SystemInfoMetrics,
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

// ============== 系统静态信息 + GPU 采集 ==============

/// 操作系统显示名（如 "macOS 14.5" / "Windows 11 Pro" / "Ubuntu 24.04"）。
fn os_label() -> String {
    #[cfg(target_os = "macos")]
    {
        let v = System::os_version().unwrap_or_default();
        if v.is_empty() { "macOS".to_string() } else { format!("macOS {v}") }
    }
    #[cfg(target_os = "windows")]
    {
        System::long_os_version().unwrap_or_else(|| "Windows".to_string())
    }
    #[cfg(target_os = "linux")]
    {
        let name = System::name().unwrap_or_else(|| "Linux".to_string());
        let v = System::os_version().unwrap_or_default();
        if v.is_empty() { name } else { format!("{name} {v}") }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        System::name().unwrap_or_else(|| std::env::consts::OS.to_string())
    }
}

/// 采集系统静态信息（OS / 架构 / 主机名 / 内核 / CPU / GPU）。
///
/// 调用前需确保 `sys.refresh_cpu_all()` 已被调用（填充 CPU brand/vendor）。
fn collect_system_info(sys: &System) -> SystemInfoMetrics {
    let cpu0 = sys.cpus().first();
    // macOS 的 sysinfo cpu_arch() 返回 "arm64"，统一归一化为 "aarch64" 展示
    let arch = System::cpu_arch();
    let arch = if arch == "arm64" { "aarch64".to_string() } else { arch };
    SystemInfoMetrics {
        os: os_label(),
        arch,
        host_name: System::host_name().unwrap_or_default(),
        kernel: System::kernel_version().unwrap_or_default(),
        cpu_brand: cpu0.map(|c| c.brand().to_string()).unwrap_or_default(),
        cpu_vendor: cpu0.map(|c| c.vendor_id().to_string()).unwrap_or_default(),
        cpu_physical_cores: System::physical_core_count(),
        cpu_logical_cores: sys.cpus().len(),
        gpus: collect_gpus_static(),
    }
}

/// 解析 "0 MB" / "8 GB" / "1.5 GB" 形式的显存字符串为字节数。
#[cfg(target_os = "macos")]
fn parse_vram_str(s: &str) -> u64 {
    let mut parts = s.trim().split_whitespace();
    let num: f64 = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0.0);
    let mult = match parts.next().unwrap_or("").to_uppercase().as_str() {
        "KB" => 1024u64,
        "MB" => 1024u64.pow(2),
        "GB" => 1024u64.pow(3),
        "TB" => 1024u64.pow(4),
        _ => 0,
    };
    (num * mult as f64) as u64
}

/// Windows：为子进程 Command 设置 `CREATE_NO_WINDOW`，避免从 GUI（Tauri）进程 spawn
/// 控制台程序（powershell / nvidia-smi 等）时闪现黑色控制台窗口。
/// 非 Windows 平台为空操作（macOS/Linux 无此问题）。
#[cfg(target_os = "windows")]
fn apply_no_window(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW = 0x08000000：新进程不创建控制台窗口
    cmd.creation_flags(0x0800_0000);
}
#[cfg(not(target_os = "windows"))]
#[allow(dead_code)] // macOS 的 system_profiler/ioreg 无需该标志，此变体仅在 Linux 被 nvidia-smi 调用
fn apply_no_window(_cmd: &mut std::process::Command) {}

/// macOS：`system_profiler SPDisplaysDataType -json` 获取 GPU 名称/厂商/显存。
#[cfg(target_os = "macos")]
fn macos_gpus_static() -> Vec<GpuMetrics> {
    let Ok(out) = std::process::Command::new("system_profiler")
        .args(["SPDisplaysDataType", "-json"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&out.stdout))
    else {
        return Vec::new();
    };
    let Some(arr) = json.get("SPDisplaysDataType").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|g| {
            let obj = g.as_object()?;
            let name = obj
                .get("_name")
                .or_else(|| obj.get("sppci_model"))
                .or_else(|| obj.get("model"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if name.is_empty() {
                return None;
            }
            let vendor = obj
                .get("spdisplays_vendor")
                .or_else(|| obj.get("sppci_vendor"))
                .or_else(|| obj.get("vendor"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let vram = obj
                .get("spdisplays_vram")
                .and_then(|v| v.as_str())
                .map(parse_vram_str)
                .unwrap_or(0);
            Some(GpuMetrics { name, vendor, vram, mem_used: None, usage: -1.0 })
        })
        .collect()
}

/// macOS：`ioreg -c IOAccelerator -r -l` 读取 PerformanceStatistics 中的
/// "Device Utilization %"（Apple Silicon / 部分 Intel Mac 可用，无需 sudo）。
/// 返回按加速器顺序的使用率列表；读取失败返回空 Vec。
#[cfg(target_os = "macos")]
fn macos_gpu_usages() -> Vec<f32> {
    static GPU_UTIL_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"Device Utilization %</key>\s*<(?:integer|real)>([0-9.]+)"#).unwrap()
    });
    let Ok(out) = std::process::Command::new("ioreg")
        .args(["-c", "IOAccelerator", "-r", "-l"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let usages: Vec<f32> = GPU_UTIL_RE
        .captures_iter(&text)
        .filter_map(|cap| cap[1].parse::<f32>().ok().map(|v| v.clamp(0.0, 100.0)))
        .collect();
    if usages.is_empty() { vec![-1.0] } else { usages }
}

/// Windows：从注册表读取 GPU 静态信息（名称/显存/厂商），进程内完成，替代 PowerShell 子进程。
///
/// 注册表位置：`HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-...}\000X`
/// - `DriverDesc` → 名称
/// - `HardwareInformation.qwMemorySize` → 显存字节（QWORD，**精确**；WMI AdapterRAM 为 32 位上限，
///   8GB+ 显卡会被截断为 ~4GB）
/// - `MatchingDeviceId` → PCI 设备 ID（`VEN_xxxx` → 厂商）
#[cfg(target_os = "windows")]
fn windows_gpus_static() -> Vec<GpuMetrics> {
    use winreg::enums::*;
    use winreg::RegKey;
    const CLASS_KEY: &str =
        r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
    let mut gpus = Vec::new();
    let Ok(class) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(CLASS_KEY) else {
        return gpus;
    };
    let mut keys: Vec<String> = class.enum_keys().flatten().collect();
    keys.sort(); // 0000..0009 顺序稳定（与 nvidia-smi 回退索引对齐）
    for sub in keys {
        let Ok(sk) = class.open_subkey(&sub) else { continue };
        let Ok(name) = sk.get_value::<String, _>("DriverDesc") else { continue };
        let name = name.trim().to_string();
        // 过滤无意义的基础显示适配器
        if name.is_empty() || name.eq_ignore_ascii_case("Microsoft Basic Display Adapter") {
            continue;
        }
        let vram = sk.get_value::<u64, _>("HardwareInformation.qwMemorySize").unwrap_or(0);
        let vendor = sk
            .get_value::<String, _>("MatchingDeviceId")
            .ok()
            .map(|id| {
                let upper = id.to_uppercase();
                upper
                    .split("VEN_")
                    .nth(1)
                    .and_then(|s| s.split('&').next())
                    .map(windows_vendor_name)
                    .unwrap_or_else(|| "Unknown".to_string())
            })
            .unwrap_or_default();
        gpus.push(GpuMetrics { name, vendor, vram, mem_used: None, usage: -1.0 });
    }
    gpus
}

/// Windows PCI Vendor ID → 厂商名。
#[cfg(target_os = "windows")]
fn windows_vendor_name(ven: &str) -> String {
    match ven {
        "10DE" => "NVIDIA",
        "1002" => "AMD",
        "8086" => "Intel",
        "13B5" => "ARM",
        "1414" => "Microsoft",
        "102B" => "Matrox",
        "15AD" => "VMware",
        "1AF4" => "Red Hat",
        "1234" => "QEMU",
        _ => "Unknown",
    }
    .to_string()
}

/// Linux：`lspci -mm` 解析 VGA/3D/Display 控制器作为 GPU 列表。
#[cfg(target_os = "linux")]
fn linux_gpus_static() -> Vec<GpuMetrics> {
    let Ok(out) = std::process::Command::new("lspci").arg("-mm").output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut gpus = Vec::new();
    for line in text.lines() {
        if !(line.contains("VGA compatible controller")
            || line.contains("3D controller")
            || line.contains("Display controller"))
        {
            continue;
        }
        // lspci -mm 输出：slot "class" "vendor" "device" "svendor" "sdevice" "rev" ...
        let parts: Vec<&str> = line.split('"').collect();
        let vendor = parts
            .get(3)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        let device = parts
            .get(5)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        if device.is_empty() && vendor.is_empty() {
            continue;
        }
        gpus.push(GpuMetrics {
            name: if device.is_empty() { vendor.clone() } else { device },
            vendor,
            vram: 0,
            mem_used: None,
            usage: -1.0,
        });
    }

    // NVIDIA 显卡：用 nvidia-smi 覆盖名称/显存/已用显存（lspci 无法给出显存总量）
    if let Some(nv) = nvidia_smi_gpus() {
        for (i, g) in gpus.iter_mut().enumerate() {
            if let Some((name, mem_used, vram)) = nv.get(i) {
                g.name = name.clone();
                g.vram = *vram;
                if *mem_used > 0 {
                    g.mem_used = Some(*mem_used);
                }
            }
        }
    }
    gpus
}

/// NVIDIA GPU 静态信息（`nvidia-smi` 提供：名称、已用显存、显存总量，单位 MiB 换算为字节）。
///
/// 仅 Linux 使用：Windows 已改用注册表（`HardwareInformation.qwMemorySize`）获取名称/显存，
/// 精度更高且无需子进程；Linux `lspci` 无法可靠给出显存。nvidia-smi 不可用时返回 None。
#[cfg(target_os = "linux")]
fn nvidia_smi_gpus() -> Option<Vec<(String, u64, u64)>> {
    let mut cmd = std::process::Command::new("nvidia-smi");
    apply_no_window(&mut cmd);
    let Ok(out) = cmd
        .args(["--query-gpu=name,memory.used,memory.total", "--format=csv,noheader,nounits"])
        .output()
    else {
        return None;
    };
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let list: Vec<(String, u64, u64)> = text
        .lines()
        .filter_map(|l| {
            let mut it = l.split(',');
            let name = it.next()?.trim().to_string();
            let used_mi: u64 = it.next()?.trim().parse().ok()?;
            let total_mi: u64 = it.next()?.trim().parse().ok()?;
            Some((name, used_mi.saturating_mul(1024 * 1024), total_mi.saturating_mul(1024 * 1024)))
        })
        .collect();
    if list.is_empty() { None } else { Some(list) }
}

/// NVIDIA GPU 实时数据（使用率 + 已用显存，每周期刷新；不可用时返回空 Vec）。
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn nvidia_smi_live() -> Vec<(f32, u64)> {
    let mut cmd = std::process::Command::new("nvidia-smi");
    apply_no_window(&mut cmd);
    let Ok(out) = cmd
        .args(["--query-gpu=utilization.gpu,memory.used", "--format=csv,noheader,nounits"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .filter_map(|l| {
            let mut it = l.split(',');
            let u: f32 = it.next()?.trim().parse().ok()?;
            let m: u64 = it.next()?.trim().parse().ok()?;
            Some((u.clamp(0.0, 100.0), m.saturating_mul(1024 * 1024)))
        })
        .collect()
}

/// 采集 GPU 静态信息（名称/厂商/显存），按平台分发。
#[cfg(target_os = "macos")]
fn collect_gpus_static() -> Vec<GpuMetrics> {
    macos_gpus_static()
}
#[cfg(target_os = "windows")]
fn collect_gpus_static() -> Vec<GpuMetrics> {
    windows_gpus_static()
}
#[cfg(target_os = "linux")]
fn collect_gpus_static() -> Vec<GpuMetrics> {
    linux_gpus_static()
}
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn collect_gpus_static() -> Vec<GpuMetrics> {
    Vec::new()
}

/// 每采集周期刷新 GPU 使用率与已用显存（就地写入 `gpus[i]`）。
///
/// Windows 不走此函数：Windows 使用 GPU Engine 性能计数器（与任务管理器同源，
/// 见 `spawn_gpu_collector`），避免与 nvidia-smi 的"内核占用"语义不一致。
#[cfg(not(target_os = "windows"))]
fn update_gpu_usages(gpus: &mut [GpuMetrics]) {
    if gpus.is_empty() {
        return;
    }
    #[cfg(target_os = "macos")]
    let live: Vec<(f32, Option<u64>)> = macos_gpu_usages().into_iter().map(|u| (u, None)).collect();
    #[cfg(target_os = "linux")]
    let live: Vec<(f32, Option<u64>)> = nvidia_smi_live().into_iter().map(|(u, m)| (u, Some(m))).collect();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let live: Vec<(f32, Option<u64>)> = Vec::new();

    if live.is_empty() {
        for g in gpus.iter_mut() {
            g.usage = -1.0;
        }
        return;
    }
    for (i, g) in gpus.iter_mut().enumerate() {
        if let Some((u, m)) = live.get(i) {
            g.usage = *u;
            if let Some(mem) = m {
                if *mem > 0 {
                    g.mem_used = Some(*mem);
                }
            }
        } else {
            g.usage = -1.0;
        }
    }
}

// ============== Windows：GPU Engine 计数器（任务管理器同源） ==============

/// Windows GPU 实时采样结果（按 GPU 索引对齐，与 `gpus` 列表顺序一致）。
#[cfg(target_os = "windows")]
#[derive(Default)]
struct GpuLiveSample {
    /// 各 GPU 使用率（任务管理器语义：该适配器全部引擎利用率求和，封顶 100）
    usages: Vec<f32>,
    /// 各 GPU 已用显存（字节）
    mem_used: Vec<u64>,
}

/// 从 PDH 实例名中提取 LUID，如 "pid_123_luid_0x00000000_0x000171C9_phys_0_eng_0_engtype_3D"
/// → "luid_0x00000000_0x000171C9"
#[cfg(target_os = "windows")]
fn extract_luid(name: &str) -> Option<String> {
    let parts: Vec<&str> = name.split('_').collect();
    let idx = parts.iter().position(|p| p.to_uppercase().starts_with("LUID"))?;
    let mut tok = parts[idx].to_string();
    for p in parts.iter().skip(idx + 1).take(2) {
        tok.push('_');
        tok.push_str(p);
    }
    Some(tok)
}

/// PDH：进程内读取一批通配计数器（路径列表）的全部实例值。
/// 同一查询内两次采样（间隔约 1 秒），使 fraction/rate 型计数器（GPU Engine 使用率）可计算。
/// 返回与 `paths` 一一对应的实例列表 `Vec<(实例名, 值)>`；单个计数器失败时对应项为空 Vec。
#[cfg(target_os = "windows")]
fn pdh_read_all(paths: &[&str]) -> Vec<Vec<(String, f64)>> {
    use std::ptr;
    use std::thread;
    use std::time::Duration;
    use windows_sys::Win32::System::Performance::*;
    unsafe {
        let empty = || vec![Vec::new(); paths.len()];
        let mut query: PDH_HQUERY = ptr::null_mut();
        if PdhOpenQueryW(ptr::null(), 0, &mut query) != 0 {
            return empty();
        }
        let mut counters: Vec<PDH_HCOUNTER> = Vec::with_capacity(paths.len());
        for p in paths {
            let wide_path: Vec<u16> = p.encode_utf16().chain(std::iter::once(0)).collect();
            let mut counter: PDH_HCOUNTER = ptr::null_mut();
            if PdhAddEnglishCounterW(query, wide_path.as_ptr(), 0, &mut counter) != 0 {
                counters.push(ptr::null_mut()); // 索引对齐
                continue;
            }
            counters.push(counter);
        }
        if counters.iter().all(|c| c.is_null()) {
            PdhCloseQuery(query);
            return empty();
        }
        // 两次采样：fraction/rate 型计数器需间隔才能计算出格式化值
        let _ = PdhCollectQueryData(query);
        thread::sleep(Duration::from_millis(1100));
        if PdhCollectQueryData(query) != 0 {
            PdhCloseQuery(query);
            return empty();
        }
        let mut result: Vec<Vec<(String, f64)>> = Vec::with_capacity(paths.len());
        for &counter in &counters {
            if counter.is_null() {
                result.push(Vec::new());
                continue;
            }
            let mut buf_size: u32 = 0;
            let mut item_count: u32 = 0;
            // 第一次调用：仅取所需缓冲区大小（返回 PDH_MORE_DATA）
            if PdhGetFormattedCounterArrayW(
                counter,
                PDH_FMT_DOUBLE,
                &mut buf_size,
                &mut item_count,
                ptr::null_mut(),
            ) != PDH_MORE_DATA
            {
                result.push(Vec::new());
                continue;
            }
            let item_size = std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
            let cap = ((buf_size as usize).max(item_size) / item_size) + 4;
            let mut buf: Vec<PDH_FMT_COUNTERVALUE_ITEM_W> = Vec::with_capacity(cap);
            let rc = PdhGetFormattedCounterArrayW(
                counter,
                PDH_FMT_DOUBLE,
                &mut buf_size,
                &mut item_count,
                buf.as_mut_ptr(),
            );
            if rc != 0 {
                result.push(Vec::new());
                continue;
            }
            buf.set_len(item_count as usize);
            let mut items = Vec::with_capacity(buf.len());
            for it in &buf {
                if it.FmtValue.CStatus != 0 || it.szName.is_null() {
                    continue;
                }
                let mut len = 0usize;
                while *it.szName.add(len) != 0 {
                    len += 1;
                }
                let name = String::from_utf16_lossy(std::slice::from_raw_parts(it.szName, len));
                items.push((name, it.FmtValue.Anonymous.doubleValue));
            }
            result.push(items);
        }
        PdhCloseQuery(query);
        result
    }
}

/// Windows：进程内读取 GPU Engine 使用率与 GPU Adapter Memory 已用显存（PDH，任务管理器同源），
/// 替代 PowerShell 子进程（消除每次约 0.7s 的进程启动开销与窗口闪现）。
///
/// 任务管理器的 GPU 百分比 = 该适配器所有引擎（3D/Copy/Video 等）利用率求和（封顶 100），
/// 与 nvidia-smi `utilization.gpu`（内核执行时间占比）语义不同（任务管理器通常更高）。
/// 按 LUID 分组聚合；过滤软件适配器（无显存且无负载，如 Microsoft Basic Render Driver）；
/// 按 LUID 排序，按索引与 `gpus` 列表（注册表顺序）对齐。
#[cfg(target_os = "windows")]
fn windows_gpu_live_sample() -> GpuLiveSample {
    let all = pdh_read_all(&[
        r"\GPU Engine(*)\utilization percentage",
        r"\GPU Adapter Memory(*)\Dedicated Usage",
    ]);
    let (usage_items, mem_items) = (all.get(0), all.get(1));

    let mut usage_map: HashMap<String, f64> = HashMap::new();
    if let Some(items) = usage_items {
        for (name, v) in items {
            if let Some(luid) = extract_luid(name) {
                *usage_map.entry(luid).or_default() += v;
            }
        }
    }
    let mut mem_map: HashMap<String, f64> = HashMap::new();
    if let Some(items) = mem_items {
        for (name, v) in items {
            if let Some(luid) = extract_luid(name) {
                *mem_map.entry(luid).or_default() += v;
            }
        }
    }

    // 合并 LUID 集合（使用率优先；部分集成显卡可能无 engine 计数器但有显存）
    let mut luids: Vec<String> = usage_map.keys().cloned().collect();
    for k in mem_map.keys() {
        if !luids.contains(k) {
            luids.push(k.clone());
        }
    }
    luids.sort();

    let mut sample = GpuLiveSample::default();
    for luid in luids {
        let usage = usage_map.get(&luid).copied().unwrap_or(0.0);
        let mem = mem_map.get(&luid).copied().unwrap_or(0.0);
        // 过滤软件适配器（Microsoft Basic Render Driver：无显存且无负载）
        if usage <= 0.0 && mem <= 1024.0 * 1024.0 {
            continue;
        }
        sample.usages.push(usage.min(100.0) as f32);
        sample.mem_used.push(mem as u64);
    }
    sample
}

/// Windows：启动独立的 GPU 采集线程（PDH 采样含 ~1s 的两次采样间隔，放独立线程避免阻塞主监控循环）。
/// 每周期采样一次，结果写入共享的 `GpuLiveSample`；随 `running` 原子标志自动停止。
#[cfg(target_os = "windows")]
fn spawn_gpu_collector(
    gpu_live: Arc<std::sync::Mutex<GpuLiveSample>>,
    running: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("gpu-monitor".into())
        .spawn(move || {
            while running.load(Ordering::SeqCst) {
                let mut sample = windows_gpu_live_sample();
                // PDH 无计数器（如旧版 Windows）时，回退 nvidia-smi（NVIDIA 显卡）
                if sample.usages.is_empty() {
                    let nv = nvidia_smi_live();
                    sample.usages = nv.iter().map(|(u, _)| *u).collect();
                    sample.mem_used = nv.iter().map(|(_, m)| *m).collect();
                }
                if let Ok(mut guard) = gpu_live.lock() {
                    *guard = sample;
                }
                for _ in 0..MONITOR_SLEEP_SEGMENTS {
                    if !running.load(Ordering::SeqCst) {
                        break;
                    }
                    thread::sleep(MONITOR_SLEEP_SEGMENT);
                }
            }
        })
        .expect("gpu-monitor 线程创建失败");
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
    system_info: &SystemInfoMetrics,
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
            system: system_info.clone(),
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
                // 一次性加载完整 CPU 信息（brand/vendor/频率），供系统信息采集使用
                sys.refresh_cpu_all();
                sys.refresh_cpu_usage();
                thread::sleep(CPU_WARMUP_DELAY);
                disks.refresh(false);

                // 系统静态信息（OS/架构/主机名/内核/CPU/GPU），启动时采集一次
                let mut system_info = collect_system_info(&sys);

                // Windows：启动独立的 GPU 采集线程（GPU Engine 计数器，任务管理器同源）
                #[cfg(target_os = "windows")]
                let gpu_live = Arc::new(std::sync::Mutex::new(GpuLiveSample::default()));
                #[cfg(target_os = "windows")]
                spawn_gpu_collector(gpu_live.clone(), running.clone());

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

                    // ---------- GPU 实时数据（每周期刷新，随 collect_metrics 一并下发） ----------
                    // Windows：读取 GPU 采集线程的最新样本（任务管理器语义）；其余平台就地采集
                    #[cfg(target_os = "windows")]
                    if let Ok(guard) = gpu_live.lock() {
                        for (i, g) in system_info.gpus.iter_mut().enumerate() {
                            g.usage = guard.usages.get(i).copied().unwrap_or(-1.0);
                            if let Some(m) = guard.mem_used.get(i).copied() {
                                if m > 0 {
                                    g.mem_used = Some(m);
                                }
                            }
                        }
                    }
                    #[cfg(not(target_os = "windows"))]
                    update_gpu_usages(&mut system_info.gpus);

                    let mut payload = collect_metrics(
                        &mut sys,
                        &disks,
                        &app_pids,
                        &cached_host_ip,
                        &cached_type,
                        &mut seen_devices,
                        &system_info,
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

            if let Err(e) = result {
                log::error!("[system-monitor] 监控线程异常退出: {:?}", e);
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

/// 设置日志级别（热切换，即时生效）。
///
/// 同时更新两侧：log:: 宏侧（log::set_max_level）与 tracing 侧
/// （经 LOG_LEVEL_HANDLE 热重载 Targets，rig span 与 log:: 桥接事件同受控制）。
#[tauri::command]
pub fn set_log_level(level: String) {
    let tracing_level = match level.to_lowercase().as_str() {
        "off" => Some(tracing::level_filters::LevelFilter::OFF),
        "error" => Some(tracing::level_filters::LevelFilter::ERROR),
        "warn" => Some(tracing::level_filters::LevelFilter::WARN),
        "info" => Some(tracing::level_filters::LevelFilter::INFO),
        "debug" => Some(tracing::level_filters::LevelFilter::DEBUG),
        "trace" => Some(tracing::level_filters::LevelFilter::TRACE),
        _ => None,
    };
    match level.to_lowercase().as_str() {
        "off" => log::set_max_level(log::LevelFilter::Off),
        "error" => log::set_max_level(log::LevelFilter::Error),
        "warn" => log::set_max_level(log::LevelFilter::Warn),
        "info" => log::set_max_level(log::LevelFilter::Info),
        "debug" => log::set_max_level(log::LevelFilter::Debug),
        "trace" => log::set_max_level(log::LevelFilter::Trace),
        _ => return,
    }
    // tracing 侧热重载（构造新的 Targets，与 init_logging 共用单一来源）
    if let Some(tracing_level) = tracing_level {
        if let Some(handle) = crate::LOG_LEVEL_HANDLE.get() {
            let _ = handle.reload(crate::log_filter_targets(tracing_level));
        }
    }
    log::info!("[config] 日志级别已切换为: {}", level);
}
