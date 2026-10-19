#!/usr/bin/env bash
# Kills the deployment switch at every step and at timed points, on an image built at DATE (plus TARGET_PACKAGES) with TO_DATE's update prefetched.
set -euo pipefail
root=$(cd "$(dirname "$0")/../../.." && pwd)
. "$root/tests/vm/work-dir.sh" kill
build_log=$(mktemp)
docker build -t archix-arch-test "$root/tests/arch" > "$build_log" 2>&1 || { cat "$build_log"; exit 1; }
rm -f "$build_log"
archive=${ARCHIX_TEST_ARCHIVE:-https://archive.archlinux.org}
docker run --rm --privileged --network host -v "$work":/work -v "$root/tests/vm":/vm:ro -e FS="${FS:-ext4}" -e DATE="${DATE:-2026-10-03}" \
  -e ARCHIVE="$archive" -e VM_ARCHIVE="${archive/127.0.0.1/10.0.2.2}" -e PREFETCH_TO="${TO_DATE:-2026-10-10}" -e EXTRA_PACKAGES="${TARGET_PACKAGES:-}" \
  archix-arch-test bash -c '/vm/deploy/build.sh > /work/build.log 2>&1 || { tail -20 /work/build.log; exit 1; }'
# The test gets a container of its own, so nothing the build started can keep the image mounted.
docker run --rm --privileged -v "$work":/work -v "$root/tests/vm":/vm:ro -e FS="${FS:-ext4}" -e KILL_SECONDS="${KILL_SECONDS:-}" archix-arch-test /vm/deploy/kill-test
