#!/usr/bin/env bash
# Boots archix's deployment initramfs hook and first-boot activation in QEMU; INITRAMFS=systemd|busybox, FS=ext4|btrfs, ROOT_OPTS the root's command-line mode (ro by default).
set -euo pipefail
root=$(cd "$(dirname "$0")/../../.." && pwd)
target=archix-target-$(printf %s "$root" | sha256sum | cut -c1-12)
. "$root/tests/vm/work-dir.sh" deploy-boot
build_log=$(mktemp)
docker build -t archix-arch-test "$root/tests/arch" > "$build_log" 2>&1 || { cat "$build_log"; exit 1; }
rm -f "$build_log"
mounts=(-v "$root":/src:ro -v "$target":/target -v archix-cargo:/root/.cargo/registry -v "$work":/work -v "$root/tests/vm":/vm:ro
  -e FS="${FS:-ext4}" -e INITRAMFS="${INITRAMFS:-systemd}" -e ROOT_OPTS="${ROOT_OPTS:-ro}" -e DATE="${DATE:-2026-10-03}")
docker run --rm "${mounts[@]}" archix-arch-test bash -c 'set -e
  cargo build --locked --release --target x86_64-unknown-linux-musl && cp /target/x86_64-unknown-linux-musl/release/archix /work/archix-static
  ARCHIX_VM_FILES=/work/files cargo test --locked --test deploy_boot boot_files_for_the_vm_image > /dev/null'
archive=${ARCHIX_TEST_ARCHIVE:-https://archive.archlinux.org}
docker run --rm --privileged --network host "${mounts[@]}" -e ARCHIVE="$archive" archix-arch-test /vm/deploy-boot/build.sh
docker run --rm --network host --device /dev/kvm "${mounts[@]}" -e VM_TIMEOUT=1800 archix-arch-test /vm/boot.sh
