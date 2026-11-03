# VM testing: what NixOS does, what archix takes

Read from nixpkgs `master` on 2026-10-27; paths are in nixpkgs.

## What NixOS does

| Practice | How | Where |
| --- | --- | --- |
| Backdoor shell | `backdoor.service` runs `bash --norc /dev/hvc0` as root, a virtio console (`-device virtconsole,chardev=shell`); it prints `Spawning backdoor root shell...` when ready. The driver listens on a unix socket that QEMU connects to. A command goes out as `timeout N bash -c CMD \| (base64 -w 0; echo)`, its status as a second `echo ${PIPESTATUS[0]}`, under `set -euo pipefail`; stderr goes to the serial console. Getty is turned off on `hvc0` and `ttyS0`, which it would break. `testing.initrdBackdoor` runs the same shell in the systemd initrd, left with `switch_root()`; `sshBackdoor` adds ssh over vsock for people. | `nixos/modules/testing/test-instrumentation.nix`, `nixos/lib/test-driver/src/test_driver/machine/__init__.py` (`QemuStartCommand.cmd`, `_execute`, `connect`) |
| Machine API | `succeed`, `fail`, `execute` (15 min default timeout, enforced by `timeout` in the guest), `wait_until_succeeds` (1 s retries, 15 min), `wait_for_unit` (polls `ActiveState`; `failed`, or `inactive` with no jobs, fails at once), `wait_for_console_text` (regex over the serial lines since start), `send_key` (monitor `sendkey`), screenshots and OCR. `crash()` is the monitor's `quit`: QEMU exits, the next `start()` boots the same disk. `shutdown()` sends `poweroff` and waits for QEMU to exit. `reboot()` sends ctrl-alt-delete and needs `start(allow_reboot=True)`, because QEMU runs with `-no-reboot` by default. Steps are `with subtest("name"):`, which only groups the log; the first exception ends the test. | `nixos/lib/test-driver/src/test_driver/machine/__init__.py` |
| Boot | By default no boot loader and no disk image: `virtualisation.directBoot` passes `-kernel`/`-initrd`/`-append`, the root is a fresh empty ext4 image, and `/nix/store` is the host's, over 9p or virtiofs (`mountHostNixStore`), so a VM is ready in seconds and nothing is built per test. Its docs say it "will not (re-)boot correctly into a system that has switched to a different configuration on disk". Boot loader tests set `useBootLoader` and `useEFIBoot`: a full disk image with the loader installed is built once (`make-disk-image.nix`) and each run boots OVMF on a qcow2 overlay of it. `systemd-boot.nix` does this; `installer.nix` installs from the ISO onto an empty disk. | `nixos/modules/virtualisation/qemu-vm.nix` (`directBoot`, `useBootLoader`, `mountHostNixStore`), `nixos/tests/systemd-boot.nix` |
| Interactive driver | `driverInteractive` is the same driver with `--interactive`: a ptpython REPL with each machine, `start_all`, `test_script` and `subtest` in scope. `--keep-machine-state` keeps the disks, and `enableDebugHook` stops a failing sandboxed test so you can attach to it. | `nixos/lib/testing/interactive.nix`, `nixos/lib/test-driver/src/test_driver/__init__.py` |
| Result caching | A test is a derivation whose build runs `nixos-test-driver -o $out`. Its hash covers the driver, the script and every VM's whole system closure, so an unchanged test is never run again: the local store or Hydra's binary cache already holds its result. The build sandbox has no network. | `nixos/lib/testing/run.nix` |
| Parallelism | Between tests: each is its own derivation, which Nix builds side by side (`max-jobs`) and Hydra spreads over builders. Within a test, `start_all()` starts every machine in a thread, joined by VDE vlans. Tests are small: `systemd-boot.nix` is about twenty tests, each with its own VM and a few reboots, not one long walk. | `nixos/lib/test-driver/src/test_driver/driver.py` (`start_all`), `vlan.py`, `nixos/tests/systemd-boot.nix` |
| Global timeout | A timer kills the test after `globalTimeout` (1 h by default). | `driver.py` (`race_timer`) |

