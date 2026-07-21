import socket
import sys

s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.settimeout(5)
try:
    s.connect(('192.168.31.152', 12345))
    print('SUCCESS')
    s.close()
    sys.exit(0)
except Exception as e:
    print(f'FAILED: {e}')
    sys.exit(1)
