---
title: Safety and recovery
description: How archix prepares before it applies, journals every step, and recovers from failed or interrupted switches without a live USB.
sidebar:
  order: 5
---

The goal is simple: no failure during a switch, including pulling the power cord halfway through, should ever need a live USB. This page explains how archix gets there.

## Prepare, then apply

`archix switch` runs in two phases.

**Prepare** does everything that can fail without touching the system:

- evaluates the config,
- resolves the lock,
- downloads every package,
- builds every AUR package,
- renders every managed file,
- checks pacman dependencies and conflicts.

If anything fails here, a typo, a network error, an AUR build that won't compile, a file conflict, archix stops and the system is exactly as it was. Most problems surface in this phase.

**Apply** changes the system: the package transaction, files, users, units, boot entries. By the time it starts, everything it needs is already on disk.

Packages are downloaded to `/var/cache/archix/pkg`, and every file is checked against the `sha256` in the lock before it's used. Files already in pacman's own cache are reused. A file that doesn't match its checksum stops the switch in prepare.

```console
$ sudo archix switch
upgrade    firefox 144.0-1 -> 144.0.1-1
upgrade    mesa 1:25.3.0-1 -> 1:25.3.1-1
apply 2 changes? [Y/n]
:: installing packages
:: removing packages
:: setting install reasons
:: verifying the result
switched to snapshot 2026-10-15
```

archix also checks pacman before it starts. If pacman is running, or a `db.lck` is left over with no pacman running, `switch` refuses and tells you why, rather than fight another package manager.

## The journal

Before apply starts, archix writes `/var/lib/archix/journal.json`. It records the target, the package set from before the switch, the exact package transaction and the list of steps. Each step is marked done as it completes. The package steps are installing packages, removing packages, setting install reasons and verifying the result.

Every step is idempotent. A step doesn't mean "upgrade mesa from A to B"; it means "make the system match the target". Running a step twice is harmless, which is what makes it safe to continue an interrupted switch from wherever it stopped.

When every step is done, the journal is deleted. A journal on disk always means a switch is unfinished.

## Atomic writes

Managed files are written to a temporary file and renamed into place. A rename is atomic, so a file is either the old version or the new one, never half of each.

The two things that decide what "current" means only change at the very end, after every step has succeeded:

- the current-generation pointer,
- the default boot entry.

Until then, a reboot lands in the previous generation.

On btrfs, archix also takes a snapshot right before apply. That snapshot becomes the generation's rollback point, and it's cleaned up under the normal [generation limits](/docs/concepts/generations/#limits-and-gc).

## Hard and soft failures

Not every failure during apply means the same thing.

**Hard failures** stop the switch: a pacman error, a file that can't be written, a user that can't be created, a verify step that finds the system doesn't match the target. The journal stays open, and archix tells you to run `archix recover`:

```console
:: installing packages
error: installing packages failed; the switch is unfinished, run `archix recover`: ...
```

**Soft failures** don't: a systemd unit that fails to start. The new generation is kept, archix lists the failed units and exits non-zero, like NixOS does. A broken unit is usually a config problem you fix and switch again, and undoing the whole generation for it would be worse.

```console
$ sudo archix switch
apply     generation 43 is live
warning: 1 unit failed to start:
  nginx.service
  see `journalctl -u nginx.service`
```

## Unfinished switches

archix never acts on an unfinished switch automatically. Guessing wrong after a crash could make things worse, so a human decides.

- Unattended runs, `--noconfirm`, and the boot-time service report the unfinished switch, print cleanup instructions and change nothing.
- `switch`, `lock` and `update` refuse to run, and say which switch is unfinished. Read-only commands such as `diff` and `status` keep working, so you can look around first.
- pacman and yay keep working. The pacman hook only warns.
- You're told at login, by a message from `/etc/profile.d` and a desktop notification.

```console
$ archix status
the switch to snapshot 2026-10-15 (started at unix time 1792051200) was interrupted while installing packages
run `archix recover` to deal with it
desktop is locked to snapshot 2026-10-15 (1214 packages)
12 changes pending; see `archix diff`
```

## `archix recover`

`recover` shows what was interrupted and at which step, then offers three ways out:

- **continue** reruns the steps that hadn't completed and finishes the switch.
- **rollback** returns to the package set from before the switch.
- **abandon** changes nothing. It shows how the installed packages differ from the switch's target, runs `pacman -Qk` on the packages the switch touched, then closes the journal, so you see the real state of the system and fix it by hand.

```console
$ sudo archix recover
the switch to snapshot 2026-10-15 (started at unix time 1792051200) was interrupted while installing packages
[c]ontinue it, [r]oll back, [a]bandon (close the journal without changes), or [q]uit? c
finished the interrupted switch to snapshot 2026-10-15
```

To skip the prompt, pass the choice:

```console
$ sudo archix recover --continue
$ sudo archix recover --rollback
$ sudo archix recover --abandon
```

Pick continue if the interruption was something outside archix, like a power cut. Pick rollback if you don't trust the new package set. Pick abandon if you want to inspect and fix things by hand; afterwards `archix diff` shows how the system differs from the lock, and the next `archix switch` reinstalls whatever the lock needs.

A rollback is journaled too. If it gets interrupted in turn, `archix recover` picks it up the same way.

## Interrupted pacman transactions

Power loss in the middle of a pacman transaction is the classic way to break Arch: a stale lock file, half-extracted packages, a local database that disagrees with the disk.

`recover` handles this before anything else. It first makes sure pacman isn't running; if it is, it stops and asks you to wait. Then it removes the stale `/var/lib/pacman/db.lck`. If the install step was the one interrupted, continuing reinstalls every package of the transaction, even ones pacman already reports as installed, so half-extracted ones are replaced. The verify step then checks the result. Prepare already downloaded everything to `/var/cache/archix/pkg`, so continuing needs no network.

## Rescue mode

Some breakage stops pacman itself: a half-installed glibc, a broken pacman. Then nothing that relies on pacman can repair the system.

For that, every generation's initramfs carries a static archix, and each generation gets an extra boot entry:

```
  archix · generation 43 · Oct 15
  archix · generation 42 · Oct 09
› archix rescue · generation 43
```

Rescue mode unpacks that generation's core packages straight from the cache, without pacman: glibc, pacman, systemd, kernel modules and archix itself. Then it repairs pacman's local database. The normal system boots and finishes the repair with the regular tools.

This is what makes ext4 safe without a live USB. On btrfs you usually just boot the previous generation's snapshot, but rescue mode is there too.

### The last resort

The static `archix` on [GitHub Releases](https://github.com/archix-org/archix/releases) does the same from outside, against a mounted root:

```console
# ./archix rollback --root /mnt
```

You shouldn't need it. It's a fallback for accidents the built-in boot entries can't cover, not a normal recovery path.
