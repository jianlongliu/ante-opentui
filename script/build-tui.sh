#!/bin/sh
# 重建「带 Ante logo 的客户端」。
#
# 为什么需要它：两处界面改动垫片喂不进去，只能在源码里改、再用源码编一个自己的二进制：
#   vendor/opencode/packages/tui/src/logo.ts            —— 首页大字 logo 换 Ante 面具 + ANTEX
#   vendor/opencode/packages/tui/src/component/dialog-model.tsx —— 模型选择器永远显示供应商
#                                                                 （antex 不提供 integration，connected() 恒假，
#                                                                  上游会因此把分组和供应商名全隐掉）
#   vendor/opencode/packages/tui/src/config/keybind.ts + routes/session/index.tsx
#                                                              —— ctrl+s 把排队的那条插进正在跑的回合
#                                                                 （对齐 Ante 自己的 Ctrl+S；上游 v2 没这个键位）
#   vendor/opencode/packages/tui/src/routes/session/index.tsx  —— 会话在跑、却还没有任何内容时补一行
#                                                                 「Thinking」占位（首个 token 前那段空窗）
#
# 上游更新（git subtree pull ... v2）之后跑一次这个脚本，然后 antex 用的就是新的。
set -eu

repo="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.local/share/mise/shims:$PATH"   # bun
export http_proxy="${http_proxy:-http://127.0.0.1:7890}"    # 依赖安装要代理
export https_proxy="${https_proxy:-http://127.0.0.1:7890}"

cd "$repo/vendor/opencode/packages/cli"
bun run script/build.ts --single --skip-web-ui

built="$repo/vendor/opencode/packages/cli/dist/cli-linux-x64/bin/opencode"
ln -sfn "$built" "$HOME/.local/bin/antex-tui"
echo "好了：~/.local/bin/antex-tui -> $built"
echo "（antex 默认用它；想临时切回官方二进制：ANTEX_CLIENT=opencode2 antex）"
