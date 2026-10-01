# 客户端补丁与上游魔改

> 本文是 [README](../README.md) 的细节分册（2026-10-02 拆出）：自编客户端打了哪些补丁、摘掉了哪些入口、怎么跟上游同步。

## 首页 logo（改过客户端）

首页那个大字 logo 是**客户端里写死的常量**（`packages/tui/src/logo.ts`）——不是服务端给的，**垫片喂不进去**；而官方二进制（203MB）**内容是压缩的**，也没法做字节替换。所以这一处只能**改源码、自己编客户端**：

```sh
./script/build-tui.sh        # 编本机平台，产物 ~177MB，约 1 分钟
```

- 补丁清单（`packages/client/src/solid/data.ts` 那处是两改）：
  - `packages/tui/src/logo.ts` —— 首页大字 logo 换成 Ante 面具 + `ANTEX`；
  - `packages/tui/src/component/dialog-model.tsx` —— 模型选择器的条目**永远带供应商**（分组标题 + 过滤时行尾的供应商名）。上游把这一栏压在 `connected()` 后面，而那个判断看的是 integration 连接数；antex 没有 integration 概念、恒假，于是 81 个模型的列表里同名条目完全分不出来。
  - `packages/tui/src/config/keybind.ts` + `routes/session/index.tsx` —— 加 `queued_prompt.steer` = **`ctrl+s`**（对齐 Ante 自己的「把排队消息插进正在跑的回合」；上游 v2 没这个键位），命令体复用客户端的 `mutatePending("steer", queuedPrompts()[0].id)`。
  - `packages/tui/src/routes/session/index.tsx` —— 首 token 前那段空窗补一行 `⠦ Thinking` 占位（会话 running 且没有任何未完成的助手消息时显示；有内容即撤）。
  - `packages/tui/src/feature-plugins/home/footer.tsx` —— 首页**右下角的版本号**改成 `ante <后端版本> · antex <构建日期>`，取自 `/api/info`（`createResource` 拉一次；拉不到才回落到客户端自己的版本）。上游那行画的是 `app.version`＝客户端二进制的版本，既不是 Ante 的也不是 antex 的，而它又是写死在界面里的，所以同样只能改源码。**验证法**：`script/tui_drive.py --dump /tmp/x.txt` 抓首页帧（不发键就只等 `--after`），右下角出 `ante 0.2.5 · antex 2026-09-29`。
  - `packages/cli/src/services/server-connection.ts` —— `--server` 模式下**不再对垫片报版本不匹配**。垫片答的是 Ante 的版本（`0.2.5`）、还带一个 `antex` 字段，跟客户端版本永远不等；上游那行 warning 会直接打在 TUI 首行、常驻不退（截图和日常观感都被它毁掉）。判断改成「带 `antex` 字段就静默」，只有连到真 server 且版本确实不同才警告。
  - `packages/tui/src/routes/session/index.tsx` —— 待递送的消息在正文下方挂一枚**斜体 `Pending...`** 角标（`ctx.pendingDelivery(id)` 有值就是还没送到），队列 dock 的 `N queued` 改成 `N Pending... · 内容`。**判据在垫片那边**（见 [implementation.md](implementation.md)），客户端只负责画。
  - `packages/tui/src/context/keymap.tsx` —— `useCommands()` 里加一张黑名单（按 slash 名 / 命令 id 匹配），把 Ante 做不了的命令从**命令面板与斜杠补全一次摘掉**（两处都从这份 entry 列表生成，所以只改这一处）。名单：`mcps` `connect` `status` `pair` `reload` `share` `rename` `fork` `unshare` `undo` `redo` `copy` `export` `skills` `worktrees` `terminal` `update` `restart` `session.background`。
  - `packages/client/src/solid/data.ts`（两处，见 [implementation.md](implementation.md) 的「记录回读」）—— ① `message.sync` 不再把「这次 read 没提到的行」删掉，保留后**按 `time.created` 插回原位**（`mergeHeldRows`：一律追加到末尾会把还没被 Ante 读走的那条挂在模型后续步骤**下面**，看着像顺序错）；垫片那份 transcript 来自 Ante 的日志，客户端自己从事件折出来的行（idle / compaction / 切换 / 正在流的那一步 / 待递送的 prompt）不在里面，原先的 reconcile 一读就抹。② `editText` 在没有 text part 时补建一个空 part，使重读忽然插进「一句话正流到一半」时后续 delta 仍有处可落（否则那一步的回答永远不显示）。
  - `packages/tui/src/feature-plugins/sidebar/footer.tsx` —— 侧栏那张 `Getting started / Connect provider` 卡片不再渲染：它读 `/api/integration`（垫片空），点「Connect provider」进的是空对话框，纯死路（同文件里那个工作目录行是好的，留着）
