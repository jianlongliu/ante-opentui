# antex（项目 ante-opentui）

> 最后核对：2026-09-28 · 目标客户端 opencode 2.0.18 · Rust 1.98

把 **opencode v2 自带的 TUI** 接到 **Ante** 后端上运行。

做法是**实现 opencode v2 要求的 server API**，让 `opencode --server <url>` 分辨不出真假；
opencode 的界面、主题、键位一行不改，Ante 提供数据。思路同「改接口，不改消费者」。

## 为什么走这条路

手写复刻 opencode 的 TUI 到不了它的完成度（它有 17k 行的界面层）。反过来做，垫片是**可丢弃**的一层：
上游界面升级时，重新拉 TUI 即可，只需跟着修垫片。代价是 Ante 没有的概念（LSP、MCP、formatter、OAuth、git 操作）必须打桩。

## 实现清单

**未实现（按优先级；每条验证法见下方「未完成项的验证法」）**

- [ ] **恢复后继续对话（已决定不修，用新会话代替）** —— **结论：新建会话一切正常**（发消息/工具/审批/压缩/切模型/`@` 都通），只有「**恢复旧会话后继续发**」在某些会话上无效，**用户已选择直接用新会话**，不再投入。以下是排查记录，供以后万一要碰时省得重走：

  人眼可用（用户实测：选旧会话 → 发消息 → Ante 回话 ✅）；**探针复现不出**：字进得了输入框，回车**静默无 POST、无报错**。
**人眼可用**（用户实测：选旧会话 → 发消息 → Ante 回话 ✅）；**探针复现不出**：字进得了输入框，回车**静默无 POST、无报错**。
  已排除：终端尺寸（150×45）、点击输入框、命令（`/sessions` 与 `/resume` 都试过）、会话新老、agent/model 解析（已修）、Mod+Enter。
  **已排除（每条都实测过，别再重复）**：终端尺寸（110×30 / 140×40 / 150×45）、点击输入框、Tab 切焦点、`/sessions` 与 `/resume` 两条命令、会话新老（本轮建的与上轮建的）、agent/model 解析（已修）、Mod+Enter、**kitty 键盘协议**（探针改成「回应 `\x1b[?u` 查询 + 全部按键按 `CSI <cp> u` 编码」后，**首轮仍能提交、恢复后依旧 0 POST**——所以与回车编码无关）。
  **观察到的确凿事实**：文字确实进了输入框（底部可见），回车**什么都不发生**——没有 POST、没有报错、没有 toast。客户端 `submit.ts` 里两处守卫（`submit.available()`、`readSubmission` 的 `!model||!agent`）都会**静默 return**，但探针无法定位是哪一处，因为**复现不出「能用的那次」**。
- [ ] **diff / LSP / formatter / MCP / VCS** —— 打桩。**这些是 Ante 根本没有的概念，只能显示为空，别指望填上**
- [ ] **撤销回滚、贴图、PTY、分享** —— **登记为「Ante 无对应能力」，不再尝试**。撤销回滚查证过：Ante 协议全部 18 个 op（`StartSession`…`Shutdown`）**没有 revert/undo/rewind**，`~/Projects/ante` 全仓 grep 同样零命中；opencode 那边是 `revert/stage` → `revert/commit` / `DELETE revert` 三步 + `staged/committed/cleared` 事件。硬做只剩「重写 `events.jsonl` 截断历史」——只对以后 resume 生效、对当前会话无效、还可能被 Ante 覆写，故不采用

**已实现（均已实测）**

