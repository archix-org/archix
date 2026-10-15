---
title: pacman and yay
description: How archix handles packages installed or upgraded outside it, how adopting works, the pacman-style commands, and how undeclared and ignored packages are treated.
sidebar:
  order: 4
---

archix doesn't lock you out of your own system. `pacman` and `yay` keep working, on your live mirrors, exactly as before. You can `pacman -Syu` at 2 a.m. to get a fix, `pacman -S` something to try it, or follow an Arch news item to the letter.

What archix adds is memory. It notices those changes and, on the next switch, asks what you want to do with them: adopt upgrades into the lock, and declare new packages in your config.

## The pacman hook

archix installs `/etc/pacman.d/hooks/archix.hook`, which runs `archix hook-record` after every pacman transaction. archix's own pacman runs are marked, so only changes made outside archix are recorded, in `/var/lib/archix/outside-changes.json`:

```console
$ sudo pacman -Syu
...
:: Running post-transaction hooks...
(1/1) Recording package changes for archix...
archix: recorded 3 packages changed outside archix; `archix switch` will offer to adopt them
```

The hook never fails or changes a pacman transaction; it only takes notes. `archix status` reports them, and the next successful switch clears them.

## Adopting

On the next `archix switch`, if installed packages are newer than the lock, archix asks:

```console
$ sudo archix switch
the system is newer than the lock for 3 packages (linux, mesa, vulkan-radeon), probably from pacman or an AUR helper
adopt them into the lock? (no reverts them to the lock) [Y/n]
```

- **Yes** moves the lock to match the system. The switch then proceeds from there, and the upgrade you did with pacman becomes part of a generation you can roll back.
- **No** reverts the system to the lock. Packages go back to their locked versions.

`--noconfirm` adopts. `archix diff` mentions it ahead of time: ``3 packages are newer than the lock; `archix switch` will offer to adopt them instead of downgrading``.

