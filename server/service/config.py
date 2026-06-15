import os

PROJECT_ROOT = os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__))))
SERVICE_ROOT = os.path.dirname(os.path.abspath(__file__))
SERVER_ROOT = os.path.dirname(SERVICE_ROOT)
STATIC_DIR = os.path.join(SERVER_ROOT, "static")
DATA_DIR = os.path.join(STATIC_DIR, "data", "bookmarks")

BOOKMARKS_JSONL = os.path.join(DATA_DIR, "bookmarks.jsonl")
TAGS_JSONL = os.path.join(DATA_DIR, "tags.jsonl")
RELATIONS_JSONL = os.path.join(DATA_DIR, "relations.jsonl")
INVERTED_JSON = os.path.join(DATA_DIR, "inverted.json")
EMBEDDINGS_JSON = os.path.join(DATA_DIR, "embeddings.json")
GRAPH_NODES_JSON = os.path.join(DATA_DIR, "graph_nodes.json")

LOCAL_LLM_API = os.getenv(
    "LOCAL_LLM_API", "http://192.168.31.152:12345/api/v0/chat/completions")
LOCAL_LLM_API_TOKEN = os.getenv(
    "LOCAL_LLM_API_TOKEN", "c17f18e31adb10978d3a9cd0d6743d30a067af602a8789e7")
LOCAL_LLM_API_MODEL = os.getenv("LOCAL_LLM_API_MODEL", "gemma-4-e4b-it")

LOCAL_EMBEDDING_API = os.getenv(
    "LOCAL_EMBEDDING_API", "http://192.168.31.152:12345/api/v0/embeddings")
LOCAL_EMBEDDING_API_TOKEN = os.getenv(
    "LOCAL_EMBEDDING_API_TOKEN", "c17f18e31adb10978d3a9cd0d6743d30a067af602a8789e7")
LOCAL_EMBEDDING_API_MODEL = os.getenv(
    "LOCAL_EMBEDDING_API_MODEL", "text-embedding-qwen3-embedding-0.6b")

OPENRESTY_CONF_PATH = os.getenv(
    "OPENRESTY_CONF_PATH", "/usr/local/etc/openresty/sites/index.conf")
FILE_SCAN_TARGET_DIR = os.getenv("FILE_SCAN_TARGET_DIR", PROJECT_ROOT)

PORT = int(os.getenv("PORT", "8091"))
HOST = os.getenv("HOST", "0.0.0.0")
LOG_LEVEL = os.getenv("LOG_LEVEL", "warning").upper()
