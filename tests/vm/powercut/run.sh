#!/usr/bin/env bash
# Power-cut replay of archix's real switch, rollback, rebuild and first boots through dm-log-writes, with real nested boots at ESP points; FS=ext4|btrfs, DATE/TO_DATE the update (2026-10-03 to 2026-10-10; 2026-06-01 for four months), TARGET_PACKAGES, REPLAY_POINTS=N for evenly spaced prefixes, SAMPLE=N real boots of every Nth other point (empty: none), BREAK=entry-before-rename to see it fail, COUNTING= without boot counting.
set -euo pipefail
root=$(cd "$(dirname "$0")/../../.." && pwd)
target=archix-target-$(printf %s "$root" | sha256sum | cut -c1-12)
. "$root/tests/vm/work-dir.sh" powercut
build_log=$(mktemp)
docker build -t archix-arch-test "$root/tests/arch" > "$build_log" 2>&1 || { cat "$build_log"; exit 1; }
rm -f "$build_log"
date=${DATE:-2026-10-03} to=${TO_DATE:-2026-10-10}
size=8G
[ $(($(date -d "$to" +%s) - $(date -d "$date" +%s))) -le $((31 * 86400)) ] || size=12G
mounts=(-v "$root":/src:ro -v "$target":/target -v archix-cargo:/root/.cargo/registry -v "$work":/work -v "$root/tests/vm":/vm:ro)
# The marks and test-only breaks are compiled in only with debug assertions, which an optimised build can keep.
docker run --rm "${mounts[@]}" -e CARGO_TARGET_DIR=/target/powercut -e CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true archix-arch-test bash -c 'set -e
  cargo build --locked --release --target x86_64-unknown-linux-musl && cp /target/powercut/x86_64-unknown-linux-musl/release/archix /work/archix-static
  mkdir /tmp/log-writes && cd /tmp/log-writes
  for f in replay-log.c log-writes.c log-writes.h; do curl -sfo $f https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git/plain/src/log-writes/$f; done
  gcc -O2 -static -o /work/replay-log replay-log.c log-writes.c'
archive=${ARCHIX_TEST_ARCHIVE:-https://archive.archlinux.org}
env=(-e ARCHIVE="$archive" -e SUITE=powercut -e FS="${FS:-ext4}" -e DATE="$date" -e TO_DATE="$to" -e SIZE="$size" -e TARGET_PACKAGES="${TARGET_PACKAGES:-}"
  -e REPLAY_POINTS="${REPLAY_POINTS:-}" -e BREAK="${BREAK:-}" -e COUNTING="${COUNTING-1}" -e SAMPLE="${SAMPLE-20}")
docker run --rm --privileged --network host "${mounts[@]}" "${env[@]}" archix-arch-test /vm/build-image.sh
docker run --rm --privileged --network host "${mounts[@]}" "${env[@]}" archix-arch-test /vm/powercut/build-runner.sh
cp --sparse=always "$work/disk.img" "$work/pristine.img"
truncate -s 32G "$work/log.img"
docker run --rm --device /dev/kvm -v "$work":/work -e VM_TIMEOUT="${VM_TIMEOUT:-21600}" archix-arch-test bash -c '
  cp /usr/share/edk2/x64/OVMF_VARS.4m.fd /work/runner-vars.fd
  timeout "$VM_TIMEOUT" qemu-system-x86_64 -enable-kvm -cpu host -m 8192 -smp 4 -machine q35 \
    -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.4m.fd \
    -drive if=pflash,format=raw,unit=1,file=/work/runner-vars.fd \
    -drive file=/work/runner.img,format=raw,if=virtio \
    -drive file=/work/disk.img,format=raw,if=virtio,cache=none \
    -drive file=/work/log.img,format=raw,if=virtio,cache=none \
    -drive file=/work/pristine.img,format=raw,if=virtio,cache=none \
    -nic none -display none -serial file:/work/serial.log -monitor none || true
  grep -a VMTEST /work/serial.log || true
  grep -aq "VMTEST: OK" /work/serial.log'