- [x] **真 v2 TUI 起界** —— 顶栏、块字 logo、composer、页脚全部由垫片喂出
- [x] **提示词送达** —— `POST /api/session` → `POST …/model` → `POST …/prompt`
- [x] **真 Ante 回复** —— `ante-sdk` 连 `ante serve --stdio`；模型名、耗时、token 均为真实数据
- [x] **流式文本** —— `step.started` → `text.started` → `text.delta` ×N → `text.ended`
- [x] **推理（thinking）** —— `session.reasoning.started/delta/ended`，显示为 `+ Thought · 762ms`
- [x] **工具调用** —— `✓ Bash [命令, 描述]`，含参数与输出
- [x] **审批** —— Ante 暂停 → TUI 弹 `△ Permission required` → 在 TUI 批准 → `ApprovalResponse` → Ante 继续执行
- [x] **消息顺序与去重** —— `inbox.enqueued`+`delivered` 入列表；复用客户端提交自带的 message id
- [x] **多轮消息落位** —— 三条连发实测：用户与回复正确交错（前两条因排队相邻属正常）
- [x] **中断收尾** —— Esc 两下（第一下上膛、第二下中断）后 Ante 正常收尾：工具单元格变 `✗`、转轮停止
- [x] **失败可见** —— Ante 那轮失败时补发 `session.step.failed` + `session.execution.failed`（两者都必带 `error`），TUI 显示 `Error: …` 而不是空转
- [x] **会话列表 / `/sessions`** —— `GET /api/session` 读 `~/.ante/sessions/*/meta.json`（实测 225 条、时间倒序、标题=首条用户消息），**选择器实测已列出真 Ante 会话**
- [x] **恢复会话：历史渲染** —— `/sessions` 选中一条即加载历史（用户消息 + 助手回复 + 工具块）。**病根：`GET /api/session/{id}` 少了 `data` 信封**（该路由 schema 是 `{data: Session.Info}` 且 `additionalProperties:false`），客户端读 `response.data.id` 得 undefined，抛 `undefined is not an object (evaluating 'Ae.id')`，**只在界面上弹个小 toast、不换视图**——所以看着像「点了没反应」
- [x] **模型选择** —— `/api/model`、`/api/provider` 由 **Ante 的 `~/.ante/catalog.json` 驱动**（实测 81 个模型 / 9 个 provider），默认项取 `settings.json` 的 `provider`+`provider_model` 并排在首位；客户端选的模型在建会话与 `POST …/model` 两条路径都会下发 Ante（对应 `StartSession` / `UpdateSession`）
- [x] **`shift+tab` 切权限模式** —— 三项**直接用 Ante 自己的说法**（`auto`/`strict`/`yolo`，不经过 opencode 的 build/plan 再翻译），实测 composer 循环 `Auto → Strict → Yolo`，且**以 `settings.json` 里配的那个打头**；切换是真差别（`strict` 下危险命令弹审批）。**agent 是在「建会话」的 body 里传的**（`{agent, id, model, location}`），不是发消息时
- [x] **boot 接口全套** —— `health` `location` `fs/list` `agent` `provider` `model` `config` `vcs` `project` `plugin` `migration` …
- [x] **`@` 文件补全** —— 垫片**自己读文件系统**（Ante 无文件 API）：`/api/fs/list` 列目录、`/api/fs/find` 递归搜索（跳过 `.git`/`node_modules`，深度≤6、limit≤50）。实测敲 `@` 列出家目录、输入 `main.rs` 命中真文件
- [x] **次要事件** —— `session.step.streamed`（每个 step 一次，`ensure_streamed!`）、`session.renamed`（新会话首条消息定标题）、`session.model.selected`（切模型，带 `previous`，读旧值在覆写之前）。`instructions.updated` 不适用（Ante 无此概念）
- [x] **命令行参数** —— `[PORT]` / `--port PORT` / `-h|--help`；不认识的参数打印用法并以 2 退出；**端口被占给明确提示、不再 panic**
- [x] **`/compact` 压缩** —— `POST …/compact` → Ante 的 **`Op::Compact`**（真压缩，不是假动作）。**Ante 不用 `CompactStart/CompactEnd` 报进度**（那只在真做了缩减时才发），日常走 **`InfoBlockStart`/`InfoBlockAppend`（id 以 `compact` 开头）** → 映射成 `compaction.started/delta/ended`；另补 **inbox 握手**（`enqueued`+`delivered`），否则客户端把队列项一直挂在底部不落正文。实测：Ante 回「Context is already within budget; nothing to compact.」，UI 出 `Compaction` 块
- [x] **客户端实际调用的接口全覆盖** —— 对照一次完整使用过程收集到的 **32 条请求**，补齐了原先漏掉的 9 个：`/api/form`、`/api/shell`（GET+POST）、`/api/reference`、`/api/integration`、`/api/project`（**裸数组**，同 `/api/config`）、`/api/mcp/resource`（`{resources,templates}`）、`/api/vcs/base`（`data: null`）、`/api/vcs/diff`、`/api/experimental/session/{id}/terminal`（`{data}`）。Ante 没有这些数据，**但形状严格照 schema**——原先落到通用 fallback，它多带一个 `info` 字段，恰好违反这些路由的 `additionalProperties:false`，客户端会把整个响应校验掉、面板**静默为空**
- [x] **事件流（SSE）** —— `{id, type, created, data}` 帧，首帧 `server.connected`

