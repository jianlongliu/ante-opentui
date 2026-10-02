# antex（项目 ante-opentui）

> 最后核对：2026-10-02 · 目标客户端 opencode 2.0.18 · Rust 1.98.1

把 **opencode v2 自带的 TUI** 接到 **Ante** 后端上运行。

做法是**实现 opencode v2 要求的 server API**，让 `opencode --server <url>` 分辨不出真假；
opencode 的界面、主题、键位一行不改，Ante 提供数据。思路同「改接口，不改消费者」。

![antex 首页](docs/home.png)

首页（本仓库自编客户端的默认界面）：顶栏是 Ante 面具 + `ANTEX`，composer 下是权限模式，右下角是 `ante <后端版本> · antex <构建日期>`。**这张图由 `script/shot-home.py` 生成**（PTY + pyte 抓屏 → Pango 渲染）：`ANTE_HOME=<空目录> ANTEX_MODEL= ANTEX_PROVIDER= uv run --with pyte script/shot-home.py --out docs/home.png`——截图里刻意只留 `Auto`，不带任何模型 / 供应商名（模型名是本机 catalog 的事，不该进仓库）。

## 状态：已实现 / 待定 / 遗弃

**绿勾 = 已实现（实测过）；空框 = 待定（做得到，没做）；删除线 = 遗弃（Ante 底层没这个概念，别再提议硬做）。**逐条细节与验证法在 [docs/implementation.md](docs/implementation.md)。

### 已实现

- [x] 起界、提示词送达、真 Ante 回复、流式文本、推理、工具调用、审批、失败可见 —— 全由垫片喂真数据
- [x] 会话：列表、恢复、恢复后继续、删除（`Ctrl+D` 两下）—— 删的是 Ante 自己那条存档目录；**打开一条会话即从它的 `events.jsonl` 重建整份记录**（每一步的思考、回答、工具都在，见 [docs/implementation.md](docs/implementation.md) 的「记录回读」）
- [x] 插嘴与排队（`Ctrl+S`）+ 斜体 `Pending...` 标记 —— 送到时机按 Ante 的真实边界判：**收下不算，下一个 step 开始才算模型读到**
- [x] `/compact`、模型选择、`shift+tab` 权限模式、`@` 文件补全、贴图、herdr 上报、boot 接口全套
- [x] `/rename` —— 走 Ante 自己的 `SessionUpdate.title`（真标题：别的前端也看得到）；只有**当前这条**会话能这样改，别的会话改名落垫片自己的 `title.txt`，会话列表读的就是它
- [x] `/copy`、`/export` —— `GET …/experimental/session/{id}/export` 直接回垫片手上那份转录（回读自 `events.jsonl`）；`sanitize` 忽略——载荷本来就是 Ante 的日志，不重写路径
- [x] `/stats` —— 数字全部来自 Ante 自己的 `meta.json`：会话数、token、活动日历、按 provider/model 分组；Ante 没写的报 0（协议里没有报价，也没有子代理计数）
- [x] `/skills` —— 优先用 Ante 播报的技能清单（那是真能调用的集合，含项目域与 `no_skills`），首轮提示词之前才退到磁盘扫描
- [x] 多标签**临时**关掉（`tabs.mode = "off"`，只是个配置项，不是能力）—— 它要服务端同时驱动多条会话，垫片只有一条连接；真做多标签见「待定」那条
- [x] 做不了的入口从界面摘掉 —— 插件 / 键位 / 命令黑名单 / 侧栏卡片四层，见 [docs/client-patches.md](docs/client-patches.md) 的「屏蔽做不了的入口」
- [x] 断线重连，且掉线期间排队的消息不丢 —— 退避重连 + `health` 说真话 + 转录里一行说明；**回合没跑完就被杀掉的会话（盘上只有 `events.jsonl`）Ante 打不开**，这种会话会新开一条继续，见 [docs/implementation.md](docs/implementation.md) 那条

### 待定

