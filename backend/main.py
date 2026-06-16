import asyncio
from contextlib import asynccontextmanager

import psutil

from service.system_service import scan_file_info, get_openresty_conf, reload_openresty_conf
from service.config import PROJECT_ROOT, FRENDEND_DIR, HOST, PORT, LOG_LEVEL
from typing import Optional
from fastapi.middleware.cors import CORSMiddleware
from fastapi.staticfiles import StaticFiles
from fastapi.responses import HTMLResponse, JSONResponse, FileResponse, StreamingResponse
import json
import logging
from fastapi import FastAPI, Request, Query, Body, WebSocket, WebSocketDisconnect
import os
from strawberry.fastapi import GraphQLRouter
from service.graphql_schema import schema
from service.websocket_service import ws_manager, ws_heartbeat_loop, ws_metrics_loop, ws_reminder_loop
from service.system_monitor_service import metrics_stream

LOG_LEVEL_VALUE = getattr(logging, LOG_LEVEL.upper(), logging.INFO)
logging.basicConfig(level=LOG_LEVEL_VALUE)
logger = logging.getLogger(__name__)
logger.setLevel(LOG_LEVEL_VALUE)
for name in ["uvicorn", "uvicorn.error", "uvicorn.access", "fastapi", "starlette", "jieba"]:
    logging.getLogger(name).setLevel(LOG_LEVEL_VALUE)


@asynccontextmanager
async def lifespan(app: FastAPI):
    # 启动 WebSocket 后台任务
    bg_tasks = [
        asyncio.create_task(ws_heartbeat_loop()),
        asyncio.create_task(ws_metrics_loop()),
        asyncio.create_task(ws_reminder_loop()),
    ]
    yield
    # 关闭后台任务
    for task in bg_tasks:
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass


app = FastAPI(title="本地文档浏览器", version="1.0.0", lifespan=lifespan)

app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_credentials=True,
    allow_methods=["GET", "POST", "PUT", "DELETE", "OPTIONS"],
    allow_headers=["Content-Type", "Authorization"],
)

STATIC_CSS_JS_DIR = os.path.join(FRENDEND_DIR, "css_js")

for mount_path, directory, mount_name in [
    ("/css_js", STATIC_CSS_JS_DIR, "css_js"),
]:
    if os.path.isdir(directory):
        app.mount(mount_path, StaticFiles(
            directory=directory), name=mount_name)
    else:
        logging.warning("静态挂载被跳过，因为路径不存在: %s", directory)

DYNAMIC_SCAN_PATH = None


def dynamic_mount_directory(real_path):
    global DYNAMIC_SCAN_PATH
    if not real_path or '..' in real_path:
        logging.warning(f"无效路径: {real_path}")
        return False
    abs_path = os.path.abspath(real_path)
    if not os.path.isdir(abs_path):
        logging.warning(f"目录未找到: {abs_path}")
        return False
    if DYNAMIC_SCAN_PATH == abs_path:
        return True
    DYNAMIC_SCAN_PATH = abs_path
    logging.info(f"动态扫描路径设置为: {abs_path}")
    return True


graphql_router = GraphQLRouter(schema, graphql_ide=None)
app.include_router(graphql_router, prefix="/graphql")


def _load_static_file(directory: str, filename: str):
    page_path = os.path.join(directory, filename)
    if not os.path.exists(page_path):
        return JSONResponse(content={"error": "页面未找到"}, status_code=404)
    return FileResponse(page_path)


@app.get("/", response_class=HTMLResponse)
async def index_page():
    return _load_static_file(FRENDEND_DIR, "main.html")


@app.get("/main", response_class=HTMLResponse)
async def index_alias():
    return _load_static_file(FRENDEND_DIR, "main.html")


@app.get("/cdn", response_class=HTMLResponse)
async def hello_page():
    return _load_static_file(FRENDEND_DIR, "main_cdn.html")


