import json
import time
import asyncio
import os
import re
import socket
import threading
import psutil

# 上次网络 IO 采样数据（用于计算速率）
_net_io_last = {"bytes_sent": 0, "bytes_recv": 0, "timestamp": time.time()}
_net_io_ready = False

# 上次磁盘 IO 采样数据（用于计算速率）
_disk_io_last = {"read_bytes": 0, "write_bytes": 0, "timestamp": time.time()}
_disk_io_ready = False

# 当前进程缓存 + 启动时间
_service_process = None
_service_start_time = time.time()

# 本地 IP 缓存（不变值，只需获取一次）
_local_ip = None

# 线程锁，保护 _net_io_last / _disk_io_last 的并发读写（SSE + WebSocket 可能同时调用）
_net_io_lock = threading.Lock()
_disk_io_lock = threading.Lock()

# === 启动时一次获取的常量（运行时永不改变） ===
_cpu_cores = psutil.cpu_count(logical=True)
_mem_total = psutil.virtual_memory().total
_boot_time = psutil.boot_time()

# CPU 预热：psutil.cpu_percent() 第一次调用总是返回 0.0，提前调用以初始化内部计数器
_ = psutil.cpu_percent(interval=0)

# === 磁盘分区结构缓存 ===
# 分区列表、物理设备映射几乎不变化，用 TTL 避免每次全量扫描
_disk_structure = None
_last_disk_refresh = 0
_DISK_REFRESH_INTERVAL = 300  # 秒（5 分钟）


def _get_local_ip():
    """获取本机非回环 IPv4 地址（缓存）。"""
    global _local_ip
    if _local_ip is not None:
        return _local_ip
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            # 连接一个外部 IP（无需可达，UDP 不真正发包）
            s.connect(("10.254.254.254", 1))
            _local_ip = s.getsockname()[0]
    except Exception:
        _local_ip = "127.0.0.1"
    return _local_ip


def _get_process():
    """获取当前进程的 psutil.Process 实例（缓存避免重复创建）"""
    global _service_process
    if _service_process is None:
        proc = psutil.Process(os.getpid())
        # CPU 预热：cpu_percent(interval=0) 第一次调用返回 0.0
        _ = proc.cpu_percent(interval=0)
        _service_process = proc
    return _service_process


def _refresh_disk_structure():
    """
    刷新磁盘物理设备映射表（每 _DISK_REFRESH_INTERVAL 秒执行一次）。
    仅缓存设备列表和总容量，用量数据在 _get_all_disks_usage 中实时查询。
    """
    global _disk_structure, _last_disk_refresh
    now = time.time()
    if _disk_structure is not None and now - _last_disk_refresh < _DISK_REFRESH_INTERVAL:
        return

    partitions = psutil.disk_partitions(all=False)
    physical = {}
    for p in partitions:
        try:
            usage = psutil.disk_usage(p.mountpoint)
            device = p.device
            if os.name == 'nt':
                physical_id = device
            else:
                # 从设备路径提取物理磁盘 ID（如 /dev/disk1, /dev/nvme0n1p1）
                match = re.match(r'/dev/(disk\d+(?:s\d+)?(?:[sp]\d+)?|nvme\d+n\d+(?:p\d+)?|mmcblk\d+p\d+)', device)
                physical_id = match.group(1) if match else device
            # 同一物理设备只保留总容量最大的分区（总容量不会变化）
            if physical_id not in physical or usage.total > physical[physical_id]['total']:
                physical[physical_id] = {
                    'mountpoint': p.mountpoint,
                    'total': usage.total,
                }
        except Exception:
            continue

    _disk_structure = physical
    _last_disk_refresh = now


def _get_all_disks_usage():
    """
    汇总所有物理磁盘用量。
    物理设备列表来自 _refresh_disk_structure（TTL 缓存），
    用量数据（used/free/percent）实时查询。
    """
    _refresh_disk_structure()

    total = 0
    used = 0
    free = 0
    for info in _disk_structure.values():
        try:
            usage = psutil.disk_usage(info['mountpoint'])
            total += usage.total
            used += usage.used
            free += usage.free
        except Exception:
            # 挂载点临时不可达时保留缓存的总容量，用量按 0 处理
            total += info['total']

    if total == 0:
        total = 1
        used = 0
        free = 1
    percent = round((used / total) * 100, 1) if total > 0 else 0
    return {
        "total": total,
        "used": used,
        "free": free,
        "percent": percent,
    }


