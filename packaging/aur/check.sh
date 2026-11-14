#!/usr/bin/env bash
# Builds archix-git from this checkout's HEAD with its check(), and namcaps every PKGBUILD and the built package, in an unprivileged Arch container.
set -euo pipefail
repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
head=$(git -C "$repo" rev-parse HEAD)
exec docker run --rm -v "$repo:/src:ro" -e HEAD="$head" archlinux:latest bash -euo pipefail -c '
  pacman -Syu --noconfirm --needed base-devel git namcap rust rust-musl musl >/dev/null
  useradd -m builder
  for pkg in archix archix-bin archix-git; do
    echo "== namcap $pkg/PKGBUILD"
    namcap /src/packaging/aur/$pkg/PKGBUILD | tee /tmp/namcap-$pkg
  done
  install -d -o builder /home/builder/archix-git
  # The git source is this checkout at HEAD, not GitHub main, so the package CI builds is the commit it tests.
  sed "s|^source=.*|source=(\"archix::git+file:///src#commit=$HEAD\")|" /src/packaging/aur/archix-git/PKGBUILD > /home/builder/archix-git/PKGBUILD
  chown builder /home/builder/archix-git/PKGBUILD
  cd /home/builder/archix-git
  git config --system --add safe.directory "*"
  su builder -c "makepkg --noconfirm"
  echo "== namcap the package"
  namcap ./*.pkg.tar.zst | tee /tmp/namcap-package
  if grep -h " E: " /tmp/namcap-*; then echo "namcap reported errors" >&2; exit 1; fi
  echo "packaging ok: $(ls ./*.pkg.tar.zst)"
'