@app.get("/api/system/scan")
async def api_scan_file_info(dir: Optional[str] = Query(None),
                             force: bool = Query(False)):
    result = scan_file_info(dir_path=dir, force=force)
    if dir:
        if dynamic_mount_directory(dir) and isinstance(result, dict):
            result["data"]["mount_path"] = "/"
            result["data"]["scan_root"] = DYNAMIC_SCAN_PATH
    return result


@app.get("/api/system/openresty/conf")
async def api_get_openresty_conf(conf_path: Optional[str] = Query(None)):
    return get_openresty_conf(conf_path)


@app.post("/api/system/openresty/reload")
async def api_reload_openresty_conf(body: dict = Body(...)):
    content = body.get("conf", "")
    conf_path = body.get("conf_path", None)
    return reload_openresty_conf(conf_path=conf_path, conf_content=content)


# ── WebSocket 统一推送通道 ──
@app.websocket("/ws")
async def websocket_endpoint(websocket: WebSocket):
    await ws_manager.connect(websocket)
    try:
        while True:
            # 接收客户端消息（预留后续扩展）
            try:
                data = await asyncio.wait_for(websocket.receive_text(), timeout=60)
                # 客户端可发送订阅/取消订阅等指令（预留）
                msg = json.loads(data)
                msg_type = msg.get("type", "")
                if msg_type == "ping":
                    await websocket.send_text(json.dumps({"type": "pong"}))
            except asyncio.TimeoutError:
                # 60s 无消息正常，保持连接
                pass
    except WebSocketDisconnect:
        pass
    except Exception as e:
        logger.warning(f"WebSocket 连接异常: {e}")
    finally:
        await ws_manager.disconnect(websocket)


# ── 保留旧 SSE 端点用于向后兼容 ──
@app.get("/api/system/metrics/stream")
async def api_system_metrics_stream():
    return StreamingResponse(metrics_stream(interval=5.0), media_type="text/event-stream")


RESERVED_PATH_PREFIXES = ("/css_js", "/graphql", "/api/")


@app.get("/{full_path:path}")
async def serve_scan_files(full_path: str):
    if not DYNAMIC_SCAN_PATH:
        return JSONResponse(content={"error": "No scan directory configured"}, status_code=404)

    if not full_path or '..' in full_path:
        return JSONResponse(content={"error": "Invalid path"}, status_code=400)

    full_path_with_slash = f"/{full_path}"
    for prefix in RESERVED_PATH_PREFIXES:
        if full_path_with_slash.startswith(prefix):
            return JSONResponse(content={"error": "File not found"}, status_code=404)

    resolved_path = os.path.realpath(os.path.join(DYNAMIC_SCAN_PATH, full_path))
    scan_root = os.path.realpath(DYNAMIC_SCAN_PATH)

    if not resolved_path.startswith(scan_root + os.sep) and resolved_path != scan_root:
        return JSONResponse(content={"error": "Access denied"}, status_code=403)

    if not os.path.isfile(resolved_path):
        return JSONResponse(content={"error": "File not found"}, status_code=404)

    return FileResponse(resolved_path)


if __name__ == "__main__":
    import sys
    import time
    import threading


    def _enforce_memory_limit(limit_bytes: int):
        import gc

        def _watch():
            while True:
                time.sleep(5)
                try:
                    proc = psutil.Process(os.getpid())
                    mem = proc.memory_info()
                    if mem.rss > limit_bytes:
                        gc.collect()
                        proc = psutil.Process(os.getpid())
                        if proc.memory_info().rss > limit_bytes:
                            if sys.platform != 'win32':
                                import resource
                                try:
                                    cur_soft, cur_hard = resource.getrlimit(resource.RLIMIT_AS)
                                    if limit_bytes < cur_soft:
                                        resource.setrlimit(resource.RLIMIT_AS, (limit_bytes, cur_hard))
                                except (ValueError, OSError):
                                    pass
                except Exception:
                    pass

        t = threading.Thread(target=_watch, daemon=True)
        t.start()


    _enforce_memory_limit(100 * 1024 * 1024)

    import uvicorn

    uvicorn.run(app, host=HOST, port=PORT)
