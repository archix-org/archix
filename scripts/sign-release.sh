#!/usr/bin/env bash
# Signs a release CI drafted: sign-release.sh <version> <key fingerprint>. Verifies the signed tag and the CI checksums, signs both tarballs, attaches the signatures and pins the AUR packages; publishing stays a separate step.
set -euo pipefail
version=$1 key=$2
root=$(cd "$(dirname "$0")/.." && pwd)
tag="v$version"
git -C "$root" verify-tag "$tag"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
gh release download "$tag" --repo archix-org/archix --dir "$work" --pattern 'archix-*'
cd "$work"
for file in "archix-$version.tar.gz" "archix-$version-x86_64-linux.tar.gz"; do
  sha256sum -c "$file.sha256"
  gpg --local-user "$key" --detach-sign --output "$file.sig" "$file"
  gpg --verify "$file.sig" "$file"
done
gh release upload "$tag" --repo archix-org/archix --clobber ./*.sig
"$root/packaging/aur/pin.sh" "$version" "$key" "$work"
cat <<NEXT
Signed $tag. Still to do, by hand:
  gh release edit $tag --repo archix-org/archix --draft=false
  publish packaging/aur/archix and packaging/aur/archix-bin (PKGBUILD and .SRCINFO) to their AUR repos
  publish the public key, so users can fetch $key
NEXT
