#!/bin/sh
# 重建「带 Ante logo 的客户端」。
#
# 为什么需要它：垫片喂不进去的界面改动，只能在源码里改、再用源码编一个自己的二进制：
#   vendor/opencode/packages/tui/src/logo.ts            —— 首页大字 logo 换 Ante 面具 + ANTEX
#   vendor/opencode/packages/tui/src/component/dialog-model.tsx —— 模型选择器永远显示供应商
#                                                                 （antex 不提供 integration，connected() 恒假，
#                                                                  上游会因此把分组和供应商名全隐掉）
#   vendor/opencode/packages/tui/src/config/keybind.ts + routes/session/index.tsx
#                                                              —— ctrl+s 把排队的那条插进正在跑的回合
#                                                                 （对齐 Ante 自己的 Ctrl+S；上游 v2 没这个键位）
#   vendor/opencode/packages/tui/src/routes/session/index.tsx  —— 会话在跑、却还没有任何内容时补一行
#                                                                 「Thinking」占位（首个 token 前那段空窗）
#   vendor/opencode/packages/tui/src/feature-plugins/home/footer.tsx
#                                                              —— 首页右下角的版本号改成 `ante <后端版本> · antex <构建日期>`
#                                                                 （上游画的是客户端自己的版本，跟两端都不相干）
#   vendor/opencode/packages/tui/src/routes/session/index.tsx  —— 待递送的消息挂斜体 `Pending...` 角标，
#                                                                 队列 dock 从 «N queued» 改成 «N Pending... · 内容»
#                                                                 （送没送到由垫片判：下一个 step 开始才算送到）
#   vendor/opencode/packages/client/src/solid/data.ts          —— ① 拉取 transcript 时不再丢掉「这次没提到的行」
#                                                                 （垫片那份来自 Ante 日志，客户端自己从事件折出来的
#                                                                  idle / compaction / 正在流的那一步不在里面）
#                                                              —— ② editText 在没有 text part 时补建一个，
#                                                                 使重读插进「正流到一半」的那步后 delta 仍有处可落
#                                                                 （两处都见 README「记录回读」）
#   vendor/opencode/packages/tui/src/context/keymap.tsx        —— 黑名单：把 Ante 做不了的命令从命令面板
#                                                                 和斜杠补全一次摘掉（名单见 README）
#   vendor/opencode/packages/tui/src/feature-plugins/sidebar/footer.tsx
#                                                              —— 侧栏那张 Getting started / Connect provider
#                                                                 卡片不再渲染（它读 /api/integration，恒空）
#   vendor/opencode/packages/cli/src/services/server-connection.ts
#                                                              —— --server 模式不再对垫片报版本不匹配
#                                                                 （垫片答的是 Ante 版本，那行 warning 会常驻首行）
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
