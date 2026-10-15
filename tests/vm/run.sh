#!/usr/bin/env bash
# Boots archix-managed generations in QEMU with OVMF: boot entries, rollback at boot, Secure Boot and rescue mode.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
work=${ARCHIX_VM_WORK:-$(mktemp -d /tmp/archix-vm.XXXXXX)}
echo "work dir: $work"
docker build -t archix-arch-test "$root/tests/arch" >/tmp/archix-docker-build.log 2>&1 || { cat /tmp/archix-docker-build.log; exit 1; }
mounts=(-v "$root":/src:ro -v archix-target:/target -v archix-cargo:/root/.cargo/registry -v "$work":/work -v "$root/tests/vm":/vm:ro)
docker run --rm "${mounts[@]}" archix-arch-test bash -c 'set -e
  cargo build --locked --release && cp /target/release/archix /work/archix
  cargo build --locked --release --target x86_64-unknown-linux-musl && cp /target/x86_64-unknown-linux-musl/release/archix /work/archix-static'
docker run --rm --privileged "${mounts[@]}" archix-arch-test /vm/build-image.sh
docker run --rm --device /dev/kvm "${mounts[@]}" archix-arch-test /vm/boot.sh