- [ ] `/rename` —— 隐藏中；补法：写 Ante 那条会话的 `meta.json` 标题
- [ ] `/copy`、`/export` —— 隐藏中；补法：垫片用 `GET …/message` 拼一份导出载荷
- [ ] `/stats` —— 隐藏中；补法：Ante 的 `meta.json` 里有 usage，够算 token / 成本
- [ ] `/skills` —— 隐藏中；补法：列出 Ante 自己的技能目录（`~/.agents/skills`、`~/.ante/.system/skills`）
- [ ] 会话内 `/cd` —— **Ante 没有换目录的接口**：目录在建会话时由 `SessionRequest.cwd` 定死，`SessionUpdate` 里没有这个字段，所以现在会明确报错（400 + 原因）而不是静默；退一步是「按新目录重开会话」（丢历史、会话列表多一行）
- [ ] `/rename` —— 隐藏中；补法：写 Ante 那条会话的 `meta.json` 标题
- [ ] `/copy`、`/export` —— 隐藏中；补法：垫片用 `GET …/message` 拼一份导出载荷
- [ ] `/stats` —— 隐藏中；补法：Ante 的 `meta.json` 里有 usage，够算 token / 成本
- [ ] `/skills` —— 隐藏中；补法：列出 Ante 自己的技能目录（`~/.agents/skills`、`~/.ante/.system/skills`）
- [ ] 真多标签（session tabs）—— **待定**：先验证 Ante 侧能否并发驱动多条会话，再谈垫片改造（现在只是把入口临时关掉）
- [ ] 断线时的终端还原 —— TUI 只弹 `Connection lost · Reconnecting to the server automatically.`，终端留一屏裸 SGR；抓原始字节：用 PTY 跑一次（`script/tui_drive.py` 就是这条路子），把读到的 chunk **先原样落盘再喂 `pyte`**——SGR 序列只在原始字节里看得见

### 遗弃

- [ ] ~~`/undo`、`/redo`、消息菜单的 Revert~~ —— Ante 协议 18 个 op（`StartSession`…`Shutdown`）没有 revert/undo/rewind，`~/Projects/ante` 全仓 grep 零命中；硬做只剩重写 `events.jsonl` 截断历史——只对以后 resume 生效、对当前会话无效、还可能被 Ante 覆写
- [ ] ~~`/fork`~~ —— 无分叉 op
- [ ] ~~`/share`、`/unshare`~~ —— 云端分享，Ante 无此概念（客户端自己也直接报 "Sharing is not implemented for V2 sessions yet"）
- [ ] ~~`/mcps`、`/status`、MCP 面板~~ —— 无 MCP
- [ ] ~~`/connect`、`/pair`~~ —— 无 provider OAuth、无设备配对（`/api/integration` 恒空）
- [ ] ~~`/diff`、`/worktrees`~~ —— 无 diff / VCS / worktree 概念（`/api/vcs/*` 只能答空；`/api/vcs` 因此答**空分支** `{branch:{}}`，位置标签才不会挂上假 `:main`）
- [ ] ~~`/terminal`、Terminals 面板、`!` shell 模式~~ —— 无 PTY、无 shell 会话 op（Bash 是工具调用，不是终端）
- [ ] ~~`/plugins`、`/btw`~~ —— 无 opencode 插件系统；`/btw` 要的是「旁问」的独立 generate op
- [ ] ~~`/reload`、`/update`、`/restart`、`Ctrl+B` 后台化工具~~ —— 重新加载服务端配置 / 更新 opencode / 把工具调用扔后台，对 Ante 都无意义（**暂时遗弃**：等哪天有对应 op 再说）
- [ ] ~~LSP、formatter~~ —— 无对应概念，打桩显示为空（入口已摘；**暂时遗弃**）
- [ ] ~~子代理面板 / 点进子代理~~ —— 客户端的子代理 UI（内联块点进去看子会话、composer 的 subagents tab、mini 的 footer inspector）**整套都挂在「子代理 = 独立 child session」上**（`session.parentID` + 自己的事件流 + `sessionFamily()`）。Ante 没有这个概念：子代理只是父会话里的一次 `Agent` 工具调用，内部活动不推给前端——本机 188 条会话日志里 `ToolUpdate` 事件 **0 条**，一次 68 秒的 `Agent` 调用（14:42:20→14:43:28）中途**零事件**，结果只在 `ToolEnd.result_json.report` 里一次交付；协议里也没有 agent id / parent turn（`SessionInfo.subagents` 只是可委派清单）。硬造 child session 只有空壳，还会污染会话列表。能做的只有把 `Agent` 调用按客户端的 subagent 块渲染（已做）

