#!/usr/bin/env bash
# Runs the whole test suite, including the pacman/network tests, inside an Arch Linux container.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
docker build -q -t archix-arch-test "$root/tests/arch" >/dev/null
docker run --rm -v "$root":/src:ro -v archix-target:/target -v archix-cargo:/root/.cargo/registry \
  archix-arch-test cargo test --locked -- --include-ignored "$@"
