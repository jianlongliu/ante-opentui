# TODO

> 最后核对：2026-09-28 · 按优先级排序；每条都注了「怎么验」

已通（勿动）：真 TUI 起界、发消息、真 Ante 回复（文本/推理/工具/审批）、消息顺序。
排错手法见 README「跟上游的例行步骤」与活文档 `~/Documents/AI Agents/opencode-shim.md`。

## 1. 中断后界面卡转轮（已知缺陷，最该先修）

按 Esc 两下（第一下「上膛」、第二下真中断）后，垫片确实收到
`POST /api/session/{id}/interrupt` 并转发给 Ante，但**界面停在转轮不动**——
中断后没发「已中断」的事件，客户端不知道轮次结束了。

- 先抓 Ante 中断时到底吐什么：`ANTE_SHIM_TRACE=/tmp/t.log <垫片>`，跑一个长命令再按两下 Esc。
- 大概要补：`session.step.ended` + `session.execution.interrupted`（真 server 会发后者）。
- 验：`run: sleep 12 then echo DONE` → 两下 Esc → 转轮停、不再出 `DONE`。

## 2. 多轮时用户消息堆在底部（pending 区）

单轮正常（用户在上、回复在下）；连发几条后，用户消息成组堆在底部，没进对话流。
疑似 `session.inbox.delivered` 没生效——客户端要求该输入已「接纳」才移动位置
（`data.ts` 的 `delivered` 分支里有 `admitted` 判断，不满足直接 return）。

- 查客户端 `result.session.input.has(...)` 的数据来源（可能是 `/api/session/{id}/input`）。
- 验：同一会话连发三条，三条用户消息应与各自回复交错，而不是堆在底部。

## 3. 会话列表 / `/sessions` / `/resume`

`GET /api/session` 目前返回空数组，选择器打开也是空的。
Ante 侧数据源现成：`~/.ante/sessions/*/meta.json`（`first_user_message`/`message_count`/`started_time`），
另有 `Event ResumeSession` 可恢复。

- 实现：`GET /api/session` 列出 Ante 会话；`POST /api/session/{id}/…` 恢复；`/api/session/active`。
- 用户报过一次「敲完报 bug、一闪而过」，**未复现**——下次先问确切命令。

## 4. `shift+tab` 切 agent、模型选择

垫片只提供一个 agent（`build`），未实现 `POST /api/session/{id}/agent`；模型同理。
Ante 侧对应 `SessionRequest`/`SessionUpdate` 的 model/provider。

## 5. 纯打桩（消除界面空洞）

`@` 文件补全（`/api/fs/list` 返回空）、diff 视图（`/api/vcs/diff`）、
LSP / formatter / MCP 面板、`/compact` 压缩、撤销回滚、贴图、PTY。

> LSP / formatter / git diff 是 **Ante 根本没有的概念**，只能让它显示为空，别指望填上。

## 6. 次要事件（缺了不致命）

真 server 会发而我未发：`session.step.streamed`、`session.instructions.updated`、
`session.renamed`、`session.model.selected`。

## 7. 小项

- 垫片不解析命令行参数：非数字参数被忽略、回落到 41999；端口被占直接 panic（`AddrInUse`）。补优雅报错。
- `SHIM_TOOL_EVENTS` / `SHIM_SKIP_EVENTS` 是排错开关，稳定后可考虑删掉。
