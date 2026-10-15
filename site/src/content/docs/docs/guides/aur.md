---
title: The AUR
description: Use archix as your AUR helper, with build recipe review, clean chroot builds, a build cache and every package recorded in your config.
sidebar:
  order: 1
---

archix replaces yay and paru. It searches and installs from the official repos, your third-party repos and the AUR, with the flags you already know. The difference is what happens around the install: the package is added to your config, its build recipe is shown to you and locked by commit, and it's built in a clean chroot so the result doesn't depend on what happens to be installed on your machine.

## Install, remove, search

```console
$ archix search zen browser
aur/zen-browser-bin 1.16.3b-1 (+412 18.53)
    Zen Browser, a Firefox-based browser focused on privacy
aur/zen-browser 1.16.3b-1 (+38 2.10)
    Zen Browser, built from source
$ sudo archix install zen-browser-bin
$ sudo archix remove zen-browser-bin
```

The pacman-style aliases do the same:

| Alias | Same as |
| --- | --- |
| `archix -S <pkg>` | `archix install <pkg>` |
| `archix -Ss <terms>` | `archix search <terms>` |
| `archix -R <pkg>` | `archix remove <pkg>` |
| `archix -Syu` | `archix upgrade`: `pacman -Syu`, adopt, newest AUR recipes, switch |
| `archix` | `archix -Syu` |

There's one `packages` list for everything. `install` looks a name up in your configured repos first, official and third-party, and only names they don't have are looked up in the AUR.

Search covers the locked snapshot's repos and the AUR's package list, most popular first, with votes, popularity and out-of-date flags. archix downloads that list (`packages-meta-v1.json.gz`) when it first needs it, refreshes it when it's more than an hour old and on `archix update`, and only loads it when a name isn't in the repos.

## Where `archix install` writes

`archix install` doesn't just install. It declares. The package name goes into `lua/hosts/<host>/installed.lua`, a file archix owns and imports automatically, and then archix switches. The next switch, on this machine or a fresh one, installs it again. If you abort or the switch fails, the file is put back as it was.

To put a package somewhere shared instead, pass a directory under the config's `lua/`:

```console
$ sudo archix install --path /etc/archix/lua/modules/dev rust-analyzer
note: import "modules.dev.installed" from a module this host loads, or archix won't see it
```

A directory outside `lua/` is refused, and nothing is installed.

archix only ever edits `installed.lua`. It never rewrites your Lua. So `archix remove` removes packages that `archix install` (or `archix init`) added; a package you declared by hand in `packages` is refused, and stays until you delete it from your file.

You can also skip the command and declare AUR packages in Lua directly, alongside official ones:

```lua title="lua/hosts/desktop/init.lua"
return {
  packages = { "firefox", "zen-browser-bin", "visual-studio-code-bin" },
}
```

To try a package without declaring it, `sudo archix install --temp <pkg>` installs it, built and reviewed the same way, and the next switch removes it unless you've declared it by then.

## How AUR packages are locked

Each AUR package is locked to the git commit of its build recipe, with the version from that commit's `.SRCINFO`:

```toml title="archix.lock"
zen-browser-bin = { version = "1.16.3b-1", repo = "aur", file = "zen-browser-bin-1.16.3b-1-x86_64.pkg.tar.zst", base = "zen-browser-bin", commit = "cd6266c6847e61c71facdd7dbf869b5cf64359be", explicit = true }
```

Dependencies the repos can satisfy, versioned ones included, are resolved from the repos at the lock's snapshot. The rest come from the AUR. An AUR package needed only to build another one is locked with `build_only = true`: it's built, and never installed.

`archix lock` and `archix switch` keep the locked commits. `archix update` and `archix -Syu` move AUR packages to the newest commit. See [The lockfile](/docs/concepts/lockfile/#aur-packages-commits).

## Build recipe review

AUR packages are code from strangers that runs on your machine. archix shows it to you before it's trusted, whenever a recipe enters the lock:

- for a new package, the full PKGBUILD;
- for an update, the git diff since the locked commit.

```console
$ sudo archix install zen-browser-bin
==> zen-browser-bin: new AUR package at cd6266c6847e
# Maintainer: ...
pkgname=zen-browser-bin
...
trust zen-browser-bin at cd6266c6847e and lock it? [Y/n]
```

Answer `n` and nothing changes: `not locking zen-browser-bin; nothing was changed`.

A commit already in the lock is never shown again. You, or whoever committed the lock, already approved it. That's what lets a second machine install from your lock without asking about every package.

Review happens wherever recipes enter the lock: `install`, `lock`, `update`, `upgrade`, `switch` when the config changed, and `init`. `--noconfirm` skips it for one command, and [`aur.review = false`](/docs/reference/options/) turns it off.

## Clean chroot builds

Every AUR package is built with devtools' `makechrootpkg` in a clean chroot at `/var/lib/archix/chroot`, as the unprivileged `archix-build` user. The chroot is created with `base-devel` at the host's locked snapshot date, and moved whenever the lock's date changes. AUR dependencies are built first and passed in. AUR builds need `devtools` and `git` installed.

```console
:: creating the build chroot at the 2026-10-09 snapshot
:: building zen-browser-bin 1.16.3b-1 (in a clean chroot)
```

This fixes two classic AUR problems:

- **Missing dependencies.** A PKGBUILD that forgot a dependency fails in the chroot instead of quietly building against whatever you have installed.
- **Unreproducible builds.** The same recipe commit at the same snapshot date builds against the same dependencies, on every machine.

The chroot only has the official repos. Your third-party repos stay out of it, since they'd also need their signing keys inside.

### `build_on_host`

Some packages don't build in a chroot: they need hardware access, a running service, or files the chroot doesn't have. List them in [`aur.build_on_host`](/docs/reference/options/), and archix builds them with `makepkg` on the host instead. Their build dependencies are installed from the locked snapshot and removed again afterwards. Use it as an escape hatch, not a default: a host build depends on the state of your machine.

```lua
aur = { build_on_host = { "some-driver-dkms" } },
```

## The build cache

Built packages are cached in `/var/cache/archix/pkg`, keyed by recipe commit and snapshot date. If neither changed, archix reuses the built package instead of building again. Each generation records the sha256 of the file it installed, so rollback puts back that exact file.

When a locked commit can't be built any more, because it's gone from the package's history or the build fails, archix offers the nearest newer version, then the nearest older one, and the lock follows. Set [`missing_versions`](/docs/reference/options/) or pass `--accept-nearest newer|older` to decide without asking. See [Missing versions](/docs/concepts/lockfile/#missing-versions).

## How installs run

archix reads pacman's local database directly and runs every transaction through the `pacman` binary. It doesn't reimplement pacman, so hooks, `pacman.conf` options and the local database behave as you expect.
