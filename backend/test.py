import requests
import os


LOCAL_EMBEDDING_API = os.getenv(
    "LOCAL_EMBEDDING_API", "http://192.168.31.152:12345/v1/embeddings")
LOCAL_EMBEDDING_API_TOKEN = os.getenv(
    "LOCAL_EMBEDDING_API_TOKEN", "c17f18e31adb10978d3a9cd0d6743d30a067af602a8789e7")
LOCAL_EMBEDDING_API_MODEL = os.getenv(
    "LOCAL_EMBEDDING_API_MODEL", "text-embedding-qwen3-embedding-0.6b")

_LOCAL_EMBEDDING_MODEL = None
_LOCAL_EMBEDDING_DIM = 768


def call_openclaw_embedding(text):
    payload = {
        "model": "text-embedding-qwen3-embedding-0.6b",
        "input": text
    }
    headers = {
        'User-Agent': 'PostmanRuntime/7.26.8',
        "Content-Type": "application/json",
        "Authorization": f"Bearer {LOCAL_EMBEDDING_API_TOKEN}"
    }
    # 使用 Session + trust_env=False 禁用系统代理，避免 Clash/Surge 拦截局域网请求
    session = requests.Session()
    session.trust_env = False
    try:
        resp = session.post(LOCAL_EMBEDDING_API,
                            json=payload, headers=headers, timeout=30)
        resp.raise_for_status()
        result = resp.json()
        embedding = result["data"][0]["embedding"]
        if isinstance(embedding, list) and len(embedding) > 0:
            return embedding
        else:
            return []
    except Exception as e:
        print(e)
        return []

if __name__ == "__main__":
    text = "你好"
    embedding = call_openclaw_embedding(text)
    print(embedding)