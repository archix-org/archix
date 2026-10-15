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

```console
$ sudo archix switch
prepare   12 packages downloaded, 1 AUR package built, 3 files rendered
apply     packages, files, users, units, boot entry
apply     generation 43 is live
```

## The journal

Before apply starts, archix writes `/var/lib/archix/journal`. It records the target generation, the exact package transaction and the list of steps. Each step is marked done as it completes.

Every step is idempotent. A step doesn't mean "upgrade mesa from A to B"; it means "make the system match generation 43". Running a step twice is harmless, which is what makes it safe to continue an interrupted switch from wherever it stopped.

## Atomic writes

Managed files are written to a temporary file and renamed into place. A rename is atomic, so a file is either the old version or the new one, never half of each.

The two things that decide what "current" means only change at the very end, after every step has succeeded:

- the current-generation pointer,
- the default boot entry.

Until then, a reboot lands in the previous generation.

On btrfs, archix also takes a snapshot right before apply. That snapshot becomes the generation's rollback point, and it's cleaned up under the normal [generation limits](/docs/concepts/generations/#limits-and-gc).

## Hard and soft failures

Not every failure during apply means the same thing.

**Hard failures** stop the switch: a pacman error, a file that can't be written, a user that can't be created. The journal stays open, and archix tells you to run `archix recover`.

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
- Every archix command except `recover`, `rollback` and `status` refuses to run.
- pacman and yay keep working. The pacman hook only warns.
- You're told at login, by a message from `/etc/profile.d` and a desktop notification.

```console
$ archix status
generation 42 (current)
unfinished switch to generation 43, stopped at step 4 of 9: install packages
run `sudo archix recover`
```

## `archix recover`

`recover` shows what was interrupted and at which step, then offers three ways out:

- **continue** finishes the switch from where it stopped.
- **rollback** returns to the previous generation.
- **abandon** closes the journal without changing anything, after verifying package files (with `pacman -Qkk`) and managed files, so you can see the real state of the system.

```console
$ sudo archix recover
Switch to generation 43 was interrupted at step 4 of 9 (install packages).
  [c]ontinue  finish the switch
  [r]ollback  return to generation 42
  [a]bandon   close the journal and show what's on disk
>
```

To skip the prompt, pass the choice:

```console
$ sudo archix recover --continue
$ sudo archix recover --rollback
$ sudo archix recover --abandon
```

Pick continue if the interruption was something outside archix, like a power cut. Pick rollback if you don't trust the new generation. Pick abandon if you want to inspect and fix things by hand.

## Interrupted pacman transactions

Power loss in the middle of a pacman transaction is the classic way to break Arch: a stale lock file, half-extracted packages, a local database that disagrees with the disk.

When recovering, archix first makes sure pacman isn't running. Then it removes the stale `db.lck`, checks the files of every package in the journaled transaction, and reinstalls the broken ones from the cache. Prepare already downloaded everything, so the cache is guaranteed to have them, even with no network.

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
