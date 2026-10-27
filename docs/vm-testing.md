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
- **Machine API: fits.** `crash()` and `reboot()` differ on purpose. archix's subject is what the firmware and systemd-boot do across a reboot, so QEMU keeps running (no `-no-reboot`), with the same OVMF variables file and swtpm. `crash()` is QMP `system_reset`, the same cut as vmtest.sh's sysrq-b. Running `quit` and starting again would also work, but reset keeps the volatile EFI state behaving like a real reset.
- **Direct kernel boot: doesn't fit the boot suite, and fits little else.** archix chooses a deployment in systemd-boot: the entry, its counters, `LoaderEntrySelected`, which the initramfs reads to mount a deployment, the fallback UKI, Secure Boot signatures and dbx. The kernel command line is inside the signed UKI. Booting `-kernel` skips every one of these. Suites that only check things after boot could boot that way, but every archix suite switches and reboots into a new deployment, which the NixOS docs say direct boot doesn't do. What a direct boot could save is at most the firmware and loader part of each boot (see the timings below), not the boot itself.
- **Shared store over 9p or virtiofs: doesn't fit.** archix's deployments live on the root filesystem, and the root's layout is what is under test.
- **Prebuilt base image as a qcow2 overlay: little to gain.** `docs/CI.md` already weighs caching VM images: the boot suite's image builds in 17 s from the archive cache, and it holds the archix binary under test.
- **Result caching: fits, without Nix.** `tests/vm/driver/cache.py` hashes what Nix would have in the closure: `src/`, `Cargo.toml`, `Cargo.lock`, the test image's Dockerfile (toolchain, QEMU, OVMF, pinned to a snapshot), `run.sh`, `build-image.sh`, the image's check script, the driver and the suite's directory. Files are found with `git ls-files --cached --others --exclude-standard`, so uncommitted and untracked files count, and each is hashed by path, exec bit and content. It also hashes `FS`, `TO_DATE`, `VM_MEM`, `VM_CPUS` and, with `ARCHIX_PREBUILT`, the two binaries inside `target.tar.zst`. Packages need no hash, because snapshot dates in hashed files pin them. Unlike Nix, the list is written by hand: an input that isn't listed can't invalidate a pass. That is why it lists whole directories and not single files.
- **Interactive driver: fits.** `tests/vm/run.sh boot2 --interactive` gives a Python REPL with `machine` and `t`; `test_script()` runs the whole test.
- **Small independent tests: fits, and is the larger speed-up.** See the plan.
- **Soft checks: archix's own.** NixOS stops at the first failure. The driver collects a step's checks and fails the step listing all of them, as `Checks` does in `tests/common`.

## The plan

1. The driver and the boot suite on it (`tests/vm/run.sh boot2`), next to the old suite until it has been proven on CI. Done here.
2. A pass is cached by its inputs, locally in `~/.cache/archix-vm-results/` (newest 500) and on CI in the Actions cache.
3. Split the boot suite, as `systemd-boot.nix` is split, into tests that each start from a laid-out image (`build-image.sh` already makes one for the other suites): layout and rollback; Secure Boot, boot counting and revocation; the /etc merge; the fallback UKI and eject. They run side by side in the heavy slots and are cached one by one, so a change that touches one area reruns only that test.
4. Port the other suites once the boot suite on the driver has been green on CI for a while. Then delete `vmtest.sh` and `boot.sh`'s console-grep path.

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
| `machine.qmp(cmd, **args)` / `machine.hmp(line)` | QMP and monitor commands (`info network`, `set_link net0 off`) |
| `with t.step(name):` | A named step; `t.check(ok, msg)` collects, `t.require(ok, msg)` stops; the step fails listing every message |

Every command, boot and step goes to the timeline with its time since the start (`/work/timeline.log`, kept with the console in `~/.cache/archix-vm-logs` when a run fails). `VM_TIMEOUT` (2400 s) kills a VM that runs too long.

## Timings

TIMINGS

## Old checks and new steps

TABLE
