#!/usr/bin/env bash
# Usage: quick.sh TESTS SELECTED SKIPPED; picks from TESTS (list-tests.sh's lines) the always-run targets of tests/quick.tsv and those its longest matching prefix maps each file changed since main (or ARCHIX_QUICK_BASE, e.g. HEAD) to, writing the skipped targets to SKIPPED.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
map=$root/tests/quick.tsv
if [ -n "${ARCHIX_QUICK_BASE:-}" ]; then
  base=$(git -C "$root" rev-parse --verify "$ARCHIX_QUICK_BASE^{commit}")
else
  base=$(git -C "$root" merge-base HEAD main 2>/dev/null || git -C "$root" rev-parse HEAD)
fi
changed=$( { git -C "$root" diff --name-only "$base"; git -C "$root" ls-files --others --exclude-standard; } | sort -u)
all=
picked=$(sed -n 's/^always\t//p' "$map")
for file in $changed; do
  case $file in
    tests/common/* | Cargo.toml | Cargo.lock | build.rs | tests/arch/Dockerfile) echo "quick: $file changed, so every test runs" >&2; all=1 ;;
    tests/*.rs) picked+=" test:$(basename "$file" .rs)" ;;
    src/*)
      targets=$(awk -F'\t' -v f="$file" '$1 != "always" && index(f, $1) == 1 && length($1) > best { best = length($1); t = $2 } END { print t }' "$map")
      if [ -n "$targets" ]; then picked+=" $targets"; else echo "quick: tests/quick.tsv maps no tests to $file, so every test runs" >&2; all=1; fi ;;
  esac
done
if [ -n "$all" ]; then
  cp "$1" "$2"
else
  awk -F'\t' -v p=" $picked " 'index(p, " " $1 " ")' "$1" > "$2"
fi
cut -f1 "$1" | sort -u | while read -r name; do grep -q "^$name	" "$2" || echo "$name $(grep -c "^$name	" "$1")"; done > "$3"
echo "quick: $(wc -l < "$2") of $(wc -l < "$1") tests for the files changed since $(git -C "$root" rev-parse --short "$base")" >&2
