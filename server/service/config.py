import os

PROJECT_ROOT = os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__))))
SERVICE_ROOT = os.path.dirname(os.path.abspath(__file__))
SERVER_ROOT = os.path.dirname(SERVICE_ROOT)
STATIC_DIR = os.path.join(SERVER_ROOT, "static")

OPENRESTY_CONF_PATH = os.getenv(
    "OPENRESTY_CONF_PATH", "/usr/local/etc/openresty/sites/index.conf")
FILE_SCAN_TARGET_DIR = os.getenv("FILE_SCAN_TARGET_DIR", PROJECT_ROOT)

PORT = int(os.getenv("PORT", "8091"))
HOST = os.getenv("HOST", "0.0.0.0")
LOG_LEVEL = os.getenv("LOG_LEVEL", "warning").upper()
