# Coverage after the removal list

SCOPE 9's rule: a kept module whose only coverage was through a removed path gets a new test. This is the account for the VM cases the removals deleted (0251258, and e87cc78 and 47dfdb4 on scope-v1-suites), and for each `tests/*.rs` target at scope-v1 (910ba9c). Kept modules are SCOPE 9's: eval and options; lock and archive, AUR fetch, srcinfo and review; system/apply, accounts, credentials, secrets, fstab (as files), firewall, flatpak; plan and plan_diff; and captured.rs as the isolated evaluator.

Nothing here was run: no VM and no test binary. Each verdict comes from reading the test and the code at 910ba9c. "Pending" means the test is in `tests/ci/rescope-pending.txt`, which CI skips with its reason (`docs/CI.md`).

"Lost — rebuilt in X" names the SCOPE 10 gate suite that has to cover the case before the gates. Those suites don't exist yet:

- **fidelity**: per plan group, `diff --json`, switch, verify, then diff exiting 0;
- **drift**: `pacman -Syu`, a hand install and a hand edit between switches;
- **concurrency**: also covers diff as non-root against diff as root;
- **secrets sweep**;
- **failure injection** and the printed rollback;
- **power cut**;
- **uninstall**;
- **images**: stock CachyOS, stock archinstall, and the lived-in image.

## The commits

`git log 160f733..scope-v1-removals` has 14 commits:

| Kind | Count | Commits |
| --- | --- | --- |
| Removals | 10 | 9ba1ccc (pacman hooks, strict, uninstall), d219f1d (rescue), f26a266 (boot), 4704972 (snapshots, layout, config.git), 9d98ca4 (generations, revert, gc), f9269c8 (package-swap machinery), 967ea20 (holds), bf2d0e5 (old_model), 9381bee (archix-static), 0251258 (old VM suites) |
| Stub | 1 | 7ece172: the switch, install, remove, upgrade and diff stubs, which also deleted commands/switch.rs and checks.rs |
| Init | 1 | 33a195f: the minimal init, which also deleted capture.rs, carried.rs and the TUI |
| fmt | 1 | bb09625 |
| Messages | 1 | 3edf434 |

Every removal message reports `cargo test --lib`, which held at 173. None ran the integration targets, which the removals broke (below): eval.rs has two plain tests that now fail under a bare `cargo test`.

## The boot suite (0251258, 22 cases)

