# VM testing: what NixOS does, what archix takes

Read from nixpkgs `master` on 2026-10-27; paths are in nixpkgs.


## Nothing runs privileged on the host

No test runs a `--privileged` container: one sees the host's devices, and test containers' systemd armed the host's `/dev/watchdog0` and hard-reset it. `tests/vm/boot/checks.py` fails on one anywhere in `tests/`, `scripts/` or `.github/`. What needs root, loop devices, btrfs or systemd runs in a test VM instead (`tests/testvm/`): QEMU with `/dev/kvm` in a plain container, as the calling user, booting archix-arch-test's own tree directly with `-kernel`/`-initrd`.

- `tests/testvm/image.sh` turns the archix-arch-test image into the VM's root without privileges: `docker export`, then `mke2fs -d` in a plain container, plus its kernel and an initramfs from `mkinitcpio`. It's cached in `~/.cache/archix-testvm` (`ARCHIX_TESTVM_CACHE`) by the image's id and `tests/testvm/guest/`.
- `tests/testvm/run.sh` runs one job in a fresh VM: a throwaway overlay of that root, host directories over 9p (read-only ones as they are, writable ones with the guest's owners in xattrs, so the host's files stay the user's), qcow2 disks by serial, the job's output on its own console and its exit status returned. The host's 127.0.0.1 is the guest's 10.0.2.2, or 10.0.2.100 only (`--offline`). `ARCHIX_TESTVM_MEM` (12G) and `ARCHIX_TESTVM_CPUS` size it.
- The VM suites' disk images: `tests/vm/build-image.sh` runs unchanged in a test VM, with `/work` and `/vm` shared. A builder VM rather than unprivileged tools (`mkfs.btrfs --rootdir`, mtools, `sgdisk` on a file), as the build needs root inside the new system: pacstrap with its hooks, `arch-chroot` for archix's update, `systemctl enable` and `mkinitcpio`, btrfs subvolumes and CachyOS's Limine layout. Each kernel's mkinitcpio preset writes a UKI to the ESP, which systemd-boot lists by itself, so pacman's own hooks keep the boot images current as on a stock install; archix writes no boot entries.
- The container suite, `tests/arch/run.sh`: a VM builds into the checkout's build disk (`~/.cache/archix-testvm/disks/archix-target-<hash>.qcow2`, or `ARCHIX_TARGET_VOLUME`, a directory, over 9p), with the crate registry in `ARCHIX_CARGO_VOLUME` (default `~/.cache/archix-testvm/disks/cargo-registry`); the tests then run in one VM, or two with `--jobs 2`, each on a throwaway overlay of that disk. The heavy slots, the archive proxy and `--quick` are as before.

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
- **Direct kernel boot: doesn't fit.** The suites reboot after kernel and DKMS changes the stock boot path (the distribution's mkinitcpio hooks, systemd-boot or Limine) has to carry, which `-kernel` would skip. What a direct boot could save is at most the firmware and loader part of each boot (see the timings below), not the boot itself.
- **Shared store over 9p or virtiofs: doesn't fit.** archix manages the root filesystem itself, which is what is under test.
- **Prebuilt base image as a qcow2 overlay: not now.** `docs/CI.md` weighs caching VM images across runs: an image builds in about 17 s from the archive cache, and it holds the archix binary under test. The split boot tests shared one base by qcow2 overlays within a run; they went with the rescope (`docs/coverage-after-removal.md`), and each suite now builds its own image. `build-image.sh` deletes the last run's disk and variables before it builds.
- **Result caching: fits, without Nix.** `tests/vm/driver/cache.py` hashes what Nix would have in the closure: `src/`, `Cargo.toml`, `Cargo.lock`, the test image's Dockerfile (toolchain, QEMU, OVMF, pinned to a snapshot), `run.sh`, `build-image.sh`, the image's check script, the driver and the suite's directory. Files are found with `git ls-files --cached --others --exclude-standard`, so uncommitted and untracked files count, and each is hashed by path, exec bit and content. It also hashes `FS`, `TO_DATE`, `VM_MEM`, `VM_CPUS`, `VM_TIMEOUT`, with `ARCHIX_PREBUILT` the two binaries inside `target.tar.zst`, and what lives outside the repository: the host's kernel (`uname -r`, which KVM is), the test image's base layer (the `archlinux:latest` it was built from, by layer digest and version label) and its `qemu-base`, `qemu-system-x86`, `edk2-ovmf` and `swtpm` versions. The guest's packages need no hash, because snapshot dates in hashed files pin them. A pass keeps its logs, the timeline, console and output, beside its record (`<suite>-<key>.logs.tar.gz`); a hit names them. The release gate and power-cut evidence run with `ARCHIX_VM_CACHE=0`, so they never take a recorded pass. Unlike Nix, the list is written by hand: an input that isn't listed can't invalidate a pass. That is why it lists whole directories and not single files.
- **Interactive driver: fits.** `tests/vm/run.sh SUITE --interactive` gives the REPL with the suite's VM as `machine`.
- **Small independent tests: fits, and is the larger speed-up.** See the split's timings below.
- **Soft checks: archix's own.** NixOS stops at the first failure. The driver collects a step's checks and fails the step listing all of them, as `Checks` does in `tests/common`.

