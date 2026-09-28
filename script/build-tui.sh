#!/bin/sh
# 重建「带 Ante logo 的客户端」。
#
# 为什么需要它：首页那个大字 logo 是客户端里**写死的常量**（不是服务端给的，垫片喂不进去），
# 所以改 logo 只能改源码、再用源码编一个自己的二进制。补丁只有一处：
#   vendor/opencode/packages/tui/src/logo.ts
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
