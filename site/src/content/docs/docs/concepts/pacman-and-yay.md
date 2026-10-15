---
title: pacman and yay
description: How archix handles packages installed or upgraded outside it, how adopting works, and what strict and warn mode do with undeclared state.
sidebar:
  order: 4
---

archix doesn't lock you out of your own system. `pacman` and `yay` keep working, on your live mirrors, exactly as before. You can `pacman -Syu` at 2 a.m. to get a fix, `pacman -S` something to try it, or follow an Arch news item to the letter.

What archix adds is memory. It notices those changes and, on the next switch, asks what you want to do with them.

## The pacman hook

archix installs a pacman `PostTransaction` hook. Whenever pacman finishes a transaction that archix didn't run, the hook records it in `/var/lib/archix`. It doesn't block or change the transaction; it only takes notes.

## Adopting

On the next `archix switch`, if the system is ahead of the lock, archix asks:

```console
$ sudo pacman -Syu
:: Synchronizing package databases...
:: Starting full system upgrade...
$ sudo archix switch
System is newer than the lock: adopt? [Y/n]
```

- **Yes** moves the lock to match the system. The switch then proceeds from there, and the upgrade you did with pacman becomes part of a generation you can roll back.
- **No** reverts the system to the lock. Packages go back to their locked versions.

`--noconfirm` adopts.

Adopting picks the ALA snapshot date closest to the pacman transaction, then records any packages that don't match that snapshot as exact-version entries. Live mirrors don't update in lockstep with the archive, so a `pacman -Syu` rarely matches one day exactly. Exact-version entries cover the difference, and every one of them can still be fetched from the ALA's `/packages/`.

```console
System is newer than the lock: adopt? [Y/n] y
snapshot 2026-10-09 -> 2026-10-15
~ linux 6.17.1.arch1-1 -> 6.17.2.arch1-1
~ mesa 1:25.3.0-1 -> 1:25.3.1-1
exact versions: 2 packages don't match the 2026-10-15 snapshot
```

## `archix -Syu`

If you want the pacman experience without the question, use the pacman-style alias:

```console
$ sudo archix -Syu
```

It updates from the live mirrors, like `pacman -Syu`, and adopts the result into the lock automatically. Running `archix` with no arguments does the same full update, the way `yay` does.

Compare it with `archix update`, which moves the lock to the newest archive snapshot without touching the live mirrors. Both get you current. `-Syu` is the newest your mirror has; `update` is a whole, consistent day of the archive.

## Strict and warn mode

Adopting covers packages archix already tracks. It doesn't cover things you added that aren't in your config at all: a package you `pacman -S`'d, an `/etc` file you edited, a service you enabled with `systemctl enable`. archix calls that undeclared state, and you choose how to treat it during onboarding.

**Strict mode** is the default. On switch, undeclared packages are removed, undeclared changes to `/etc` files are reverted, and undeclared enabled services are disabled. The system is exactly what the config says, which is the guarantee that makes rollback and a fresh reinstall meaningful.

**Warn mode** leaves undeclared state alone and lists it in `archix diff` with a hint on how to declare it:

```console
$ archix diff
snapshot 2026-10-15: 1214 packages
the system matches the lock
undeclared:
  package    htop 3.4.1-1 (explicitly installed)
  service    sshd.service (enabled)
  file       /etc/pacman.d/mirrorlist (modified)
  adopt them by declaring them in your config, or run `archix install htop` for packages
```

Warn mode is a good way to start while you move things into your config. Switch to strict once `diff` is quiet.

## "Don't ask again"

When archix asks about something undeclared, you can tell it not to ask again. That decision is stored only on that machine, in `/var/lib/archix`. It isn't part of your config, so it doesn't follow you to your other machines or to a reinstall.

That's on purpose: a one-off decision on one machine shouldn't silently change every machine. To ignore something everywhere, say so explicitly in your config, where it's visible and committed.

## When a switch is unfinished

If an archix switch was interrupted, archix refuses to run anything except `recover`, `rollback` and `status` until you deal with it. pacman and yay still work, but the hook warns you that a switch is unfinished. See [Safety](/docs/concepts/safety/).
