# archix design

Declarative, lockable, rollback-able system configuration for Arch Linux. No Nix required: Arch's packages and the AUR, with Nix's declarative config, lockfile and rollback.

Agreed 2026-10-15.

## Audience and project

- Built for the author first, then a few maintainers, then a public release. Config syntax is not frozen until it has run real machines.
- License: MIT OR Apache-2.0.
- Hosted at `github.com/archix-org/archix`. Website at `archix.dev`.
- Acceptance test: the author's three machines (desktop, laptop, Arch box) move to archix. The NixOS configs are ported by hand.

## Config

- Lua 5.4, embedded via mlua. Laid out like a neovim config: `init.lua`, `lua/hosts/<hostname>.lua`, `lua/modules/...`.
- Host chosen by hostname, overridable with `--host`.
- Evaluation is pure. The only side effects allowed are declared escape hatches (`read_file("./x")` inside the config dir, `hostname()`). Anything with side effects runs in activation hooks at switch time.
- NixOS-style typed module system: modules declare options with types, defaults and descriptions. Unknown keys and type errors are rejected, and docs and completion are generated from the declarations.
- Merge priorities: option default (1500) < `default()` (1000) < plain (100) < `force()` (50), plus `override(n, v)`. Wrapping a table pushes the priority down to every option inside it.
- A module returns `{ imports, options, config }`. A table with none of those keys is treated as config, and a bare function is treated as `config`. `config = function(cfg)` receives the merged config, read-only. Function modules are re-run until their output stops changing (at most 32 rounds), which gives NixOS-style "read other modules' config" without laziness.
- Definition order is pre-order: a module's own config comes first, then its imports in order. Lists concatenate in that order.
- `pairs` iterates keys in sorted order, so evaluation is reproducible despite Lua's per-run hash seed.
- Location: `/etc/archix` by default. Onboarding offers a user-owned repo with `/etc/archix` linked to it. `--config <dir>` overrides.
- git is optional. When the config dir is a git repo, the generation records the commit and archix warns about uncommitted changes. It never refuses to run because of git.

## Locking

Lockfile: `archix.lock` (TOML), committed next to `init.lua`.

### Official repos
- The lock records a snapshot date on the Arch Linux Archive (ALA), plus each package's exact version and sha256. Pins hold or bump individual packages.
- archix installs through its own temporary `pacman.conf` pointing at the ALA snapshot. The user's `pacman.conf` stays on live mirrors, and the two are never mixed, which the wiki warns causes partial upgrades.
- `archlinux-keyring` always comes from the latest date, because old keyrings fail signature checks on old snapshots.
- Every exact version can be fetched from ALA `/packages/`, even when it doesn't match the snapshot date.

### AUR
- Locked by `aur.git` commit.
- Name validation runs offline against the pacman sync DBs plus the AUR metadata dump (`aur.archlinux.org/packages-meta-v1.json.gz`), fetched on sync.

### Third-party repos (CachyOS, chaotic-aur, ...)
- Declared in config. archix writes their `pacman.conf` sections and imports their keys.
- No dated archive exists for them, so archix keeps a content-addressed copy of every package file it installs from them. They also use the missing-version fallback below.

### Missing versions
Order: local package cache, then rebuild from the locked commit, then prompt.
- The prompt is `Use 0.5.1 (nearest newer) instead of 0.5.0? [Y/n]`.
- A policy set in the config or via `--accept-nearest=newer|older` skips the prompt.
- `--noconfirm` accepts the nearest newer version.
- The chosen version is written back to the lock.

## Out-of-band changes (pacman / yay)

- pacman and yay keep working normally on live mirrors. This keeps Arch flexibility, so users aren't overwhelmed.
- A pacman hook (PostTransaction) records transactions archix didn't run.
- On the next `switch`, when the system is ahead of the lock, archix prompts `System is newer than the lock: adopt? [Y/n]`. Yes moves the lock; no reverts to it. `--noconfirm` adopts.
- Adopting picks the ALA date closest to the transaction. Packages not matching that snapshot become exact-version entries.
- `archix -Syu` updates from live mirrors and adopts automatically.

## Undeclared state

- Strict mode by default: undeclared packages, `/etc` files and enabled services are removed or reverted. Warn mode lists them in `archix diff` with an adopt hint instead.
- "Don't ask again" decisions are stored only on the machine (`/var/lib/archix`). To ignore something on every machine, it has to be set explicitly in the config.

