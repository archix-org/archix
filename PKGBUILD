# Maintainer: archix contributors <https://github.com/archix-org/archix>
# Builds this checkout as it stands, uncommitted changes included: `makepkg -si` here. The AUR packages are in packaging/aur.
pkgname=archix-git
pkgver=0.2.0.r643.gb491b87
pkgrel=1
pkgdesc="Declarative, lockable, rollback-able system configuration for Arch Linux (local checkout)"
arch=('x86_64')
url="https://archix.dev"
license=('MIT OR Apache-2.0')
depends=('pacman' 'libgcc' 'glibc')
makedepends=('git' 'rust' 'rust-musl' 'musl')
optdepends=(
  'git: AUR packages and recording the config commit'
  'devtools: building AUR packages in a clean chroot'
  'mkinitcpio: boot entries'
  'systemd-ukify: boot entries'
  'sbctl: signing boot entries for Secure Boot'
  'btrfs-progs: snapshots before each switch'
  'flatpak: Flatpak apps'
)
provides=('archix')
conflicts=('archix' 'ax')
options=('!lto' '!debug')
# makepkg's own src/ would be this repo's Rust sources, which -c and -C delete.
BUILDDIR="$startdir/target/makepkg"

pkgver() {
  cd "$startdir"
  printf "%s.r%s.g%s" \
    "$(grep -m1 '^version' Cargo.toml | cut -d '"' -f2)" \
    "$(git rev-list --count HEAD)" \
    "$(git rev-parse --short=7 HEAD)"
}

prepare() {
  export RUSTUP_TOOLCHAIN=stable
  cd "$startdir"
  if command -v rustup >/dev/null; then
    rustup target add $CARCH-unknown-linux-musl
  fi
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
  cargo fetch --locked --target $CARCH-unknown-linux-musl
}

build() {
  export RUSTUP_TOOLCHAIN=stable CARGO_TARGET_DIR="$srcdir/target"
  cd "$startdir"
  cargo build --frozen --release
  CFLAGS= CXXFLAGS= LDFLAGS= cargo build --frozen --release --target $CARCH-unknown-linux-musl
}

check() {
  export RUSTUP_TOOLCHAIN=stable CARGO_TARGET_DIR="$srcdir/target"
  cd "$startdir"
  cargo test --frozen --release --lib
}

package() {
  cd "$startdir"
  install -Dm755 "$srcdir/target/release/archix" "$pkgdir/usr/bin/archix"
  ln -s archix "$pkgdir/usr/bin/ax"
  install -Dm755 "$srcdir/target/$CARCH-unknown-linux-musl/release/archix" "$pkgdir/usr/lib/archix/archix-static"
  install -Dm644 LICENSE-MIT LICENSE-APACHE -t "$pkgdir/usr/share/licenses/$pkgname"
}
