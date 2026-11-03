#!/usr/bin/env python3
# Usage: checks.py; fails unless run.sh's boot tests each have a test.py and a cache key in cache.py, and ci.yml's boot groups run each exactly once, none larger than boot-results.sh's MAX.
import json
import re
import sys
from collections import Counter
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]


def boot_tests():
    """The split boot tests tests/vm/run.sh runs, in its order."""
    found = re.search(r"^boot_tests=\(([^)]*)\)$", (ROOT / "tests/vm/run.sh").read_text(), re.M)
    return found.group(1).split() if found else []


def problems(tests, ci, results):
    """What's wrong with tests against ci.yml's text ci and boot-results.sh's text results."""
    found = []
    if not tests:
        found.append("tests/vm/run.sh has no boot_tests=(...) line")
    found += [f"run.sh's boot test {name} has no tests/vm/boot/{name}/test.py" for name in tests if not (HERE / name / "test.py").is_file()]
    cached = re.search(r"^BOOT_TESTS = (\[.*\])$", (ROOT / "tests/vm/driver/cache.py").read_text(), re.M)
    if not cached or sorted(json.loads(cached.group(1))) != sorted(tests):
        found.append(f"tests/vm/driver/cache.py's BOOT_TESTS isn't run.sh's boot_tests ({' '.join(tests)}), so a test would have no cache key")
    limit = re.search(r"^MAX=(\d+)$", results, re.M)
    groups = re.search(r"^\s*tests: (\[.*\])$", ci, re.M)
    if not limit or not groups:
        return found + ["ci.yml has no boot `tests: [...]` matrix line or boot-results.sh no MAX="]
    limit = int(limit.group(1))
    groups = [group.split() for group in json.loads(groups.group(1))]
    found += [f"ci.yml's boot group {' '.join(g)!r} has {len(g)} tests, more than boot-results.sh's MAX={limit}" for g in groups if len(g) > limit]
    placed = Counter(name for group in groups for name in group)
    found += [f"ci.yml's boot groups run {name} {placed[name]} times, not once" for name in tests if placed[name] != 1]
    found += [f"ci.yml's boot groups name {name}, which isn't a split boot test" for name in placed if name not in tests]
    return found


def main():
    tests = boot_tests()
    found = problems(tests, (ROOT / ".github/workflows/ci.yml").read_text(), (ROOT / "tests/ci/boot-results.sh").read_text())
    if found:
        print("\n".join(found))
        return 1
    print(f"ci.yml's boot groups run each of the {len(tests)} split boot tests once: {' '.join(tests)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
