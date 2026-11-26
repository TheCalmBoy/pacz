# Builds pacz from this local tree: run `makepkg -si` in the project directory.
pkgname=pacz
pkgver=0.3.0
pkgrel=1
pkgdesc='pacman wrapper with batched, mirror-aware parallel downloads via aria2-next'
arch=('x86_64' 'aarch64')
url='https://github.com/TheCalmBoy/pacz'
license=('custom')
depends=('pacman' 'glibc' 'gcc-libs' 'aria2-next')
optdepends=('paru: AUR updates during -Su/-Syu')
makedepends=('cargo')
backup=('etc/pacz.conf')
options=('!lto')

prepare() {
  cd "$startdir"
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
  cd "$startdir"
  export CARGO_TARGET_DIR="$srcdir/target"
  cargo build --frozen --release
}

check() {
  cd "$startdir"
  export CARGO_TARGET_DIR="$srcdir/target"
  cargo test --frozen
}

package() {
  cd "$startdir"
  install -Dm755 "$srcdir/target/release/pacz" "$pkgdir/usr/bin/pacz"
  install -Dm644 pacz.conf.example "$pkgdir/etc/pacz.conf"
}