## The suites

`tests/vm/run.sh SUITE` builds archix (or takes `ARCHIX_PREBUILT`'s), builds the suite's image in a test VM, and runs `tests/vm/SUITE/test.py` on the driver; FS=btrfs, and FS=ext4 for those in run.sh's `ext4_tests`. `tests/vm/suite.py` makes the first switch on the plain image and reboots. `tests/vm/boot/checks.py` (run by CI's lint job) fails unless each suite in run.sh's `suite_tests` has a `test.py` and a cache key, and ci.yml's suites job names only them.

| Suite | What it checks |
| --- | --- |
| `dkms2` | linux and linux-lts: a switch adding v4l2loopback-dkms locks both kernels' headers and builds it for both, live; the 2026-06-01 to 2026-10-10 update rebuilds it for both new kernels, and the boot after loads the new build |
| `nvidia` | nvidia-open-dkms added live, the `TO_DATE` update building the new driver for the new kernel and booting it, the open driver dropped without a trace, the closed nvidia-580xx-dkms built from the AUR, added and dropped |
| `cachyos` | stock CachyOS on Limine: diff and switch refuse ufw beside archix's firewall and a root= naming no device; a first switch from a relative --config; CachyOS's v3 builds locked from its repos; a full disk refused before any change; zlib-ng-compat swapped for zlib and back, and cut mid-swap, never without libz.so.1; a failed switch keeps the lock it resolved; an AUR build whose dependency only CachyOS has |
| `firewall` | ssh through a port forward, and LAN, public, IPv4 and IPv6 sources over a second NIC, answered by each service's declared sources; a switch dropping the services closes them live and after a reboot |
| `secrets` | consumers read their secrets on a real boot (wg-quick, NetworkManager under /run, LoadCredential=, EnvironmentFile=, a crypt_hash login); a switch changes a value live; a lost identity fails closed and the admin key recovers it; a damaged ciphertext costs only its secret; a crashed decrypt leaves no core; swap and hibernation; a raw grep of the root and swap partitions |

Until the switch rewrite (docs/SCOPE.md 2.2) every suite stops at its first switch, which `suite.py` fails loudly naming the stub; CI lists them in `tests/ci/rescope-pending.txt` (`docs/CI.md`). The boot, power-cut, write-replay, Limine, rescue, recovery, pcr, churn and selfupgrade suites went with the rescope; `docs/coverage-after-removal.md` lists what each case covered and where that coverage lives now.

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

The measurements below are from the deployment era (2026-10-27 and 28), kept for the method; the tests they name are gone, and so is the split.

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
