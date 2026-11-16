#!/usr/bin/env python3
# Usage: rescope-pending.py check | filter TESTS SKIPPED | vm SUITE; the one list of tests CI skips until the rescope's rewrite, tests/ci/rescope-pending.txt, each with why.
import os
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LIST = ROOT / "tests/ci/rescope-pending.txt"


def entries():
    """(target, test, why) per line; target is vm for a tests/vm suite."""
    found = []
    for n, line in enumerate(LIST.read_text().splitlines(), 1):
        if not line.strip() or line.startswith("#"):
            continue
        fields = line.split("\t")
        if len(fields) != 3 or not all(f.strip() for f in fields):
            sys.exit(f"{LIST.name}:{n}: expected TARGET, TEST and WHY, tab-separated: {line!r}")
        found.append(tuple(fields))
    return found


def summary(text):
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with open(path, "a") as out:
            out.write(text)


def check():
    listed = entries()
    problems = [f"{t} {name} is listed twice" for i, (t, name, _) in enumerate(listed) if (t, name) in [(a, b) for a, b, _ in listed[:i]]]
    suites = re.search(r"^suite_tests=\(([^)]*)\)$", (ROOT / "tests/vm/run.sh").read_text(), re.M)
    known = suites.group(1).split() if suites else []
    problems += [f"vm {name} isn't in tests/vm/run.sh's suite_tests" for t, name, _ in listed if t == "vm" and name not in known]
    problems += [f"{t} {name}: tests/{t[5:]}.rs doesn't exist" for t, name, _ in listed if t.startswith("test:") and not (ROOT / f"tests/{t[5:]}.rs").is_file()]
    problems += [f"{t} {name}: no fn {name} in tests/{t[5:]}.rs" for t, name, _ in listed if t.startswith("test:") and (ROOT / f"tests/{t[5:]}.rs").is_file()
                 and not re.search(rf"\bfn {re.escape(name)}\(", (ROOT / f"tests/{t[5:]}.rs").read_text())]
    if problems:
        print("\n".join(problems))
        return 1
    print(f"{LIST.name}: {sum(t != 'vm' for t, _, _ in listed)} cargo tests and {sum(t == 'vm' for t, _, _ in listed)} VM suites pending, each named once with why")
    return 0


def filter_tests(tests, skipped):
    """Drops the pending tests from TESTS (target, executable, test per line) in place and writes them, with why, to SKIPPED; a pending test TESTS doesn't list fails it, so the list can't go stale."""
    pending = {(t, name): why for t, name, why in entries() if t != "vm"}
    rows = [line.split("\t") for line in Path(tests).read_text().splitlines() if line.strip()]
    kept = [r for r in rows if (r[0], r[2]) not in pending]
    dropped = [(r[0], r[2], pending[(r[0], r[2])]) for r in rows if (r[0], r[2]) in pending]
    stale = sorted(set(pending) - {(r[0], r[2]) for r in rows})
    if stale:
        sys.exit("rescope-pending.txt names tests the build didn't list; drop them from it:\n" + "\n".join(f"  {t} {name}" for t, name in stale))
    Path(tests).write_text("".join("\t".join(r) + "\n" for r in kept))
    Path(skipped).write_text("".join(f"{t}\t{name}\t{why}\n" for t, name, why in dropped))
    print(f"{len(kept)} tests to run, {len(dropped)} skipped as pending the rescope (tests/ci/rescope-pending.txt)")
    summary(f"### {len(dropped)} tests skipped until the rescope's rewrite\n\n| Target | Test | Why |\n| --- | --- | --- |\n" + "".join(f"| {t} | {name} | {why} |\n" for t, name, why in dropped))
    return 0


def vm(suite):
    """Prints why SUITE is pending and sets run=false in GITHUB_OUTPUT, or sets run=true."""
    why = next((w for t, name, w in entries() if t == "vm" and name == suite), None)
    out = os.environ.get("GITHUB_OUTPUT")
    if out:
        with open(out, "a") as f:
            f.write(f"run={'false' if why else 'true'}\n")
    if why:
        print(f"::notice title={suite} skipped until the rescope's rewrite::{why} (tests/ci/rescope-pending.txt)")
        summary(f"{suite}: skipped until the rescope's rewrite: {why}\n")
    return 0


def main():
    command = sys.argv[1] if len(sys.argv) > 1 else ""
    if command == "check" and len(sys.argv) == 2:
        return check()
    if command == "filter" and len(sys.argv) == 4:
        return filter_tests(sys.argv[2], sys.argv[3])
    if command == "vm" and len(sys.argv) == 3:
        return vm(sys.argv[2])
    sys.exit("usage: rescope-pending.py check | filter TESTS SKIPPED | vm SUITE")


if __name__ == "__main__":
    sys.exit(main())
