#!/usr/bin/env bash
# Runs the whole suite, including pacman, AUR-build and network tests, inside a systemd-booted Arch container.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
# One build volume per checkout: cargo goes by mtimes, so a worktree sharing one could run another checkout's binary.
target=archix-target-$(printf %s "$root" | sha256sum | cut -c1-12)
# A second run on the same volume would rebuild the debug binaries this one's tests are running.
exec 9> "${XDG_RUNTIME_DIR:-/tmp}/$target.lock"
flock -n 9 || { echo "another test run is using the build volume $target; waiting for it" >&2; flock 9; }
. "$root/tests/heavy-slot.sh"
build_log=$(mktemp)
docker build -t archix-arch-test "$root/tests/arch" >"$build_log" 2>&1 || { cat "$build_log"; exit 1; }
rm -f "$build_log"
snapshot=$(sed -n 's/^ARG SNAPSHOT=//p' "$root/tests/arch/Dockerfile")
# The newest snapshot the tests see, an old one so it's the same every day and a cache can hold it.
today=2026-10-20
cid= relay= relay_dir=
cleanup() {
  [ -z "$cid" ] || docker rm -f "$cid" >/dev/null
  [ -z "$relay" ] || docker rm -f "$relay" >/dev/null
  [ -z "$relay_dir" ] || rm -rf "$relay_dir"
}
trap cleanup EXIT
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
cid=$(docker run -d --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw --tmpfs /run --tmpfs /tmp \
  -v "$root":/src:ro -v "$target":/target -v archix-cargo:/root/.cargo/registry ${relay_dir:+-v "$relay_dir":/archive-relay} \
  archix-arch-test /usr/lib/systemd/systemd)
until state=$(docker exec "$cid" systemctl is-system-running 2>/dev/null); [[ $state == running || $state == degraded ]]; do sleep 1; done
archive=()
if [ -n "$proxy_port" ]; then
  url=http://127.0.0.1:$proxy_port
  docker exec "$cid" systemd-run --quiet --unit=archive-relay --socket-property="ListenStream=127.0.0.1:$proxy_port" \
    /usr/lib/systemd/systemd-socket-proxyd /archive-relay/archive.sock
  if docker exec "$cid" curl -sf -m 60 -o /dev/null "$url/repos/$snapshot/core/os/x86_64/core.db"; then
    echo "the tests reach archive.archlinux.org only through the proxy at $ARCHIX_TEST_ARCHIVE" >&2
    archive=(-e "ARCHIX_TEST_ARCHIVE=$url")
    docker exec "$cid" sed -i "s|https://archive.archlinux.org/|$url/|" /etc/pacman.d/mirrorlist
    # A request that misses the proxy then fails instead of passing while the archive happens to be up.
    docker exec "$cid" sh -c 'for a in $(getent ahosts archive.archlinux.org | cut -d" " -f1 | sort -u); do ip route add blackhole "$a" || [ "${a#*:}" != "$a" ] || exit 1; done'
  else
    echo "warning: the container can't reach the archive proxy at $ARCHIX_TEST_ARCHIVE, so the tests use archive.archlinux.org" >&2
  fi
fi
# The initramfs runs the static musl build, which can lack libc items the glibc build has.
docker exec -e USER=root "$cid" cargo build --locked --release --target x86_64-unknown-linux-musl
# docker exec's umask is 0000 on some hosts and 0022 on GitHub's runners; pin systemd's, which boot-time hooks get.
docker exec -e USER=root -e ARCHIX_TEST_TODAY="$today" "${archive[@]}" "$cid" sh -c 'umask 022 && exec "$@"' sh cargo test --locked --no-fail-fast "$@" -- --include-ignored
