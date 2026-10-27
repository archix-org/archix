#!/usr/bin/env bash
# Usage: proxy-up.sh CACHE [READ-ONLY-CACHE...]; verifies the caches, starts archive-proxy.py on 127.0.0.1 and docker0, and builds archix-arch-test through it.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
cache=$1
shift
port=${ARCHIX_PROXY_PORT:-8079}
state=${RUNNER_TEMP:-/tmp}/archive-proxy
mkdir -p "$state" "$cache"
extra=()
for dir in "$@"; do
  [ "$dir" = "$cache" ] || extra+=(--extra "$dir")
done
python3 "$here/archive-proxy.py" verify "$cache" "$@" | tee "$state/verify.log"
grep '^removed ' "$state/verify.log" | sed 's/^/::warning::archive cache: /' || true
bridge=$(ip -4 -o addr show docker0 2> /dev/null | awk '{ sub(/\/.*/, "", $4); print $4 }')
binds=(--bind 127.0.0.1)
[ -z "$bridge" ] || binds+=(--bind "$bridge")
nohup python3 "$here/archive-proxy.py" serve --port "$port" "${binds[@]}" --cache "$cache" "${extra[@]}" \
  --log "$state/proxy.log" --new-list "$state/new" > "$state/stderr.log" 2>&1 &
echo $! > "$state/pid"
for _ in $(seq 50); do
  curl -sf -o /dev/null "http://127.0.0.1:$port/archix-proxy" && break
  sleep 0.2
done
curl -sf -o /dev/null "http://127.0.0.1:$port/archix-proxy" || { cat "$state/stderr.log"; echo "::error::the archive proxy didn't start" >&2; exit 1; }
echo "ARCHIX_TEST_ARCHIVE=http://127.0.0.1:$port" >> "${GITHUB_ENV:-/dev/null}"
# Image builds run on the bridge network, which reaches the proxy on docker0; the secret doesn't enter the layer cache key, so the runners' own builds reuse this one.
mirror=
if [ -n "$bridge" ] && docker run --rm archlinux:latest curl -sf -m 10 -o /dev/null "http://$bridge:$port/archix-proxy"; then
  mirror=http://$bridge:$port
else
  echo "::warning::image builds can't reach the archive proxy on docker0, so the test image comes straight from archive.archlinux.org"
fi
ARCHIVE_MIRROR=$mirror docker build ${mirror:+--secret id=archive,env=ARCHIVE_MIRROR} --label "archix.dockerfile=$(sha256sum "$root/tests/arch/Dockerfile" | cut -c1-64)" -t archix-arch-test "$root/tests/arch"
