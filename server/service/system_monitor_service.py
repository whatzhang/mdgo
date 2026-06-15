import json
import time
import asyncio
import os
import re
import psutil

_net_io_last = {"bytes_sent": 0, "bytes_recv": 0, "timestamp": time.time()}
_net_io_ready = False

_service_process = None
_service_start_time = time.time()


def _get_process():
    global _service_process
    if _service_process is None:
        _service_process = psutil.Process(os.getpid())
    return _service_process


def _get_all_disks_usage():
    total = 0
    used = 0
    free = 0
    partitions = psutil.disk_partitions(all=False)
    physical_disks = {}
    for p in partitions:
        try:
            usage = psutil.disk_usage(p.mountpoint)
            device = p.device
            if os.name == 'nt':
                physical_id = device
            else:
                match = re.match(r'/dev/(disk\d+(?:s\d+)?(?:[sp]\d+)?|nvme\d+n\d+(?:p\d+)?|mmcblk\d+p\d+)', device)
                physical_id = match.group(1) if match else device
            if physical_id not in physical_disks:
                physical_disks[physical_id] = {
                    'total': usage.total,
                    'used': usage.used,
                    'free': usage.free,
                    'percent': usage.percent,
                    'mountpoint': p.mountpoint
                }
            else:
                if usage.percent > physical_disks[physical_id]['percent']:
                    physical_disks[physical_id] = {
                        'total': usage.total,
                        'used': usage.used,
                        'free': usage.free,
                        'percent': usage.percent,
                        'mountpoint': p.mountpoint
                    }
        except Exception:
            continue
    for disk_info in physical_disks.values():
        total += disk_info['total']
        used += disk_info['used']
        free += disk_info['free']
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
    now = time.time()
    cpu_percent = psutil.cpu_percent(interval=0.5)
    cpu_cores = psutil.cpu_count(logical=True)
    mem = psutil.virtual_memory()
    net = psutil.net_io_counters()
    global _net_io_last, _net_io_ready
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
        "timestamp": now
    }
    disk = _get_all_disks_usage()
    uptime_seconds = int(time.time() - psutil.boot_time())
    service_process = _get_process()
    service_mem = service_process.memory_info()
    service_uptime_seconds = int(now - _service_start_time)

    return {
        "cpu": {
            "percent": round(cpu_percent, 1),
            "core_count": cpu_cores,
        },
        "memory": {
            "total": mem.total,
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
        "disk": {
            "total": disk["total"],
            "used": disk["used"],
            "free": disk["free"],
            "percent": disk["percent"],
        },
        "uptime": uptime_seconds,
        "service": {
            "rss": service_mem.rss,
            "vms": service_mem.vms,
            "uptime": service_uptime_seconds,
        }
    }


async def metrics_stream(interval: float = 5.0):
    while True:
        data = get_metrics()
        yield f"data: {json.dumps(data, ensure_ascii=False)}\n\n"
        await asyncio.sleep(interval)
