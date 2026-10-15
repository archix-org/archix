#!/usr/bin/env bash
# Builds archix from this checkout and installs it, with the static binary rescue boot entries need.
set -euo pipefail

cd "$(dirname "$0")/.."
bin=/usr/local/bin/archix
static=/usr/lib/archix/archix-static
musl=x86_64-unknown-linux-musl
target=${CARGO_TARGET_DIR:-target}

as_root() {
  if [[ $EUID -eq 0 ]]; then "$@"; else sudo "$@"; fi
}

if [[ ${1:-} == --uninstall ]]; then
  as_root rm -f "$bin" "$static"
  echo "removed $bin and $static"
  exit 0
fi

missing=()
command -v cargo >/dev/null || missing+=(rust)
pacman -Q musl >/dev/null 2>&1 || missing+=(musl)
if ! command -v rustup >/dev/null && ! pacman -Q rust-musl >/dev/null 2>&1; then
  missing+=(rust-musl)
fi
if ((${#missing[@]})); then
  echo "archix needs these to build: ${missing[*]}"
  as_root pacman -S --needed "${missing[@]}"
fi
if command -v rustup >/dev/null; then
  rustup target add "$musl"
fi

echo "building archix"
cargo build --release --locked
echo "building the static rescue binary"
CFLAGS= CXXFLAGS= LDFLAGS= cargo build --release --locked --target "$musl"

as_root install -Dm755 "$target/release/archix" "$bin"
as_root install -Dm755 "$target/$musl/release/archix" "$static"
echo "installed $("$bin" --version) to $bin, and the rescue binary to $static"
echo "next: sudo archix init"