## AUR helper (replaces yay/paru)

- Native subcommands: `install`, `remove`, `search`, `switch`, `update`, `diff`, `rollback`, `gc`, `init`.
- pacman-style aliases: `-S`, `-Ss`, `-Syu`, ... Running `archix` with no args does a full update, like yay.
- `archix install foo` writes to an archix-owned data file that the user's Lua merges in. The default location is under `hosts/`. A path can be passed instead; if it isn't a valid directory, the command fails.
- Builds run in a clean chroot (devtools `makechrootpkg`) at the locked snapshot date. Per-package `build_on_host = true` is the escape hatch.
- Built packages are cached, keyed by PKGBUILD commit + snapshot date.
- PKGBUILD review: the full file on first install and a diff on updates. Skipped when the lock already contains that exact commit. Configurable globally and per package.
- Package state is read via libalpm, and transactions run through the `pacman` binary.

## What archix manages

- Packages: official, AUR, local PKGBUILDs, third-party repos.
- System: `/etc` files, systemd units (system and user), sysctl, kernel params, udev, modprobe, users and groups.
- Passwords are never managed. This matches the NixOS default of mutable users. Onboarding runs `passwd` for newly created users.
- Homes of every declared user. archix runs as root and drops to each user for their home.
- Dotfiles are written as real files with recorded hashes, so out-of-band edits show up in `diff` and are never silently overwritten. Live symlinks remain available for app-written configs (e.g. matugen).
- App configs, Home Manager `programs.*` style. See "App modules and Home Manager" below.
- Activation hooks run on switch.

## App modules and Home Manager

archix maintains its own module set, ported from Home Manager (MIT, credited) and kept in sync with it. Upstream projects never have to do anything for archix.

- **Three layers:**
  1. Format writers (toml, json, yaml, ini, kdl, key-value, kconfig) configure any app with no module: `files["~/.config/foot/foot.ini"] = archix.ini { ... }`.
  2. `archix.program "foot" { package = "foot", file = "~/.config/foot/foot.ini", format = "ini" }` defines a thin module in one line. Users then write `programs.foot = { enable = true, settings = { ... } }`, and `settings` passes straight through to the file.
  3. Hand-written modules, ported from Home Manager, only where options interact or need validation (git, zsh, ssh, ...).
- **Porting** is LLM-assisted. Home Manager's option JSON supplies names, types, defaults and descriptions.
- **Cross-validation.** Each ported module's Home Manager test cases (input config plus expected rendered file) are ported too. A port only lands when archix renders byte-identical output for the same input.
- **Tracking upstream.**
  - The repo records the Home Manager commit each module was ported from.
  - A scheduled CI job compares that against Home Manager's HEAD. When a ported module or its tests change upstream, it opens an issue with the diff, and optionally a draft PR with an LLM re-port that is checked against the updated tests.
  - Because `settings` passes through, new upstream app keys need no archix change.
- **Order:** first the author's set (kitty, git, zsh, starship, fastfetch, mimeapps, user units, KDE kconfig). The bulk port of Home Manager comes before the public release, so people arriving from Nix find their modules.

## Generations and rollback

- A generation is the lock, the rendered managed files (content-addressed), referenced package files, and the config git commit if any.
- Filesystem-agnostic baseline: each generation keeps its kernel and modules. Booting an older generation's entry starts an early-boot service that reinstalls that generation's packages and files from cache before the desktop starts. This can fail if pacman or glibc themselves are broken; then a live USB is needed.
- Fast path: on a btrfs root, archix uses snapshots instead. Onboarding recommends btrfs and shows a disclaimer otherwise. Reinstalling is never required.
- Bootloader: systemd-boot only in v1, with a signed UKI per generation via sbctl when keys are present (Secure Boot in v1). GRUB and Limine come later, once they can be tested properly.
- Default limits: 5 boot entries (also capped by ESP free space), generations kept for the last 10 or 30 days whichever keeps more, the package cache limited to what kept generations reference, and `gc` after every switch. Onboarding asks about the important ones.

## Interrupted and failed switches

Goal: no failure, including power loss mid-switch, ever requires a live USB.

