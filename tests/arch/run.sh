#!/usr/bin/env bash
# Runs the whole suite, including pacman, AUR-build and network tests, in systemd-booted Arch test VMs (tests/testvm), built in one and run on throwaway copies of its build; `--jobs 2` splits it over two VMs (both heavy slots), `--quick` runs the unit tests and only the test files tests/quick.tsv maps a change to, and CI's modes are ARCHIX_BUILD_OUT=DIR (build only), ARCHIX_PREBUILT=DIR (that build) and ARCHIX_TEST_LIST=FILE with ARCHIX_TEST_RESULTS=FILE (only FILE's tests).
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
quick= jobs=1
while [ $# -gt 0 ]; do
  case $1 in
    --quick) quick=1 ;;
    --jobs) jobs=${2:-}; shift ;;
    *) break ;;
  esac
  shift
done
case $jobs in 1 | 2) ;; *) echo "--jobs takes 1 or 2: one container per heavy slot" >&2; exit 2 ;; esac
listed=
[ -z "$quick${ARCHIX_BUILD_OUT:-}${ARCHIX_TEST_LIST:-}" ] && [ "$jobs" = 1 ] || listed=1
[ -z "$listed" ] || [ $# = 0 ] || { echo "--quick, --jobs and the CI modes take no cargo test arguments" >&2; exit 2; }
# One build disk per checkout: cargo goes by mtimes, so a worktree sharing one could run another checkout's binary.
volume=archix-target-$(printf %s "$root" | sha256sum | cut -c1-12)
disks=${ARCHIX_TESTVM_CACHE:-$HOME/.cache/archix-testvm}/disks
mkdir -p "$disks"
# ARCHIX_TARGET_VOLUME and ARCHIX_CARGO_VOLUME, a host directory each (CI caches them), stand in for the build disk and the crate registry.
registry=${ARCHIX_CARGO_VOLUME:-$disks/cargo-registry}
mkdir -p "$registry"
# A second run on the same build disk would rebuild the debug binaries this one's tests are running.
exec 9> "${XDG_RUNTIME_DIR:-/tmp}/$volume.lock"
flock -n 9 || { echo "another test run is using the build disk $volume; waiting for it" >&2; flock 9; }
. "$root/tests/heavy-slot.sh"
[ "$jobs" = 1 ] || . "$root/tests/heavy-slot.sh"
"$root/tests/arch/image.sh"
snapshot=$(sed -n 's/^ARG SNAPSHOT=//p' "$root/tests/arch/Dockerfile")
# The newest snapshot the tests see, an old one so it's the same every day and a cache can hold it.
today=2026-10-20
out= made_out=
cleanup() {
  [ -z "$made_out" ] || rm -rf "$out"
}
trap cleanup EXIT
if [ -n "$listed" ]; then
  out=${ARCHIX_BUILD_OUT:-}
  [ -n "$out" ] || { out=$(mktemp -d); made_out=1; }
  mkdir -p "$out"
fi
# The suite runs in test VMs (tests/testvm), so nothing it does, loop devices, btrfs mounts or systemd, reaches the host; the host's 127.0.0.1 archive proxy is the guest's 10.0.2.2, or 10.0.2.100 offline.
proxy_port=
if [[ ${ARCHIX_TEST_ARCHIVE:-} =~ ^http://(127\.0\.0\.1|localhost):([0-9]+)/?$ ]]; then
  proxy_port=${BASH_REMATCH[2]}
elif [ -n "${ARCHIX_TEST_ARCHIVE:-}" ]; then
  echo "warning: ARCHIX_TEST_ARCHIVE=$ARCHIX_TEST_ARCHIVE isn't an http://127.0.0.1:PORT proxy, so the tests use archive.archlinux.org" >&2
fi
# vm TARGET-DISK-MODE JOB...: one test VM with the source read-only, the registry and OUT; TARGET-DISK-MODE rw builds into the checkout's disk, overlay runs on a throwaway copy of it.
vm() {
  local target_mode=$1 target_args=()
  shift
  if [ -n "${ARCHIX_TARGET_VOLUME:-}" ]; then
    mkdir -p "$ARCHIX_TARGET_VOLUME"
    target_args=(--share "target=$ARCHIX_TARGET_VOLUME" --mount 9p target /target rw)
  elif [ "$target_mode" = overlay ]; then
    target_args=(--disk "target=$disks/$volume.qcow2:overlay" --mount disk target /target rw)
  else
    target_args=(--disk "target=$disks/$volume.qcow2:200G" --mount disk target /target rw)
  fi
  "$root/tests/testvm/run.sh" --share "src=$root:ro" --mount 9p src /src ro "${target_args[@]}" \
    --share "registry=$registry" --mount 9p registry /root/.cargo/registry rw \
    ${out:+--share "out=$out" --mount 9p out /out rw} ${ARCHIX_PREBUILT:+--share "prebuilt=$ARCHIX_PREBUILT:ro" --mount 9p prebuilt /prebuilt ro} \
    ${ARCHIX_TEST_OFFLINE:+--offline --env ARCHIX_TEST_OFFLINE=1} ${proxy_port:+--proxy "$proxy_port" --env PROXY_PORT="$proxy_port"} \
    --env PROXY_HOST="$([ -n "${ARCHIX_TEST_OFFLINE:-}" ] && echo 10.0.2.100 || echo 10.0.2.2)" --env SNAPSHOT="$snapshot" --env ARCHIX_TEST_TODAY="$today" \
    ${ARCHIX_PREBUILT:+--env PREBUILT=1} ${ARCHIX_BUILD_OUT:+--env BUILD_OUT=1} ${ARCHIX_TARGET_VOLUME:+--env PRUNE_TARGET=1} \
    --cwd /src -- /src/tests/arch/in-vm.sh "$@"
}
if [ -z "$listed" ]; then
  vm rw single "$@"
  exit
fi
vm rw build
if [ -n "${ARCHIX_BUILD_OUT:-}" ]; then
  ls -l "$out"
  exit
fi
selected=$out/selected.tsv
if [ -n "${ARCHIX_TEST_LIST:-}" ]; then
  cp "$ARCHIX_TEST_LIST" "$selected"
elif [ -n "$quick" ]; then
  "$root/tests/arch/quick.sh" "$out/tests.tsv" "$selected" "$out/skipped"
else
  cp "$out/tests.tsv" "$selected"
fi
results=$out/results.tsv
: > "$results"
# The tests run on throwaway copies of the build, so two VMs can run side by side.
if [ "$jobs" = 2 ]; then
  "$root/tests/ci/shard-plan.py" plan "$selected" "$root/tests/ci/test-times.tsv" 2 "$out/shards"
  status=0
  (vm overlay tests /out/shards/shard-1.tsv /out/results-1.tsv 2>&1 | sed -u 's/^/[1] /') &
  one=$!
  vm overlay tests /out/shards/shard-2.tsv /out/results-2.tsv 2>&1 | sed -u 's/^/[2] /' || status=1
  wait "$one" || status=1
  cat "$out/results-1.tsv" "$out/results-2.tsv" > "$results" 2>/dev/null || true
else
  status=0
  vm overlay tests /out/selected.tsv /out/results.tsv || status=1
fi
[ -z "${ARCHIX_TEST_RESULTS:-}" ] || { mkdir -p "$(dirname "$ARCHIX_TEST_RESULTS")" && cp "$results" "$ARCHIX_TEST_RESULTS"; }
"$root/tests/ci/shard-plan.py" check "$selected" "$results" || status=1
if [ -n "$quick" ] && [ -s "$out/skipped" ]; then
  echo "--quick did NOT run these test targets (tests in each); the full tests/arch/run.sh is still needed before pushing a change to src/:"
  sed 's/^/  /' "$out/skipped"
fi
exit "$status"
