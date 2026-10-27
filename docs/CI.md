# CI

| Workflow | When | Jobs | Takes |
| --- | --- | --- | --- |
| `ci.yml` | Every push to `main`, every pull request, and by hand (workflow_dispatch) | `lint`: fmt, clippy and the archive proxy's own test; `image`: the test image; `build`: archix and every test executable, built once (see below); `container`: `tests/arch/run.sh` over six shards; `container-sum`: the shards ran every test once; `boot`: `tests/vm/run.sh boot` on ext4 and btrfs; `deploy-boot`: `tests/vm/deploy-boot/run.sh` with systemd and busybox initramfs, on ext4 and btrfs | about 16 min with the cargo cache (21 on a miss), the build then boot's 10-11 the long pole |
| `release-gate.yml` | Nightly, by hand (workflow_dispatch), and from `release.yml` | `build` as in CI, then the VM suites on ext4 and btrfs, plus CI's `boot` and `deploy-boot` (both initramfs flavours in one job) when `suites` names them, each suite job `repeat` times side by side; per filesystem, `powercut-log` logs one power-cut run and uploads it as `powercut-log-<fs>`, six `powercut` shards replay it, and `powercut-sum` checks their summaries cover each of its points once | the suites within 48 min of the start (churn's 31-37 the longest), the power-cut shards then the long pole; 4 h VM timeout, 5.5 h job limit |
| `release.yml` | A `v*` tag | Runs both of the above, then builds the draft release only if they pass | |

The VM jobs need KVM: `tests/ci/check-kvm.sh` fails the job if the runner user can't create a VM, rather than letting QEMU emulate.

## Building once

The `build` job runs `tests/arch/run.sh` with `ARCHIX_BUILD_OUT=DIR`: in the test image it builds the glibc and static musl release binaries and every test executable `cargo test` would run, runs the doc tests (they have no executable to hand on), and uploads `target.tar.zst` and `tests.tsv` (every test, by target and executable) as the `archix-build` artifact (`archix-build-gate` in the gate). Test jobs take it as `ARCHIX_PREBUILT=DIR`: `tests/vm/run.sh` and `tests/vm/deploy-boot/run.sh` copy the binaries out instead of compiling, and `tests/arch/run.sh` unpacks the executables into the build volume at the paths they were built for. `selfupgrade` still builds `ARCHIX_OLD`, and `powercut-log` its own build with debug assertions. The job caches the compiled dependencies and the crate registry (`cargo-<hash of Cargo.lock, Cargo.toml and the Dockerfile>`, archix's own artifacts pruned), so only archix itself is compiled when they haven't changed.

The dev profile compiles dependencies with `opt-level = 3` (`Cargo.toml`); archix's own code stays unoptimised with its debug assertions. The tests run the debug binary, and unoptimised sha2 and zstd took half the suite's time: 3761 s of tests became 1887 s.

## The container shards

`tests/ci/shard-plan.py plan` splits the build's `tests.tsv`, so a new test or test file is in a shard without anyone listing it, over `CONTAINER_SHARDS` (6) by `tests/ci/test-times.tsv` (each test's time from a CI run; a test it doesn't know counts as its target's mean). Each shard runs `tests/arch/run.sh` with `ARCHIX_TEST_LIST`, which runs its tests from the prebuilt executables with `--include-ignored --exact`, one executable at a time as `cargo test` does, fails if an executable ran fewer tests than it was given, and writes each test's outcome to `ARCHIX_TEST_RESULTS`. `container-sum` (`shard-plan.py check`) fails unless the shards' results hold every test of `tests.tsv` exactly once, each passed, and prints the count per target. Tests in one shard still share a container and run as before; tests in different shards no longer see each other's leftovers.

When the shards drift apart, refresh the times from a green run's shard logs: save each with `gh run view <run> --job <id> --log > shard-<i>.log` and run `tests/ci/test-times.py shard-*.log > tests/ci/test-times.tsv`.

## Running it locally

