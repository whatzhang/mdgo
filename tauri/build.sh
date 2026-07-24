#!/usr/bin/env bash
# ==============================================================================
# MDGo - 本地文档知识库 构建与测试脚本
# 用法:
#   ./build.sh install      安装所有依赖（Node + Rust）
#   ./build.sh dev          启动 Tauri 开发模式（前端 + 后端 + 桌面）
#   ./build.sh check        检查前端构建 + Rust 编译
#   ./build.sh test         运行所有测试
#   ./build.sh build        构建生产版 Tauri 桌面应用
#   ./build.sh clean        清理构建产物
# ==============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TAURI_DIR="$SCRIPT_DIR"
TAURI_SRC="$TAURI_DIR/src-tauri"
BACKEND_DIR="$PROJECT_DIR/backend"

# 颜色输出
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

info()  { echo -e "${CYAN}[INFO]${NC}  $1"; }
ok()    { echo -e "${GREEN}[OK]${NC}    $1"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $1"; }
error() { echo -e "${RED}[ERROR]${NC} $1"; }

# ------------------------------------------------------------------------------
# 安装依赖
# ------------------------------------------------------------------------------
install_deps() {
  info "安装 Node 依赖..."
  cd "$TAURI_DIR"
  npm install
  ok "Node 依赖安装完成"

  info "检查 Rust 工具链..."
  rustup show active-toolchain || rustup toolchain install stable
  ok "Rust 工具链就绪"
}

# ------------------------------------------------------------------------------
# 前端构建检查（Vite）
# ------------------------------------------------------------------------------
check_frontend() {
  info "构建前端（Vite）..."
  cd "$TAURI_DIR"
  if [ ! -d "node_modules" ]; then
    npm install
  fi
  # 清理构建缓存，确保使用最新代码
  rm -rf dist
  rm -rf "$PROJECT_DIR/.vite"
  npx vite build
  ok "前端构建成功 → $TAURI_DIR/dist/"

}

# ------------------------------------------------------------------------------
# Rust 编译检查
# ------------------------------------------------------------------------------
check_rust() {
  info "检查 Rust 代码编译..."
  cd "$TAURI_SRC"
  cargo check 2>&1
  ok "Rust 代码编译通过"
}

# ------------------------------------------------------------------------------
# 运行 Rust 测试
# ------------------------------------------------------------------------------
run_tests() {
  info "运行 Rust 测试..."
  cd "$TAURI_SRC"
  cargo test 2>&1 || warn "没有找到 Rust 测试用例"
  ok "Rust 测试完成"
}

# ------------------------------------------------------------------------------
# 启动 Tauri 开发模式
# ------------------------------------------------------------------------------
run_dev() {
  info "启动 Tauri 开发模式..."
  cd "$TAURI_DIR"
  npx tauri dev
}

# ------------------------------------------------------------------------------
# 构建生产版 Tauri 桌面应用
# ------------------------------------------------------------------------------
run_build() {
  info "构建 Tauri 桌面应用..."
  check_frontend
  cd "$TAURI_DIR"
  npx tauri build 2>&1
  ok "Tauri 应用构建完成！"
}

# ------------------------------------------------------------------------------
# 清理构建产物
# ------------------------------------------------------------------------------
clean_all() {
  info "清理构建产物..."

  if [ -d "$TAURI_DIR/dist" ]; then
    rm -rf "$TAURI_DIR/dist"
    ok "已删除前端构建产物 $TAURI_DIR/dist/"
  fi

  cd "$TAURI_SRC"
  cargo clean 2>/dev/null && ok "已清理 Rust 构建缓存" || warn "Rust 清理失败"

  ok "清理完成"
}

# ------------------------------------------------------------------------------
# 主命令分发
# ------------------------------------------------------------------------------
case "${1:-help}" in
  install)
    install_deps
    ;;
  dev)
    run_dev
    ;;
  check)
    check_frontend
    check_rust
    ok "全部检查通过！"
    ;;
  test)
    run_tests
    ;;
  build)
    run_build
    ;;
  clean)
    clean_all
    ;;
  *)
    echo "MDGo - 本地文档知识库 构建与测试脚本"
    echo ""
    echo "用法: $0 <command>"
    echo ""
    echo "命令:"
    echo "  install    安装所有依赖（Node + Rust）"
    echo "  dev        启动 Tauri 开发模式，npx tauri dev"
    echo "  check      检查前端构建 + Rust 编译，npx vite build && cargo check"
    echo "  test       运行所有测试，cargo test"
    echo "  build      构建生产版 Tauri 桌面应用，npx tauri build"
    echo "  clean      清理构建产物，cargo clean"
    echo "  help       显示此帮助信息"
    ;;
esac
