#!/usr/bin/env python3
# Usage: checks.py [--table] | --extract FILE...; fails unless the split boot tests hold each check of checks.tsv exactly once and no other: vmtest.sh's 156 by stage and message, a few renamed, plus those marked added.
import ast
import sys
from collections import Counter
from pathlib import Path

HERE = Path(__file__).resolve().parent
TESTS = ["layout", "secureboot", "reset-mid-switch", "counting", "revoke", "etc-merge", "fallback", "usr-local"]


def message(call):
    """The check's message as source, if call is one: t.check/t.require's second argument, what= of succeed and wait_until_succeeds, select_entry's entry."""
    func = call.func
    if not isinstance(func, ast.Attribute):
        return None
    if func.attr in ("check", "require") and isinstance(func.value, ast.Name) and func.value.id == "t" and len(call.args) == 2:
        return ast.unparse(call.args[1])
    if func.attr in ("succeed", "wait_until_succeeds"):
        what = [k.value for k in call.keywords if k.arg == "what"]
        return ast.unparse(what[0]) if what else None
    if func.attr == "select_entry":
        return f"bootctl set-oneshot {ast.unparse(call.args[0])}"
    return None


def extract(path):
    """(stage, message) of each check in path, the stage being its step's name up to the colon; checks outside a step are 'every boot'."""
    found = []

    def walk(node, stage):
        if isinstance(node, ast.With):
            for item in node.items:
                call = item.context_expr
                if isinstance(call, ast.Call) and isinstance(call.func, ast.Attribute) and call.func.attr == "step":
                    stage = ast.literal_eval(call.args[0]).split(":")[0]
        if isinstance(node, ast.Call):
            msg = message(node)
            if msg is not None:
                found.append((stage, msg))
        for child in ast.iter_child_nodes(node):
            walk(child, stage)

    walk(ast.parse(Path(path).read_text()), "every boot")
    return found


def expected():
    """(stage, message the tests use, kind): kind is old, renamed or added."""
    rows = []
    for line in (HERE / "checks.tsv").read_text().splitlines():
        cols = line.split("\t")
        kind = cols[2] if len(cols) > 2 else "old"
        rows.append((cols[0], cols[3] if kind == "renamed" else cols[1], kind))
    return rows


def main():
    if sys.argv[1:2] == ["--extract"]:
        for path in sys.argv[2:]:
            for stage, msg in extract(path):
                print(f"{stage}\t{msg}")
        return 0
    rows = expected()
    want = Counter((s, msg) for s, msg, _ in rows)
    where = {}
    have = Counter()
    for name, path in [("common", HERE / "common.py"), ("base", HERE / "base.py")] + [(t, HERE / t / "test.py") for t in TESTS]:
        for check in extract(path):
            have[check] += 1
            where.setdefault(check, []).append(name)
    problems = [f"missing: {s}: {m}" for (s, m), n in want.items() if have[(s, m)] < n]
    problems += [f"more than once ({', '.join(where[(s, m)])}): {s}: {m}" for (s, m), n in have.items() if (s, m) in want and n > want[(s, m)]]
    problems += [f"not in checks.tsv ({', '.join(where[(s, m)])}): {s}: {m}" for (s, m) in have if (s, m) not in want]
    if problems:
        print("\n".join(problems))
        return 1
    old = [r for r in rows if r[2] != "added"]
    if "--table" in sys.argv:
        print("| vmtest.sh stage | Checks | Split test (its checks) | Added |\n| --- | --- | --- | --- |")
        for stage in dict.fromkeys(s for s, _, _ in rows):
            per = Counter(where[(s, m)][0] for s, m, kind in old if s == stage)
            added = sum(1 for s, _, kind in rows if s == stage and kind == "added")
            print(f"| {stage} | {sum(per.values())} | {', '.join(f'`{t}` ({n})' for t, n in per.items())} | {added or ''} |")
        print(f"| all | {len(old)} | | {len(rows) - len(old)} |")
    renamed = sum(1 for r in rows if r[2] == "renamed")
    print(f"the split boot tests hold each of vmtest.sh's {len(old)} checks exactly once ({renamed} renamed), and {len(rows) - len(old)} added ones")
    return 0


if __name__ == "__main__":
    sys.exit(main())
