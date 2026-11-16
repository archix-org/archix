#!/usr/bin/env python3
# Usage: checks.py; fails on any privileged container in tests/, scripts/ or .github/, and unless run.sh's suite_tests each have a test.py and a cache key in cache.py, with ci.yml's suites job naming only them.
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]


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


# Spelled in two pieces so this file doesn't match itself.
PRIVILEGED = "--" + "privileged"


def privileged_problems():
    """Every privileged container in the test runners, scripts and CI: a privileged container sees the host's devices, as one arming the host's watchdog did."""
    found = []
    for top in ("tests", "scripts", ".github", "Makefile"):
        base = ROOT / top
        files = [base] if base.is_file() else sorted(p for p in base.rglob("*") if p.is_file()) if base.is_dir() else []
        for path in files:
            try:
                text = path.read_text()
            except (UnicodeDecodeError, OSError):
                continue
            for n, line in enumerate(text.splitlines(), 1):
                if PRIVILEGED in line or re.search(r"privileged:\s*true", line):
                    found.append(f"{path.relative_to(ROOT)}:{n}: a privileged container; run it in a test VM (tests/testvm) instead")
    return found


def main():
    ci = (ROOT / ".github/workflows/ci.yml").read_text()
    found = suite_problems(listed("suite_tests"), listed("ext4_tests"), ci) + privileged_problems()
    if found:
        print("\n".join(found))
        return 1
    print(f"run.sh's {len(listed('suite_tests'))} suites each have a test.py and a cache key, and no container is privileged")
    return 0


if __name__ == "__main__":
    sys.exit(main())
