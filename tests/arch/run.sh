#!/usr/bin/env bash
# Runs the whole suite, including pacman, AUR-build and network tests, inside a systemd-booted Arch container; `--jobs 2` splits it over two containers (both heavy slots), `--quick` runs the unit tests and only the test files tests/quick.tsv maps a change to, and CI's modes are ARCHIX_BUILD_OUT=DIR (build only), ARCHIX_PREBUILT=DIR (that build) and ARCHIX_TEST_LIST=FILE with ARCHIX_TEST_RESULTS=FILE (only FILE's tests).
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
# One build volume per checkout: cargo goes by mtimes, so a worktree sharing one could run another checkout's binary.
volume=archix-target-$(printf %s "$root" | sha256sum | cut -c1-12)
# ARCHIX_TARGET_VOLUME and ARCHIX_CARGO_VOLUME, a volume or host directory each, stand in for the build volume and the crate registry (CI caches them).
target=${ARCHIX_TARGET_VOLUME:-$volume} registry=${ARCHIX_CARGO_VOLUME:-archix-cargo}
# A second run on the same volume would rebuild the debug binaries this one's tests are running.
exec 9> "${XDG_RUNTIME_DIR:-/tmp}/$volume.lock"
flock -n 9 || { echo "another test run is using the build volume $target; waiting for it" >&2; flock 9; }
. "$root/tests/heavy-slot.sh"
[ "$jobs" = 1 ] || . "$root/tests/heavy-slot.sh"
"$root/tests/arch/image.sh"
snapshot=$(sed -n 's/^ARG SNAPSHOT=//p' "$root/tests/arch/Dockerfile")
# The newest snapshot the tests see, an old one so it's the same every day and a cache can hold it.
today=2026-10-20
cids=() relay= relay_dir= out= made_out=
cleanup() {
  [ -z "$out" ] || [ ${#cids[@]} = 0 ] || docker exec "${cids[0]}" chown -R "$(id -u):$(id -g)" /out || true
  [ ${#cids[@]} = 0 ] || docker rm -f "${cids[@]}" >/dev/null
  [ -z "$relay" ] || docker rm -f "$relay" >/dev/null
  [ -z "$relay_dir" ] || rm -rf "$relay_dir"
  [ -z "$made_out" ] || rm -rf "$out"
}
trap cleanup EXIT
if [ -n "$listed" ]; then
  out=${ARCHIX_BUILD_OUT:-}
  [ -n "$out" ] || { out=$(mktemp -d); made_out=1; }
  mkdir -p "$out"
fi
# ARCHIX_TEST_ARCHIVE, a caching proxy of the archive on this host's 127.0.0.1, reaches the container through a Unix socket, as the host's firewall may drop the bridge's traffic to it.
proxy_port=
if [[ ${ARCHIX_TEST_ARCHIVE:-} =~ ^http://(127\.0\.0\.1|localhost):([0-9]+)/?$ ]]; then
  proxy_port=${BASH_REMATCH[2]}
  relay_dir=$(mktemp -d)
  relay=$(docker run -d --network host -v "$relay_dir":/relay archix-arch-test \
    systemd-socket-activate -l /relay/archive.sock /usr/lib/systemd/systemd-socket-proxyd "127.0.0.1:$proxy_port")
elif [ -n "${ARCHIX_TEST_ARCHIVE:-}" ]; then
  echo "warning: ARCHIX_TEST_ARCHIVE=$ARCHIX_TEST_ARCHIVE isn't an http://127.0.0.1:PORT proxy, so the tests use archive.archlinux.org" >&2
fi
archive=()
# start: boots a test container, sets $cid, and points it at the archive proxy when there is one.
start() {
  cid=$(docker run -d --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw --tmpfs /run --tmpfs /tmp \
    -v "$root":/src:ro -v "$target":/target -v "$registry":/root/.cargo/registry ${relay_dir:+-v "$relay_dir":/archive-relay} \
    ${out:+-v "$out":/out} ${ARCHIX_PREBUILT:+-v "$ARCHIX_PREBUILT":/prebuilt:ro} archix-arch-test /usr/lib/systemd/systemd)
  cids+=("$cid")
  until state=$(docker exec "$cid" systemctl is-system-running 2>/dev/null); [[ $state == running || $state == degraded ]]; do sleep 1; done
  [ -n "$proxy_port" ] || return 0
  local url=http://127.0.0.1:$proxy_port
  docker exec "$cid" systemd-run --quiet --unit=archive-relay --socket-property="ListenStream=127.0.0.1:$proxy_port" \
    /usr/lib/systemd/systemd-socket-proxyd /archive-relay/archive.sock
  if docker exec "$cid" curl -sf -m 60 -o /dev/null "$url/repos/$snapshot/core/os/x86_64/core.db"; then
    [ ${#archive[@]} != 0 ] || echo "the tests reach archive.archlinux.org only through the proxy at $ARCHIX_TEST_ARCHIVE" >&2
    archive=(-e "ARCHIX_TEST_ARCHIVE=$url")
    docker exec "$cid" sed -i "s|https://archive.archlinux.org/|$url/|" /etc/pacman.d/mirrorlist
    # A request that misses the proxy then fails instead of passing while the archive happens to be up.
    docker exec "$cid" sh -c 'for a in $(getent ahosts archive.archlinux.org | cut -d" " -f1 | sort -u); do ip route add blackhole "$a" || [ "${a#*:}" != "$a" ] || exit 1; done'
  else
    echo "warning: the container can't reach the archive proxy at $ARCHIX_TEST_ARCHIVE, so the tests use archive.archlinux.org" >&2
  fi
}
start
# docker exec's umask is 0000 on some hosts and 0022 on GitHub's runners; pin systemd's, which boot-time hooks get.
run() { docker exec -e USER=root -e ARCHIX_TEST_TODAY="$today" "${archive[@]}" "$1" sh -c 'umask 022 && exec "$@"' sh "${@:2}"; }
if [ -z "$listed" ]; then
  # The initramfs runs the static musl build, which can lack libc items the glibc build has.
  docker exec -e USER=root "$cid" cargo build --locked --release --target x86_64-unknown-linux-musl
  run "$cid" cargo test --locked --no-fail-fast "$@" -- --include-ignored
  exit
fi
if [ -n "${ARCHIX_PREBUILT:-}" ]; then
  docker exec "$cid" tar -I zstd -xf /prebuilt/target.tar.zst -C /
  docker exec "$cid" cp /prebuilt/tests.tsv /out/tests.tsv
else
  docker exec -e USER=root "$cid" cargo build --locked --release --target x86_64-unknown-linux-musl
  # The glibc release build is only for the VM suites, which take it from ARCHIX_BUILD_OUT.
  [ -z "${ARCHIX_BUILD_OUT:-}" ] || docker exec -e USER=root "$cid" cargo build --locked --release
  docker exec -e USER=root "$cid" /src/tests/arch/list-tests.sh /out/tests.tsv
  # Doc tests have no executable of their own to hand a shard, so the build runs them.
  run "$cid" cargo test --locked --doc -- --include-ignored
fi
if [ -n "${ARCHIX_BUILD_OUT:-}" ]; then
  docker exec "$cid" sh -c 'tar -I "zstd -T0" -cf /out/target.tar.zst /target/release/archix /target/debug/archix \
    /target/x86_64-unknown-linux-musl/release/archix $(cut -f2 /out/tests.tsv | sort -u)'
  ls -l "$out"
  # A cached target keeps only the dependencies: archix's own artifacts would go stale against a fresh checkout's mtimes.
  [ -z "${ARCHIX_TARGET_VOLUME:-}" ] || docker exec "$cid" sh -c 'rm -rf /target/*/incremental /target/*/*/incremental
    find /target -path "*/deps/*" \( -name "libarchix-*" -o ! -name "lib*" ! -name "*.d" \) -delete
    find /target -maxdepth 4 \( -path "*/.fingerprint/archix-*" -o -path "*/build/archix-*" \) -prune -exec rm -rf {} +
    find /target -maxdepth 3 -type f -name "archix*" -delete
    du -sh /target /root/.cargo/registry'
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
if [ "$jobs" = 2 ]; then
  "$root/tests/ci/shard-plan.py" plan "$selected" "$root/tests/ci/test-times.tsv" 2 "$out/shards"
  first=$cid
  start
  status=0
  (run "$first" /src/tests/arch/run-tests.sh /out/shards/shard-1.tsv /out/results-1.tsv 2>&1 | sed -u 's/^/[1] /') &
  one=$!
  run "$cid" /src/tests/arch/run-tests.sh /out/shards/shard-2.tsv /out/results-2.tsv 2>&1 | sed -u 's/^/[2] /' || status=1
  wait "$one" || status=1
  cat "$out/results-1.tsv" "$out/results-2.tsv" > "$results" 2>/dev/null || true
else
  status=0
  run "$cid" /src/tests/arch/run-tests.sh /out/selected.tsv /out/results.tsv || status=1
fi
[ -z "${ARCHIX_TEST_RESULTS:-}" ] || { mkdir -p "$(dirname "$ARCHIX_TEST_RESULTS")" && cp "$results" "$ARCHIX_TEST_RESULTS"; }
"$root/tests/ci/shard-plan.py" check "$selected" "$results" || status=1
if [ -n "$quick" ] && [ -s "$out/skipped" ]; then
  echo "--quick did NOT run these test targets (tests in each); the full tests/arch/run.sh is still needed before pushing a change to src/:"
  sed 's/^/  /' "$out/skipped"
fi
exit "$status"
