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
cid=$(docker run -d --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw --tmpfs /run --tmpfs /tmp \
  -v "$root":/src:ro -v "$target":/target -v archix-cargo:/root/.cargo/registry \
  archix-arch-test /usr/lib/systemd/systemd)
trap 'docker rm -f "$cid" >/dev/null' EXIT
until state=$(docker exec "$cid" systemctl is-system-running 2>/dev/null); [[ $state == running || $state == degraded ]]; do sleep 1; done
# The initramfs runs the static musl build, which can lack libc items the glibc build has.
docker exec -e USER=root "$cid" cargo build --locked --release --target x86_64-unknown-linux-musl
docker exec -e USER=root "$cid" cargo test --locked --no-fail-fast "$@" -- --include-ignored
