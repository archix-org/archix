#!/usr/bin/env python3
# Usage: test-times.py LOG... > tests/ci/test-times.tsv; takes each test's time from container shard logs (gh run view RUN --job ID --log) as the gap since the previous result of its executable.
import re
import sys
from datetime import datetime

rows = {}
for path in sys.argv[1:]:
    target = prev = None
    for line in open(path, errors="replace"):
        m = re.search(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+)Z (.*)$", line.rstrip("\n"))
        if not m:
            continue
        t = datetime.fromisoformat(m.group(1)[:26])
        text = m.group(2)
        if r := re.match(r"\s+Running (\S+) \(\S+\): \d+ tests", text):
            target, prev = r.group(1), t
        elif (r := re.match(r"test (\S+) \.\.\. (ok|FAILED)$", text)) and target:
            rows[(target, r.group(1))] = (t - prev).total_seconds()
            prev = t
for (target, name), seconds in sorted(rows.items()):
    print(f"{target}\t{name}\t{seconds:.0f}")
