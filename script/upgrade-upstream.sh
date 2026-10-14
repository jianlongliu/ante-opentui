#!/bin/sh
# 一条命令跟上上游：拉 opencode 主线 + 装依赖 + 重建我们那份客户端。
#
# 为什么需要它：这几步本来散在 README 里（`git subtree pull` 在「上游同步」一节、
# `script/build-tui.sh` 在「首页 logo」一节）。少跑第二步的症状是「上游明明更新了，
# 界面还是旧的」，而少跑第一步则是「客户端更新了，却要的是我们没有的接口」。
#
# ref 取上游的**主线**（默认为默认分支 `dev`）。原先跟的 `v2` 分支 **2026-10-10 就停更了**
# （版本停在 2.0.26）——上游把 v2 线并回了主线，之后按 `v2.0.x` 打 tag。想跟着某个已发布的
# 版本走就显式传 tag：
#
# 用法：./script/upgrade-upstream.sh            # 主线最新（dev）
#       ./script/upgrade-upstream.sh v2.0.29    # 某个已发布的 tag
set -eu

repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo"

# 网络：本机代理现在在路由器上做透明转发，直连就行。只有本地 mihomo 真在 7890 监听时才设代理
# ——老写法无条件设 7890，代理一搬走就把本来能通的请求塞进死端口（2026-10-13 踩到）。
if [ -z "${http_proxy:-}" ] && [ -z "${https_proxy:-}" ] && ss -ltn 2>/dev/null | grep -q '127\.0\.0\.1:7890'; then
  export http_proxy=http://127.0.0.1:7890
  export https_proxy=http://127.0.0.1:7890
fi

upstream="https://github.com/anomalyco/opencode"
ref="${1:-dev}"

echo "==> 1/3 拉上游 opencode（$ref，squash 进 vendor/opencode）"
git subtree pull --prefix=vendor/opencode "$upstream" "$ref" --squash

# 上游的 bun.lock 常跟着变；不先按 lock 装一遍，下一步可能缺包（构建脚本自己那只 `bun add`
# 补的是 node-pty 那两个，别指望它补齐别的）。
echo "==> 2/3 按 lock 装依赖"
export PATH="$HOME/.local/share/mise/shims:$PATH"
(cd vendor/opencode && bun install --frozen-lockfile)

echo "==> 3/3 重建客户端（带 Ante logo 的那份）"
./script/build-tui.sh

echo "==> 完成：~/.local/bin/antex-tui 已更新，antex 下次启动就用它。"
echo "    （antex 本身若也动了源码，记得 cargo build --release）"