## 细节去哪了

本 README 只留「是什么、怎么跑、现状」；实现过程、坑与验收按主题分册：

| 文件 | 内容 |
| --- | --- |
| [docs/implementation.md](docs/implementation.md) | 实现清单（逐条实测）· 未完成项的验证法 · 审批 · 验收与排错开关 · 下一步 |
| [docs/pitfalls.md](docs/pitfalls.md) | 关键坑 · v2 API 约定与事件名 · Herdr 集成 · 权威出处 |
| [docs/client-patches.md](docs/client-patches.md) | 客户端补丁（首页 logo、Pending 角标、命令黑名单…）· 屏蔽做不了的入口 · 客户端现状 · 魔改上游 TUI |

## 它在你系统里的位置

```
opencode 的 TUI（本仓库自己编的，改了 logo）      ← 界面
        ↕  opencode 协议（本仓库实现的「垫片」）
antex（垫片，本仓库，一个二进制）
        ↕  ante-sdk / `ante serve --stdio`
Ante（**你自己装的**：官方脚本装、`ante update` 升级）
```

**Ante 不在本仓库里，也不用管它**——你装你的、它更它的，垫片按它的协议趴在上面。
唯一耦合点就是协议：若 Ante 升级改了协议，改垫片即可（通常只动 `Cargo.toml` 里的 `ante-sdk` 版本 + `cargo build`）。

## 为什么走这条路

手写复刻 opencode 的 TUI 到不了它的完成度（它有 17k 行的界面层）。反过来做，垫片是**可丢弃**的一层：
上游界面升级时，重新拉 TUI 即可，只需跟着修垫片。代价是 Ante 没有的概念（LSP、MCP、formatter、OAuth、git 操作）必须打桩。
思路同「改接口，不改消费者」（参照 `onarchi`——把 Omarchy 真移植到 niri 的那个仓，原名 omarchy-on-niri）。

几条定下来的选择：

- **Rust 写垫片**：它是长驻进程，要稳、要单文件产物。
- **上游进同一个仓**（`vendor/opencode/`，git subtree，分支 `v2`）：升级即拉上游，本质就是打补丁。
- **只做垫片、不 fork 界面**：日常使用不必依赖自建前端（源码模式要 3.1G `node_modules`），**只有要改界面本身才必须 fork**。
- **映射原则**：Ante 真有对应概念的才做成真差别（`shift+tab`→权限模式、模型选择→provider/model）；没有的（LSP/formatter/VCS）打桩显示为空，不假装。

## 怎么跑

装一次（软链到 `PATH`，之后任意目录可用）：

```sh
cd ~/Projects/ante-opentui
export http_proxy=http://127.0.0.1:7890 https_proxy=http://127.0.0.1:7890   # 编译要代理
cargo build --release
ln -sfn "$PWD/target/release/antex" ~/.local/bin/antex
```

日常就一条命令——**起服务并直接进 TUI，退出时服务一起停**：

```sh
antex
```

想分开（一个终端起服务、另一个连）也行：

```sh
antex serve 41999                             # 终端 A：只起服务，留在前台
opencode2 --server http://127.0.0.1:41999     # 终端 B
```

