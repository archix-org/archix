#!/usr/bin/env bash
# Rehearses a signed release of the committed HEAD in the Arch test container with a throwaway key; the steps are in rehearse.sh.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
"$root/tests/arch/image.sh"
bundle=$(mktemp -d)
trap 'rm -rf "$bundle"' EXIT
git -C "$root" bundle create "$bundle/archix.bundle" HEAD >/dev/null 2>&1
chmod -R a+rX "$bundle"
docker run --rm -v "$bundle":/bundle:ro archix-arch-test bash -euo pipefail -c '
  useradd -m builder
  su builder -c "git clone -q /bundle/archix.bundle ~/archix && ~/archix/packaging/aur/rehearse.sh"
'
