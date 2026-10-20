#!/usr/bin/env bash
# Boots archix deployments in QEMU with OVMF; `run.sh` tests the first layout on a booted system, deployments, boot entries, boot counting and Secure Boot, `run.sh rollback` the rollback torture test, `run.sh nvidia` NVIDIA DKMS drivers across deployments (TO_DATE: the newer snapshot, 2026-10-10 by default), `run.sh cachyos` the rollback torture test on CachyOS's real repos.
set -euo pipefail
suite=${1:-boot}
case $suite in upgrade | atomic) echo "the $suite suite tests the in-place model; it waits for a second release with deployments" >&2; exit 2 ;; esac
root=$(cd "$(dirname "$0")/../.." && pwd)
# One build volume per checkout, as in tests/arch/run.sh.
target=archix-target-$(printf %s "$root" | sha256sum | cut -c1-12)
. "$root/tests/vm/work-dir.sh" vm
build_log=$(mktemp)
docker build -t archix-arch-test "$root/tests/arch" >"$build_log" 2>&1 || { cat "$build_log"; exit 1; }
rm -f "$build_log"
mounts=(-v "$root":/src:ro -v "$target":/target -v archix-cargo:/root/.cargo/registry -v "$work":/work -v "$root/tests/vm":/vm:ro -e SUITE="$suite" -e FS="${FS:-ext4}" -e TO_DATE="${TO_DATE:-}")
docker run --rm "${mounts[@]}" archix-arch-test bash -c 'set -e
  cargo build --locked --release && cp /target/release/archix /work/archix
  cargo build --locked --release --target x86_64-unknown-linux-musl && cp /target/x86_64-unknown-linux-musl/release/archix /work/archix-static'
if [ "$suite" = upgrade ]; then
  rm -rf "$work/old-src" && mkdir -p "$work/old-src" "$work/old"
  git -C "$root" archive "${ARCHIX_OLD:-0.1.0-dogfood}" | tar -x -C "$work/old-src"
  docker run --rm -v "$work":/work -v "$target":/target -v archix-cargo:/root/.cargo/registry -w /work/old-src -e CARGO_TARGET_DIR=/target/old archix-arch-test bash -c 'set -e
    cargo build --locked --release --target x86_64-unknown-linux-musl && cp /target/old/x86_64-unknown-linux-musl/release/archix /work/old/archix-static'
fi
# ARCHIX_TEST_ARCHIVE (e.g. a local caching mirror on http://127.0.0.1:8079) stands in for archive.archlinux.org; the VM reaches the host as 10.0.2.2.
archive=${ARCHIX_TEST_ARCHIVE:-https://archive.archlinux.org}
docker run --rm --privileged --network host "${mounts[@]}" -e ARCHIVE="$archive" -e VM_ARCHIVE="${archive/127.0.0.1/10.0.2.2}" archix-arch-test /vm/build-image.sh
if [ "$suite" = rollback ] || [ "$suite" = nvidia ] || [ "$suite" = cachyos ] || [ "$suite" = atomic ]; then mounts+=(-e VM_TIMEOUT=14400 -e VM_MEM=8192 -e VM_CPUS=6); fi
if [ "$suite" = upgrade ]; then mounts+=(-e VM_TIMEOUT=7200); fi
docker run --rm --network host --device /dev/kvm "${mounts[@]}" archix-arch-test /vm/boot.sh
