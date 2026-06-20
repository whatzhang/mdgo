import os

# 项目根目录（向上三级：config.py -> service -> backend -> PROJECT_ROOT）
PROJECT_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

# 前端数据目录（书签、图谱等）
FRENDEND_DTAT_DIR = os.path.join(PROJECT_ROOT, "data")

# 服务端口、监听地址、日志级别（均可通过环境变量覆盖）
PORT = int(os.getenv("PORT", "8091"))
HOST = os.getenv("HOST", "0.0.0.0")
LOG_LEVEL = os.getenv("LOG_LEVEL", "warning").upper()
