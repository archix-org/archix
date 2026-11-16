#!/usr/bin/env bash
# Pins archix and archix-bin to a release: pin.sh <version> <dir holding the release tarballs>.
set -euo pipefail
version=$1 files=$2
here=$(cd "$(dirname "$0")" && pwd)
for pkg in archix archix-bin; do
  dir="$here/$pkg"
  sed -i -e "s/^pkgver=.*/pkgver=$version/" -e "s/^pkgrel=.*/pkgrel=1/" "$dir/PKGBUILD"
  case $pkg in
    archix) file="archix-$version.tar.gz" ;;
    archix-bin) file="archix-$version-x86_64-linux.tar.gz" ;;
  esac
  sum=$(sha256sum "$files/$file" | cut -d ' ' -f1)
  sed -i -E "s/^sha256sums(_x86_64)?=.*/sha256sums\1=('$sum')/" "$dir/PKGBUILD"
  # makepkg uses the file already beside the PKGBUILD instead of downloading it.
  ln -sf "$(cd "$files" && pwd)/$file" "$dir/$file"
  (cd "$dir" && makepkg --printsrcinfo > .SRCINFO)
  echo "pinned $pkg $version"
done
