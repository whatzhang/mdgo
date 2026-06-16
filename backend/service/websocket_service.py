"""
统一 WebSocket 服务
替代原有的多个 SSE 端点，作为所有实时推送的唯一通道。

消息格式 (JSON):
  推送:     { "type": "msg_type", ...data... }
  心跳:     { "type": "heartbeat" }

支持的消息类型:
  - reminder:    日程提醒推送
  - metrics:     系统监控数据推送
  - heartbeat:   心跳 (服务端每 30s 发送)

前端可订阅的频道:
  - schedule:    日程提醒
  - monitor:     系统监控
  默认订阅所有频道。
"""
import asyncio
import json
import logging
from typing import Set
from fastapi import WebSocket

logger = logging.getLogger(__name__)


class ConnectionManager:
    def __init__(self):
        self._connections: Set[WebSocket] = set()
        self._lock = asyncio.Lock()

    async def connect(self, websocket: WebSocket):
        await websocket.accept()
        async with self._lock:
            self._connections.add(websocket)
        logger.info(f"WebSocket 客户端已连接，当前连接数: {len(self._connections)}")

    async def disconnect(self, websocket: WebSocket):
        async with self._lock:
            self._connections.discard(websocket)
        try:
            await websocket.close()
        except Exception:
            pass
        logger.info(f"WebSocket 客户端已断开，当前连接数: {len(self._connections)}")

    async def broadcast(self, message: dict):
        """广播消息给所有连接的客户端"""
        payload = json.dumps(message, ensure_ascii=False)
        async with self._lock:
            dead_connections = []
            for ws in self._connections:
                try:
                    await ws.send_text(payload)
                except Exception:
                    dead_connections.append(ws)
            for ws in dead_connections:
                self._connections.discard(ws)

    @property
    def count(self) -> int:
        return len(self._connections)


# 全局单例
ws_manager = ConnectionManager()


async def ws_heartbeat_loop():
    """后台心跳任务：每 30s 向所有客户端发送心跳"""
    while True:
        await asyncio.sleep(30)
        try:
            await ws_manager.broadcast({"type": "heartbeat"})
        except Exception as e:
            logger.warning(f"WebSocket 心跳发送异常: {e}")


async def ws_metrics_loop(interval: float = 5.0):
    """后台指标推送任务：定期推送系统监控数据"""
    from backend.service.system_monitor_service import get_metrics

    # 首次收集（interval=0 避免阻塞事件循环）
    try:
        loop = asyncio.get_running_loop()
        data = await loop.run_in_executor(None, get_metrics)
        await ws_manager.broadcast({"type": "metrics", "data": data})
    except Exception as e:
        logger.warning(f"WebSocket 首次指标推送异常: {e}")

    while True:
        await asyncio.sleep(interval)
        try:
            loop = asyncio.get_running_loop()
            data = await loop.run_in_executor(None, get_metrics)
            await ws_manager.broadcast({
                "type": "metrics",
                "data": data
            })
        except Exception as e:
            logger.warning(f"WebSocket 指标推送异常: {e}")


async def ws_reminder_loop():
    """后台提醒推送任务：从 ScheduleManager 队列读取提醒并推送"""
    from service.schedule_service import schedule_manager

    while True:
        try:
            reminder = await schedule_manager._reminders.get()
            await ws_manager.broadcast(reminder)
        except Exception as e:
            logger.warning(f"WebSocket 提醒推送异常: {e}")
