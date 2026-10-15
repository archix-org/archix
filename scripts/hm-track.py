#!/usr/bin/env python3
"""Opens an issue for every ported Home Manager module whose source or tests changed since the commit in tests/hm/cases.toml."""
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def run(*args, cwd=None):
    return subprocess.run(args, cwd=cwd, check=True, capture_output=True, text=True).stdout


def main():
    manifest = tomllib.loads((ROOT / "tests/hm/cases.toml").read_text())
    ported = manifest["commit"]
    with tempfile.TemporaryDirectory() as tmp:
        run("git", "clone", "--quiet", "--filter=blob:none", manifest["home_manager"], tmp)
        head = run("git", "rev-parse", "HEAD", cwd=tmp).strip()
        open_titles = set(run("gh", "issue", "list", "--state", "open", "--limit", "500", "--json", "title", "--jq", ".[].title").splitlines())
        changed = 0
        for module, paths in manifest["modules"].items():
            stat = run("git", "diff", "--stat", ported, head, "--", *paths, cwd=tmp).strip()
            if not stat:
                continue
            changed += 1
            title = f"Home Manager changed the {module} module"
            print(f"{module}: changed since {ported[:12]}")
            if title in open_titles:
                continue
            log = run("git", "log", "--oneline", f"{ported}..{head}", "--", *paths, cwd=tmp).strip()
            body = (
                f"Home Manager's `{module}` port changed between {ported[:12]} and {head[:12]}.\n\n"
                f"Commits:\n```\n{log}\n```\n\nFiles:\n```\n{stat}\n```\n\n"
                "Re-port the change, copy any new or updated expected files into tests/hm, "
                "check `cargo test --test hm` still passes, then move `commit` in tests/hm/cases.toml forward."
            )
            run("gh", "issue", "create", "--title", title, "--body", body, "--label", "home-manager")
        print(f"{changed} ported modules changed upstream")


if __name__ == "__main__":
    sys.exit(main())