- 编出来的客户端装在 `~/.local/bin/antex-tui`，**`antex` 默认就用它**（源码模式启动不慢，且底部没有 dev 模式的 `✓ Server ○ UI…` 那行）。
- **上游更新后**（`git subtree pull`）跑一次 `script/build-tui.sh` 即可；两步合一就是下面那条 `./script/upgrade-upstream.sh`。
- 想临时切回官方二进制：`ANTEX_CLIENT=opencode2 antex`。

## 屏蔽做不了的入口

Ante 没有 VCS/diff、MCP、revert、分享、PTY、provider OAuth。垫片对这些一律答 **200 + 空**（落进通用 fallback 也是 200），**不报错**——所以它们的坏法是「点了没反应」或空面板，不是弹错误框。这类入口分四层摘：

| 层 | 怎么摘 | 覆盖 |
| --- | --- | --- |
| 上游插件 | `cli.json` 的 `plugins` 写 `"-opencode.<id>"` | `-opencode.diffs`（`/diff`）、`-opencode.stats`（`/stats`）、`-opencode.plugins`（`/plugins`）、`-opencode.btw`（`/btw`）、`-opencode.sidebar.mcp` |
| 键位 | `cli.json` 的 `keybinds` 设 `"none"` | `session.undo`/`redo`/`export`/`background`、`terminal.toggle`/`select`/`close`（`diff.open`、`mcp.list`、`provider.connect`、`session.fork`、`session.share` 上游本来就是 `none`） |
| 命令本体 | 客户端补丁 `context/keymap.tsx` 的黑名单 | 上游把内建斜杠命令**从 keymap 注册表直接摊出来**，配置里没有过滤字段，只能改源码（见本节上面的补丁清单） |
| 整块面板 | `cli.json` 的 `session.sidebar: "hide"` 可关掉侧栏 | 侧栏里那张 `Getting started / Connect provider` 卡片，由补丁清单里的 `sidebar/footer.tsx` 那处摘掉 |

本机 `~/.config/opencode/cli.json` 现在长这样（改前备份到 `~/.local/state/backups/.config/opencode/cli.json.bak-<后缀>`；**这份文件会被客户端整份重写**，见 [implementation.md](implementation.md) 的「贴图」那节）：

```json
{
  "$schema": "https://opencode.ai/v2/cli.json",
  "diffs": { "wrap": "word" },
  "session": { "sidebar": "auto", "scrollbar": false, "thinking": "hide", "image_preview": true },
  "tabs": { "mode": "off" },
  "animations": true,
  "plugins": ["./herdr-opencode", "-opencode.diffs", "-opencode.stats", "-opencode.plugins", "-opencode.btw", "-opencode.sidebar.mcp"],
  "prompt": { "image_preview": true },
  "keybinds": { "session.undo": "none", "session.redo": "none", "session.export": "none", "session.background": "none", "terminal.toggle": "none", "terminal.select": "none", "terminal.close": "none" }
}
```

⚠ **靠文件的那两层丢了就回面板**（症状：`/diff`、`/stats`、`/plugins`、`/btw` 重新出现在面板里，`<leader>t`、`<leader>u` 这些键又活了）⇒ 文件里要有，垫片（`.6` 起）也把它们钉住。**`keybinds` 那层不是装饰**：`config/keybind.ts` 给 `session.undo/redo/export`、`terminal.toggle/select/close`、`session.background` 都配了默认键（`<leader>u/r/x/t/down/up`、`ctrl+b`），而客户端补丁的黑名单只把它们从**面板**里摘掉，**按键照样触发**——那些请求垫片一律答 200 + 空，点了就是静默无反应。

