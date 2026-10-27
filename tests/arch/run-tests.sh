#!/usr/bin/env bash
# Usage, in the test container: run-tests.sh LIST RESULTS; runs LIST's tests (list-tests.sh's lines) from their built executables, one executable at a time as cargo test does, appending each test's target, name and outcome to RESULTS.
set -uo pipefail
cd /src || exit 1
status=0
for exe in $(cut -f2 "$1" | awk '!seen[$0]++'); do
  name=$(awk -F'\t' -v e="$exe" '$2 == e { print $1; exit }' "$1")
  mapfile -t tests < <(awk -F'\t' -v e="$exe" '$2 == e { print $3 }' "$1")
  log=$(mktemp)
  echo "     Running $name ($exe): ${#tests[@]} tests"
  "$exe" --include-ignored --exact --logfile "$log" "${tests[@]}" || status=1
  # libtest logs "ok NAME" or "failed NAME" per test.
  awk -v n="$name" '{ outcome = $1; sub(/^[^ ]+ /, ""); print n "\t" $0 "\t" outcome }' "$log" >> "$2"
  ran=$(wc -l < "$log")
  [ "$ran" = "${#tests[@]}" ] || { echo "$name: ran $ran of the ${#tests[@]} tests listed" >&2; status=1; }
  rm -f "$log"
done
exit "$status"
