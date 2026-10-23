#!/usr/bin/env bash
# Power-cut replay of archix's real switch, rollback, rebuild and first boots through dm-log-writes, with real nested boots at ESP points: one runner boot logs the run (FS=ext4|btrfs, DATE/TO_DATE the update, 2026-10-03 to 2026-10-10 or 2026-06-01 for four months, TARGET_PACKAGES, BREAK=entry-before-rename|renamed-unsynced|data-unsynced to see it fail), SAVE_LOG=DIR keeps that log, RECORD_ONLY=1 stops there, and LOG=DIR (or its .tar.zst) replays a kept log instead of logging one; a second boot replays it, REPLAY_POINTS=N for evenly spaced prefixes, SAMPLE=N real boots of every Nth other point (empty: none), SUBSETS=N seeded random subsets (16; 0: none) of the unflushed ESP writes at each ESP flush point, REORDER_SAMPLE=N real boots of every Nth subset (16; empty: none) and of each point's first that breaks a FAT chain, BOOT_RATIO (3) and BOOT_CEILING (1200 s) failing a real boot slower than that many times the pristine image's or than that, BOOT_TIMEOUT (3600 s) for a hung one, SHARD=i/N checking every Nth point of the log by its index, BREAK=torn-file|slow-boot to see the replay fail, COUNTING= without boot counting, SUMMARY=FILE for the shard's coverage as JSON; SEED defaults to GITHUB_RUN_ID or today's date and is printed first, and a failure reproduces exactly as `LOG=<the logged run's dir or powercut-log-FS.tar.zst artifact> SEED=<printed seed> SHARD=<its shard> tests/vm/run.sh powercut`; COMPARE=1 instead logs one switch and its first boot twice with this build and twice with a plain release build and compares their writes, flushes and FUAs.
set -euo pipefail
root=$(cd "$(dirname "$0")/../../.." && pwd)
target=archix-target-$(printf %s "$root" | sha256sum | cut -c1-12)
. "$root/tests/vm/work-dir.sh" powercut
build_log=$(mktemp)
docker build -t archix-arch-test "$root/tests/arch" > "$build_log" 2>&1 || { cat "$build_log"; exit 1; }
rm -f "$build_log"
seed=${SEED:-${GITHUB_RUN_ID:-$(date -u +%Y%m%d)}}
echo "power-cut replay: SEED=$seed"
mounts=(-v "$root":/src:ro -v "$target":/target -v archix-cargo:/root/.cargo/registry -v "$work":/work -v "$root/tests/vm":/vm:ro)
log=${LOG:-}
if [ -n "$log" ]; then
  if [ -f "$log" ]; then
    mkdir "$work/log"
    tar -I zstd -xf "$log" -C "$work/log"
    log=$work/log
  fi
  log=$(cd "$log" && pwd)
  cp "$log/archix-static" "$log/replay-log" "$work/"
  echo "replaying the logged run in $log: $(cat "$log/meta")"
else
  # The marks and test-only breaks are compiled in only with debug assertions, which an optimised build can keep.
  docker run --rm "${mounts[@]}" -e CARGO_TARGET_DIR=/target/powercut -e CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true -e COMPARE="${COMPARE:-}" archix-arch-test bash -c 'set -e
    cargo build --locked --release --target x86_64-unknown-linux-musl && cp /target/powercut/x86_64-unknown-linux-musl/release/archix /work/archix-static
    if [ -n "$COMPARE" ]; then
      CARGO_TARGET_DIR=/target/powercut-plain CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=false cargo build --locked --release --target x86_64-unknown-linux-musl
      cp /target/powercut-plain/x86_64-unknown-linux-musl/release/archix /work/archix-plain
    fi
    mkdir /tmp/log-writes && cd /tmp/log-writes
    for f in replay-log.c log-writes.c log-writes.h; do curl -sfo $f https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git/plain/src/log-writes/$f; done
    gcc -O2 -static -o /work/replay-log replay-log.c log-writes.c'
