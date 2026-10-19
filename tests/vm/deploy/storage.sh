#!/usr/bin/env bash
# Measures deployment storage against an in-place upgrade on ext4, btrfs and btrfs with zstd, for the four-month and one-week updates.
set -euo pipefail
root=$(cd "$(dirname "$0")/../../.." && pwd)
work=${ARCHIX_VM_WORK:-$(mktemp -d /tmp/archix-storage.XXXXXX)}
echo "work dir: $work"
build_log=$(mktemp)
docker build -t archix-arch-test "$root/tests/arch" > "$build_log" 2>&1 || { cat "$build_log"; exit 1; }
rm -f "$build_log"
archive=${ARCHIX_TEST_ARCHIVE:-https://archive.archlinux.org}
packages="base-devel devtools firefox rust git"
for fs in ext4 btrfs btrfs:compress=zstd; do
  for range in 2026-06-01:2026-10-10 2026-10-03:2026-10-10; do
    env=(-e FS="${fs%%:*}" -e BTRFS_OPTS="$([ "$fs" = "${fs#*:}" ] || echo "${fs#*:}")" -e DATE="${range%:*}" -e TO_DATE="${range#*:}")
    docker run --rm --privileged --network host -v "$work":/work -v "$root/tests/vm":/vm:ro "${env[@]}" -e ARCHIVE="$archive" \
      -e VM_ARCHIVE="${archive/127.0.0.1/10.0.2.2}" -e EXTRA_PACKAGES="$packages" -e PREFETCH_TO="${range#*:}" archix-arch-test \
      bash -c '/vm/deploy/build.sh > /work/build.log 2>&1 || { tail -20 /work/build.log; exit 1; }'
    docker run --rm --privileged -v "$work":/work -v "$root/tests/vm":/vm:ro "${env[@]}" archix-arch-test /vm/deploy/storage-measure
  done
done
