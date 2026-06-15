#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

PORT=${1:-8091}

PIDS=$(lsof -ti:"$PORT" 2>/dev/null || true)
if [ -z "$PIDS" ]; then
  echo "端口 $PORT 上没有运行的服务"
  exit 0
fi

echo "正在停止端口 $PORT 的服务 (pid(s): $PIDS)..."
for p in $PIDS; do
  kill "$p" 2>/dev/null || true
done

# 等待进程退出
for i in {1..10}; do
  REMAINING=$(lsof -ti:"$PORT" 2>/dev/null || true)
  if [ -z "$REMAINING" ]; then
    echo "已停止"
    exit 0
  fi
  sleep 1
done

REMAINING=$(lsof -ti:"$PORT" 2>/dev/null || true)
if [ -n "$REMAINING" ]; then
  echo "强制终止 $REMAINING..."
  for p in $REMAINING; do
    kill -9 "$p" 2>/dev/null || true
  done
  echo "已强制停止"
fi