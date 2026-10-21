#!/usr/bin/env bash
# Usage: collect-logs.sh DEST; copies the logs, never the disk images, out of ARCHIX_VM_WORK and the archive proxy's state into DEST.
set -euo pipefail
dest=$1
mkdir -p "$dest"
work=${ARCHIX_VM_WORK:-}
if [ -n "$work" ] && [ -d "$work" ]; then
  sudo find "$work" -type f \( -name '*.log' -o -name '*.txt' -o -name '*.out' \) -size -500M -printf '%P\n' |
    while read -r file; do sudo install -D -m 0644 "$work/$file" "$dest/work/$file"; done
  find "$dest/work" -name serial.log -print0 2> /dev/null | while IFS= read -r -d '' serial; do
    echo "::group::last 200 lines of $serial"
    tail -n 200 "$serial" | tr -d '\000'
    echo "::endgroup::"
  done
fi
cp -r "${RUNNER_TEMP:-/tmp}/archive-proxy" "$dest/" 2> /dev/null || true
sudo chown -R "$(id -u):$(id -g)" "$dest"
du -sh "$dest"