Adopting re-resolves your config at the lock's date and at the days around your manual upgrade, and keeps the snapshot that matches the most installed versions. Live mirrors don't update in lockstep with the archive, so a `pacman -Syu` rarely matches one day exactly. Anything still newer is [pinned](/docs/concepts/lockfile/#pins) to its exact installed version, fetched from the Arch Linux Archive's per-package history and listed under `pinned`. Nothing is downgraded.

```console
adopt them into the lock? (no reverts them to the lock) [Y/n] y
snapshot 2026-10-09 -> 2026-10-15
~ linux 7.2.8.arch1-1 -> 7.2.9.arch1-1
~ mesa 1:25.3.0-1 -> 1:25.3.1-1
~ vulkan-radeon 1:25.3.0-1 -> 1:25.3.1-1
214 packages locked
wrote /etc/archix/archix.lock
```

AUR packages upgraded by yay or paru are adopted too: archix finds the commit whose build recipe builds the installed version, and locks that.

Pins survive `archix lock` at the same date. `archix update` to a new snapshot drops them.

## The pacman-style commands

archix is also an AUR helper, with the flags you already type:

| Command | Same as |
| --- | --- |
| `archix -S <pkg>` | `archix install <pkg>` |
| `archix -Ss <terms>` | `archix search <terms>` |
| `archix -R <pkg>` | `archix remove <pkg>` |
| `archix -Syu` | `archix upgrade` |
| `archix -Syu <pkg>` | `archix upgrade`, also installing `<pkg>` |
| `archix` | `archix upgrade` |

`-Sy` on its own, and `-S` or `-Ss` without arguments, fail with a hint: archix never syncs databases without upgrading. See the [CLI reference](/docs/reference/cli/#install--s) for every flag.

### `archix -Syu`

If you want the yay experience without the question, use the full upgrade:

```console
$ sudo archix -Syu
```

It runs `pacman -Syu` from your live mirrors, adopts the result into the lock without asking, moves AUR packages to the newest commit of their build recipes (showing each changed recipe for review), and switches. Running `archix` with no arguments does the same, the way `yay` does.

Compare it with `archix update`, which moves the lock to the newest archive snapshot and applies nothing until you `archix switch`. Both get you current. `-Syu` is the newest your mirror has; `update` is a whole, consistent day of the archive.

### `installed.lua`

`archix install` declares before it installs. The package name goes into `lua/hosts/<host>/installed.lua`, a file archix owns:

```lua title="lua/hosts/desktop/installed.lua"
-- Managed by `archix install` and `archix remove`.
return {
  packages = {
    "htop",
    "zen-browser-bin",
  },
}
```

archix imports it automatically whenever it exists, so you don't have to. `archix remove` takes names out of it. archix never rewrites your own Lua: a package you declared elsewhere is refused by `remove`, with a pointer to where it's declared. `archix init` puts the packages it captures in this file, so `install` and `remove` work on them from day one.

`archix install --path <dir>` edits the `installed.lua` in another directory under `lua/`, for example a module shared by several hosts. Import it yourself; archix prints the name to use.

## Undeclared packages

Adopting covers packages archix already tracks. It doesn't cover a package you `pacman -S`'d that isn't in your config at all. It's installed, but it isn't in the lock. The [`undeclared_packages`](/docs/reference/options/) option decides what happens to it, and `archix init` asks you to choose: `Later, when you install something with plain pacman or yay`.

**`"remove"`** is the default: `switch offers to declare it, and removes it if you say no`. The next switch lists the explicitly installed packages the pacman hook saw arrive since the last switch, and asks:

```console
$ sudo pacman -S htop ncdu
...
$ sudo archix switch
installed with pacman or an AUR helper but not declared: htop ncdu
declare them in installed.lua? (no removes them) [Y/n]
```

- **Yes** adds them to [`installed.lua`](#installedlua) and switches with them in the lock, the same as if you'd run `archix install`.
- **No** removes them.

`--noconfirm` declares them. Anything else that isn't in the lock, such as a package installed before the hook could see it or one that `archix install --temp` installed, is removed on switch. The system is exactly what the config says, which is the guarantee that makes rollback and a fresh reinstall meaningful.

This covers packages only. Changes to `/etc` files and services that archix never managed are left alone; see [Safety](/docs/concepts/safety/#managed-files-backups-and-hand-edits).

**`"warn"`** is `switch leaves it installed and only lists it`. Undeclared packages are listed in `archix diff` and `archix switch` instead:

```lua title="lua/hosts/desktop/init.lua"
return {
  undeclared_packages = "warn",
}
```

```console
$ archix diff
snapshot 2026-10-15: 214 packages
the system matches the lock and the config
not declared, kept because undeclared_packages = "warn": htop ncdu
```

To keep one, add it to `packages` or run `archix install htop`. To get rid of it, remove it with pacman or switch back to `"remove"`.

To try something without declaring it, `sudo archix install --temp <pkg>` installs it as undeclared on purpose. archix ran that transaction itself, so the hook doesn't record it and switch doesn't offer to declare it: the next switch removes it unless you've declared it by then.

## Ignoring packages everywhere

Some packages you manage by hand on purpose. List them in [`ignored_packages`](/docs/reference/options/), and archix never touches or reports them, whatever `undeclared_packages` says:

```lua title="lua/modules/common.lua"
return {
  ignored_packages = { "nvidia-dkms-custom" },
}
```

Because it's in your config, the decision is visible, committed and the same on every machine that imports the module. `archix init` lists installed packages that are in neither the repos nor the AUR here, and, if you choose, orphaned dependencies.

## Third-party repos in `pacman.conf`

So that plain pacman sees the same repos as archix, archix keeps your declared [third-party repos](/docs/guides/third-party-repos/) in `/etc/pacman.conf`, inside marked blocks around the official repos:

```ini title="/etc/pacman.conf"
[options]
HoldPkg = pacman glibc
Architecture = auto

# BEGIN archix custom repos (before the official repos)
[cachyos]
SigLevel = Required DatabaseOptional
Server = https://mirror.cachyos.org/repo/$arch/$repo

# END archix custom repos (before the official repos)
[core]
Include = /etc/pacman.d/mirrorlist

[extra]
Include = /etc/pacman.d/mirrorlist

# BEGIN archix custom repos (after the official repos)
[chaotic-aur]
SigLevel = Required DatabaseOptional
Server = https://cdn-mirror.chaotic.cx/$repo/$arch

# END archix custom repos (after the official repos)
```

Repos with `before_official = true` go in the first block, the rest in the second. archix rewrites only these blocks, after each successful switch. A hand-written section for a repo your config declares is taken over, so it's never listed twice. Removing a repo from your config removes it from `pacman.conf`. Everything else in the file, including the official repos and their mirrorlists, is yours.

## When a switch is unfinished

If an archix switch was interrupted, `switch`, `lock`, `update` and the other commands that change things refuse to run until you deal with it with `archix recover`. Read-only commands like `diff` and `status` still work, and so do pacman and yay, but the hook warns you after every transaction that a switch is unfinished. See [Safety](/docs/concepts/safety/#unfinished-switches).
