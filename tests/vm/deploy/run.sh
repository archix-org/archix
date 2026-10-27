#!/usr/bin/env bash
# Builds the deployment prototype's VM image and boots it through its stages; FS=ext4|btrfs, DATE the base snapshot.
set -euo pipefail
root=$(cd "$(dirname "$0")/../../.." && pwd)
. "$root/tests/vm/work-dir.sh" deploy
"$root/tests/arch/image.sh"
archive=${ARCHIX_TEST_ARCHIVE:-https://archive.archlinux.org}
mounts=(-v "$work":/work -v "$root/tests/vm":/vm:ro -e FS="${FS:-ext4}" -e DATE="${DATE:-2026-10-03}")
docker run --rm --privileged --network host "${mounts[@]}" -e ARCHIVE="$archive" -e VM_ARCHIVE="${archive/127.0.0.1/10.0.2.2}" \
  archix-arch-test /vm/deploy/build.sh
docker run --rm --network host --device /dev/kvm "${mounts[@]}" -e VM_TIMEOUT=3600 -e VM_MEM=4096 -e VM_CPUS=4 archix-arch-test /vm/boot.sh