| Command | Runs | Takes (this host, 24 threads) |
| --- | --- | --- |
| `tests/arch/run.sh` | every test in one container, one heavy slot, as `cargo test` | 16.7 min (40-42 min before the dependencies were optimised) |
| `tests/arch/run.sh --jobs 2` | every test, over two containers balanced as in CI; takes both heavy slots | 10.5 min |
| `tests/arch/run.sh --quick` | the unit tests, the cheap always-run targets and the test files `tests/quick.tsv` maps the files changed since `main` (or `ARCHIX_QUICK_BASE`: `HEAD` for the uncommitted changes) to (any unmapped file, `tests/common/`, `Cargo.*` or the Dockerfile: everything); it ends by naming every target it skipped | 68 s after an edit to `src/flatpak.rs`, rebuilds included (253 of 422 tests) |
| `tests/arch/run.sh <cargo test args>` | as `cargo test <args> -- --include-ignored` | |
| `FS=ext4 tests/vm/run.sh boot` | one VM suite | 6.5 min: 27 s of incremental release builds, 17 s of image, 5.8 min in the VM |

Every runner builds the test image through `tests/arch/image.sh`, which skips `docker build` when the image is labelled with the Dockerfile's hash. The build volume is one per checkout, so `cargo` rebuilds only what changed (0.06 s for the release build and 1 s for the tests with nothing changed).

`--quick` is for iterating; the full suite is still the gate before pushing a change to `src/`.

## The archive cache

Every job runs `tests/ci/archive-proxy.py` on port 8079 and points `ARCHIX_TEST_ARCHIVE` at it. It caches only immutable files, `/repos/<date>/` and `/packages/`, and keeps a file only once it's whole and checked: a package against its repo db's size and sha256, a signature against the db's, a db or a package in no db by reading its whole compressed stream. Downloads go to a temporary file that's renamed into place after the check. `actions/cache` saves the cache only when a job added files to it. A restored cache is checked again before use, and a bad file is dropped and downloaded again. The `base` cache holds the test image's packages and the container suite's, and every other job adds to a cache of its own. The container shards don't each save a copy of `base`: each uploads just the files it newly cached as `archive-new-container-<i>`, and `container-sum` merges them into one `base` cache, checked again before it's saved.

## Where the time went

Measured on CI 40048123448 and gate 40048113502 (both c42290b, 2026-10-24), from each job's step times and its log's timestamps; minutes.

| Job | Total | Setup (runner, archive cache, test image) | cargo builds | VM image (pacstrap, archix's first switch) | Suite | Logs, cache save |
| --- | --- | --- | --- | --- | --- | --- |
| CI `container` | 68.2 | 1.6 | 3.6 (static 2.5 with crate download, tests 1.1) | | 62.8 (switch.rs 25.0, boot.rs 7.2, aur.rs 5.6, system.rs 5.6, recipes.rs 4.1, helper.rs 3.6, repos.rs 3.4, the other 14 targets 8.3) | 0 |
| CI `boot` (ext4, btrfs) | 13.1, 12.8 | 1.2 | 3.4 | 0.6 | 7.8 | 0 |
| CI `deploy-boot` (4 jobs) | 5.1-6.2 | 1.3-1.5 | 2.2 | 0.9 | 1.1 | 0 |
| CI `image`, `lint` | 1.3, 0.7 | 1.2 | | | | |
| gate `suites` (18 jobs) | 6.4-43.6 | 1.0-1.4 | 3.4-4.6 | 0.4-0.5 (churn 2.2 with its 60 packages) | 5-40 | 0-0.1 |
| gate `powercut-log` (2) | 8.6, 9.9 | 1.4 | in the suite step | | 6.6, 7.5 | 0.5-0.8 |
| gate `powercut` shards (6) | 26.3-302.4 | 1.3-1.5 | | | 24.5-300.5, after 0.4-10.4 of log download | 0 |

The docker test image itself took about 1 min a job: 30 s of pacman from the restored archive cache and 6-8 s exporting layers (60-89 s on ubuntu-26.04 until `runner-setup.sh` turned off its containerd image store). The CI run took 70.4 min from its first job's start (78.5 from its creation), the container job alone 68; the gate 315 min, two powercut shards needing 142 and 302 min of it, and nightly gates 40168408855 and 40279455909 took 334 and 264 min with shards of up to 323 min.

## After

Minutes per job, each from its start to its end; a run's wall time is from its first job's start to its last job's end, as queueing for runners (other branches' gates share them) isn't the workflow's.