| Case | What it checked | Kept module | Where that coverage is now |
| --- | --- | --- | --- |
| counting | a panicking kernel's two tries, then the slot read-only | none (boot counting) | — |
| try | an older snapshot's entry booted read-only with a throwaway layer | none (try mode) | — |
| guard | an entry whose snapshot was deleted boots the newest survivor | none | — |
| unattended | a missing required mount stops counted boots in emergency mode until the slot boots | fstab: required mounts are not `nofail` | Unit: `system::fstab::tests::mounts_are_nofail_with_a_short_device_wait_unless_required` and `mounts_at_home_var_and_srv_are_required_unless_said_otherwise`. The real boot is lost — rebuilt in fidelity (the fstab group) |
| hang | a hung unit's counted boots reboot to the slot | none | — |
| glibc | a garbage libc from plain pacman falls back to the snapshot before it | none | — |
| pacman | archix's pacman hook snapshots, the health check fails, unattended fallback | none (hooks, health) | — |
| order | fallback order between switch and pacman snapshots | none | — |
| health | the health check doesn't fall back without cause | none | — |
| keep | `archix rollback N`, the refusals while pending, `--cancel`, accounts carried | accounts (carried across a keep) | The keep is gone. Accounts across a snapper rollback are lost — rebuilt in failure injection (the printed rollback, followed literally) |
| hook | a masked snapshot hook named by status and the boot notice | none | — |
| layer | the try layer's size on a full disk | none | — |
| records | newer-format snapshot and boot records still boot | none | — |
| strict | strict mode's pacman refusals and its ways out | none | — |
| ptrace | the owner can't ptrace a root switch's evaluator (transient uid, no_new_privs, yama at 0 with a positive control), and limits stop a runaway config with nothing applied | captured.rs / eval isolated | `tests/captured.rs::the_evaluator_runs_as_a_transient_uid_the_owner_cant_ptrace` and `a_runaway_config_is_stopped_by_its_limits` (they run). Lost: the positive control at `ptrace_scope=0` (in the test VM yama may give the EPERM by itself), and the end-to-end root command applying nothing. Rebuilt in concurrency (root diff of an owner's config) |
| accounts | users made or deleted since a snapshot, locked placeholders, `user unlock` | accounts | Placeholders and `user unlock` are gone with keep. Plain account management is in `tests/system.rs::files_users_and_services_follow_the_config` and `a_removed_system_user_comes_back_with_its_old_uid`, both pending. Lost on a real boot — rebuilt in fidelity (the users group) |
| revoke | dbx revocation of dropped images | none (secureboot, revoke) | — |
| secureboot | Secure Boot setup, signed images, the read-only ESP bless | none | — |
| entry-edit | edited snapshot entries boot with a login note | none | — |
| file-systems | data disk, LUKS with a key file, bind and tmpfs mounts, swap file, zram, hibernate and resume, a switch with swap on, the boot without the data disk, /home required; emergency hint and slot fallback | fstab (as files), swap, zram, crypttab, sysctl | Unit: `system::fstab::tests::*` (18), including `encrypted_mounts_get_crypttab_lines`. Pending: `tests/system.rs::file_systems_and_swap_are_the_fstab_after_archixs_own_root_and_esp_lines`. Running: `a_swap_file_is_made_once_and_again_only_at_another_size` and `a_swap_file_on_btrfs_gets_a_subvolume_of_its_own_and_never_shares_one`. Lost: the real boot that mounts, opens LUKS, swaps, hibernates and survives a missing disk. Rebuilt in fidelity (the fstab group). The emergency hint and fallback are removed |
| removal | `pacman -R archix-git` takes the hooks away; the system boots and reinstalls its kernel without archix | none (uninstall hooks) | Rebuilt in the uninstall gate: remove archix, then `pacman -Syu`, paru and snapper must still work |
| ext4 | switches live on ext4, revert, `rollback` refused, counted boot | system/apply on ext4 | revert, rollback and counting are gone. Apply on ext4 is in the ext4 lanes of dkms2, nvidia, firewall and secrets (`ext4_tests`), all pending |

## The power-cut, rescue and replay cases (0251258)

| Case | Kept module | Where |
| --- | --- | --- |
| powercut esp (cut after `pacman -Syu` replaced the running kernel) | none (boot image step, fallback) | Rebuilt in the power cut gate, as a documented risk window (mkinitcpio on the FAT ESP) |
| powercut keep | none | — |
| powercut pacman (cuts in a real `-Syu` through archix's hooks) | none (hooks) | Rebuilt in the power cut gate (a cut mid-transaction; recovery by `archix switch` again) |
| powercut rootmove | none | — |
| powercut varlib | none | — |
| rescue (sorting, repair from the rescue entry, password reset, Secure Boot, lockout, TPM LUKS) | credentials (the lockout rule) | `system::credentials::tests::logins_are_root_and_wheel_or_sudo_members_with_a_checkable_unexpired_password` (d219f1d). The rest is removed |
| replay varlib | none | — |

## Suites deleted on this branch (e87cc78)

| Suite | Kept module | Where |
| --- | --- | --- |
| selfupgrade | none (old_model's refusal) | — |
| pcr | none (Secure Boot, PCR signing) | — |
| recovery | none (chroot, `rescue rollback`, archix-static) | — |
| churn | packages over many switches, and archix's state staying bounded | Lost — rebuilt in drift (repeated switches over package updates) and the lived-in image. The ESP, menu, snapshots, generations and dbx bounds are removed |
| limine keep, resync, migration, handover | none (Limine handling) | — |
| replay pacman, install, keep, rootmove, uncounted | none | — |
| replay switch, powercut switch | the old switch's write ordering (durable.rs stays) | Lost with the old switch. Rebuilt in the power cut gate. The write-replay harness is in history: `git show e87cc78^:tests/vm/replay/replay.py`, `writelog.py`, `fat.py` and `tests/vm/powercut/cut.py` |

## Steps cut from the kept suites (47dfdb4)

| Suite, step | Kept module | Where |
| --- | --- | --- |
| dkms2: entries and images per kernel | none (boot entries) | Replaced by each kernel's own UKI from its stock preset (`both_kernels`, `boot_kernel`) |
| dkms2: try, keep and rebuild after the keep | none | — |
| nvidia: try and keep | none | — |
| nvidia: revert from archix's package cache | none (generations, gc) | — |
| cachyos: boot tools installed, rescue entry, @archix copies, Limine keep refusal, offline revert, the move to systemd-boot, CachyOS-named entries, keep | none | — |
| cachyos: `archix status` after the cut | none (status) | Logs `archix diff` instead |
| firewall: try boot of the snapshot before | none | — |
| firewall: revert reopens the services live | firewall | Firewall step 3: a switch allowing them again, live and after a reboot |
| secrets: a snapshot's entry boots its own values | none | — |
| secrets: identity lost on a counted boot falls back to the slot | none (health) | The lost identity failing closed and the admin key recovering it stay (secrets step s3) |
| secrets: rescue entry decrypts nothing; its password holds over the secret | credentials (the rescue override) | The override has no writer since d219f1d. `tests/credentials.rs::a_password_set_in_the_rescue_holds_until_the_declared_hash_changes` is pending; it goes or gets rewritten with the switch |
| secrets: `archix chroot` keeps a snapshot without the identity | none | — |
| secrets: `archix status` names a missing secret and why hibernation is off | secrets (`secret … is missing at this boot`), sleep (`hibernation is off …`) | The strings are still in src/secrets/mod.rs and src/system/sleep.rs, with no command printing them since 7ece172. Lost — rebuilt in the secrets sweep once diff or the journal reports them |

## Integration targets at scope-v1 (910ba9c)

CI runs every target in the Arch test VM with `--include-ignored`. Three things break tests that never reach the switch:

- **The stub.** switch, install, remove and upgrade exit 3.
- **tests/common's `Setup`.** It writes `boot = { loader, esp, kernel, rescue_entry }` into every config, and eval refuses that since f26a266 (`REMOVED: boot`). So every `Setup` test fails at its first `archix update`, before any switch.
- **Removed commands.** status, revert, generations, gc, rescue, recover, chroot, snapshot and user.

| Target | Tests | Can run and pass now | Pending (why) |
| --- | --- | --- | --- |
| archlinux | 8 | 6 lock tests | 2: `diff_lists_installs_and_removals` (the old diff's text and exit 0; the new diff exits 1 on changes), `a_dkms_module_pulled_in_as_a_dependency_brings_the_kernels_headers` (its config sets `boot`) |
| aur | 11 | 1 (`real_aur_packages_resolve`) | 10: `Setup`; 6 of them also switch, and one calls `status` |
| captured | 15 | 10, including the ptrace and limits tests | 5 call `archix status` |
| credentials | 3 | 0 | 3: switch and `Setup`; they also call revert, status and chroot |
| eval | 28 | 26 | 2 that fail under plain `cargo test` too: `subcommand_help_answers_without_a_config` (snapshot, user and rescue are gone), `a_config_that_writes_fstab_or_what_the_mount_options_write_is_refused` (the /.archix swap refusal is gone) |
| fetch | 1 | 0 | 1: switch, revert |
| flatpak | 1 | 0 | 1: switch |
| formats | 6 | 6 | — |
| helper | 5 | 0 | 5: `Setup`; 4 also install, remove or switch |
| hm | 1 | 1 | — |
| home | 5 | 0 | 5: switch |
| init | 2 | 2; the diff-after-init test is new on scope-v1 and has never been run | — |
| no_bare_rename, no_global_sync | 1 each | 1 each | — |
| recipes | 7 | 2 (the newer-libraries refusals) | 5: switch |
| repos | 8 | 0 | 8: switch; some also revert or gc |
| secrets | 9 | 0 | 9: switch through `Lab` (`Setup`); also revert, gc, status and rescue |
| switch | 29 | 0 | 29: `Setup`; 27 also switch; `an_old_lock_is_called_out` calls status, and `recover_is_gone_…` calls recover and rescue |
| system | 14 | 2 (the swap file tests) | 12: switch; some also revert or status |

That is 174 tests: 77 can run now, and 97 are pending.

The fix that takes the most tests out of the list is dropping `boot_body` from tests/common, which is outside this branch: `Setup` tests that don't switch would run again, and aur's three lock tests with them. For each kept module, the coverage that only these pending targets held is lost until the switch rewrite:

- **apply, files and services**: system.rs;
- **accounts and credentials**: system.rs, credentials.rs;
- **secrets activation**: secrets.rs;
- **firewall**: system.rs;
- **flatpak**: flatpak.rs;
- **fetch and AppImages**: fetch.rs;
- **home files**: home.rs;
- **repos and keyrings**: repos.rs;
- **AUR build and review**: aur.rs, recipes.rs;
- **lock through Setup**: aur.rs, switch.rs.

The rewrite has to bring each target back with it, and each test leaves the pending list in the commit that makes it pass.
