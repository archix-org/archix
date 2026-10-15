---
title: CLI reference
description: Every archix subcommand, pacman-style alias and flag, with examples and exit behaviour.
sidebar:
  order: 1
---

```
archix [--config <dir>] [--host <name>] <command> [flags]
```

Running `archix` with no command does a full upgrade, like `yay`. See [`upgrade`](#upgrade--syu).

## Global flags

These work with every command.

| Flag | Default | Description |
| --- | --- | --- |
| `--config <dir>` | `/etc/archix` | The config directory containing `init.lua`. Also read from the `ARCHIX_CONFIG` environment variable. |
| `--host <name>` | the machine's hostname | Evaluate as this host instead. Sets `archix.hostname`. |
| `--help` | | Show help for archix or a command. |
| `--version` | | Print the archix version. |

```console
$ archix --config ~/archix --host laptop diff
$ ARCHIX_CONFIG=~/archix archix eval
```

`--host` changes which host's config is evaluated and locked. It doesn't reach other machines: switching over ssh is not supported.

## Setting up

### `init`

Write a config that describes this machine as it is, and a lock pinned to exactly the package versions that are installed. Needs root.

```console
$ sudo archix init
```

`init` asks a series of questions: where the config should live, whether to make it a git repository, what to do with undeclared and orphaned packages, which config files to manage, whether to write boot entries, how many generations to keep, and whether to bring in dotfiles. Then it writes the config and the lock, and shows what a switch would change:

```console
what `archix switch` would change now:
snapshot 2026-10-09: 212 packages
the system matches the lock and the config

the config is in /etc/archix; read it over, then run `sudo archix switch` to make this the first generation
```

`init` changes nothing on the system except the config directory. The first `sudo archix switch` creates generation 1 without changing any packages. See the [quick start](/docs/quick-start/) for every question and what it captures.

| Flag | Description |
| --- | --- |
| `--defaults` | Take every question's default without asking. Without it, `init` needs a terminal. |

`init` refuses to run if the config directory already has an `init.lua`.

## Commands that change the system

These need root. Run without it, they stop with `` `archix switch` changes the system; run it as root (sudo archix switch) `` (with the command's own name).

### `switch`

Make this machine match the config and the lock, and record the result as a new generation.

```console
$ sudo archix switch
resolving 87 declared packages against the 2026-10-09 snapshot
+ htop 3.4.1-1
213 packages locked
wrote /etc/archix/archix.lock
install    htop 3.4.1-1
boot entry for the new generation
apply 2 changes? [Y/n]
downloading htop-3.4.1-1-x86_64.pkg.tar.zst
:: installing packages
:: removing packages
:: setting install reasons
:: updating users and groups
:: stopping and disabling services
:: writing managed files
:: reloading systemd, sysctl, udev and locales
:: enabling and starting services
:: verifying the result
:: writing the boot entry
:: making the new generation current
switched to generation 2 (snapshot 2026-10-09)
```

`switch` re-locks at the locked date if the config changed, lists the package, file, user, group and service changes, and asks before applying them. Then it builds AUR packages in a clean chroot, downloads every package to `/var/cache/archix/pkg` (each file checked against the lock's `sha256`) and applies the steps above from a [journal](/docs/concepts/safety/#the-journal).

If the system already matches, it prints `nothing to do: the system matches generation 2 (snapshot 2026-10-09)`. If only settings changed, such as `ignored_packages`, it records a new generation without touching packages: `generation 3 is current (snapshot 2026-10-09, no package changes)`.

`switch` refuses to start while pacman is running, while a stale pacman lock (`/var/lib/pacman/db.lck`) exists, or while an earlier switch is unfinished. If installed packages are newer than the lock because of pacman or an AUR helper, it offers to adopt them first:

```console
the system is newer than the lock for 3 packages (linux, mesa, vulkan-radeon), probably from pacman or an AUR helper
adopt them into the lock? (no reverts them to the lock) [Y/n]
```

See [pacman and yay](/docs/concepts/pacman-and-yay/#adopting) and [Safety](/docs/concepts/safety/).

| Flag | Description |
| --- | --- |
| `--noconfirm` | Don't ask. Applies without the `apply N changes?` question, adopts newer packages, trusts new AUR build recipes without showing them, falls back when a third-party package is gone, and takes the nearest newer version when a locked AUR version can't be built. |
| `--accept-nearest newer\|older` | When a locked AUR version can't be built, take the nearest newer or older one without asking. Overrides the `missing_versions` option. |

### `upgrade`, `-Syu`

The yay-style full upgrade. Bare `archix` and `archix -Syu` do the same.

```console
$ sudo archix -Syu
$ sudo archix
```

In order, it:

1. runs `pacman -Syu` against your live mirrors;
2. adopts the result into the lock, without asking;
3. moves AUR packages to the newest commit of their build recipes, showing each changed recipe for review;
4. switches, which builds the AUR updates and applies everything else.

`archix -Syu <packages>` also installs the packages, like `archix install` after the upgrade. If `pacman -Syu` fails, archix stops: `pacman -Syu failed; nothing else was changed`.

| Flag | Description |
| --- | --- |
| `--noconfirm` | Passed to `pacman -Syu`, and skips archix's own questions as in `switch`. |

`-Sy` on its own is refused, because archix never syncs databases without upgrading: ``archix doesn't sync databases on their own; use -Syu to upgrade, or `archix update` to move the lock``. For a whole, consistent day of the Arch Linux Archive instead of whatever your mirror has, use [`update`](#update) and `switch`.

### `install`, `-S`

Declare packages and install them.

```console
$ sudo archix install zen-browser-bin
$ sudo archix -S neovim ripgrep
```

The names go into `lua/hosts/<host>/installed.lua`, a file archix owns and imports automatically, and then archix switches. Names that aren't in your configured repos are looked up in the AUR, and new AUR build recipes are shown for review. If you abort, the switch fails or the config stops evaluating, `installed.lua` is put back as it was.

| Flag | Description |
| --- | --- |
| `--path <dir>` | Edit the `installed.lua` in this directory instead. It must be inside the config's `lua/` directory, or archix refuses. archix prints the module name to import, such as `modules.dev.installed`, since only the host's own `installed.lua` is imported automatically. |
| `--temp` | Install without declaring anything. The package shows up as undeclared, and the next `switch` removes it unless you declare it by then. |
| `--noconfirm` | Don't ask. |

```console
$ sudo archix install --temp sl
install    sl 5.05-4
install 1 packages without declaring them? [Y/n]
...
installed sl without declaring it; the next `archix switch` removes it unless it's declared
```

### `remove`, `-R`

Take packages out of `installed.lua` and uninstall them.

```console
$ sudo archix remove zen-browser-bin
$ sudo archix -R neovim
```

archix only edits `installed.lua`, never your own Lua. A package declared elsewhere is refused: `htop isn't in /etc/archix/lua/hosts/desktop/installed.lua; it's declared elsewhere in the config, so remove it there`.

| Flag | Description |
| --- | --- |
| `--path <dir>` | Edit the `installed.lua` in this directory instead. |
| `--noconfirm` | Don't ask. |

### `rollback`

Put back the packages, files, users, groups and services of an earlier generation: the previous one, or the generation you name.

```console
$ sudo archix rollback
downgrade  mesa 1:25.3.1-1 -> 1:25.3.0-1
downgrade  vulkan-radeon 1:25.3.1-1 -> 1:25.3.0-1
roll back to generation 41 (snapshot 2026-10-09) with 2 changes? [Y/n]
:: installing packages
...
:: making the new generation current
rolled back to generation 41 (snapshot 2026-10-09); archix.lock is unchanged, so the next `archix switch` returns to the config
```

Packages are reinstalled at the generation's exact versions, from the cache when they're there and from the Arch Linux Archive otherwise. `rollback` doesn't evaluate the config, so it works even when the config is broken. `archix rollback 42` moves forward again. It runs through the same journal as a switch, and refuses while a switch is unfinished; use `archix recover --rollback` for that. See [Generations and rollback](/docs/concepts/generations/).

| Argument or flag | Description |
| --- | --- |
| `[generation]` | The generation number. `archix generations` lists them. |
| `--noconfirm` | Don't ask. |

### `gc`

Delete old generations and the cached files no kept generation needs: package files, snapshot databases, managed-file contents, boot entries, preserved kernel modules and btrfs snapshots. It also runs after every switch.

```console
$ sudo archix gc
deleted generations 30, 31
freed 812.4 MiB: 46 cached package files, 3 snapshot databases
```

It keeps the current generation, the newest [`generations.keep`](/docs/reference/options/) (10) and every generation younger than `generations.keep_days` (30), using the limits the current generation was applied with.

### `recover`

Deal with a switch that was interrupted: continue it, roll it back or abandon it. Shows what was interrupted and at which step, then asks which one you want.

```console
$ sudo archix recover
the switch to generation 43 (snapshot 2026-10-15) (started at unix time 1792051200) was interrupted while installing packages
[c]ontinue it, [r]oll back, [a]bandon (close the journal without changes), or [q]uit?
```

Before it does anything, `recover` checks that pacman isn't running and removes a stale pacman lock. With no unfinished switch, it prints `nothing to recover: no switch is unfinished`. It doesn't evaluate the config.

The flags skip the question. Only one can be given.

| Flag | Description |
| --- | --- |
| `--continue` | Finish the interrupted switch by rerunning the steps that hadn't completed. |
| `--rollback` | Return to the packages, files, users and services from before the interrupted switch. |
| `--abandon` | Close the journal without changing anything, after showing what state the system is in: the differences from the switch's target, and `pacman -Qk` on the packages it touched. |

```console
$ sudo archix recover --continue
finished the interrupted switch to generation 43 (snapshot 2026-10-15)
```

### `rescue`

Restore a generation's core packages under a root directory without pacman, for a system pacman can no longer repair. The rescue boot entries run it from the initramfs; you can also run it from a live USB.

```console
# /mnt/usr/lib/archix/archix-static rescue --root /mnt
restoring the core packages of generation 42 under /mnt
restored glibc 2.42+r17+gd7274d718e6f-1
restored pacman 7.0.0.r7.g1f38429-1
...
36 packages restored; boot normally, then run `sudo archix rollback 42` to put everything else back
```

It unpacks glibc, pacman, systemd, the kernel, archix itself and the rest of the core set straight from the cache, keeps edited config files (the package's version is saved as `.pacnew`) and rewrites their entries in pacman's database. See [Safety](/docs/concepts/safety/#rescue-mode).

| Flag | Default | Description |
| --- | --- | --- |
| `--root <dir>` | `/` | The mounted system to repair, such as `/mnt`. |
| `--generation <n>` | the current one | The generation whose packages to restore. |
| `--package <name>` | | Also restore this package, beyond the core set. Repeat it for more. |

## Commands that read the system or the config

These don't change the system and don't need root. As a normal user, archix keeps its download cache in `~/.cache/archix` (or `$XDG_CACHE_HOME/archix`); as root it uses `/var/cache/archix`.

### `diff`

Show what a switch would change to make this machine match the lock and the config.

```console
$ archix diff
snapshot 2026-10-15: 214 packages
install    htop 3.4.1-1
upgrade    mesa 1:25.3.0-1 -> 1:25.3.1-1
downgrade  linux 7.2.9.arch1-1 -> 7.2.8.arch1-1
remove     libnl-legacy 1.1.4-1 (not in the lock)
mark       python-pip as a dependency
create     /etc/sysctl.d/80-swappiness.conf
update     /etc/issue (edited outside archix; your version gets saved)
enable     sshd.service
```

When everything matches, it prints `the system matches the lock and the config`. If installed packages are newer than the lock, it says so first: ``3 packages are newer than the lock; `archix switch` will offer to adopt them instead of downgrading``. If the lock is out of date with the config, `diff` resolves a fresh lock in memory, prints `note: archix.lock is out of date for desktop; showing a fresh resolution`, and shows the result without saving it. Packages in `ignored_packages` never appear. With `undeclared_packages = "warn"`, undeclared packages are listed on their own line instead of as removals:

```console
not declared, kept because undeclared_packages = "warn": htop ncdu
```

### `status`

Show any unfinished switch, the current generation, changes made outside archix, the lock and pending changes. It works while a switch is unfinished.

```console
$ archix status
no unfinished switch
current generation: 41 (snapshot 2026-10-09)
2 packages were changed outside archix in 1 transactions since the last switch
desktop is locked to snapshot 2026-10-09 (212 packages)
2 changes pending; see `archix diff`
```

When everything is in place, the last line is `the system matches the lock`. When a switch is unfinished, the first lines say which one and at which step, and tell you to run `archix recover`. If the config changed since the lock was written, it adds ``the config changed since the lock was written; `archix lock` or `archix switch` will re-resolve``.

### `generations`

List generations. The current one is marked with `*`.

```console
$ archix generations
    40  2026-10-02 18:40 UTC  snapshot 2026-10-02  210 packages  config 8c1f0e2a7b
    41  2026-10-09 09:12 UTC  snapshot 2026-10-09  212 packages  config 3f2a9c1e07
*   42  2026-10-15 08:55 UTC  snapshot 2026-10-15  214 packages  config 5d7b21c4e9+uncommitted
```

`+uncommitted` marks a generation switched from a config with uncommitted changes. `generations` doesn't evaluate the config.

### `search`, `-Ss`

Search the locked snapshot's repos, including your third-party repos, and the AUR. Every term has to match the name or the description.

```console
$ archix search zen browser
aur/zen-browser-bin 1.16.3b-1 (+412 18.53)
    Zen Browser, a Firefox-based browser focused on privacy
$ archix -Ss wezterm
extra/wezterm 20240203.110809.5046fc22-3 [installed]
    GPU-accelerated cross-platform terminal emulator and multiplexer written in Rust
```

Repo results come first, then AUR results, most popular first, with their votes and popularity. `[installed]` marks installed packages and `(out of date)` flagged AUR packages.

### `lock`

Resolve the config into `archix.lock`, keeping the host's snapshot date and the locked AUR commits.

```console
$ archix lock
resolving 87 declared packages against the 2026-10-09 snapshot
+ htop 3.4.1-1
213 packages locked
wrote /etc/archix/archix.lock
```

If the lock already matches, it prints `archix.lock is up to date for desktop (snapshot 2026-10-09)` and changes nothing. New AUR build recipes are shown for review before they're locked.

| Flag | Description |
| --- | --- |
| `--noconfirm` | Trust new AUR build recipes without showing them. |

### `update`

Move the host's lock to the newest archive snapshot, or to a date you choose, move AUR packages to their newest commits, and re-resolve. It applies nothing.

```console
$ archix update
resolving 87 declared packages against the 2026-10-15 snapshot
snapshot 2026-10-09 -> 2026-10-15
~ linux 7.2.8.arch1-1 -> 7.2.9.arch1-1
~ mesa 1:25.3.0-1 -> 1:25.3.1-1
+ libdisplay-info 0.3.0-1
- libnl-legacy 1.1.4-1
214 packages locked
wrote /etc/archix/archix.lock
```

`+` is a new package, `~` a changed version and `-` a package that's no longer needed. Changed AUR build recipes are shown as a diff for review in between.

| Flag | Description |
| --- | --- |
| `--to YYYY-MM-DD` | Snapshot date to move to, instead of the newest. Works in both directions. |
| `--noconfirm` | Trust new AUR build recipes without showing them. |

Follow it with `sudo archix switch` to apply. See [The lockfile](/docs/concepts/lockfile/).

`lock` and `update` write to the config directory, so you need write access to it: `sudo` for a root-owned `/etc/archix`, nothing for a config in your own repo.

### `eval`

Print the evaluated config as JSON. Useful to check what your modules merged into, and to script against.

```console
$ archix eval | jq .repos
[
  "core",
  "extra",
  "multilib"
]
```

### `options`

List every declared option: its path, type, default, description and the file that declared it.

```console
$ archix options
ignored_packages
    type: list of str
    default: []
    Installed packages archix leaves alone and never reports as undeclared.
    declared in <archix>/packages.lua
packages
    type: list of str
    default: []
    Packages to install: from the configured repositories, else from the AUR. Their dependencies are installed too and recorded in archix.lock.
    declared in <archix>/packages.lua
...
```

| Flag | Description |
| --- | --- |
| `--json` | Print a JSON array of `{ path, type, default, description, declared_in }` objects instead. |

## Internal commands

Two hidden commands are run by files archix installs, not by you: `archix hook-record`, from the pacman hook, and `archix boot-check`, from `archix-boot.service` at boot.

## Exit behaviour

- `0` when the command succeeded.
- `1` on errors, with each error printed to stderr as `error: ...`. That includes config errors (every one is reported, not just the first), resolution failures, a declined prompt and failures before apply starts, which leave the system untouched.
- Non-zero after a switch or rollback with soft failures: the new generation is current, and the services and reloads that failed are listed. See [Safety](/docs/concepts/safety/#hard-and-soft-failures).
- Non-zero when a hard failure stopped a switch. The journal stays open until you run `archix recover`.
- While a switch is unfinished, `switch`, `upgrade`, `install`, `remove`, `rollback`, `lock`, `update` and `gc` refuse to run and exit `1`, naming the interrupted switch and pointing to `archix recover`.

Piping output works as expected: `archix diff | head` exits quietly when `head` closes the pipe.