| Job | Before: CI 40048123448 (c42290b) | After: CI 40438995443 (c7f80f8) | After: CI 40442678442 (f2525d8) |
| --- | --- | --- | --- |
| `lint` | 0.6 (failed on clippy 1.99) | 2.0 | 1.9 |
| `image` | 1.3 | 1.5 | 1.3 |
| `build` | | 3.6 (10.5 in 40437945251, a cargo cache miss) | 4.6 |
| `container` | 68.2, one job | 6 shards, 7.4-8.7 | 6 shards, 5.7-8.8 |
| `container-sum` | | 0.1 | 0.1 |
| `boot` ext4, btrfs | 13.1, 12.7 | 10.2, 9.8 | 10.0, 11.2 |
| `deploy-boot` (4) | 5.0-6.1 | 2.6-3.9 | 2.6-3.0 |
| Wall | 70.4 | 15.1 (one shard waited 2.7 min for a runner) | 15.8 |
| Runner minutes | 119 | 88 | 84 |

A shard spends about 2 min before its first test: 1.5 min of runner setup (74 s of it building the test image through the archive proxy) and 20 s downloading the build. The `build` job is 1.5 min of the same setup, 1.8 min of `cargo` with the cache (35 s each release build, 24 s the tests) and the upload. The boot job is 1.1 min of setup, 0.5 of download and 8.4 of suite, 7.8 of it in the VM.

| Gate job | Before: 40048113502 (c42290b) | After: 40444198508 (f2525d8), 105 min in |
| --- | --- | --- |
| `build` | | 4.6 |
| `suites` (18) | 6.4-43.6 | 3.8-37.0, all passed |
| `powercut-log` (2) | 8.6, 9.9 | 10.5, 9.2 |
| `powercut` shards | 3 per filesystem, 26.3-302.4 | 6 per filesystem; the seven finished took 35.3-85.8 |
| Wall | 315.1 | the suites and logs done at 48 min; shards still running at 105 |
| Runner minutes | 1018 | 707 at 105 min |

Six of the seven finished shards failed in the replay itself, as every shard of 40048113502 did; those read so far failed on a real boot over the timing judge's 3x, a pristine image that didn't boot within 3600 s on the runner's nested KVM, and failed units after the ESP's fsck. That is the power-cut work's to fix, and the shard count doesn't change which points fail.

Coverage: the shards of 40438995443 listed and ran 425 tests, the unsharded count of their commit (424 on c42290b, one test added since); 40442678442 ran 422, the merges below taking three test functions and none of their checks, per target as before but `helper` 9 to 7 and `switch` 42 to 41. The unsharded local run of f2525d8's tests passed 422. The VM matrix is unchanged: CI's boot on ext4 and btrfs and deploy-boot over both initramfs flavours and filesystems, the gate's nine suites and the power-cut replay on both filesystems.

## Considered and not done

