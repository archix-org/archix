#!/usr/bin/env bash
# Pushes each named package's PKGBUILD and .SRCINFO to its AUR repo, skipping a pinned package whose release isn't published: publish.sh archix-git [archix archix-bin].
# Runs as a non-root user in an Arch container with ssh set up for aur@aur.archlinux.org; GH_TOKEN lets it ask GitHub whether a release is published.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
for pkg in "$@"; do
  src="$here/$pkg"
  if [ "$pkg" != archix-git ]; then
    version=$(sed -n 's/^pkgver=//p' "$src/PKGBUILD")
    draft=$(curl -fsS -H "Authorization: Bearer $GH_TOKEN" "https://api.github.com/repos/archix-org/archix/releases/tags/v$version" | jq -r .draft) || draft=missing
    if [ "$draft" != false ]; then
      echo "skipping $pkg: release v$version isn't published ($draft)"
      continue
    fi
  fi
  git clone -q "ssh://aur@aur.archlinux.org/$pkg.git" "$work/$pkg"
  cp "$src/PKGBUILD" "$work/$pkg/PKGBUILD"
  if [ "$pkg" = archix-git ]; then
    repo=$(git -C "$here" rev-parse --show-toplevel)
    ver="$(grep -m1 '^version' "$repo/Cargo.toml" | cut -d '"' -f2).r$(git -C "$repo" rev-list --count HEAD).g$(git -C "$repo" rev-parse --short=7 HEAD)"
    sed -i "s/^pkgver=.*/pkgver=$ver/" "$work/$pkg/PKGBUILD"
  fi
  (cd "$work/$pkg" && makepkg --printsrcinfo > .SRCINFO)
  cd "$work/$pkg"
  git add PKGBUILD .SRCINFO
  if git diff --cached --quiet; then
    echo "$pkg: the AUR already has this PKGBUILD"
  else
    git -c user.name="archix CI" -c user.email="ci@archix.dev" commit -q -m "Update from archix ${GITHUB_SHA:-local}"
    git push -q origin HEAD:master
    echo "$pkg: published"
  fi
  cd "$here"
done