**不摘的**：`/cd`（家目录下能用）、`/editor`、`/timeline`、`/variants`、`/themes`、`/settings`、`/debug`、`/open`、`/sessions`——要么纯客户端、要么垫片答得出真数据。

**验收**：进 TUI 敲 `/un`、`/for`、`/mcps`、`/up` 应全是 `No matching commands`；`/` 列表里不该出现 `undo` `redo` `share` `unshare` `fork` `rename` `copy` `export` `skills` `worktrees` `terminal` `update`（这些走客户端补丁的黑名单，与配置文件无关）。**靠 `cli.json` 那两层摘的**（`/di`、`/st`、`/pl`、`/btw`）同样不该出现，只是它们不在补丁黑名单里——文件里那批 `-opencode.*` 一旦丢了就回到面板，见上一节的 ⚠。**残留**：像 `/rename`、`/stats`、`/skills` 这种「其实做得到」（写 `meta.json` / 读 Ante 自己的 usage 与技能目录）先按做不了摘了，要恢复就照 [README](../README.md)「状态」清单里对应那条的补法做。

## 客户端现状（本机）

**日常用的是我们自己编的那份**（源码在 `vendor/opencode/`，改了 logo）：

| 命令 | 解析到 | 说明 |
| --- | --- | --- |
| `~/.local/bin/antex-tui` | `vendor/opencode/packages/cli/dist/cli-linux-x64/bin/opencode` | **我们的构建**，`antex` 默认用它；版本显示 `0.0.0-master-<日期>`；随时可用 `./script/build-tui.sh` 重建 |
| `opencode` / `opencode2` | `~/.opencode/bin/…` | 官方 2.0.18，**留作兜底**：`ANTEX_CLIENT=opencode2 antex`，或删掉 `antex-tui` 软链即回退 |

- **官方那份别删**：`~/.opencode/bin` 是官方脚本的安装位置（删掉就没有官方 v2 可回退了）。
- 升级官方用 `opencode upgrade`（自带）；我们那份随 `git subtree pull` + `./script/build-tui.sh` 跟上。
注意：omarchy 的 `stable-mirror` 冻结在 2026-09-08，pacman 看不到 v2。

## 魔改上游 TUI

上游 opencode 以 **git subtree** 放在 `vendor/opencode/`（分支 `v2`，约 7.8k 文件）。

```sh
### 改完跑起来（入口在 packages/cli）
cd vendor/opencode
bun install                    # 首次，需代理
bun run dev -- --server http://127.0.0.1:41999
```

**依赖**：Bun（本机 `1.4.2`，mise 装的，`bun` 在 `~/.local/share/mise/shims`）。

**补丁只压一条缝**：改动集中在上游「TUI 调后端」的那层（`packages/client/`，生成式 HTTP client 所在），
`packages/tui/` 的界面代码尽量不动。这样上游更新等于 rebase 一小块，冲突可控；
散着改则每次合并都要打仗。

**跟上游的例行步骤**（上游源码在仓库里，就等于有了契约测试）：

```sh
./script/upgrade-upstream.sh             # 一条命令：拉 v2 子树 + 重建客户端
cargo build --release                    # 垫片（源码有改动才需要）
./target/release/antex serve 41999 &
cd vendor/opencode && bun run dev -- --server http://127.0.0.1:41999
```

`script/upgrade-upstream.sh` 干的就是下面两件事，只是省得你记得第二步（忘了它的症状是「上游明明更新了、界面还是旧的」）：

```sh
git subtree pull --prefix=vendor/opencode https://github.com/anomalyco/opencode v2 --squash
cd vendor/opencode && bun install        # 依赖有变时才需要
cd - && ./script/build-tui.sh
```

跑起来看界面有没有坏——**垫片假装的是接口，上游改了接口既不会有合并冲突、也不会有类型检查**，
只能靠「拿真客户端跑一遍」当场发现。这是本方案唯一的防漂移手段，别省。



**体积**：工作树约 159M，其中宣传视频（`packages/console/app/src/asset/lander/*.mp4`）与 `artifacts/` 占大头；
`.git` 经 `git gc --prune=now` 回收后约 84M。裁剪这些文件会让 subtree 每次都冲突，故保持原样。
