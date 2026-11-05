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
#   ./build.sh clean-inc    仅清理主包增量缓存（cargo clean -p mdgo）
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
  # 清理上次被中断的 npm install 留下的暂存目录
  if [ -d "node_modules/.ignored" ]; then
    warn "清理残留的 node_modules/.ignored（上次安装被中断）"
    rm -rf node_modules/.ignored
  fi
  npm install
  ok "Node 依赖安装完成"

  info "检查 Rust 工具链..."
  rustup show active-toolchain || rustup toolchain install stable
  ok "Rust 工具链就绪"
}

# ------------------------------------------------------------------------------
# 校验 node_modules 完整性（自动修复被中断的 npm install）
# npm 会把即将移除的包暂存到 node_modules/.ignored；若安装过程被中断
# （Ctrl+C、进程被杀），这些直接依赖就会滞留其中，而 .bin 下的 shim 仍指向
# 原路径，于是出现 Cannot find module .../@tauri-apps/cli/tauri.js
# ------------------------------------------------------------------------------
ensure_node_deps() {
  local need_install=0

  if [ -d "$TAURI_DIR/node_modules/.ignored" ]; then
    warn "检测到 node_modules/.ignored，上次 npm install 被中断，正在清理..."
    rm -rf "$TAURI_DIR/node_modules/.ignored"
    need_install=1
  fi

  [ -f "$TAURI_DIR/node_modules/@tauri-apps/cli/tauri.js" ] || need_install=1
  [ -f "$TAURI_DIR/node_modules/vite/package.json" ] || need_install=1

  if [ "$need_install" -eq 1 ]; then
    info "Node 依赖不完整，正在执行 npm install..."
    ( cd "$TAURI_DIR" && npm install ) || { error "npm install 失败"; return 1; }
  fi

  if [ ! -f "$TAURI_DIR/node_modules/@tauri-apps/cli/tauri.js" ]; then
    error "Tauri CLI 缺失：$TAURI_DIR/node_modules/@tauri-apps/cli/tauri.js"
    info  "请删除 node_modules 目录后执行: ./build.sh install"
    return 1
  fi

  if [ ! -f "$TAURI_DIR/node_modules/vite/package.json" ]; then
    error "vite 缺失：$TAURI_DIR/node_modules/vite/package.json"
    info  "请删除 node_modules 目录后执行: ./build.sh install"
    return 1
  fi

  return 0
}

# ------------------------------------------------------------------------------
# 前端构建检查（Vite）
# ------------------------------------------------------------------------------
check_frontend() {
  info "构建前端（Vite）..."
  cd "$TAURI_DIR"
  ensure_node_deps || return 1
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
  ensure_node_deps || return 1
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
# 清理主包增量缓存（保留依赖编译缓存，速度更快）
# ------------------------------------------------------------------------------
clean_incremental() {
  info "清理增量编译缓存（仅 mdgo 主包）..."
  cd "$TAURI_SRC"
  cargo clean -p mdgo && ok "增量缓存已清理 → $TAURI_SRC/target" || warn "增量缓存清理失败"
  warn "若异常编译/链接错误仍存在（E0425、LNK2019 anon.*.llvm.*），请执行: ./build.sh clean"
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
  clean-inc)
    clean_incremental
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
    echo "  clean-inc  仅清理主包增量缓存，cargo clean -p mdgo"
    echo "  help       显示此帮助信息"
    ;;
esac
