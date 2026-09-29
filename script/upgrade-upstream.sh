#!/bin/sh
# 一条命令跟上上游：拉 opencode 的 v2 分支 + 重建我们那份客户端。
#
# 为什么需要它：这两步本来散在 README 里（`git subtree pull` 在「上游同步」一节、
# `script/build-tui.sh` 在「首页 logo」一节）。少跑第二步的症状是「上游明明更新了，
# 界面还是旧的」，而少跑第一步则是「客户端更新了，却要的是我们没有的接口」。
#
# 用法：./script/upgrade-upstream.sh
set -eu

repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo"

# 拉上游走 GitHub；编依赖走 crates.io/bun。本机这两个都得过代理。
export http_proxy="${http_proxy:-http://127.0.0.1:7890}"
export https_proxy="${https_proxy:-http://127.0.0.1:7890}"

upstream="https://github.com/anomalyco/opencode"
branch="v2"

echo "==> 1/2 拉上游 opencode（$branch，squash 进 vendor/opencode）"
git subtree pull --prefix=vendor/opencode "$upstream" "$branch" --squash

echo "==> 2/2 重建客户端（带 Ante logo 的那份）"
./script/build-tui.sh

echo "==> 完成：~/.local/bin/antex-tui 已更新，antex 下次启动就用它。"
echo "    （antex 本身若也动了源码，记得 cargo build --release）"
