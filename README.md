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

- [x] **会话列表 / `/sessions` / `/resume`** —— `GET /api/session` 读 `~/.ante/sessions/*/meta.json`（实测 225 条、时间倒序、标题=首条用户消息），**选择器实测已列出真 Ante 会话**
- [x] **恢复会话：历史渲染** —— `/sessions` 选中一条即加载历史。**病根：`GET /api/session/{id}` 少了 `data` 信封**（该路由 schema 是 `{data: Session.Info}` 且 `additionalProperties:false`），客户端读 `response.data.id` 得 undefined，抛 `undefined is not an object (evaluating 'Ae.id')`，**只在界面上弹个小 toast、不换视图**——所以看着像「点了没反应」
- [ ] **在恢复的会话里继续对话** —— 历史能看，但选中后直接打字发不出去（探针实测垫片收到 **0 个 POST**，疑为选择器焦点未交还输入框，待用真人操作复核）
