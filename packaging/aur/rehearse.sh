#!/usr/bin/env bash
# The release rehearsal's steps, run as an unprivileged user inside the Arch test container by test-release.sh.
set -euo pipefail
cd ~/archix
# The test image points CARGO_TARGET_DIR at root's /target.
export CARGO_TARGET_DIR=$HOME/archix/target
version=$(grep -m1 '^version' Cargo.toml | cut -d '"' -f2)
echo "== release tarballs for $version at $(git rev-parse --short HEAD)"
gpg --batch --passphrase '' --quick-gen-key 'archix test release key' ed25519 sign never 2>/dev/null
gpg --batch --passphrase '' --quick-gen-key 'someone else' ed25519 sign never 2>/dev/null
fpr() { gpg --list-keys --with-colons "$1" | awk -F: '/^fpr/ {print $10; exit}'; }
key=$(fpr 'archix test release key')
other=$(fpr 'someone else')
release=~/release
mkdir -p "$release"
git archive --format=tar.gz --prefix="archix-$version/" -o "$release/archix-$version.tar.gz" HEAD
cargo build --quiet --locked --release
cargo build --quiet --locked --release --target x86_64-unknown-linux-musl
bin="archix-$version-x86_64-linux"
mkdir "$release/$bin"
cp target/release/archix "$release/$bin/archix"
cp target/x86_64-unknown-linux-musl/release/archix "$release/$bin/archix-static"
cp LICENSE-MIT LICENSE-APACHE "$release/$bin/"
tar -C "$release" -czf "$release/$bin.tar.gz" "$bin"
for file in "archix-$version.tar.gz" "$bin.tar.gz"; do
  gpg --local-user "$key" --detach-sign --output "$release/$file.sig" "$release/$file"
done
packaging/aur/pin.sh "$version" "$key" "$release"

for pkg in archix archix-bin; do
  cd ~/archix/packaging/aur/$pkg
  makepkg -f --noconfirm --nocheck > ~/makepkg-$pkg.log 2>&1 || { tail -30 ~/makepkg-$pkg.log; exit 1; }
  file="$pkg-$version-1-x86_64.pkg.tar.zst"
  contents=$(tar -tf "$file")
  for path in usr/bin/archix usr/bin/ax usr/lib/archix/archix-static; do
    grep -qx "$path" <<< "$contents" || { echo "FAIL: $file lacks $path"; exit 1; }
  done
  grep -q "validpgpkeys = $key" .SRCINFO || { echo "FAIL: $pkg's .SRCINFO doesn't name the key"; exit 1; }
  grep -Eq 'sha256sums = [0-9a-f]{64}' .SRCINFO || { echo "FAIL: $pkg's .SRCINFO doesn't pin the tarball"; exit 1; }
  echo "PASS: $pkg $version builds from the signed tarball, pinned to $key, and ships archix, ax and archix-static"
done

cd ~/archix/packaging/aur/archix-bin
rm -rf src pkg ./*.pkg.tar.zst
cp --remove-destination "$release/$bin.tar.gz" . && printf x >> "$bin.tar.gz"
if makepkg -f --noconfirm > ~/tampered.log 2>&1; then echo "FAIL: a tampered tarball was accepted"; exit 1; fi
echo "PASS: a tampered tarball is refused: $(grep -m1 -i 'failed' ~/tampered.log)"
cp --remove-destination "$release/$bin.tar.gz" .
rm -f "$bin.tar.gz.sig"
gpg --local-user "$other" --detach-sign --output "$bin.tar.gz.sig" "$bin.tar.gz"
if makepkg -f --noconfirm --skipchecksums > ~/wrongkey.log 2>&1; then echo "FAIL: a signature from another key was accepted"; exit 1; fi
echo "PASS: a signature from a key outside validpgpkeys is refused: $(grep -m1 -i 'failed\|unknown public key\|not trusted\|invalid' ~/wrongkey.log)"
echo OK
