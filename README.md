# opencode-shim

> 最后核对：2026-09-28 · 目标客户端 opencode 2.0.18 · Rust 1.98

把 **opencode v2 自带的 TUI** 接到 **Ante** 后端上运行。

做法是**实现 opencode v2 要求的 server API**，让 `opencode --server <url>` 分辨不出真假；
opencode 的界面、主题、键位一行不改，Ante 提供数据。思路同「改接口，不改消费者」。

## 为什么走这条路

手写复刻 opencode 的 TUI 到不了它的完成度（它有 17k 行的界面层）。反过来做，垫片是**可丢弃**的一层：
上游界面升级时，重新拉 TUI 即可，只需跟着修垫片。代价是 Ante 没有的概念（LSP、MCP、formatter、OAuth、git 操作）必须打桩。

## 现状

| 项 | 状态 |
| --- | --- |
| 真 v2 TUI 起界 | ✅ 顶栏、块字 logo、composer、页脚全部由本垫片喂出 |
| 提示词送达 | ✅ `POST /api/session` → `POST …/model` → `POST …/prompt` |
| 助手回复渲染 | ❌ 事件已改名 `session.text.delta`，仍不显示；下一步补 durable 信封 |
| 回复内容 | ❌ 仍是垫片里的假字符串，**尚未接 Ante** |
| 打桩覆盖 | LSP / MCP / formatter / VCS / OAuth / revert / fork 全部返回空 |

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
| `session.execution.started` / `.succeeded` | `{sessionID}`（durable） |
| `session.text.delta` | `{sessionID, assistantMessageID, ordinal, delta}`（ephemeral） |
| `session.text.ended` | `{sessionID, assistantMessageID, ordinal, text}`（durable） |
| `session.message.content.updated` | `{sessionID, messageID, content:[…]}`（replay 用） |

其余：`session.inbox.enqueued/delivered`、`session.step.started/ended`、`session.reasoning.delta`、
`session.permissions`、`session.model.selected`、`session.compaction.*`、`session.revert.*`。

## 关键坑

| 现象 | 根因 |
| --- | --- |
| 回车后 composer 清空、**一个请求都不发**，界面不报错 | 响应信封的 `location` 缺 `project`；客户端读 `.info.project.id` 抛未处理异常 |
| 回车毫无反应 | `/api/agent`、`/api/provider`、`/api/model` 为空 → composer 没有模型，拒绝提交 |
| `Failed to switch model: UnexpectedStatus` | `POST …/model` 必须回 204 |
| 助手回复不渲染 | 发的是上一代的 `message.part.updated`；v2 用 `session.text.delta` |
| 事件流 `Connection lost` | SSE 帧必须是 `{id,type,data}` 形式且**首帧 `server.connected`**，并且走 chunked |

## 权威出处

| 内容 | 位置 |
| --- | --- |
| HTTP 规格（136 端点） | opencode 仓库 `v2` 分支 `packages/protocol/openapi.json` |
| 事件定义 | 同分支 `packages/schema/src/event.ts`、`session-event.ts` |
| 真 server 形状对照 | 真 server 用 HTTP Basic 认证（用户名 `opencode`，密码见启动日志），可 `curl -u` 直接抄 |

## 下一步

1. 给 durable 事件补 `durable:{aggregateID:sessionID, seq, version:1}`，验助手回复是否渲染。
2. 若仍不渲染，试在 `session.text.delta` 前先发 `session.step.started`（建助手消息占位）。
3. 把假回复换成真 Ante：进程内用 `ante-sdk` 连 `ante serve --stdio`，把 Ante 事件翻译成上面的 `session.*`。
4. 逐个补桩端点直到 TUI 不再报错。

## 魔改上游 TUI

上游 opencode 以 **git subtree** 放在 `vendor/opencode/`（分支 `v2`，约 7.8k 文件）。

```sh
# 改完跑起来（入口在 packages/cli）
cd vendor/opencode
bun install                    # 首次，需代理
bun run dev -- --server http://127.0.0.1:41999

# 跟上游
git subtree pull --prefix=vendor/opencode https://github.com/anomalyco/opencode v2 --squash
```

**补丁只压一条缝**：改动集中在上游「TUI 调后端」的那层（`packages/client/`，生成式 HTTP client 所在），
`packages/tui/` 的界面代码尽量不动。这样上游更新等于 rebase 一小块，冲突可控；
散着改则每次合并都要打仗。

**依赖**：Bun（本机 `1.4.2`，mise 装的）。

**体积**：工作树约 159M，其中宣传视频（`packages/console/app/src/asset/lander/*.mp4`）与 `artifacts/` 占大头；
`.git` 经 `git gc --prune=now` 回收后约 84M。裁剪这些文件会让 subtree 每次都冲突，故保持原样。

## 目录

```
src/                 垫片本体：路由、信封、SSE、会话/消息存储
vendor/opencode/     上游 opencode（subtree，v2 分支）——魔改对象
Cargo.toml           axum + tokio + serde
```
