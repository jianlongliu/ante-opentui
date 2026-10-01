# CI 里编（见 .github/workflows/release.yml），本地也能编。
#
# 源码锚点 = tag：`source` 指向 `v$pkgver`，所以「同一个 pkgver 编出来的东西永远一样」。
# 改版本 = 改下面这行 pkgver，提交，然后打同名 tag 推上去。
#
# 本地试跑（不想连 GitHub、或 tag 还没推）：
#   git tag v2026.10.01                       # tag 得先在本地存在
#   ANTEX_SRC=git+file:///path/to/ante-opentui makepkg -f
#
# 装完之后 antex 认哪个客户端：源码里写死 `$HOME/.local/bin/antex-tui`，再退到 PATH 上的
# `opencode2` / `opencode`（src/main.rs 的 resolve client）。所以这里除了装 antex-tui，
# 还放一个 opencode2 软链让它能从 PATH 找到。
pkgname=antex
pkgver=2026.10.01
pkgrel=2   # 只改了打包元数据；换 pkgver 时退回 1
pkgdesc="Ante agent, driven by the opencode v2 TUI (antex)"
arch=('x86_64')
url="https://github.com/jianlongliu/ante-opentui"
# 仓库根有 LICENSE（MIT）。vendor/opencode 是上游的 MIT，各是各的。
license=('MIT')
depends=()
# 后端本体不在任何仓库（官方脚本装到 ~/.ante/bin），只能声明成可选依赖。
optdepends=('ante: the agent backend this TUI talks to (installed by its own installer)')
makedepends=('rust' 'bun' 'git')
options=('!strip')   # bun 编出来的单文件二进制里塞了运行时，别去动它的段

_src="${ANTEX_SRC:-git+https://github.com/jianlongliu/ante-opentui.git#tag=v$pkgver}"
source=("$pkgname-$pkgver::$_src")
sha256sums=('SKIP')

build() {
  cd "$srcdir/$pkgname-$pkgver"

  # 垫片本体（ante-sdk 从 crates.io 拉）
  cargo build --release --locked

  # 魔改客户端：界面补丁就在 vendor/opencode 里，直接编
  cd vendor/opencode
  bun install --frozen-lockfile
  cd packages/cli
  bun run script/build.ts --single --skip-web-ui
}

package() {
  cd "$srcdir/$pkgname-$pkgver"

  install -Dm755 target/release/antex "$pkgdir/usr/bin/antex"
  install -Dm755 \
    vendor/opencode/packages/cli/dist/cli-linux-x64/bin/opencode \
    "$pkgdir/usr/bin/antex-tui"
  ln -s antex-tui "$pkgdir/usr/bin/opencode2"
}
