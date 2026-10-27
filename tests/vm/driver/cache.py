#!/usr/bin/env python3
# Usage: cache.py key SUITE | inputs SUITE | hit SUITE KEY | record SUITE KEY SECONDS; the VM suites' result cache, in ARCHIX_VM_RESULTS (~/.cache/archix-vm-results): a suite whose inputs hash to a recorded pass is skipped.
import hashlib
import json
import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
# Everything that decides what the VM runs: archix's source and toolchain, the image builder and the driver.
COMMON = ["src", "Cargo.toml", "Cargo.lock", "tests/arch/Dockerfile", "tests/vm/run.sh", "tests/vm/build-image.sh", "tests/vm/deploy/proto/check-deployment", "tests/vm/driver"]
SUITES = {"boot2": ["tests/vm/boot"]}
ENV = {"FS": "ext4", "TO_DATE": "", "VM_MEM": "", "VM_CPUS": ""}
KEEP = 500


def results():
    return Path(os.environ.get("ARCHIX_VM_RESULTS") or Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache") / "archix-vm-results")


def files(suite):
    listed = subprocess.run(["git", "-C", str(ROOT), "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", *COMMON, *SUITES[suite]],
                            check=True, capture_output=True).stdout
    return sorted(set(filter(None, listed.decode().split("\0"))))


def inputs(suite):
    """Each input as a line: a file's path, mode and sha256, a variable's value, and the prebuilt binaries' sha256."""
    lines = [f"suite {suite}"]
    for name in files(suite):
        path = ROOT / name
        if path.is_file():
            lines.append(f"file {name} {'x' if os.access(path, os.X_OK) else '-'} {hashlib.sha256(path.read_bytes()).hexdigest()}")
        else:
            lines.append(f"file {name} missing")
    for name, default in ENV.items():
        lines.append(f"env {name}={os.environ.get(name) or default}")
    prebuilt = os.environ.get("ARCHIX_PREBUILT")
    if prebuilt:
        for member in ("target/release/archix", "target/x86_64-unknown-linux-musl/release/archix"):
            binary = subprocess.run(["tar", "-I", "zstd", "-xOf", f"{prebuilt}/target.tar.zst", member], check=True, capture_output=True).stdout
            lines.append(f"prebuilt {member} {hashlib.sha256(binary).hexdigest()}")
    return lines


def key(suite):
    return hashlib.sha256("\n".join(inputs(suite)).encode()).hexdigest()


def main():
    command, suite = sys.argv[1], sys.argv[2]
    if suite not in SUITES:
        sys.exit(f"cache.py: no inputs listed for suite {suite}")
    if command == "key":
        print(key(suite))
    elif command == "inputs":
        print("\n".join(inputs(suite)))
    elif command == "hit":
        record = results() / f"{suite}-{sys.argv[3]}.json"
        if not record.is_file():
            sys.exit(1)
        seen = json.loads(record.read_text())
        print(f"cached: {suite} on {seen['fs']} passed with these inputs at {seen['at']} (commit {seen['commit']}, {seen['seconds']} s); --no-cache runs it again")
        record.touch()
    elif command == "record":
        commit = subprocess.run(["git", "-C", str(ROOT), "describe", "--always", "--dirty"], capture_output=True, text=True).stdout.strip()
        results().mkdir(parents=True, exist_ok=True)
        seen = {"suite": suite, "key": sys.argv[3], "fs": os.environ.get("FS") or "ext4", "commit": commit, "seconds": int(float(sys.argv[4])),
                "at": time.strftime("%Y-%m-%dT%H:%M:%S%z")}
        (results() / f"{suite}-{sys.argv[3]}.json").write_text(json.dumps(seen) + "\n")
        for old in sorted(results().glob("*.json"), key=lambda p: p.stat().st_mtime)[:-KEEP]:
            old.unlink()
    else:
        sys.exit(f"cache.py: unknown command {command}")


if __name__ == "__main__":
    main()
