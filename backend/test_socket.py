import socket
import requests

# 测试1: 原始 socket 连接
print("=== 测试1: 原始 socket 连接 ===")
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.settimeout(5)
try:
    s.connect(('192.168.31.152', 12345))
    print("socket connect: SUCCESS")
    s.close()
except Exception as e:
    print(f"socket connect: FAILED - {e}")

# 测试2: requests 不设代理
print("\n=== 测试2: requests 无代理 ===")
try:
    resp = requests.post(
        'http://192.168.31.152:12345/v1/embeddings',
        json={"model": "test", "input": "hello"},
        headers={"Content-Type": "application/json"},
        timeout=10,
        proxies={"http": None, "https": None},
    )
    print(f"requests: SUCCESS - status={resp.status_code}")
except Exception as e:
    print(f"requests: FAILED - {e}")

# 测试3: requests 用 trust_env=False
print("\n=== 测试3: requests trust_env=False ===")
try:
    session = requests.Session()
    session.trust_env = False
    resp = session.post(
        'http://192.168.31.152:12345/v1/embeddings',
        json={"model": "test", "input": "hello"},
        headers={"Content-Type": "application/json"},
        timeout=10,
    )
    print(f"requests(trust_env=False): SUCCESS - status={resp.status_code}")
except Exception as e:
    print(f"requests(trust_env=False): FAILED - {e}")

# 测试4: 检查系统代理设置
print("\n=== 测试4: 系统代理检查 ===")
import urllib.request
proxy_handler = urllib.request.getproxies()
print(f"系统代理设置: {proxy_handler}")

# 测试5: curl 子进程
print("\n=== 测试5: curl 子进程 ===")
import subprocess
result = subprocess.run(
    ['curl', '-s', '-m', '10', '-o', '/dev/null', '-w', '%{http_code}',
     'http://192.168.31.152:12345/v1/embeddings',
     '-X', 'POST',
     '-H', 'Content-Type: application/json',
     '-d', '{"model":"test","input":"hello"}'],
    capture_output=True, text=True
)
print(f"curl exit code: {result.returncode}")
print(f"curl http_code: {result.stdout}")
if result.stderr:
    print(f"curl stderr: {result.stderr}")
