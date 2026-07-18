"""
统一 WebSocket 服务 —— 替代原有的多个 SSE 端点，作为所有实时推送的唯一通道。

消息格式 (JSON):
  推送:     { "type": "msg_type", ...data... }
  心跳:     { "type": "heartbeat" }

消息类型:
  - metrics:     系统监控数据
  - heartbeat:   心跳（服务端每 30s 发送）

前端可订阅频道:
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
    """WebSocket 连接管理器，负责客户端连接的增删与消息广播"""

    def __init__(self):
        self._connections: Set[WebSocket] = set()
        self._lock = asyncio.Lock()  # 保护连接集

    async def connect(self, websocket: WebSocket):
        """接受新 WebSocket 连接并加入管理"""
        await websocket.accept()
        async with self._lock:
            self._connections.add(websocket)
        logger.info(f"WebSocket 客户端已连接，当前连接数: {len(self._connections)}")

    async def disconnect(self, websocket: WebSocket):
        """断开并移除 WebSocket 连接"""
        async with self._lock:
            self._connections.discard(websocket)
        try:
            await websocket.close()
        except Exception:
            pass
        logger.info(f"WebSocket 客户端已断开，当前连接数: {len(self._connections)}")

    async def broadcast(self, message: dict):
        """
        向所有客户端广播消息，自动清理已断开的连接。

        高并发：使用 asyncio.gather 并发发送，避免慢客户端头阻塞所有其他客户端。
        """
        payload = json.dumps(message, ensure_ascii=False)
        async with self._lock:
            connections = list(self._connections)

        if not connections:
            return

        # 并发发送：每个连接独立 await，gather 并行执行
        async def _send(ws):
            try:
                await ws.send_text(payload)
            except Exception:
                return ws  # 失败时返回 ws 引用以便清理
            return None

        results = await asyncio.gather(*[_send(ws) for ws in connections])

        # 批量清理已断开连接
        dead = [ws for ws, r in zip(connections, results) if r is not None]
        if dead:
            async with self._lock:
                for ws in dead:
                    self._connections.discard(ws)

    @property
    def count(self) -> int:
        return len(self._connections)


# 全局单例
ws_manager = ConnectionManager()


async def ws_heartbeat_loop():
    """后台心跳任务：每 30s 向所有客户端发送心跳保活"""
    while True:
        await asyncio.sleep(30)
        try:
            await ws_manager.broadcast({"type": "heartbeat"})
        except Exception as e:
            logger.warning(f"WebSocket 心跳发送异常: {e}")


async def ws_metrics_loop(interval: float = 5.0):
    """后台指标推送：定期采集并推送系统监控数据（使用线程池避免阻塞事件循环）"""
    from service.system_monitor_service import get_metrics

    loop = asyncio.get_running_loop()

    # 启动后立即推送一次
    try:
        data = await loop.run_in_executor(None, get_metrics)
        await ws_manager.broadcast({"type": "metrics", "data": data})
    except Exception as e:
        logger.warning(f"WebSocket 首次指标推送异常: {e}")

    while True:
        await asyncio.sleep(interval)
        try:
            data = await loop.run_in_executor(None, get_metrics)
            await ws_manager.broadcast({
                "type": "metrics",
                "data": data
            })
        except Exception as e:
            logger.warning(f"WebSocket 指标推送异常: {e}")
