#!/usr/bin/env bash
# Runs the whole suite, including pacman, AUR-build and network tests, inside a systemd-booted Arch container.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
docker build -t archix-arch-test "$root/tests/arch" >/tmp/archix-docker-build.log 2>&1 || { cat /tmp/archix-docker-build.log; exit 1; }
cid=$(docker run -d --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw --tmpfs /run --tmpfs /tmp \
  -v "$root":/src:ro -v archix-target:/target -v archix-cargo:/root/.cargo/registry \
  archix-arch-test /usr/lib/systemd/systemd)
trap 'docker rm -f "$cid" >/dev/null' EXIT
until state=$(docker exec "$cid" systemctl is-system-running 2>/dev/null); [[ $state == running || $state == degraded ]]; do sleep 1; done
docker exec -e USER=root "$cid" cargo test --locked --no-fail-fast "$@" -- --include-ignored
