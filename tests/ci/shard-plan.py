#!/usr/bin/env python3
# Usage: shard-plan.py plan TESTS TIMES N OUT | check TESTS RESULTS...; TESTS is tests/arch/run.sh's target/executable/test list, TIMES target/test/seconds (an unmeasured test counts as its target's mean), and check fails unless RESULTS hold every listed test once, ok.
import collections
import statistics
import sys
from pathlib import Path


def rows(path, width):
    out = []
    for line in Path(path).read_text().splitlines():
        if line.strip():
            fields = line.split("\t")
            if len(fields) != width:
                sys.exit(f"{path}: expected {width} tab-separated fields: {line!r}")
            out.append(tuple(fields))
    return out


def plan(tests, times, n, out):
    tests = rows(tests, 3)
    known = {(t, name): float(s) for t, name, s in rows(times, 3)}
    overall = statistics.mean(known.values()) if known else 1.0
    by_target = collections.defaultdict(list)
    for (t, _), s in known.items():
        by_target[t].append(s)
    if len(set((t, name) for t, _, name in tests)) != len(tests):
        sys.exit("a test is listed twice")

    def weight(row):
        return known.get((row[0], row[2]), statistics.mean(by_target[row[0]]) if by_target[row[0]] else overall)

    weighted = sorted(tests, key=lambda r: (-weight(r), r[0], r[2]))
    load = [0.0] * n
    shards = [[] for _ in range(n)]
    for row in weighted:
        i = min(range(n), key=lambda k: (load[k], k))
        shards[i].append(row)
        load[i] += weight(row)
    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    for i, shard in enumerate(shards, 1):
        (out / f"shard-{i}.tsv").write_text("".join("\t".join(r) + "\n" for r in sorted(shard)))
        print(f"shard {i}: {len(shard)} tests, about {load[i - 1] / 60:.1f} min")
    unknown = sum((t, name) not in known for t, _, name in tests)
    print(f"{len(tests)} tests in {n} shards; {unknown} without a measured time counted as their target's mean")


def check(tests, results):
    want = collections.Counter((t, name) for t, _, name in rows(tests, 3))
    got = collections.Counter()
    bad = []
    for path in results:
        for t, name, outcome in rows(path, 3):
            got[(t, name)] += 1
            if outcome != "ok":
                bad.append(f"{t} {name}: {outcome}")
    missing = sorted(set(want) - set(got))
    extra = sorted(set(got) - set(want))
    twice = sorted(k for k, c in got.items() if c > 1)
    for label, items in (("never ran", missing), ("not in the list", extra), ("ran more than once", twice)):
        for t, name in items:
            bad.append(f"{t} {name}: {label}")
    per_target = collections.Counter(t for t, _ in got)
    for t in sorted(per_target):
        print(f"{t}: {per_target[t]}")
    print(f"listed {sum(want.values())}, ran {sum(got.values())} in {len(results)} result files")
    if bad:
        print("\n".join(bad), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    if len(sys.argv) == 6 and sys.argv[1] == "plan":
        plan(sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5])
    elif len(sys.argv) >= 3 and sys.argv[1] == "check":
        check(sys.argv[2], sys.argv[3:])
    else:
        sys.exit("usage: shard-plan.py plan TESTS TIMES N OUT | check TESTS RESULTS...")
