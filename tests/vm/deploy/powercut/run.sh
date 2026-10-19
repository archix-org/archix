#!/usr/bin/env bash
# Power-cut replay of a switch on a target disk; FS, TARGET_PACKAGES, and REPLAY_POINTS (evenly spaced log prefixes instead of flush points).
set -euo pipefail
root=$(cd "$(dirname "$0")/../../../.." && pwd)
work=${ARCHIX_VM_WORK:-$(mktemp -d /tmp/archix-powercut.XXXXXX)}
echo "work dir: $work"
build_log=$(mktemp)
docker build -t archix-arch-test "$root/tests/arch" > "$build_log" 2>&1 || { cat "$build_log"; exit 1; }
rm -f "$build_log"
archive=${ARCHIX_TEST_ARCHIVE:-https://archive.archlinux.org}
docker run --rm -v "$work":/work archix-arch-test bash -c 'set -e
  git clone -q --depth 1 https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git /tmp/xfstests
  cd /tmp/xfstests/src/log-writes && gcc -O2 -o /work/replay-log replay-log.c log-writes.c'
build() {
  local dir=$1
  shift
  mkdir -p "$work/$dir"
  docker run --rm --privileged --network host -v "$work/$dir":/work -v "$work/replay-log":/work/replay-log:ro -v "$root/tests/vm":/vm:ro \
    -e ARCHIVE="$archive" -e VM_ARCHIVE="${archive/127.0.0.1/10.0.2.2}" -e DATE="${DATE:-2026-10-03}" "$@" archix-arch-test /vm/deploy/build.sh
}
build target -e FS="${FS:-ext4}" -e PREFETCH_TO="${TO_DATE:-2026-10-10}" -e EXTRA_PACKAGES="${TARGET_PACKAGES:-}"
build runner -e FS=ext4 -e POWERCUT=1 -e REPLAY_POINTS="${REPLAY_POINTS:-}" -e EXTRA_PACKAGES="lvm2 dosfstools e2fsprogs btrfs-progs" \
  -e TEST_SCRIPT=/vm/deploy/powercut/vmtest.sh
cp --sparse=always "$work/target/disk.img" "$work/pristine.img"
truncate -s 16G "$work/log.img"
docker run --rm --device /dev/kvm -v "$work":/work archix-arch-test bash -c '
  cp /usr/share/edk2/x64/OVMF_VARS.4m.fd /work/vars.fd
  timeout 14400 qemu-system-x86_64 -enable-kvm -cpu host -m 8192 -smp 4 -machine q35 \
    -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.4m.fd \
    -drive if=pflash,format=raw,unit=1,file=/work/vars.fd \
    -drive file=/work/runner/disk.img,format=raw,if=virtio \
    -drive file=/work/target/disk.img,format=raw,if=virtio,cache=none \
    -drive file=/work/log.img,format=raw,if=virtio,cache=none \
    -drive file=/work/pristine.img,format=raw,if=virtio,cache=none \
    -netdev user,id=net0 -device virtio-net-pci,netdev=net0,romfile= \
    -display none -serial file:/work/serial.log -monitor none || true
  grep -a VMTEST /work/serial.log || true
  grep -aq "VMTEST: OK" /work/serial.log'