- **Prepare, then apply.** `switch` first evaluates, resolves the lock, downloads every package, builds every AUR package, renders every file and checks pacman dependencies and conflicts. Failures in this phase leave the system untouched.
- **Journal.** Before apply, archix writes `/var/lib/archix/journal` with the target generation, the exact package transaction and the list of steps. Each step is marked done as it completes, and every step is idempotent ("make the system match generation N").
- **Atomic writes.** Managed files are written to a temp file and renamed into place. The current-generation pointer and the default boot entry are only flipped after every step succeeds.
- **Hard failures** (pacman errors, file writes, user creation) stop the switch and leave the journal open.
- **Soft failures** (systemd units failing to start) keep the new generation, list the failed units and exit non-zero, like NixOS.
- **Unfinished switches are never acted on automatically.** Unattended runs, `--noconfirm` and the boot-time service report the switch, print cleanup instructions and do nothing. While a switch is unfinished, every archix command except `recover`, `rollback` and `status` refuses to run. pacman and yay keep working; the hook only warns.
- **Reporting.** A login message via `/etc/profile.d`, plus a desktop notification at login.
- **`archix recover`** shows what was interrupted and at which step, then offers:
  - continue: finish the switch;
  - rollback: return to the previous generation;
  - abandon: close the journal without changes, after verifying package files (`pacman -Qkk`) and managed files so the real state is visible.

  `--continue`, `--rollback` and `--abandon` skip the prompt.
- **Interrupted pacman transactions.** After making sure pacman isn't running, archix removes a stale `db.lck`, checks the files of every package in the journaled transaction and reinstalls broken ones from the cache. Prepare guarantees the cache has them.
- **btrfs.** A snapshot is always taken before apply. It becomes that generation's rollback point and is cleaned up under the generation limits.
- **Rescue mode.** Every generation's UKI initramfs carries a static archix, and each generation gets an extra "archix rescue (generation N)" boot entry. Rescue mode unpacks the generation's core packages from the cache itself (glibc, pacman, systemd, kernel modules, archix), without pacman, then repairs pacman's local database. The normal system then boots and finishes the repair. This is what makes ext4 safe without a live USB.
- **Static rescue binary** on GitHub Releases (`archix rollback --root /mnt`), as a fallback for development accidents only.

## Onboarding (`archix init`, TUI)

1. Detect filesystem; btrfs recommendation and disclaimer.
2. Config location: `/etc/archix` or own repo plus link.
3. Strict or warn mode.
4. Capture: explicit packages (official and AUR), non-default enabled services, modified `/etc` files (`pacman -Qkk`), users and groups, kernel params.
5. Dotfile picker (opt-in).
6. Third-party repos found in `pacman.conf`: declare?
7. Secure Boot: enable signing if sbctl keys exist.
8. Generation and cache limits.
9. Dry-run `diff` of the generated config against the live system, which must be empty, then the first `switch`.

## Distribution

- AUR packages `archix` and `archix-bin`.
- The README documents a yay-style manual bootstrap (`git clone https://aur.archlinux.org/archix.git && cd archix && makepkg -si`).
- After the first switch, archix tracks and updates itself through its own lock.

A static `archix` binary on GitHub Releases exists only as a rescue tool (see above), not as an install path.

## Out of v1

- Encrypted secrets (age, agenix/sops-style).
- Remote switch (`--host` over ssh).
- GRUB and Limine.

## Testing

- Unit tests.
- Integration tests in a throwaway Arch container: packages, lock, files, AUR.
- A QEMU VM with OVMF: boot entries, rollback, Secure Boot.
- The container and VM suites run in GitHub CI.
- The Arch box (192.168.0.3) is for real-world testing. Always ask before a real switch there.

## Build order

1. Lua evaluation, typed options, merging.
2. Official packages: lock, ALA install, `diff`/`switch` with prepare/apply and the journal, `recover`, pacman hook and adopt.
3. Generations, runtime rollback, gc.
4. AUR: helper, chroot builds, review, fallbacks. Then third-party repos.
5. System config.
6. Dotfiles, format writers, `archix.program`, Home Manager ports with cross-validation and the upstream tracking job. The Arch box can switch over here.
7. Boot entries, Secure Boot, boot-time rollback, rescue mode in the initramfs. The desktop can switch over here.
8. `archix init` and the TUI.
