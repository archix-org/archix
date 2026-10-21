# CI

| Workflow | When | Jobs | Takes |
| --- | --- | --- | --- |
| `ci.yml` | Every push to `main` and every pull request | `lint`: fmt, clippy and the archive proxy's own test; `image`: the test image; `container`: `tests/arch/run.sh` (cargo test with the ignored tests, and the musl build); `boot`: `tests/vm/run.sh boot` on ext4 and btrfs; `deploy-boot`: `tests/vm/deploy-boot/run.sh` with systemd and busybox initramfs, on ext4 and btrfs | about 50 min, all VM jobs in parallel |
| `release-gate.yml` | Nightly, by hand (workflow_dispatch), and from `release.yml` | `rollback`, `nvidia` and `cachyos` on ext4 and btrfs; `powercut` on both once `tests/vm/powercut/` exists | an estimated 30 min to 3 h per job; 4 h VM timeout, 5.5 h job limit |
| `release.yml` | A `v*` tag | Runs both of the above, then builds the draft release only if they pass | |

The VM jobs need KVM: `tests/ci/check-kvm.sh` fails the job if the runner user can't create a VM, rather than letting QEMU emulate.

## The archive cache

Every job runs `tests/ci/archive-proxy.py` on port 8079 and points `ARCHIX_TEST_ARCHIVE` at it. It caches only immutable files, `/repos/<date>/` and `/packages/`, and keeps a file only once it's whole and checked: a package against its repo db's size and sha256, a signature against the db's, a db or a package in no db by reading its whole compressed stream. Downloads go to a temporary file that's renamed into place after the check. `actions/cache` saves the cache only when a job added files to it. A restored cache is checked again before use, and a bad file is dropped and downloaded again. The `base` cache holds the test image's packages, and every other job adds to a cache of its own.

## Reproducing a job

Run the same script the job runs, for example `FS=btrfs tests/vm/run.sh rollback` or `INITRAMFS=busybox FS=btrfs tests/vm/deploy-boot/run.sh`. To use the same cache locally:

```sh
tests/ci/archive-proxy.py serve --cache ~/.cache/archix-archive --port 8079 &
ARCHIX_TEST_ARCHIVE=http://127.0.0.1:8079 tests/vm/run.sh boot
```

A failed job uploads its logs as the `logs-<job>` artifact: each work dir's `serial.log` and other logs, plus the proxy's log, but never disk images.
