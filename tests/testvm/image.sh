#!/usr/bin/env bash
# Builds the test VM from archix-arch-test, without privileges: its root filesystem as an ext4 image (mke2fs -d in a plain container), its kernel and an initramfs; cached by the image's id under ARCHIX_TESTVM_CACHE. Prints the directory.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
"$here/../arch/image.sh"
id=$(docker image inspect -f '{{.Id}}' archix-arch-test | cut -d: -f2 | cut -c1-16)
guest=$(cat "$here"/guest/* | sha256sum | cut -c1-12)
cache=${ARCHIX_TESTVM_CACHE:-$HOME/.cache/archix-testvm}
dir=$cache/$id-$guest
if [ -s "$dir/root.img" ] && [ -s "$dir/initramfs.img" ]; then
  echo "$dir"
  exit 0
fi
mkdir -p "$cache"
exec 9> "$cache/build.lock"
flock 9
[ -s "$dir/root.img" ] && [ -s "$dir/initramfs.img" ] && { echo "$dir"; exit 0; }
tmp=$(mktemp -d "$cache/.build.XXXXXX")
trap 'docker run --rm -v "$tmp":/out archix-arch-test rm -rf /out/stage > /dev/null 2>&1; rm -rf "$tmp"' EXIT
cid=$(docker create archix-arch-test true)
docker export "$cid" | docker run --rm -i -v "$tmp":/out -v "$here/guest":/guest:ro -e OWNER="$(id -u):$(id -g)" archix-arch-test bash -c '
  set -euo pipefail
  mkdir /out/stage && tar -x -C /out/stage
  bash /guest/setup.sh /out/stage
  kernel=$(ls /out/stage/usr/lib/modules)
  cp "/out/stage/usr/lib/modules/$kernel/vmlinuz" /out/vmlinuz
  mkinitcpio -c /guest/mkinitcpio.conf -k "$kernel" -g /out/initramfs.img > /out/mkinitcpio.log 2>&1 || { cat /out/mkinitcpio.log; exit 1; }
  mke2fs -q -t ext4 -L archix-testvm -d /out/stage /out/root.img 64G
  rm -rf /out/stage
  chown "$OWNER" /out/vmlinuz /out/initramfs.img /out/root.img'
docker rm "$cid" > /dev/null
rm -f "$tmp/mkinitcpio.log"
mv "$tmp" "$dir"
trap - EXIT
# Older builds go once a newer one is made.
find "$cache" -mindepth 1 -maxdepth 1 -type d ! -name "$(basename "$dir")" ! -name disks -exec rm -rf {} +
echo "$dir"
