#!/usr/bin/env bash
# quill 本地 CI —— 四门验收(本地跑,不依赖远端)。
#   ./scripts/ci.sh          全门:fmt + clippy + build + test
#   ./scripts/ci.sh --fast   快门:fmt + clippy + build(跳过 test,给 pre-push 用)
#
# 为什么本地不走 GitHub Actions:quill 的渲染/Wayland e2e 测试需要本机 GPU + Wayland
# 会话,GitHub runner 无 GPU/显示器跑不了;本机(9950X3D + 5090)反而更快更全。
#
# AI 协作流程:写码 agent 写完 → 自跑 ./scripts/ci.sh 调到全绿 → 审码 agent 审 →
#             合并 main → main 上再跑全门把门。
set -uo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null)" || { echo "不在 git 仓库内"; exit 2; }

fast=0
[ "${1:-}" = "--fast" ] && fast=1
fail=0
gate() { echo; echo "── $* ──"; "$@" || { echo "✗ FAILED: $*"; fail=1; }; }

gate cargo fmt --all -- --check
gate cargo clippy --all-targets -- -D warnings
gate cargo build --all-targets
[ "$fast" -eq 0 ] && gate cargo test

echo
if [ "$fail" -ne 0 ]; then echo "❌ CI 未通过"; exit 1; fi
echo "✅ CI 全绿$([ "$fast" -eq 1 ] && echo ' (fast: 跳过 test)')"
