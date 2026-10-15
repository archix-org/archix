---
title: CLI reference
description: Every archix subcommand, pacman-style alias and flag, with examples and exit behaviour.
sidebar:
  order: 1
---

```
archix [--config <dir>] [--host <name>] <command> [flags]
```

Running `archix` with no command does a full update, like `yay`. See [`archix -Syu`](#-syu).

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

## Commands that change the system

These need root.

### `switch`

Make this machine match the config and the lock, and create a new generation.

```console
$ sudo archix switch
resolving 86 declared packages against the 2026-10-14 snapshot
+ htop 3.4.1-1
1213 packages locked
wrote /etc/archix/archix.lock
install    htop 3.4.1-1
apply 1 changes? [Y/n]
:: installing packages
:: removing packages
:: setting install reasons
:: verifying the result
switched to snapshot 2026-10-14
```

`switch` locks first if the config changed, lists the changes and asks before applying them. Then it prepares everything (downloads, AUR builds, rendered files, dependency checks) and only then applies. Downloads go to `/var/cache/archix/pkg`, and every file is checked against the lock's `sha256` before it's used. If the system already matches, it prints `nothing to do: the system matches snapshot 2026-10-14`.

`switch` refuses to start while pacman is running, while a stale pacman lock (`/var/lib/pacman/db.lck`) exists, or while an earlier switch is unfinished. If the system is ahead of the lock because of pacman or yay, it asks `System is newer than the lock: adopt? [Y/n]`. See [Safety](/docs/concepts/safety/) and [pacman and yay](/docs/concepts/pacman-and-yay/).

| Flag | Description |
| --- | --- |
| `--noconfirm` | Don't ask for confirmation. Applies without the `apply N changes?` question, adopts out-of-band changes, and accepts the nearest newer version when a locked one is missing. |
| `--accept-nearest=newer\|older` | When a locked version can't be restored, take the nearest newer or older one without asking. |

### `rollback`

Return the system to the previous generation, using cached packages and files.

```console
$ sudo archix rollback
```

Rollback changes the system, not your config. The next `switch` applies your config again. See [Generations and rollback](/docs/concepts/generations/).

| Flag | Description |
| --- | --- |
| `--root <dir>` | Operate on a system mounted at `<dir>`. Meant for the static rescue binary from a live USB: `archix rollback --root /mnt`. |

### `recover`

Deal with a switch that was interrupted: continue it, roll it back or abandon it. Shows what was interrupted and at which step, then asks which one you want.

```console
$ sudo archix recover
the switch to snapshot 2026-10-15 (started at unix time 1792051200) was interrupted while installing packages
[c]ontinue it, [r]oll back, [a]bandon (close the journal without changes), or [q]uit?
```

Before it does anything, `recover` checks that pacman isn't running and removes a stale pacman lock. With no unfinished switch, it prints `nothing to recover: no switch is unfinished`.

The flags skip the question. Only one can be given.

| Flag | Description |
| --- | --- |
| `--continue` | Finish the interrupted switch by rerunning the steps that hadn't completed. |
| `--rollback` | Return to the packages from before the interrupted switch. |
| `--abandon` | Close the journal without changing anything, after showing what state the system is in: the differences from the switch's target, and `pacman -Qk` on the packages it touched. |

```console
$ sudo archix recover --continue
finished the interrupted switch to snapshot 2026-10-15
```

### `gc`

Remove generations outside the configured limits, their boot entries and btrfs snapshots, and package files no kept generation references. Runs automatically after every switch.

```console
$ sudo archix gc
```

### `init`

The onboarding TUI. Captures the running system into a new config, then makes the first switch. See the [quick start](/docs/quick-start/).

```console
$ sudo archix init
```

## Package commands

archix is also your AUR helper. These commands search the official repos, your declared third-party repos and the AUR. See [The AUR](/docs/guides/aur/).

### `install`, `-S`

Add packages to the config and install them.

```console
$ sudo archix install zen-browser-bin
$ sudo archix -S neovim ripgrep
```

The names go into an archix-owned data file under `lua/hosts/` that your config merges in. AUR packages show their PKGBUILD for review on first install.

| Flag | Description |
| --- | --- |
| `--path <dir>` | Write to a data file in this directory instead. Fails if it isn't a valid directory. |
| `--noconfirm` | Don't prompt. |

### `remove`, `-R`

Remove packages from archix's data file and from the system.

```console
$ sudo archix remove zen-browser-bin
$ sudo archix -R neovim
```

archix only edits its own data files. A package you declared by hand in your Lua stays until you remove it there.

### `search`, `-Ss`

Search package names and descriptions, offline, in the sync databases and the AUR metadata.

```console
$ archix search wezterm
$ archix -Ss wezterm
```

### `-Syu`

Update from the live mirrors, like `pacman -Syu`, then adopt the result into the lock. Bare `archix` does the same.

```console
$ sudo archix -Syu
$ sudo archix
```

This is the quick way to get the newest packages your mirror has. For a whole, consistent day of the Arch Linux Archive instead, use `archix update` and `archix switch`.

## Commands that only read or write the config

These don't change the system and don't need root. As a normal user, archix keeps its download cache in `~/.cache/archix` (or `$XDG_CACHE_HOME/archix`); as root it uses `/var/cache/archix`.

### `diff`

Show what a switch would change to make this machine match the lock.

```console
$ archix diff
snapshot 2026-10-15: 1214 packages
install    htop 3.4.1-1
upgrade    mesa 1:25.3.0-1 -> 1:25.3.1-1
downgrade  linux 6.17.2.arch1-1 -> 6.17.1.arch1-1
remove     libnl-legacy 1.1.4-1 (not in the lock)
mark       python-pip as a dependency
```

When everything matches, it prints `the system matches the lock`. If the lock is out of date with the config, `diff` resolves a fresh lock in memory, prints `note: archix.lock is out of date for desktop; showing a fresh resolution`, and shows the result without saving it. Packages in `ignored_packages` never appear. With `undeclared_packages = "warn"`, undeclared packages are listed on their own line instead of as removals:

```console
not declared, kept because undeclared_packages = "warn": htop ncdu
```

### `lock`

Resolve the config into `archix.lock`, keeping the host's snapshot date.

```console
$ archix lock
resolving 86 declared packages against the 2026-10-14 snapshot
+ htop 3.4.1-1
1215 packages locked
wrote /etc/archix/archix.lock
```

If the lock already matches, it prints `archix.lock is up to date for desktop (snapshot 2026-10-14)` and changes nothing.

### `update`

Move the host's lock to the newest archive snapshot, or to a date you choose, and re-resolve.

```console
$ archix update
resolving 86 declared packages against the 2026-10-15 snapshot
snapshot 2026-10-14 -> 2026-10-15
~ mesa 1:25.3.0-1 -> 1:25.3.1-1
1215 packages locked
wrote /etc/archix/archix.lock
```

| Flag | Description |
| --- | --- |
| `--to YYYY-MM-DD` | Snapshot date to move to, instead of the newest. Works in both directions, so it also moves a host back to an older day. |

```console
$ archix update --to 2026-10-01
```

Follow it with `sudo archix switch` to apply. See [The lockfile](/docs/concepts/lockfile/).

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
    Packages from the official repositories. Their dependencies are installed too and recorded in archix.lock.
    declared in <archix>/packages.lua
...
```

| Flag | Description |
| --- | --- |
| `--json` | Print a JSON array of `{ path, type, default, description, declared_in }` objects instead. |

### `status`

Show the lock, pending changes and any unfinished switch. It doesn't need root, and it works while a switch is unfinished.

```console
$ archix status
no unfinished switch
desktop is locked to snapshot 2026-10-14 (1213 packages)
the config changed since the lock was written; `archix lock` or `archix switch` will re-resolve
3 changes pending; see `archix diff`
```

When everything is in place, the last line is `the system matches the lock`. When a switch is unfinished, the first lines say which one and at which step, and tell you to run `archix recover`.

## Exit behaviour

- `0` when the command succeeded.
- `1` on errors, with each error printed to stderr as `error: ...`. That includes config errors (every one is reported, not just the first), resolution failures and failures in the prepare phase, which leave the system untouched.
- Non-zero after a switch with soft failures: the new generation is kept, and the units that failed to start are listed.
- Non-zero when a hard failure stopped a switch. The journal stays open until you run `archix recover`.
- While a switch is unfinished, `switch`, `lock` and `update` refuse to run and exit `1`, naming the interrupted switch and pointing to `archix recover`.
- `switch` and `recover` exit `1` when not run as root: `` `archix switch` changes the system; run it as root (sudo archix switch) ``.

Piping output works as expected: `archix diff | head` exits quietly when `head` closes the pipe.
