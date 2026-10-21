#!/usr/bin/env bash
# Stops archive-proxy.py, summarises its log, and sets the step output new=true when it cached files worth saving.
set -euo pipefail
state=${RUNNER_TEMP:-/tmp}/archive-proxy
[ -f "$state/pid" ] || { echo "the archive proxy never started"; exit 0; }
kill "$(cat "$state/pid")" 2> /dev/null || true
rm -f "$state/pid"
log=$state/proxy.log
for outcome in HIT MISS PASS NOCACHE BAD; do
  printf '%s %s\n' "$outcome" "$(grep -c " $outcome " "$log" || true)"
done | tee "$state/summary"
grep ' BAD ' "$log" | sed 's/^/::warning::archive proxy: /' || true
new=0
[ ! -f "$state/new" ] || new=$(sort -u "$state/new" | wc -l)
echo "newly cached files: $new"
[ "$new" -eq 0 ] || echo "new=true" >> "${GITHUB_OUTPUT:-/dev/null}"
