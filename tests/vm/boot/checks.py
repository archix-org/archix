#!/usr/bin/env python3
# Usage: checks.py; fails unless run.sh's boot tests each have a test.py and a cache key in cache.py, and ci.yml's boot groups run each exactly once, none larger than boot-results.sh's MAX, and its suite_tests each have a test.py and a cache key, with ci.yml's suites job naming only them, and its powercut and replay tests each a test.py and a cache key too.
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
    groups = [group.split() for group in json.loads(groups.group(1))] + [line.split() for line in re.findall(r"^\s*- tests: ([\w ]+)$", ci, re.M)]
    found += [f"ci.yml's boot group {' '.join(g)!r} has {len(g)} tests, more than boot-results.sh's MAX={limit}" for g in groups if len(g) > limit]
    placed = Counter(name for group in groups for name in group)
    found += [f"ci.yml's boot groups run {name} {placed[name]} times, not once" for name in tests if placed[name] != 1]
    found += [f"ci.yml's boot groups name {name}, which isn't a split boot test" for name in placed if name not in tests]
    return found


def listed(name):
    """A tests/vm/run.sh array, such as suite_tests."""
    found = re.search(rf"^{name}=\(([^)]*)\)$", (ROOT / "tests/vm/run.sh").read_text(), re.M)
    return found.group(1).split() if found else []


def suite_problems(suites, ext4, ci):
    """What's wrong with run.sh's suite_tests and ext4_tests against cache.py and ci.yml's suites job."""
    found = [f"run.sh's suite {name} has no tests/vm/{name}/test.py" for name in suites if not (ROOT / "tests/vm" / name / "test.py").is_file()]
    cached = re.search(r"^OWN_TESTS = (\[.*\])$", (ROOT / "tests/vm/driver/cache.py").read_text(), re.M)
    if not cached or sorted(json.loads(cached.group(1))) != sorted(suites):
        found.append(f"tests/vm/driver/cache.py's OWN_TESTS isn't run.sh's suite_tests ({' '.join(suites)}), so a suite would have no cache key")
    found += [f"run.sh's ext4_tests names {name}, which isn't in suite_tests" for name in ext4 if name not in suites]
    found += [f"ci.yml's suites job names {name}, which isn't in run.sh's suite_tests" for name in re.findall(r"^\s*- \{ suite: ([\w-]+), fs: \w+ \}$", ci, re.M) if name not in suites]
    return found


def split_problems():
    """What's wrong with run.sh's powercut and replay tests against their directories and cache.py's lists."""
    found = []
    for suite in ("powercut", "replay"):
        tests = listed(f"{suite}_tests")
        found += [f"run.sh's {suite} test {name} has no tests/vm/{suite}/{name}/test.py" for name in tests if not (ROOT / "tests/vm" / suite / name / "test.py").is_file()]
        cached = re.search(rf"^{suite.upper()}_TESTS = (\[.*\])$", (ROOT / "tests/vm/driver/cache.py").read_text(), re.M)
        if not tests or not cached or sorted(json.loads(cached.group(1))) != sorted(tests):
            found.append(f"tests/vm/driver/cache.py's {suite.upper()}_TESTS isn't run.sh's {suite}_tests ({' '.join(tests)}), so a test would have no cache key")
    return found


def main():
    tests = boot_tests()
    ci = (ROOT / ".github/workflows/ci.yml").read_text()
    found = problems(tests, ci, (ROOT / "tests/ci/boot-results.sh").read_text()) + suite_problems(listed("suite_tests"), listed("ext4_tests"), ci) + split_problems()
    if found:
        print("\n".join(found))
        return 1
    print(f"ci.yml's boot groups run each of the {len(tests)} split boot tests once: {' '.join(tests)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