def get_metrics():
    """采集 CPU、内存、网络、磁盘、服务进程等系统指标。

    高可用：使用线程锁保护网络/磁盘 IO 的增量计数器，避免 SSE 和 WebSocket
    两个生产者并发调用时的数据竞争。
    """
    now = time.time()

    # ---- CPU（非阻塞，~0.01ms，不阻塞事件循环） ----
    cpu_percent = psutil.cpu_percent(interval=0)

    # ---- 内存（仅有 few 字段变化，total 使用缓存） ----
    mem = psutil.virtual_memory()

    # ---- 网络（增量计算收发速率，线程锁保护） ----
    net = psutil.net_io_counters()
    global _net_io_last, _net_io_ready
    with _net_io_lock:
        elapsed = now - _net_io_last["timestamp"]
        if _net_io_ready and elapsed > 0:
            sent_rate = (net.bytes_sent - _net_io_last["bytes_sent"]) / elapsed
            recv_rate = (net.bytes_recv - _net_io_last["bytes_recv"]) / elapsed
        else:
            sent_rate = 0
            recv_rate = 0
            _net_io_ready = True
        _net_io_last = {
            "bytes_sent": net.bytes_sent,
            "bytes_recv": net.bytes_recv,
            "timestamp": now,
        }

    # ---- 磁盘 IO 速率（增量计算，类似网络速率） ----
    disk_io = psutil.disk_io_counters()
    global _disk_io_last, _disk_io_ready
    with _disk_io_lock:
        elapsed_io = now - _disk_io_last["timestamp"]
        if _disk_io_ready and elapsed_io > 0:
            disk_read_rate = (disk_io.read_bytes - _disk_io_last["read_bytes"]) / elapsed_io
            disk_write_rate = (disk_io.write_bytes - _disk_io_last["write_bytes"]) / elapsed_io
        else:
            disk_read_rate = 0
            disk_write_rate = 0
            _disk_io_ready = True
        _disk_io_last = {
            "read_bytes": disk_io.read_bytes,
            "write_bytes": disk_io.write_bytes,
            "timestamp": now,
        }

    # ---- 磁盘（结构缓存 + 用量实时） ----
    disk = _get_all_disks_usage()

    # ---- 系统运行时长（boot_time 使用缓存） ----
    uptime_seconds = int(now - _boot_time)

    # ---- 服务进程自身信息 ----
    service_process = _get_process()
    service_mem = service_process.memory_info()
    # cpu_percent 已在 _get_process 预热，第二次调用起即有真实值
    service_cpu = service_process.cpu_percent(interval=0)
    service_mem_pct = service_process.memory_percent()
    service_uptime_seconds = int(now - _service_start_time)

    return {
        "host_ip": _get_local_ip(),
        "cpu": {
            "percent": round(cpu_percent, 1),
            "core_count": _cpu_cores,
        },
        "memory": {
            "total": _mem_total,
            "used": mem.used,
            "available": mem.available,
            "percent": round(mem.percent, 1),
        },
        "network": {
            "sent_rate": round(sent_rate, 0),
            "recv_rate": round(recv_rate, 0),
            "total_sent": net.bytes_sent,
            "total_recv": net.bytes_recv,
        },
        "disk_io": {
            "read_rate": round(disk_read_rate, 0),
            "write_rate": round(disk_write_rate, 0),
        },
        "disk": disk,
        "uptime": uptime_seconds,
        "service": {
            "rss": service_mem.rss,
            "vms": service_mem.vms,
            "uptime": service_uptime_seconds,
            "app_cpu_percent": round(service_cpu, 1),
            "app_mem_percent": round(service_mem_pct, 1),
            "app_mem_bytes": service_mem.rss,
        },
    }


async def metrics_stream(interval: float = 5.0):
    """SSE 流：按固定间隔推送系统指标（使用线程池避免阻塞事件循环）"""
    loop = asyncio.get_running_loop()
    while True:
        data = await loop.run_in_executor(None, get_metrics)
        yield f"data: {json.dumps(data, ensure_ascii=False)}\n\n"
        await asyncio.sleep(interval)
