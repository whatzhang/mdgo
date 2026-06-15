#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

PORT=${1:-8091}

if command -v lsof >/dev/null 2>&1; then
  EXISTING_PIDS=$(lsof -ti:"$PORT" 2>/dev/null || true)
  if [ -n "$EXISTING_PIDS" ]; then
    echo "端口 $PORT 已被占用 (pid(s): $EXISTING_PIDS)，尝试停止占用进程..."
    for p in $EXISTING_PIDS; do
      echo "停止进程 $p"
      kill "$p" 2>/dev/null || true
    done
    # 等待进程退出
    for i in {1..10}; do
      REMAINING=$(lsof -ti:"$PORT" 2>/dev/null || true)
      if [ -z "$REMAINING" ]; then
        break
      fi
      sleep 1
    done
    REMAINING=$(lsof -ti:"$PORT" 2>/dev/null || true)
    if [ -n "$REMAINING" ]; then
      echo "进程未正常退出，强制终止: $REMAINING"
      for p in $REMAINING; do
        kill -9 "$p" 2>/dev/null || true
      done
      sleep 1
    fi
    echo "端口 $PORT 已释放，继续启动"
  fi
fi

if [ -f ../.venv/bin/activate ]; then
  source ../.venv/bin/activate
fi

LOGFILE="server_${PORT}.log"
echo "正在启动服务 (端口 $PORT)... (日志 -> $LOGFILE)"
nohup python -m uvicorn main:app --host 0.0.0.0 --port "$PORT" > "$LOGFILE" 2>&1 &
PID=$!

sleep 1
if kill -0 "$PID" >/dev/null 2>&1; then
  echo "已启动 (pid $PID, 端口 $PORT)"
else
  echo "启动失败，请检查日志 $LOGFILE"
  exit 1
fi