- `antex PORT` 指定端口（默认 41999）；`antex -h` 看用法。
- **可以多开**：一体化模式下端口被占会**自动往后顺延**（并打印改用哪个），所以第二个、第三个 `antex` 直接跑就行。`serve` 模式相反——按你给的端口，被占就明确报错（换端口会让人连错服务）。
- 环境变量：`ANTE_BIN` 指定 `ante` 可执行文件（默认 `$PATH`，再退到 `~/.ante/bin/ante`）；`ANTEX_CLIENT` 指定客户端（默认 `~/.local/bin/antex-tui`，再退到 `opencode2` → `opencode`）。**所以不必先 export PATH。**
- **一体化模式下请求日志写 `/tmp/antex.log`**——写 stdout 会糊在 TUI 上；`serve` 模式仍打在 stdout。
- **启动自检**：`antex` 起来时先核对 Ante —— 找不到 `ante` 就直接报错退出（否则 TUI 能开、消息全石沉大海）；`ante --version` 与二进制里编进去的 `ante-sdk` 版本对不上就**警告**（这正是「界面在跑但没反应」最常见的成因）。运行期连不上、事件流断掉、消息被丢弃也都写同一处日志（`/tmp/antex.log`），不留哑谜。想看是否通过：`tail /tmp/antex.log`。

## 依赖与版本

工具链（本机实测，pacman 的 `rust` 包）：

| 项 | 版本 |
| --- | --- |
| rustc / cargo | **1.98.1**（Arch `rust 1:1.98.1-1`） |
| edition | **2024**（`let` 链语法需要，改回 2021 会编译失败） |

Cargo 依赖（`Cargo.lock` 实际解析值，非 `Cargo.toml` 的约束范围）：

| 依赖 | 版本 | 用途 |
| --- | --- | --- |
| axum | 0.8.9 | HTTP 路由与 SSE |
| tokio | 1.53.1 | 运行时（features: macros, rt-multi-thread, sync, time, process, io-util） |
| tokio-stream | 0.1.19 | 广播转事件流（**必须开 `sync` feature**，否则找不到 `BroadcastStream`） |
| serde | 1.0.229 | 派生（features: derive） |
| serde_json | 1.0.151 | 信封与事件载荷 |
| futures | 0.3.34 | 流拼接 |

外部依赖：

| 项 | 说明 |
| --- | --- |
| 网络 | 编译时 **必须走代理**，crates.io 直连被 TLS 拦（`export http_proxy=https_proxy=http://127.0.0.1:7890`） |
| opencode 客户端 | **2.0.18**，官方脚本装在 `~/.opencode/bin`（`opencode`/`opencode2` 均指向它） |
| Ante | 接后端时用 **`ante-sdk` 0.2.5**——**已发布 crates.io**，直接写 `ante-sdk = "0.2.5"` 即可。本机另有 clone 在 `~/Projects/ante`（同版本），需要魔改上游时才改走 path 引用 |

## 目录

```
docs/                本 README 的细节分册（见上「细节去哪了」）
src/                 垫片本体：路由、信封、SSE、会话/消息存储
build.rs             把编译用的 ante-sdk 版本写进二进制（启动自检要用）
script/              build-tui.sh（重建客户端）、upgrade-upstream.sh（拉上游 + 重建，一条命令）、shot-home.py（把首页渲染成 PNG，README 那张图就是它出的）、tui_drive.py（在 PTY 里驱动客户端：按秒发键 + 抓最终屏文本，验收自动化探针）
vendor/opencode/     上游 opencode（subtree，v2 分支）——魔改对象
Cargo.toml           axum + tokio + serde
```

## 致谢

**TUI、键位系统、渲染、插件机制，连「用服务端 API 驱动一个现成客户端」这条路子本身**，全部来自
**opencode**（<https://github.com/sst/opencode>）。本仓库做的只是把它 git subtree 收进来、在「TUI 调后端」那一层
打几个补丁接到 Ante 上——等于站在人家肩上，上游不再更新，这里也就跟着停。感谢 opencode 的作者和所有贡献者。

许可跟上游一致（MIT，见 `LICENSE`）：随意分发修改