### 未完成项的验证法

| 项 | 怎么验 | 备注 |
| --- | --- | --- |
| **恢复后继续对话** | `/sessions` 选中旧会话后**直接打字回车**；看垫片终端有没有刷出 `POST …/prompt` | **人眼：部分会话可用**；**探针：始终不行**——文字确实进了输入框（底部可见），回车后**静默无 POST、无报错**。已排除：终端尺寸（150×45 同样）、点击输入框、会话新老（本次运行建的和上轮建的都不行）、agent/model 解析（已修，仍不提交）。`submit.ts` 的 `submit.available()` 与 `readSubmission` 的 `!model||!agent` 两处守卫都会**静默 return**，尚未定位是哪一处 |
| **diff / LSP / formatter / MCP / VCS** | 敲 `/diff`、开 MCP 面板看是否空 | **Ante 根本没有这些概念**，只能显示为空，别指望填上 |
| **撤销回滚、贴图、PTY、分享** | 不用验了 | **Ante 无此能力**（详见实现清单该条的查证记录）；不要再提议硬做 |


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
- 环境变量：`ANTE_BIN` 指定 `ante` 可执行文件（默认 `$PATH`，再退到 `~/.ante/bin/ante`）；`OANTE_CLIENT` 指定客户端（默认 `opencode2` → `opencode`）。**所以不必先 export PATH。**
- **一体化模式下请求日志写 `/tmp/antex.log`**——写 stdout 会糊在 TUI 上（踩过）；`serve` 模式仍打在 stdout。

## 首页 logo（改过客户端）

首页那个大字 logo 是**客户端里写死的常量**（`packages/tui/src/logo.ts`）——不是服务端给的，**垫片喂不进去**；而官方二进制（203MB）**内容是压缩的**，也没法做字节替换。所以这一处只能**改源码、自己编客户端**：

```sh
./script/build-tui.sh        # 编本机平台，产物 ~177MB，约 1 分钟
```

- 补丁只有一处：`vendor/opencode/packages/tui/src/logo.ts`（Ante 面具 + `ANTEX`）。
- 编出来的客户端装在 `~/.local/bin/antex-tui`，**`antex` 默认就用它**（源码模式启动不慢，且底部没有 dev 模式的 `✓ Server ○ UI…` 那行）。
- **上游更新后**（`git subtree pull`）跑一次 `script/build-tui.sh` 即可。
- 想临时切回官方二进制：`ANTEX_CLIENT=opencode2 antex`。

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

## 客户端现状（本机）

官方脚本安装，`opencode` 与 `opencode2` 都指向它：

| 命令 | 解析到 | 版本 |
| --- | --- | --- |
| `opencode` | `~/.opencode/bin/opencode` | 2.0.18 |
| `opencode2` | `~/.opencode/bin/opencode2` | 2.0.18 |

升级用 `opencode upgrade`（自带），与 pacman 无关。
注意：omarchy 的 `stable-mirror` 冻结在 2026-09-08，pacman 看不到 v2。

## v2 API 约定（实测）

- 端点全在 **`/api/*`** 下（v1 用根路径，两代不要混）。
- 读接口多数套信封 **`{location, data}`**；但 `/api/config` 是**裸数组**——逐个查规格，不能一刀切。
- **`location` 必须含 `project`**：`{directory, project:{id,directory,canonical}}`。
- **变更类端点回 `204 No Content`**，回 200 带 body 会被判 `UnexpectedStatus`。
- 事件信封：`{id:"evt_…", type, created, data, location?, metadata?}`，**`created` 必填**；
  durable 事件另需 `durable:{aggregateID, seq, version}`。

### 事件名（v2）

流式用 `session.*`，**没有 `message.part.updated`**（那是上一代的名字）：

| 事件 | data |
| --- | --- |
| `session.step.started` | `{sessionID, assistantMessageID, agent, model, started}`（durable）——**追加助手消息本身** |
| `session.text.started` | `{sessionID, assistantMessageID, ordinal}`（durable）——在消息内开一个文本块 |
| `session.text.delta` | `{sessionID, assistantMessageID, ordinal, delta}`（ephemeral） |
| `session.text.ended` | `{sessionID, assistantMessageID, ordinal, text}`（durable） |
| `session.execution.started` / `.succeeded` | `{sessionID}`（durable） |
| `session.message.content.updated` | `{sessionID, messageID, content:[…]}`（replay 用） |

