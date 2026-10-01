# 发版与编译

**编译出包一律在 GitHub Actions 上做，本机不编包、不发版**（2026-10-01 用户明确要求）。

- 不在本机跑 `makepkg` / `makepkg -si` / 手工出 `.pkg.tar.zst`，也不为了「先装上看看」在本机编一遍。
- 发版流程 = 改 `PKGBUILD` 的 `pkgver` → 提交 → 打 tag → 推上去，CI 出包挂到 Release（2026-10-01 实测 4 分 18 秒）：
  - **tag 必须正好是 `v$pkgver`**：`release.yml` 有一道校验，对不上直接停；`source` 也锚在 `v$pkgver`，所以「同一个 pkgver 编出来的东西永远一样」。
  - **同名 tag 不能复用** ⇒ 同一天发第二版就把 `pkgver` 往上走一档（`2026.10.01` → `2026.10.01.2`），`pkgrel` 退回 `1`。
  - 看进度：`gh run list --limit 3`；出完包 `gh release view v<pkgver>` 里应有 `.pkg.tar.zst`。
- **分支和 tag 都用 SSH 推**：`git push git@github.com:jianlongliu/ante-opentui <ref>`。HTTPS 那条 gh token 缺 `workflow` 权限，只要提交里动了 `.github/` 就会被 GitHub 挡回来。
- 装上：从 Release 取包 `pacman -U`（本机 root 走 `pkexec`）。**装完必须重开 TUI 才生效**——已经在跑的进程还用着旧二进制。

# 验证时别污染真实数据

`antex serve PORT` 起的垫片连的是**真的 Ante**：测试 prompt 会真的开会话、真的跑回合、真的花 token，会话还会落进 `~/.ante/sessions/`、出现在用户的会话列表里（2026-10-01 就因为这样被骂过一次）。

- 用**空闲端口**——本机 TUI 常占着 `41999`，撞上时垫片直接退出，测试等于没跑（还会让人误以为「测过了」）。
- 测完把为测试建的会话删掉（`~/.ante/sessions/<id>/`），别留给用户。
- 「后端起不来 / 一启动就死」这类用例用**假后端**（只答 `--version`、`serve --stdio` 立刻退出），不必碰真 Ante。
