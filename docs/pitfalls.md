# 坑与协议约定

> 本文是 [README](../README.md) 的细节分册（2026-10-02 拆出）：接口约定、事件名、踩过的坑、herdr 集成与权威出处。

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
| 整轮空白（不是少一个块，是整段没了） | 帧**解码失败会静默丢弃该事件，并连带丢掉它之后的整轮**；先看客户端的 `--print-logs --log-level debug` |
| 事件流 `Connection lost` | SSE 帧必须是 `{id,type,data}` 形式且**首帧 `server.connected`**，并且走 chunked |
| 第一轮就 `Error: … invalid_request` / `HTTP 400 Bad Request`（`Model "…" is not supported on …`） | 垫片顶部的 `MODEL`/`PROVIDER` 兜底常量（可用 `ANTEX_MODEL`/`ANTEX_PROVIDER` 覆盖）必须是 **你自己 catalog 里的 provider-scoped 名**，形如 `<provider>/<model-id>`；名字错了只有 provider 会告诉你，Ante 侧不报错 |
| 恢复旧会话后消息记在别的会话里（界面看着正常） | 垫片没把会话 id 交给 Ante：`StartSession` 建的那条会话从此吸收所有 prompt，而事件按 `active` 路由，所以 UI 不露馅。**已修**（`Ante.live` + `ResumeSession` + `Ante.replay_turn`，见 [implementation.md](implementation.md)）；要判断落点只能看 `.ante/sessions/<id>/events.jsonl` |
| 自己发的提问显示在它的**回复下面**（或同一轮里飘到回复之后） | 客户端处理 `session.inbox.delivered` 时会把那条 prompt **移到转录末尾**，所以垫片**先发 `step.started`（追加回复）再发 `delivered`** 就会颠倒；回合结束的收尾还会把已送达的 steer 再宣布一次，于是又推一次。**已修**（先 `delivered` 后 `step.started` + `claim_delivered` 去重，见 [implementation.md](implementation.md) 的「消息顺序与去重」） |
| 提问只有 `→ Asked 1 question` 那一格，**选项列表永远不出现** | `form.created` 落在客户端 store 的**第二段 switch** 里，而它的开头是 `if (!event.location) return`——垫片所有事件都不带 `location`，于是事件被静默丢掉（`GET …/form` 却有数据，只按 curl 验会误判成「垫片没问题」）。**已修**（三条 form 事件走 `publish_located()`，form 对象也自带一份；见 `.4`） |
| 长回答越流越卡、正文行「定位不到」却**不报错** | `ordinal` 不是 delta 序号，而是**同类型 part 在消息里的第几个**：客户端 `rows.ts` 用 `text:{ordinal}`/`reasoning:{ordinal}` 当 part 键，`resolvePart` 取 `content.filter(type)[ordinal]`，而 `appendPart` 对每个没见过的键都会**插一行**。垫片从前按 delta 递增 → 每个 token 建一行、再解析成空；现在每个 part 一个恒定序号 |
| `/sessions` 第二次打开只显示 **Could not load sessions.** | 会话选择器按「当前会话的 `location.directory`」先 `location.sync`，再拿该目录的 project 去过滤。垫片从前把 Ante `meta.dir`（会话真实目录）当 location 报出去，可 `/api/location` 只会答 `default_directory()` → 客户端永远解析不到那个目录、整个列表报错。**规则**：垫片只有一个 `prj_shim`（家目录），所有会话 info 的 location 都必须用它 |

## Herdr 集成（本机 herdr 0.9.1）

本机在 **herdr**（terminal workspace manager）里跑 antex。herdr 自带 opencode 识别，但那条路看到的是**客户端**（把 pane 报成 `opencode`），状态靠屏幕内容猜；垫片自己上报更准，也是 Herdr 官方给 agent 作者的路子——<https://herdr.dev/docs/add-herdr-support>，不用等上游发包。

| 项 | 做法 |
| --- | --- |
| 门控 | 仅当 `HERDR_ENV=1` 且 `HERDR_PANE_ID`／`HERDR_BIN_PATH` 都在；不在 herdr 里跑则全程无操作 |
| 上报 | `--source antex`、`--agent Ante`（用户看到的名字；客户端只是个皮） |
| 状态映射 | Ante 连上 → `idle`；`Evt::TurnStart` → `working`；`Evt::TurnEnd` → `idle`；`TurnPause{Approval}` → `blocked`（`--message` 写「等待批准：工具名」） |
| 会话 | 切到别的会话时补一条 `idle`，带 `--agent-session-id <Ante 会话 id>`；herdr 对「跑完一轮的 idle」显示为 `done`（它自己的完成态） |
| `--seq` | 毫秒时间戳，且只增不减（同毫秒也严格递增）——否则 herdr 丢这条上报 |
| 子进程 | stdout/stderr 全丢、3 秒超时、失败静默：TUI 占着终端，child 往里写会糊屏 |
| 退出 | 默认模式下退出前发 `pane release-agent`；`serve` 模式被 Ctrl+C 杀时靠 herdr 兜底（pane 回到 shell 即清空） |

**两个本版限制**（不是没写，是上游没有）：`report-agent -- <resume命令>` 要 herdr **0.10.0**（本机 0.9.1 里连这个参数都没有），且 antex 也没有「按会话 id 直接开」的入口，故不报 resume；`--message` 在 0.9.1 的 `pane get` 里不落地（发了不报错，留着待上游）。

**验收（不必进 TUI）**：`herdr tab create` 造个测试 tab → `herdr pane run <pane> "antex serve 41999"` → `herdr agent list` 出现 `Ante / idle`；发一条 prompt 变 `working`、跑完变 `done`；`SHIM_PERMISSION_MODE=strict` 下拦到危险命令变 `blocked`。测完 `herdr tab close <tab>`。**旧版垫片跑的 pane 仍显示 `opencode`**（那是 herdr 内置检测），换新二进制重开即变 `Ante`。

## 权威出处

| 内容 | 位置 |
| --- | --- |
| HTTP 规格（136 端点） | opencode 仓库 `v2` 分支 `packages/protocol/openapi.json` |
| 事件定义 | 同分支 `packages/schema/src/event.ts`、`session-event.ts` |
| 真 server 形状对照 | 起一个真 server 当 oracle：`opencode2 serve --hostname 127.0.0.1 --port 41998`（日志打印 `server password <PW>`；API 用 HTTP Basic，用户名 `opencode`），再订阅 `/api/event` 抓权威事件序列 |
