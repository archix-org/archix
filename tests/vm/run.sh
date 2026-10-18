#!/usr/bin/env bash
# Boots archix-managed generations in QEMU with OVMF; `run.sh` tests boot entries, Secure Boot and rescue, `run.sh rollback` the rollback torture test.
set -euo pipefail
suite=${1:-boot}
root=$(cd "$(dirname "$0")/../.." && pwd)
work=${ARCHIX_VM_WORK:-$(mktemp -d /tmp/archix-vm.XXXXXX)}
echo "work dir: $work"
docker build -t archix-arch-test "$root/tests/arch" >/tmp/archix-docker-build.log 2>&1 || { cat /tmp/archix-docker-build.log; exit 1; }
mounts=(-v "$root":/src:ro -v archix-target:/target -v archix-cargo:/root/.cargo/registry -v "$work":/work -v "$root/tests/vm":/vm:ro -e SUITE="$suite" -e FS="${FS:-ext4}")
docker run --rm "${mounts[@]}" archix-arch-test bash -c 'set -e
  cargo build --locked --release && cp /target/release/archix /work/archix
  cargo build --locked --release --target x86_64-unknown-linux-musl && cp /target/x86_64-unknown-linux-musl/release/archix /work/archix-static'
# ARCHIX_TEST_ARCHIVE (e.g. a local caching mirror on http://127.0.0.1:8079) stands in for archive.archlinux.org; the VM reaches the host as 10.0.2.2.
archive=${ARCHIX_TEST_ARCHIVE:-https://archive.archlinux.org}
docker run --rm --privileged --network host "${mounts[@]}" -e ARCHIVE="$archive" -e VM_ARCHIVE="${archive/127.0.0.1/10.0.2.2}" archix-arch-test /vm/build-image.sh
if [ "$suite" = rollback ]; then mounts+=(-e VM_TIMEOUT=14400 -e VM_MEM=6144 -e VM_CPUS=4); fi
docker run --rm --network host --device /dev/kvm "${mounts[@]}" archix-arch-test /vm/boot.sh
