import socket
import subprocess
import os

TARGET = '192.168.31.152'

# 测试1: 测试多个端口
print("=== 测试1: 测试多个端口 ===")
for port in [80, 443, 22, 12345, 8080, 3389]:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(3)
    try:
        s.connect((TARGET, port))
        print(f"  端口 {port}: SUCCESS")
        s.close()
    except socket.timeout:
        print(f"  端口 {port}: TIMEOUT (防火墙可能DROP)")
    except Exception as e:
        print(f"  端口 {port}: {e}")
    finally:
        s.close()

# 测试2: 用 curl 测试
print("\n=== 测试2: curl 测试 ===")
result = subprocess.run(
    ['curl', '-v', '-m', '5', f'http://{TARGET}:12345/'],
    capture_output=True, text=True
)
print(f"  curl exit: {result.returncode}")
print(f"  curl stdout: {result.stdout[:200]}")
print(f"  curl stderr: {result.stderr[:500]}")

# 测试3: 用 nc 测试
print("\n=== 测试3: nc 测试 ===")
result = subprocess.run(
    ['nc', '-zv', '-w', '3', TARGET, '12345'],
    capture_output=True, text=True
)
print(f"  nc exit: {result.returncode}")
print(f"  nc output: {result.stderr[:200]}")

# 测试4: 检查网络接口和路由
print("\n=== 测试4: 本机网络信息 ===")
result = subprocess.run(['ifconfig', 'en0'], capture_output=True, text=True)
print(result.stdout[:300])

result = subprocess.run(['route', 'get', TARGET], capture_output=True, text=True)
print(f"\n路由到 {TARGET}:")
print(result.stdout[:300])

# 测试5: ARP 表
print("\n=== 测试5: ARP 表 ===")
result = subprocess.run(['arp', '-n', TARGET], capture_output=True, text=True)
print(result.stdout[:200])
