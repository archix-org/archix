---
title: pacman and yay
description: How archix handles packages installed or upgraded outside it, how adopting works, and how undeclared and ignored packages are treated.
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

## Undeclared packages

Adopting covers packages archix already tracks. It doesn't cover a package you `pacman -S`'d that isn't in your config at all. It's installed, but it isn't in the lock. The [`undeclared_packages`](/docs/reference/options/#undeclared_packages) option decides what happens to it, and onboarding asks you to choose.

**`"remove"`** is the default. On switch, packages that aren't in the lock are removed. The system is exactly what the config says, which is the guarantee that makes rollback and a fresh reinstall meaningful. The same goes for undeclared changes to `/etc` files, which are reverted, and undeclared enabled services, which are disabled.

**`"warn"`** keeps them and lists them in `archix diff` and `archix switch` instead:

```lua title="lua/hosts/desktop.lua"
return {
  undeclared_packages = "warn",
}
```

```console
$ archix diff
snapshot 2026-10-15: 1214 packages
the system matches the lock
not declared, kept because undeclared_packages = "warn": htop ncdu
```

To keep one, add it to `packages` or run `archix install htop`. To get rid of it, remove it with pacman or switch back to `"remove"`.

`"warn"` is a good way to start while you move an existing system into your config. Switch back to `"remove"` once that line is gone.

## Ignoring packages everywhere

Some packages you manage by hand on purpose. List them in [`ignored_packages`](/docs/reference/options/#ignored_packages), and archix never touches or reports them, whatever `undeclared_packages` says:

```lua title="lua/modules/common.lua"
return {
  ignored_packages = { "nvidia-dkms-custom" },
}
```

Because it's in your config, the decision is visible, committed and the same on every machine that imports the module.

## "Don't ask again"

When archix asks about something undeclared, you can tell it not to ask again. That decision is stored only on that machine, in `/var/lib/archix`. It isn't part of your config, so it doesn't follow you to your other machines or to a reinstall.

That's on purpose: a one-off decision on one machine shouldn't silently change every machine. To ignore a package everywhere, use `ignored_packages`.

## When a switch is unfinished

If an archix switch was interrupted, `switch`, `lock` and `update` refuse to run until you deal with it with `archix recover`. Read-only commands like `diff` and `status` still work, and so do pacman and yay, but the hook warns you that a switch is unfinished. See [Safety](/docs/concepts/safety/#unfinished-switches).
