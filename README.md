# opencode-shim

> 最后核对：2026-09-28 · 目标客户端 opencode 2.0.18 · Rust 1.98

把 **opencode v2 自带的 TUI** 接到 **Ante** 后端上运行。

做法是**实现 opencode v2 要求的 server API**，让 `opencode --server <url>` 分辨不出真假；
opencode 的界面、主题、键位一行不改，Ante 提供数据。思路同「改接口，不改消费者」。

## 为什么走这条路

手写复刻 opencode 的 TUI 到不了它的完成度（它有 17k 行的界面层）。反过来做，垫片是**可丢弃**的一层：
上游界面升级时，重新拉 TUI 即可，只需跟着修垫片。代价是 Ante 没有的概念（LSP、MCP、formatter、OAuth、git 操作）必须打桩。

## 实现清单

**未实现（按优先级；每条验证法见 `TODO.md`）**

- [ ] **中断收尾** —— `POST …/interrupt` 已通、Ante 也收到了，但**中断后界面卡转轮**；缺收尾事件（`step.ended` / `execution.interrupted`）
- [ ] **多轮消息落位** —— 单轮正常；连发多条时用户消息成组堆在底部、没进对话流（疑 `inbox.delivered` 的 `admitted` 条件不满足）
- [ ] **会话列表 / `/sessions` / `/resume`** —— 接口返回空，选择器打开是空的。Ante 侧数据现成：`~/.ante/sessions/*/meta.json` + `ResumeSession`
- [ ] **`shift+tab` 切 agent、模型选择** —— 垫片只提供一个 agent（`build`），未实现 `POST …/agent`
- [ ] **`@` 文件补全** —— `/api/fs/list` 返回空
- [ ] **diff / LSP / formatter / MCP / VCS** —— 打桩。**这些是 Ante 根本没有的概念，只能显示为空，别指望填上**
- [ ] **`/compact` 压缩、撤销回滚、贴图、PTY、分享**
- [ ] **次要事件** —— `session.step.streamed`、`instructions.updated`、`renamed`、`model.selected` 未发（缺了不致命）
- [ ] **垫片命令行参数** —— 不解析；非数字参数被忽略、端口被占直接 panic（`AddrInUse`）

**已实现（均已实测）**

- [x] **真 v2 TUI 起界** —— 顶栏、块字 logo、composer、页脚全部由垫片喂出
- [x] **提示词送达** —— `POST /api/session` → `POST …/model` → `POST …/prompt`
- [x] **真 Ante 回复** —— `ante-sdk` 连 `ante serve --stdio`；模型名、耗时、token 均为真实数据
- [x] **流式文本** —— `step.started` → `text.started` → `text.delta` ×N → `text.ended`
- [x] **推理（thinking）** —— `session.reasoning.started/delta/ended`，显示为 `+ Thought · 762ms`
- [x] **工具调用** —— `✓ Bash [命令, 描述]`，含参数与输出
- [x] **审批** —— Ante 暂停 → TUI 弹 `△ Permission required` → 在 TUI 批准 → `ApprovalResponse` → Ante 继续执行
- [x] **消息顺序与去重** —— `inbox.enqueued`+`delivered` 入列表；复用客户端提交自带的 message id
- [x] **boot 接口全套** —— `health` `location` `fs/list` `agent` `provider` `model` `config` `vcs` `project` `plugin` `migration` …
- [x] **事件流（SSE）** —— `{id, type, created, data}` 帧，首帧 `server.connected`


## 怎么跑

```sh
cd ~/Projects/opencode-shim
cargo build --release                      # 需要代理：crates.io 直连会被 TLS 拦
./target/release/opencode-shim 41999       # 终端 A
opencode --server http://127.0.0.1:41999   # 终端 B
```

- 编译要走代理：`export http_proxy=http://127.0.0.1:7890 https_proxy=http://127.0.0.1:7890`
- 环境变量 `ANTECODE_DEBUG` 不影响本程序；请求日志直接打在 stdout。

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
| Ante | 接后端时用 **`ante-sdk` 0.2.5**——**已发布 crates.io**，直接写 `ante-sdk = "0.2.5"` 即可。本机另有 clone 在 `~/Documents/ante`（同版本），需要魔改上游时才改走 path 引用 |

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

见上面的**实现清单 · 未实现**，按那份优先级做；每条的具体验证法在 `TODO.md`。
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
./target/release/opencode-shim 41999 &
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
