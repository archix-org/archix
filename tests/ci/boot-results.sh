#!/usr/bin/env bash
# Usage: boot-results.sh keys SUITE... | gather | split; a CI suites job's per-suite result caches: each suite's pass is kept under its own key in $RUNNER_TEMP/vm-cache/SUITE, whichever job runs it.
set -euo pipefail
# The workflow restores and saves up to this many caches per job, a step each.
MAX=4
cache=$RUNNER_TEMP/vm-cache
keys=$RUNNER_TEMP/vm-cache-keys
results=${ARCHIX_VM_RESULTS:?}
command=$1
shift
case $command in
  keys)
    [ $# -le $MAX ] || { echo "a suites job runs at most $MAX suites, not $#" >&2; exit 1; }
    python3 tests/vm/driver/cache.py keys "$@" > "$keys"
    i=0
    while read -r suite hash; do
      i=$((i + 1))
      echo "test$i=$suite" >> "$GITHUB_OUTPUT"
      echo "key$i=vm-result-$suite-${FS:-ext4}-$hash" >> "$GITHUB_OUTPUT"
    done < "$keys"
    ;;
  gather)
    mkdir -p "$results"
    for dir in "$cache"/*/; do
      [ ! -d "$dir" ] || cp -a "$dir". "$results"/
    done
    ;;
  split)
    i=0
    while read -r suite hash; do
      i=$((i + 1))
      test=$suite
      record=$results/$suite-$hash.json
      # A pass restored from the cache is saved already; only a new one is.
      if [ -f "$record" ] && [ ! -f "$cache/$test/${record##*/}" ]; then
        mkdir -p "$cache/$test"
        cp -a "$record" "$cache/$test/"
        [ ! -f "${record%.json}.logs.tar.gz" ] || cp -a "${record%.json}.logs.tar.gz" "$cache/$test/"
        echo "save$i=true" >> "$GITHUB_OUTPUT"
      fi
    done < "$keys"
    ;;
  *)
    echo "boot-results.sh: unknown command $command" >&2
    exit 2
    ;;
esac