## What fits archix

- **Backdoor: fits.** `tests/vm/driver/backdoor.sh` is a named virtio-serial port (`archix.shell`) rather than a virtio console. That port is no tty, so there is no `stty raw` and no getty to fight. One line out (`TOKEN TIMEOUT BASE64-COMMAND`) gets one line back with the status, stdout and stderr, which goes into the failure message. It announces every boot with its boot id, so the driver knows a reboot has finished. Commands run without `pipefail`, as vmtest.sh's did, because `cmd | grep -q` would fail when grep closes the pipe early.
- **Machine API: fits.** `crash()` and `reboot()` differ on purpose. archix's subject is what the firmware and systemd-boot do across a reboot, so QEMU keeps running (no `-no-reboot`), with the same OVMF variables file and swtpm. `crash()` is QMP `system_reset`, a reset as vmtest.sh's sysrq-b was: what the guest hadn't written is lost, and the disks and variables stay. A reset keeps what reached the virtual disk; the power-cut tests below use it at chosen steps, and say what it doesn't simulate.
- **Direct kernel boot: doesn't fit the boot suite, and fits little else.** archix chooses a deployment in systemd-boot: the entry, its counters, `LoaderEntrySelected`, which the initramfs reads to mount a deployment, the fallback UKI, Secure Boot signatures and dbx. The kernel command line is inside the signed UKI. Booting `-kernel` skips every one of these. Suites that only check things after boot could boot that way, but every archix suite switches and reboots into a new deployment, which the NixOS docs say direct boot doesn't do. What a direct boot could save is at most the firmware and loader part of each boot (see the timings below), not the boot itself.
- **Shared store over 9p or virtiofs: doesn't fit.** archix's deployments live on the root filesystem, and the root's layout is what is under test.
- **Prebuilt base image as a qcow2 overlay: within a run, not across runs.** `docs/CI.md` weighs caching VM images across runs: the boot suite's image builds in 17 s from the archive cache, and it holds the archix binary under test. Within a run it fits: the split boot tests share one base, laid out by this run's archix (`tests/vm/boot/base.py`), and each boots a qcow2 overlay of it (`disk.qcow2`, backed by the raw `disk.img`) with its own copy of the OVMF variables, as `systemd-boot.nix`'s tests boot overlays of one built image. `build-image.sh` deletes the last run's disk and variables before it builds, so no test boots a layout older code made.
- **Result caching: fits, without Nix.** `tests/vm/driver/cache.py` hashes what Nix would have in the closure: `src/`, `Cargo.toml`, `Cargo.lock`, the test image's Dockerfile (toolchain, QEMU, OVMF, pinned to a snapshot), `run.sh`, `build-image.sh`, the image's check script, the driver and the suite's directory. Files are found with `git ls-files --cached --others --exclude-standard`, so uncommitted and untracked files count, and each is hashed by path, exec bit and content. It also hashes `FS`, `TO_DATE`, `VM_MEM`, `VM_CPUS`, `VM_TIMEOUT`, with `ARCHIX_PREBUILT` the two binaries inside `target.tar.zst`, and what lives outside the repository: the host's kernel (`uname -r`, which KVM is), the test image's base layer (the `archlinux:latest` it was built from, by layer digest and version label) and its `qemu-base`, `qemu-system-x86`, `edk2-ovmf` and `swtpm` versions. The guest's packages need no hash, because snapshot dates in hashed files pin them. A pass keeps its logs, the timeline, console and output, beside its record (`<suite>-<key>.logs.tar.gz`); a hit names them. The release gate and power-cut evidence run with `ARCHIX_VM_CACHE=0`, so they never take a recorded pass. Unlike Nix, the list is written by hand: an input that isn't listed can't invalidate a pass. That is why it lists whole directories and not single files.
- **Interactive driver: fits.** `tests/vm/run.sh boot TEST --interactive` (or `powercut TEST`) runs the base, then gives the REPL on TEST's overlay (`base` for the base itself).
- **Small independent tests: fits, and is the larger speed-up.** See the split below.
- **Soft checks: archix's own.** NixOS stops at the first failure. The driver collects a step's checks and fails the step listing all of them, as `Checks` does in `tests/common`.

