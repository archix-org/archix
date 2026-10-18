#!/usr/bin/env bash
# Fails when a lock (archix.lock by default) has a dkms the NVIDIA VM suite hasn't passed with; archix's DKMS fix depends on how dkms handles original_module/.
set -euo pipefail
lock=${1:-/etc/archix/archix.lock}
tested=$(dirname "$0")/nvidia/dkms-tested
versions=$(sed -n 's/^dkms = .*version = "\([^"]*\)".*/\1/p' "$lock" | sort -u)
[ -n "$versions" ] || { echo "no dkms in $lock"; exit 0; }
status=0
for version in $versions; do
  if grep -qx "$version" "$tested"; then
    echo "dkms $version: tested"
  else
    echo "dkms $version: not tested; run TO_DATE=<a snapshot with it> tests/vm/run.sh nvidia, then add it to tests/vm/nvidia/dkms-tested"
    status=1
  fi
done
exit $status
