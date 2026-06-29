import asyncio
from contextlib import asynccontextmanager

from service.system_service import get_openresty_conf, reload_openresty_conf
from service.config import PROJECT_ROOT, HOST, PORT, LOG_LEVEL
from typing import Optional
from fastapi.middleware.cors import CORSMiddleware
from fastapi.staticfiles import StaticFiles
from fastapi.responses import HTMLResponse, JSONResponse, FileResponse, StreamingResponse
import json
import logging
from fastapi import FastAPI, Query, Body, WebSocket, WebSocketDisconnect
import os
from strawberry.fastapi import GraphQLRouter
from service.graphql_schema import schema
from service.websocket_service import ws_manager, ws_heartbeat_loop, ws_metrics_loop
from service.system_monitor_service import metrics_stream

# 配置日志级别，默认 warning（覆盖 uvicorn/fastapi 的日志避免太吵）
LOG_LEVEL_VALUE = getattr(logging, LOG_LEVEL.upper(), logging.INFO)
logging.basicConfig(level=LOG_LEVEL_VALUE)
logger = logging.getLogger(__name__)
logger.setLevel(LOG_LEVEL_VALUE)
for name in ["uvicorn", "uvicorn.error", "uvicorn.access", "fastapi", "starlette", "jieba"]:
    logging.getLogger(name).setLevel(LOG_LEVEL_VALUE)


@asynccontextmanager
async def lifespan(app: FastAPI):
    """应用生命周期管理：启动时创建 WebSocket 后台任务，关闭时清理"""
    bg_tasks = [
        asyncio.create_task(ws_heartbeat_loop()),
        asyncio.create_task(ws_metrics_loop()),
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

# 挂载前端静态资源目录
STATIC_CSS_JS_DIR = os.path.join(PROJECT_ROOT, "css_js")

for mount_path, directory, mount_name in [
    ("/css_js", STATIC_CSS_JS_DIR, "css_js"),
]:
    if os.path.isdir(directory):
        app.mount(mount_path, StaticFiles(
            directory=directory), name=mount_name)
    else:
        logging.warning("静态挂载被跳过，因为路径不存在: %s", directory)

# 用户动态扫描目录（通过 /api/system/mount 设置）
DYNAMIC_SCAN_PATH = None


def dynamic_mount_directory(real_path):
    """设置动态扫描目录路径（含安全检查，防止路径穿越）"""
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
    """读取静态页面文件并返回 FileResponse"""
    page_path = os.path.join(directory, filename)
    if not os.path.exists(page_path):
        return JSONResponse(content={"error": "页面未找到"}, status_code=404)
    return FileResponse(page_path)


# ── 页面路由 ──

@app.get("/", response_class=HTMLResponse)
async def index_page():
    """主页面"""
    return _load_static_file(PROJECT_ROOT, "index.html")


@app.get("/index", response_class=HTMLResponse)
async def index_alias():
    """index 别名"""
    return _load_static_file(PROJECT_ROOT, "index.html")


@app.get("/cdn", response_class=HTMLResponse)
async def hello_page():
    """CDN 版本页面"""
    return _load_static_file(PROJECT_ROOT, "index_cdn.html")


@app.get("/api/system/health")
async def api_health():
    """健康检查（无状态、幂等）"""
    return {"success": True, "message": "healthy", "code": 200}


@app.get("/api/system/mount")
async def api_mount(dir: str = Query(..., description="要挂载的扫描目录绝对路径")):
    """设置动态扫描目录"""
    if dynamic_mount_directory(dir) is False:
        return {"success": False, "message": "无效的扫描目录", "code": 500}
    return {"success": True, "message": "挂载成功", "code": 200}


@app.get("/api/system/openresty/conf")
async def api_get_openresty_conf(conf_path: Optional[str] = Query(None)):
    """读取 OpenResty 配置文件"""
    return get_openresty_conf(conf_path)


@app.post("/api/system/openresty/reload")
async def api_reload_openresty_conf(body: dict = Body(...)):
    """写入配置并重载 OpenResty（失败自动回滚备份）"""
    content = body.get("conf", "")
    conf_path = body.get("conf_path", None)
    return reload_openresty_conf(conf_path=conf_path, conf_content=content)


# ── WebSocket 统一推送通道 ──
@app.websocket("/ws")
async def websocket_endpoint(websocket: WebSocket):
    """WebSocket 实时推送（监控指标）"""
    await ws_manager.connect(websocket)
    try:
        while True:
            try:
                data = await asyncio.wait_for(websocket.receive_text(), timeout=60)
                msg = json.loads(data)
                msg_type = msg.get("type", "")
                if msg_type == "ping":
                    await websocket.send_text(json.dumps({"type": "pong"}))
            except asyncio.TimeoutError:
                # 60s 无消息是正常的，保持长连接
                pass
    except WebSocketDisconnect:
        pass
    except Exception as e:
        logger.warning(f"WebSocket 连接异常: {e}")
    finally:
        await ws_manager.disconnect(websocket)


# ── SSE 端点（向后兼容） ──
@app.get("/api/system/metrics/stream")
async def api_system_metrics_stream():
    """SSE 流：系统监控指标（旧接口，保留兼容）"""
    return StreamingResponse(metrics_stream(interval=5.0), media_type="text/event-stream")


# 保留路径前缀（这些路径由其他路由处理，不应被动态文件服务拦截）
RESERVED_PATH_PREFIXES = ("/css_js", "/graphql", "/api/")


@app.get("/{full_path:path}")
async def serve_scan_files(full_path: str):
    """
    动态扫描目录下的文件服务。
    优先检查路径穿越和目录逃逸，确保安全后才能读取文件。
    """
    if not DYNAMIC_SCAN_PATH:
        return JSONResponse(content={"error": "No scan directory configured"}, status_code=404)

    if not full_path or '..' in full_path:
        return JSONResponse(content={"error": "Invalid path"}, status_code=400)

    # 排除系统保留路径
    full_path_with_slash = f"/{full_path}"
    for prefix in RESERVED_PATH_PREFIXES:
        if full_path_with_slash.startswith(prefix):
            return JSONResponse(content={"error": "File not found"}, status_code=404)

    # 解析真实路径，防止目录穿越
    resolved_path = os.path.realpath(os.path.join(DYNAMIC_SCAN_PATH, full_path))
    scan_root = os.path.realpath(DYNAMIC_SCAN_PATH)

    if not resolved_path.startswith(scan_root + os.sep) and resolved_path != scan_root:
        return JSONResponse(content={"error": "Access denied"}, status_code=403)

    if not os.path.isfile(resolved_path):
        return JSONResponse(content={"error": "File not found"}, status_code=404)

    return FileResponse(resolved_path)


if __name__ == "__main__":
    import uvicorn

    uvicorn.run(app, host=HOST, port=PORT)
