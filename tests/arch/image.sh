#!/usr/bin/env bash
# Usage: image.sh [docker build args]; builds archix-arch-test, showing the build only if it fails, unless the image is labelled as built from this Dockerfile, which has no COPY so is all it's built from.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
want=$(sha256sum "$here/Dockerfile" | cut -c1-64)
[ "$(docker image inspect -f '{{index .Config.Labels "archix.dockerfile"}}' archix-arch-test 2> /dev/null)" != "$want" ] || exit 0
log=$(mktemp)
docker build --label "archix.dockerfile=$want" "$@" -t archix-arch-test "$here" > "$log" 2>&1 || { cat "$log"; rm -f "$log"; exit 1; }
rm -f "$log"