fi
archive=${ARCHIX_TEST_ARCHIVE:-https://archive.archlinux.org}
date=${DATE:-2026-10-03} to=${TO_DATE:-2026-10-10}
size=8G
[ $(($(date -d "$to" +%s) - $(date -d "$date" +%s))) -le $((31 * 86400)) ] || size=12G
env=(-e ARCHIVE="$archive" -e SUITE=powercut -e FS="${FS:-ext4}" -e DATE="$date" -e TO_DATE="$to" -e SIZE="$size" -e TARGET_PACKAGES="${TARGET_PACKAGES:-}")
[ -n "$log" ] || docker run --rm --privileged --network host "${mounts[@]}" "${env[@]}" archix-arch-test /vm/build-image.sh
docker run --rm --privileged --network host "${mounts[@]}" "${env[@]}" archix-arch-test /vm/powercut/build-runner.sh

# vm PHASE TARGET LOG PRISTINE EXPORT [readonly]: boots a fresh copy of the runner with this phase's settings on vdf; paths are inside the container, where $work is /work and LOG's directory /log.
vm() {
  local phase=$1 ro=${6:+,readonly=on}
  printf 'PHASE=%s\nPOINTS=%s\nBREAK=%s\nCOUNTING=%s\nSAMPLE=%s\nSUBSETS=%s\nREORDER_SAMPLE=%s\nSEED=%s\nBOOT_TIMEOUT=%s\nBOOT_RATIO=%s\nBOOT_CEILING=%s\nSHARD=%s\n' "$phase" "${REPLAY_POINTS:-}" \
    "${BREAK:-}" "${COUNTING-1}" "${SAMPLE-20}" "${SUBSETS-16}" "${REORDER_SAMPLE-16}" "$seed" "${BOOT_TIMEOUT:-3600}" "${BOOT_RATIO:-3}" "${BOOT_CEILING:-1200}" "${SHARD:-}" > "$work/conf.img"
  truncate -s 1M "$work/conf.img"
  cp --sparse=always "$work/runner.img" "$work/run.img"
  rm -f "$work/serial.log"
  docker run --rm --device /dev/kvm -v "$work":/work ${log:+-v "$log":/log:ro} -e VM_TIMEOUT="${VM_TIMEOUT:-21600}" archix-arch-test bash -c '
    cp /usr/share/edk2/x64/OVMF_VARS.4m.fd /work/runner-vars.fd
    timeout "$VM_TIMEOUT" qemu-system-x86_64 -enable-kvm -cpu host -m 8192 -smp 4 -machine q35 \
      -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.4m.fd \
      -drive if=pflash,format=raw,unit=1,file=/work/runner-vars.fd \
      -drive file=/work/run.img,format=raw,if=virtio \
      -drive file="$1",format=raw,if=virtio,cache=none \
      -drive file="$2",format=raw,if=virtio,cache=none$5 \
      -drive file="$3",format=raw,if=virtio,cache=none \
      -drive file="$4",format=raw,if=virtio,cache=none$5 \
      -drive file=/work/conf.img,format=raw,if=virtio,readonly=on \
      -nic none -display none -serial file:/work/serial.log -monitor none || true
    grep -a VMTEST /work/serial.log || true
    grep -aq "VMTEST: OK" /work/serial.log' vm "$2" "$3" "$4" "$5" "$ro"
}

if [ -z "$log" ]; then
  cp --sparse=always "$work/disk.img" "$work/pristine.img"
  truncate -s 32G "$work/log.img"
  if [ -n "${COMPARE:-}" ]; then
    truncate -s 1M "$work/export.img"
    vm compare /work/disk.img /work/log.img /work/pristine.img /work/export.img
    exit
  fi
  truncate -s 512M "$work/export.img"
  vm record /work/disk.img /work/log.img /work/pristine.img /work/export.img
  # The log's sectors past what dm-log-writes used, and the target after the run, aren't needed to replay it.
  read -r logged used points esp_points log_id < <(tar -xOf "$work/export.img" logged)
  truncate -s $(((used + 8192) * 512)) "$work/log.img"
  mkdir "$work/log"
  mv "$work/pristine.img" "$work/log.img" "$work/export.img" "$work/log/"
  cp "$work/archix-static" "$work/replay-log" "$work/log/"
  echo "$logged $used $points $esp_points $log_id" > "$work/log/logged"
  rm -f "$work/disk.img"
  echo "log $log_id: fs ${FS:-ext4}, $date to $to, $logged writes, $points points, $esp_points after ESP writes, archix $(git -C "$root" rev-parse --short HEAD)" > "$work/log/meta"
  cat "$work/log/meta"
  if [ -n "${SAVE_LOG:-}" ]; then
    mkdir -p "$SAVE_LOG"
    cp --sparse=always "$work/log/"* "$SAVE_LOG/"
    echo "kept the logged run in $SAVE_LOG"
  fi
  [ -z "${RECORD_ONLY:-}" ] || exit 0
  log=$work/log
fi
cp --sparse=always "$log/pristine.img" "$work/pristine.img"
truncate -s 1M "$work/target.img"
status=0
vm replay /work/target.img /log/log.img /work/pristine.img /log/export.img ro || status=$?
summary=$(grep -a -o 'VMTEST: SUMMARY: .*' "$work/serial.log" | tail -1 | sed 's/^VMTEST: SUMMARY: //' | tr -d '\r')
if [ -n "${SUMMARY:-}" ] && [ -n "$summary" ]; then
  mkdir -p "$(dirname "$SUMMARY")"
  echo "$summary" > "$SUMMARY"
fi
[ "$status" = 0 ] || echo "power-cut replay failed: reproduce with LOG=<the logged run> SEED=$seed${SHARD:+ SHARD=$SHARD} tests/vm/run.sh powercut ($(cat "$log/meta"))"
exit "$status"