**一轮回复的必需顺序**（缺任何一环，后面的都被静默丢弃）：

```
session.step.started → session.text.started → session.text.delta ×N → session.text.ended
```

理由是客户端实现（`packages/client/src/solid/data.ts`）：
`step.started` 用 `message.append` **把助手消息追加进列表**；`text.started` 起才 `editAssistant` 往消息里推文本块。
**消息不存在时，所有 `editAssistant`/`editText` 都是空操作**——事件照收，界面照旧不动，也不报错。

其余：`session.inbox.enqueued/delivered`、`session.reasoning.delta`、
`session.permissions`、`session.model.selected`、`session.compaction.*`、`session.revert.*`。

## 关键坑

| 现象 | 根因 |
| --- | --- |
| 回车后 composer 清空、**一个请求都不发**，界面不报错 | 响应信封的 `location` 缺 `project`；客户端读 `.info.project.id` 抛未处理异常 |
| 回车毫无反应 | `/api/agent`、`/api/provider`、`/api/model` 为空 → composer 没有模型，拒绝提交 |
| `Failed to switch model: UnexpectedStatus` | `POST …/model` 必须回 204 |
| 助手回复不渲染（事件照发，界面不动也不报错） | 两层原因：① 事件名用了上一代的 `message.part.updated`；② **即使名字对了，还缺 `session.step.started`**——助手消息没被追加进列表，后续 `editAssistant` 全是空操作 |
| 事件流 `Connection lost` | SSE 帧必须是 `{id,type,data}` 形式且**首帧 `server.connected`**，并且走 chunked |

## 权威出处

| 内容 | 位置 |
| --- | --- |
| HTTP 规格（136 端点） | opencode 仓库 `v2` 分支 `packages/protocol/openapi.json` |
| 事件定义 | 同分支 `packages/schema/src/event.ts`、`session-event.ts` |
| 真 server 形状对照 | 真 server 用 HTTP Basic 认证（用户名 `opencode`，密码见启动日志），可 `curl -u` 直接抄 |

## 下一步

见上面的**实现清单 · 未实现**，按那份优先级做；每条的具体验证法见同节的「未完成项的验证法」。
其中「中断收尾」和「多轮消息落位」是当前功能的直接缺陷，先修；「会话列表」「agent 切换」是缺口；LSP/formatter/git 那类 Ante 没有的概念，只打桩。

## 魔改上游 TUI

上游 opencode 以 **git subtree** 放在 `vendor/opencode/`（分支 `v2`，约 7.8k 文件）。

```sh
# 改完跑起来（入口在 packages/cli）
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
git subtree pull --prefix=vendor/opencode https://github.com/anomalyco/opencode v2 --squash
cd vendor/opencode && bun install        # 依赖有变时才需要
cd - && cargo build --release            # 垫片
./target/release/antex serve 41999 &
cd vendor/opencode && bun run dev -- --server http://127.0.0.1:41999
```

跑起来看界面有没有坏——**垫片假装的是接口，上游改了接口既不会有合并冲突、也不会有类型检查**，
只能靠「拿真客户端跑一遍」当场发现。这是本方案唯一的防漂移手段，别省。



**体积**：工作树约 159M，其中宣传视频（`packages/console/app/src/asset/lander/*.mp4`）与 `artifacts/` 占大头；
`.git` 经 `git gc --prune=now` 回收后约 84M。裁剪这些文件会让 subtree 每次都冲突，故保持原样。

## 审批

Ante 暂停时（`TurnPause{Approval}`）垫片发 `permission.asked`，TUI 弹窗；用户在 TUI 里选
`once` / `always` / `reject`，垫片经 `POST /api/session/{id}/permission/{requestID}/reply` 收回，
转成 Ante 的 `Accept` / `AcceptAlways` / `Deny` 发 `ApprovalResponse`。

Ante 的权限模式由环境变量决定：`SHIM_PERMISSION_MODE=strict|auto|yolo`（默认 `auto`）。
要测审批弹窗用 `strict`——`auto` 下多数命令被判定为「可证明安全」直接放行。

## 目录

```
src/                 垫片本体：路由、信封、SSE、会话/消息存储
vendor/opencode/     上游 opencode（subtree，v2 分支）——魔改对象
Cargo.toml           axum + tokio + serde
```
