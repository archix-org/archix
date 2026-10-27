# Agent instructions

- Any change to behaviour, CLI, config syntax or options needs a matching docs PR in archix-org/archix.dev (`src/content/docs/docs/`).
- Run the full suite, `tests/arch/run.sh --jobs 2` (two containers, both heavy slots; without `--jobs 2` it takes one), before pushing anything outside `tests/vm/` and `.github/`, and always before pushing a change to `src/`; it runs every test, including the pacman and network tests, inside Arch.
- While iterating, run `tests/arch/run.sh --quick` (the unit tests plus the test files `tests/quick.tsv` maps your change since `main`, or `ARCHIX_QUICK_BASE`, to; it lists what it skipped), `tests/arch/run.sh <cargo test args>` for one test, or one VM suite on one filesystem.
- Confirm long VM suites wherever they finish first: locally in the two heavy slots (a power-cut run takes about 76 min per filesystem here, hours on GitHub's nested VMs), or on the release gate (`gh workflow run release-gate.yml --ref <branch> -f suites=<suite,...>`); don't wait on a slow GitHub queue, cancel superseded runs, and run the full gate once, for the commit being released.
