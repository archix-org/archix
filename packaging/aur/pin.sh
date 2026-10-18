#!/usr/bin/env bash
# Pins archix and archix-bin to a signed release: pin.sh <version> <key fingerprint> <dir holding the release tarballs and .sig files>.
set -euo pipefail
version=$1 key=$2 files=$3
here=$(cd "$(dirname "$0")" && pwd)
for pkg in archix archix-bin; do
  dir="$here/$pkg"
  sed -i -e "s/^pkgver=.*/pkgver=$version/" -e "s/^pkgrel=.*/pkgrel=1/" -e "s/^validpgpkeys=.*/validpgpkeys=('$key')/" "$dir/PKGBUILD"
  case $pkg in
    archix) file="archix-$version.tar.gz" ;;
    archix-bin) file="archix-$version-x86_64-linux.tar.gz" ;;
  esac
  sum=$(sha256sum "$files/$file" | cut -d ' ' -f1)
  sed -i "s/^sha256sums=.*/sha256sums=('$sum' 'SKIP')/" "$dir/PKGBUILD"
  # makepkg uses the files already beside the PKGBUILD instead of downloading them.
  for f in "$file" "$file.sig"; do ln -sf "$(cd "$files" && pwd)/$f" "$dir/$f"; done
  (cd "$dir" && makepkg --printsrcinfo > .SRCINFO)
  echo "pinned $pkg $version to key $key"
done
