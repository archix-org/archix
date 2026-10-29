#!/usr/bin/env bash
# Usage: offline-check.sh [--warm] [tests/arch/run.sh args]; runs the container suite (default --jobs 2) with no network but its own archive proxy, whose upstream is a closed port, so any test that needs the internet fails; --warm runs it online first to fill the proxy's cache.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
warm=
[ "${1:-}" != --warm ] || { warm=1; shift; }
[ $# != 0 ] || set -- --jobs 2
state=${ARCHIX_OFFLINE_STATE:-${XDG_CACHE_HOME:-$HOME/.cache}/archix-offline-check}
port=${ARCHIX_OFFLINE_PORT:-8093}
mkdir -p "$state/cache"
# Port 9 (discard) has no listener, so a cache miss is refused at once.
upstream=http://127.0.0.1:9 offline=1
[ -z "$warm" ] || upstream=${ARCHIX_OFFLINE_UPSTREAM:-https://archive.archlinux.org} offline=
log=$state/proxy-$(date +%Y%m%d-%H%M%S)${warm:+-warm}.log
python3 "$root/tests/ci/archive-proxy.py" serve --port "$port" --cache "$state/cache" --upstream "$upstream" --log "$log" &
proxy=$!
trap 'kill "$proxy" 2> /dev/null || true' EXIT
for _ in $(seq 50); do
  curl -sf -o /dev/null "http://127.0.0.1:$port/archix-proxy" && break
  sleep 0.2
done
status=0
ARCHIX_TEST_ARCHIVE=http://127.0.0.1:$port ARCHIX_TEST_OFFLINE=$offline "$root/tests/arch/run.sh" "$@" || status=$?
for outcome in HIT MISS PASS UNLISTED; do
  printf '%s %s\n' "$outcome" "$(grep -c " $outcome " "$log" || true)"
done
[ -n "$warm" ] || grep -E ' (MISS|PASS) ' "$log" | sed 's/^/the archive proxy could not answer: /' || true
echo "proxy log: $log"
exit "$status"
