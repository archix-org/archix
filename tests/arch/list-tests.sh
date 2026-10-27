#!/usr/bin/env bash
# Usage, in the test container: list-tests.sh OUT; builds every test executable cargo test would run and writes OUT, one target, executable and test per line.
set -euo pipefail
exes=$(mktemp)
cargo test --locked --no-run --message-format=json-render-diagnostics |
  jq -r 'select(.reason == "compiler-artifact" and .executable != null and .profile.test) | "\(.target.kind[0]):\(.target.name)\t\(.executable)"' | sort -u > "$exes"
: > "$1"
while IFS=$'\t' read -r name exe; do
  "$exe" --list --include-ignored | sed -n 's/: test$//p' | while read -r test; do printf '%s\t%s\t%s\n' "$name" "$exe" "$test"; done >> "$1"
done < "$exes"
echo "$(wc -l < "$1") tests in $(wc -l < "$exes") executables"
