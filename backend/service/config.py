import os

PROJECT_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

FRENDEND_DTAT_DIR = os.path.join(PROJECT_ROOT, "data")

PORT = int(os.getenv("PORT", "8091"))
HOST = os.getenv("HOST", "0.0.0.0")
LOG_LEVEL = os.getenv("LOG_LEVEL", "warning").upper()
