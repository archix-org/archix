# Agent instructions

- Any change to behaviour, CLI, config syntax or options needs a matching docs PR in archix-org/archix.dev (`src/content/docs/docs/`).
- Run `tests/arch/run.sh` before pushing anything outside `tests/vm/` and `.github/`; it runs the full suite, including the pacman and network tests, inside Arch. While iterating, run only the tests a change touches (`tests/arch/run.sh <filter>`, one VM suite on one filesystem).
- Long VM confirmation runs go to the release gate on GitHub (`gh workflow run release-gate.yml --ref <branch> -f suites=<suite,...>`), not the local machine; run the full gate once, for the commit being released.