- Caching the docker test image across runs: it is 4.5 GB, the repository's Actions cache is at 9.5 of its 10 GB with archive caches, and the image builds in about 40 s from the restored archive cache. Locally a cached `docker build` takes 0.2 s.
- Caching each suite's VM disk image, on CI or in `~/.cache/archix-vm-images`: pacstrap takes 0.4-0.5 min a CI job (churn's 2.2 includes building its 60 packages) and the boot suite's whole image build 17 s on this host from the 8079 cache, and everything after pacstrap installs and runs the archix under test; on ext4 a cached multi-GB image would be copied, not reflinked, so a hit would save seconds, and cachyos pacstraps from CachyOS's live repos, which no key can pin.
- Merging VM suites with similar flows: rollback and cachyos differ in kernel, repos and snapshot (CachyOS's real repos, `linux-cachyos`, 2026-10-16); nvidia and dkms2 in packages (devtools against `linux-lts`), `keep` and disk size; health and firewall in the image (nftables, the probe's ssh key) and in the config they start from (firewall on from the first switch, which would change what health's fallbacks see); boot lays out on its first boot where the others start laid out; deploy-boot's matrix is the initramfs and filesystem under test. None share a setup, and the gate's wall time is the power-cut shards', so merging would save runner minutes, not time.
- Merging more container tests: grouping every test by its leading statements, the only other shared runs are `Setup::new` plus one switch (the boot-counting trio of `switch.rs`, `temporary_installs_last_until_the_next_switch`), after which each test changes the machine in its own way (a failed try, a read-only ESP, a temporary install), so a merge would start one from another's end state.
- Starting the VM jobs alongside the build and polling for its artifact, so their 1.2 min of setup overlaps it: about 1 min off CI's critical path, for jobs that would have to notice a failed build themselves.

## Merged tests

Each old test's checks run in the merged test, in its order; `Checks` (`tests/common`) collects them and the test fails listing every one that failed, while a refusal or kill that later steps depend on still stops it. Each phase reads its own starting point (the lock, the generation, the deployments) as the old test did.

| Old test | Checks | New test | Checks |
| --- | --- | --- | --- |
| `switch::rollback_to_a_generation_whose_deployment_was_dropped_rebuilds_it_from_the_package_cache` | 19 | `switch::rollback_to_a_generation_whose_deployment_was_dropped_rebuilds_it_from_the_package_cache_unless_the_esp_cant_hold_its_entry` | 23 |
| `switch::a_rollback_that_rebuilds_is_refused_before_anything_changes_when_the_esp_cant_hold_its_entry` | 5 (its first, the dropped deployment, is the other test's first too, so runs once) | | |
| `helper::declining_a_switch_leaves_the_lock_byte_for_byte` | 7 | `helper::declining_or_a_crash_before_or_inside_a_switch_leaves_the_lock_and_config_until_its_deployment_is_committed` | 21 |
| `helper::a_crash_before_the_switch_leaves_the_config_and_lock_untouched` | 5 | | |
| `helper::a_crash_inside_the_switch_writes_the_config_only_once_its_deployment_is_committed` | 9 | | |

The suite has 422 tests where it had 425; the five took 48 s in CI and the two take about 22 s locally (12.7 and 9.3).
- Running tests in one container in parallel instead of under `lock_system()`: the machines bind the container's own `/var/lib/archix`, package caches and pacman database, so tests would need a container's worth of state each; two containers (`--jobs 2`, CI's shards) give that without changing the tests.

## Reproducing a job

Run the same script the job runs, for example `FS=btrfs tests/vm/run.sh rollback` or `INITRAMFS=busybox FS=btrfs tests/vm/deploy-boot/run.sh`. To use the same cache locally:

```sh
tests/ci/archive-proxy.py serve --cache ~/.cache/archix-archive --port 8079 &
ARCHIX_TEST_ARCHIVE=http://127.0.0.1:8079 tests/vm/run.sh boot
```

A failed container shard names its tests; `tests/arch/run.sh --test <target> <name>` runs it (and any test whose name contains it). To run a shard's exact set, download the run's `container-shards` and `archix-build` artifacts and run `ARCHIX_PREBUILT=<archix-build dir> ARCHIX_TEST_LIST=<dir>/shard-<i>.tsv tests/arch/run.sh`.

A failed `powercut` shard prints its seed and the name of the log artifact it replayed, kept 90 days, as a failure in the UKI-copy windows reproduces only from that log; download its filesystem's `powercut-log-<fs>` artifact and run `LOG=powercut-log-<fs>.tar.zst SEED=<seed> SHARD=<i>/6 tests/vm/run.sh powercut` to replay the same log with the same subsets.

An intermittent failure is hunted on GitHub rather than locally: `gh workflow run release-gate.yml --ref <branch> -f suites=boot -f fs=ext4 -f repeat=10` runs ten boot jobs side by side. When the boot suite's guest reports a NIC networkd didn't configure, `tests/vm/boot.sh` adds QEMU's view of the device (`info network` and the rx and tx queues' `info virtio-queue-status`) to the job's output as `qemu:` lines, and pulls and puts back the cable with `set_link` if the guest's own link down/up didn't help.

A failed job uploads its logs as the `logs-<job>` artifact: each work dir's `serial.log` and other logs, plus the proxy's log, but never disk images.