## The plan

1. The driver, and the boot tests on it split into small tests that run side by side from one base and are cached one by one by their inputs (locally in `~/.cache/archix-vm-results/`, newest 500; on CI in the Actions cache). Done; CI runs them on every push.
2. With deployments gone (2026-11), the boot tests were rewritten for the in-place model and the power-cut replay for test-hook cuts on the driver (below). Done.
3. Port the suites still driving deployments (nvidia, dkms2, cachyos, churn, pcr, firewall, selfupgrade, rescue, recovery, secrets) to the driver; `run.sh` refuses them until then.

## The boot tests

`tests/vm/run.sh boot [TEST...]` builds archix and the image once, runs `tests/vm/boot/base.py` (the first switch on the booted plain image, which on btrfs sets rollbacks up, and the counted boot after it; then `bootctl set-timeout 0` and a poweroff), and then each test in its own container, VM and heavy slot, on a qcow2 overlay of the base's disk and a copy of its variables. `tests/vm/boot/checks.py` (run by CI's lint job) fails unless each test in `run.sh`'s `boot_tests` has a `test.py` and a cache key, and CI's boot groups run each once, none over `boot-results.sh`'s `MAX`. All but `ext4` need `FS=btrfs`.

| Test | What it checks |
| --- | --- |
| `counting` | a kernel command line that panics gets its two tries, then systemd-boot starts the slot read-only in try mode; pacman and switches refuse there |
| `try` | an older snapshot picked by hand boots read-only with a throwaway layer over the root and /var/lib; the next boot has none of it |
| `guard` | an entry whose snapshot was deleted by hand boots the newest survivor whose entry hasn't failed, else the rescue entry |
| `unattended` | a required mount that's missing stops each counted boot in emergency mode, which reboots itself after `boot.unblessed_timeout` until the slot boots; logs the time from the bad boot to the fallback |
| `hang` | a unit that never finishes keeps each counted boot from boot-complete.target; the unblessed timeout reboots it until the slot boots; logs the time |
| `glibc` | a kernel update by plain pacman and a good boot, then a glibc whose libc.so.6 is garbage by plain pacman and a reset: init can't start, the kernel panics and reboots (`panic=10`, which archix's counted images carry), and the slot, the snapshot just before glibc, boots rather than the older kernel snapshot; logs the time |
| `pacman` | a plain pacman transaction whose unit fails at every boot: the health check fails, the default unblessed timeout reboots, the slot boots |
| `order` | a switch, a plain kernel update and a broken package with no boot between: the slot (the switch's snapshot) boots first although the kernel snapshot's entry is newer; with the slot's entry failed too, the kernel snapshot |
| `health` | no display manager, and a unit that failed at the last blessed boot and fails again: counted boots pass the health check and are blessed |
| `keep` | `archix rollback N` with accounts made since, refusals of switch and plain pacman while pending, `--cancel`, keep from a try boot |
| `hook` | archix's snapshot hook masked by a /dev/null link in /etc/pacman.d/hooks: no snapshot, `archix status` and the boot's notice say so; removing the link brings snapshots back |
| `layer` | a try boot that fills its 4 GiB layer gets ENOSPC; on a near-full disk the layer file is 1 GiB, with no room it's in memory, and the boot still comes up |
| `records` | records rewritten with a higher format and fields this build doesn't know, as a newer archix might write: the snapshot's entry (whose initramfs this build made) still boots it, and the current entry the current system |
| `strict` | strict mode refuses plain pacman, and lets it through with archix's binary, its flag or the config gone, for `pacman -R archix` and after it, and once strict = false is switched |
| `ext4` | the lane with rollbacks off on ext4: switches and `archix revert` apply live, `archix rollback` refuses, the boot after a switch is counted and blessed through the health check, no snapshot entries |

## The power-cut tests

`tests/vm/run.sh powercut [TEST...]` (FS=btrfs) builds archix with the test hooks' debug assertions (`CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true`, the same optimised build otherwise) and runs its tests like the boot tests, from the same base. A cut is armed through the EFI variable `ArchixTestCut-0b9c7b5e-5f43-4f0e-9a4e-6a2d7c1e3f21`, whose data after the attributes is a step name, or `STEP#K` for its Kth hit: in a debug build, `testhooks::mark(step)` reads it (also in the initramfs), deletes it, prints `archix-test: power cut at STEP#K` on the console and waits; the test resets the VM with QMP `system_reset` when it sees that line (the mark resets the machine by SysRq itself after 30 s, for a cut on real hardware). Cuts during package extraction have no mark: the test resets once pacman prints `upgrading NAME...` for the chosen package.

A reset keeps what reached the virtual disk and loses the guest's page cache, as on a disk without a volatile write cache; writes the disk itself would lose or reorder aren't simulated (the deployment era's dm-log-writes replay did that; it went with deployments).

After each cut nobody touches the machine until a boot settles, blessed or in try mode (`settle()` in `tests/vm/powercut/cut.py` follows the reboots a failing counted boot makes by itself); then `problems()` judges the system: `archix status` exits 0 and doesn't call the root foreign, every entry's image is on the ESP, the boot state parses and its current image is there, every snapshot has its root and record, no keep clone is left, `pacman -Dk` and `pacman -Qkk` find nothing outside /etc and /var/log; each test adds its own. Each cut prints `CUT point=... booted=... boots=... took=... entry=... try=... consistent=...` and the test a `CUTS:` summary, and a failed cut doesn't stop the ones after it.

| Test | Cuts |
| --- | --- |
| `pacman` | a real `pacman -Syu` from the 2026-10-10 to the 2026-10-24 archive (22 packages, linux 7.2.9 to 7.2.11, systemd 262 to 262.1): during extraction at five packages; then in the step after the transaction at `boot-image-built`, `boot-image-recorded`, `boot-image-copy`, `boot-image-copied` (the image rebuild) and `menu-entry-written`, `menu-entry-replaced`, `menu-synced` (the boot counter reset on the ESP); after each, `archix rollback` to the transaction's snapshot for the next; then the update without a cut |
| `keep` | `keep-cloned`, `keep-before-exchange`, `keep-exchanged`, `keep-replaced-moved`, `keep-recorded`, `keep-var-lib`, `keep-menu`, and `--cancel`'s `cancel-before-exchange` and `cancel-exchanged`: the root that boots is the one before or the kept snapshot, with an account made before the keep, and a switch works after |
| `varlib` | from the plain image each time: the first switch, then the move of /var/lib into @varlib at the next boot cut at `split-entry-copied` and `split-entry-removed` (first, third and fourth entries) and `split-binds-ready`; every file of /var/lib must be whole in @varlib after the boots that follow |
| `rootmove` | from an image whose btrfs root is the top level (`ROOT_AT_TOP=1`): rollbacks refused, `archix snapshot setup --move-root`, and the move into @ in the initramfs cut at `move-root-snapshotted`, `move-root-fstab`, `move-root-nested-moved` and `move-root-cleaned`, then once without a cut; the root's files, a subvolume nested in it and one beside it at the top level must survive, and rollbacks set up after |

## Using the driver

A suite is `tests/vm/<suite>/test.py` with a `run(machine, t)`; `run.sh` runs it in the test image as `python -m driver /vm/<suite>/test.py`, with the image built as `build-image.sh` builds it for that suite.

| Call | Does |
| --- | --- |
| `machine.start()` | Starts QEMU+OVMF as `boot.sh` does (Secure Boot capable vars, swtpm with `tpm=True`, the user-mode NIC, QMP at `/work/qmp.sock`), plus the shell port; returns at once |
| `machine.execute(cmd, timeout=900)` | `(status, stdout)`, running `cmd` with bash as root; the first call after a boot waits for the shell |
| `machine.succeed(cmd, timeout, what=)` / `machine.fail(...)` | stdout, or an error naming the step, `what` (or the command), the exit status and stderr |
| `machine.wait_until_succeeds(cmd, timeout, interval=1, what=)` | Retries until the command succeeds |
| `machine.wait_for_unit(unit, timeout)` | Waits for `active`; fails at once on `failed`, or `inactive` with no jobs |
| `machine.wait_for_console_text(regex, timeout)` | Searches the serial console since this boot began |
| `machine.wait_for_shell(timeout)` | Waits for a new boot's shell (default `boot_timeout`, 900 s) |
| `machine.reboot()` / `machine.crash()` / `machine.shutdown()` | `systemctl reboot` / QMP `system_reset` / `systemctl poweroff` and wait for QEMU to exit |
| `machine.select_entry(name)` | `bootctl set-oneshot name` |
| `machine.disks`, `machine.extra` | The `-drive` specs and further QEMU arguments the next `start()` uses, such as a second disk or `-kernel`, `-initrd` and `-append` for a live system |
| `machine.qmp(cmd, **args)` / `machine.hmp(line)` | QMP and monitor commands (`info network`, `set_link net0 off`) |
| `with t.step(name):` | A named step; `t.check(ok, msg)` collects, `t.require(ok, msg)` stops; the step fails listing every message |

Every command, boot and step goes to the timeline with its time since the start (`/work/timeline.log`, kept with the console in `~/.cache/archix-vm-logs` when a run fails). `VM_TIMEOUT` (2400 s) kills a VM that runs too long.

## Timings

The measurements below are from the deployment era (2026-10-27 and 28), kept for the method; the tests they name are gone.

### The split, 2026-10-28

Measured on this host at c7d630a-7b6a675 with the 8079 archive cache, archix already built, `ARCHIX_HEAVY_JOBS=4` (the default is 2) and nothing else in the slots; whole wall times of `tests/vm/run.sh`.

| Run | ext4 | btrfs |
| --- | --- | --- |
| `boot` (the split), cold (`--no-cache`), one filesystem at a time | 198 s | 193 s |
| of which: build, image and base until laid out | 56 s | 58 s |
| `boot` again, all seven cached | 0.85 s | 0.85 s |
| `boot` after an edit to `counting/test.py` only: the base and `counting` rerun, six cached | 109 s | |
| `boot2` (the walk), four runs side by side with `boot-sh` (vmtest.sh) | 492 s, passed | failed at 257 s in stage 6 (below) |
| `boot-sh` (vmtest.sh), in the same batch | failed at 406 s: the etc-merge status race, which vmtest.sh didn't poll for until 7b6a675 | failed at 410 s, the same |

Each test's own time, cold on ext4 and btrfs: `revoke` 93 and 86 s, `fallback` 89 and 86 s, `layout` 77 and 78 s, `etc-merge` 60 and 60 s, `counting` 54 and 53 s, `secureboot` 43 and 41 s, `reset-mid-switch` 38 and 32 s (47 s since it switches once more, 5bb7fc5). With four slots, three tests wait for the first to finish, so the wall time is the base, one of the long tests and one of the short ones; with seven slots it would be about the base and `revoke`, 150 s. A boot is 7.2 to 7.6 s with the menu timeout at 0, against 7.8 to 10 s before.

An edit reruns only what it feeds. `tests/vm/boot/common.py` (or `base.py`, `config.lua`) changes all seven keys and not `boot2`'s; `layout/test.py` changes only `boot-layout`'s; `src/`, the driver, `run.sh` or `build-image.sh` change all of them.

On CI (run 40639243875, 387e051, cold) a boot job took 4.7 to 6.0 min against 12.7 and 13.1 min for vmtest.sh's two jobs; the fourteen jobs' last ended 10.8 min after the first started, as some waited for a runner. `boot (secureboot, ext4)` failed once on a NIC that never got a carrier (`systemd-networkd-wait-online.service` failed at its first boot), the CI flake vmtest.sh has shown before, and passed on a rerun. Run 40642009003 (7b6a675, same inputs) took every boot job from the Actions cache, 1.8 to 2.5 min each, nearly all of it runner setup, which the key needs: the test image's QEMU, OVMF and swtpm versions come from the image. Run 40649673183 (the retiring commit, cold but for two tests) took 4.0 to 6.5 min a job, but the run 28.6 min, as its 14 boot jobs, 6 container shards and 4 deploy-boot jobs queued for runners, some boot jobs 18 min: the matrix trades runner concurrency for wall time, and when runners are short, fewer jobs that each run several tests side by side would finish sooner. Grouped as two jobs per filesystem (CI 40661120528, 4c7c548, cold), a boot job took 8.2 to 9.3 min, `run.sh` 348 to 411 s of it: the base 94 to 113 s, then its tests two at a time, each 1.5 to 1.9 times as long as alone (revoke 197 and 220 s, fallback 187 and 212, layout 132 and 182, counting 103 and 114, etc-merge 105 and 107, reset-mid-switch 97 and 112, secureboot 63). The run took 14.1 min against 28.6, its boot jobs 35 runner minutes against 66 and the whole run 98 against 134; it met no queue (17 jobs ran at once, 40649673183 at most 7), so with 7 runners it would take about 19 min. In the release gate (40652861394, `suites=boot2,boot`, uncached) `boot2` took 14.2 min on ext4 and 12.6 on btrfs, and `boot`, all seven tests in one job two at a time, 10.9 and 13.7.

`boot2` on btrfs failed in stage 6: after the reset mid-switch and the switch after it (deployment 6, pending), the next switch, building deployment 7 from 6, failed with `error: No such file or directory (os error 2)`. It didn't recur in the other runs; the split's `counting` builds its panicking deployment from a booted one, so `reset-mid-switch` now switches once more from the pending deployment, as the walk does.

### The driver, 2026-10-27

Measured on this host on 2026-10-27 (cbcaad7), with the 8079 archive cache, archix already built, and `tests/vm/run.sh`'s whole wall time, from the release build to the VM's end.

| Run | ext4 | btrfs |
| --- | --- | --- |
| `boot` (vmtest.sh), four runs side by side | 429 s | 425 s |
| `boot2` (the driver), in the same batch | 436 s (419 s of steps) | failed at 330 s: the etc-merge status race, fixed in cbcaad7 |
| `boot2`, two runs side by side | 414 s (398 s of steps) | 411 s (394 s of steps) |
| `boot2` again, cached | 86 ms | 85 ms |

The driver makes a run neither faster nor slower: the VM does the same work. 18 boots took 205 s of the 419 s of steps, counted from the reset or reboot to the guest's shell. A boot is 7.8 to 10 s, and 38 s for step 7's three panicking boots plus the fallback. loader.conf's `timeout 2` is 2 s of each. The time saved comes from the cache and from the split above.

The cache can't hide a change. Flipping one byte of `src/main.rs`, `Cargo.lock`, the Dockerfile, `build-image.sh`, `check-deployment`, `machine.py`, `backdoor.sh`, `test.py` or `config.lua`, an exec bit, a new untracked file under `src/`, or `FS` each gave a new key, and restoring it gave the old one back. With one byte of a comment in `src/main.rs` changed, `run.sh boot2` rebuilt archix and ran the suite (413 s, passed); once it was restored, the old pass was a hit again (84 ms). With two checks in step plain broken, the run failed at 37 s with:

```
FAIL: step 'plain: the first switch on a booted system' failed, 2 check(s):
- the first switch should lay out deployment 1 and build 2: 1 2
- /boot should be read-only once laid out
```
