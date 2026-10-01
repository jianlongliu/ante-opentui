# 编译与发版

**编译出包只在 GitHub Actions 上做，本机不编包、不发版。** 触发 = 推 `v*` tag（`.github/workflows/release.yml`，也可 `workflow_dispatch` 手动点）。

发版流程：改 `PKGBUILD` 的 `pkgver` → 提交 → 打 tag `v$pkgver` → 推 tag。约束与验收：

- `release.yml` 校验 tag 必须正好等于 `v$pkgver`，对不上直接停；`source` 也锚在 `v$pkgver`，所以同一个 pkgver 编出来的东西永远一样。
- tag 不可复用：同一天的第二版给 `pkgver` 加一档（`2026.10.01` → `2026.10.01.2`），`pkgrel` 回到 `1`。
- 推 ref 走 SSH：`git push git@github.com:jianlongliu/ante-opentui <ref>`。HTTPS 那条 gh token 缺 `workflow` 权限，提交里动过 `.github/` 会被 GitHub 拒。
- 验收（都不需要本机编译）：
  - `gh run list --limit 3` —— run 的 conclusion 应为 `success`
  - `gh release view v<pkgver>` —— 应有 `antex-<pkgver>-<pkgrel>-x86_64.pkg.tar.zst`
  - 抽查包内容：`gh release download v<pkgver> -p '*.pkg.tar.zst' -D /tmp/antex-pkg`，再 `tar --zstd -xOf <包> .PKGINFO` 看 `pkgver` / `license`
- 装上：`pkexec pacman -U <包>`。装前把 `/usr/bin/{antex,antex-tui,opencode2}` 备份到 `~/.local/state/backups/usr/bin/`；**装完必须重开 TUI**，在跑的进程仍用旧二进制。

# 版本号的落点

- `PKGBUILD` 的 `pkgver` 是**唯一**版本来源：tag、Release、包名都从它来。
- `Cargo.toml` 的 `version` 与发布无关（`publish = false`，没人读）。
- 二进制自报的 `antex` 版本是**编译当天日期**（`build.rs` 把 `ANTEX_VERSION` 写成当天），设计如此，不跟 `pkgver` 走；`antex serve` 的 `/api/health` 里 `"antex"` 就是它。
- `README.md` 顶部的「最后核对：<日期>」是核对时间戳，不是版本。

**待定**：版本号怎么取（纯日期 / 日期+序号 / 语义化），以及发版时除 `pkgver` 外还有哪些地方要跟着更新——用户尚未定；定下来后把本节换成规则。

# 验证时的数据卫生

`antex serve PORT` 起的垫片连的是**真 Ante**：测试 prompt 会真的开会话、真的跑回合、真的花 token，会话还会落进 `~/.ante/sessions/` 并出现在用户的会话列表里。

- 用**空闲端口**：本机 TUI 常占 `41999`，撞上时垫片直接退出，测试等于没跑。
- 测完删掉为测试建的会话目录 `~/.ante/sessions/<id>/`。
- 「后端起不来 / 一启动就死」这类用例用**假后端**（只答 `--version`，`serve --stdio` 立刻退出），不碰真 Ante。
