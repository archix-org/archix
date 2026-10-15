---
title: The AUR
description: Use archix as your AUR helper, with clean chroot builds, a build cache, PKGBUILD review and every package recorded in your config.
sidebar:
  order: 1
---

archix replaces yay and paru. It searches and installs from the official repos and the AUR, with the flags you already know. The difference is what happens around the install: the package is added to your config, its AUR commit is locked, and it's built in a clean chroot so the result doesn't depend on what happens to be installed on your machine.

## Install, remove, search

```console
$ archix search zen-browser
aur/zen-browser-bin 1.16.3b-1
    Zen Browser, a Firefox-based browser focused on privacy
aur/zen-browser 1.16.3b-1
    Zen Browser, built from source
$ sudo archix install zen-browser-bin
$ sudo archix remove zen-browser-bin
```

The pacman-style aliases do the same:

| Alias | Same as |
| --- | --- |
| `archix -S <pkg>` | `archix install <pkg>` |
| `archix -Ss <query>` | `archix search <query>` |
| `archix -R <pkg>` | `archix remove <pkg>` |
| `archix -Syu` | Full update from live mirrors, adopted into the lock |
| `archix` | `archix -Syu` |

`install` works for official packages too. archix looks in the official and third-party repos first, then the AUR.

Search and name checks run offline against the pacman sync databases and the AUR's metadata dump (`packages-meta-v1.json.gz`), which archix downloads when it syncs. A misspelled name fails before anything is built.

## Where `archix install` writes

`archix install` doesn't just install. It declares. The package name goes into a data file that archix owns, and your Lua config merges that file in like any other module. The next switch, on this machine or a fresh one, installs it again.

By default the data file lives under `lua/hosts/`, next to your host file, so installs are per machine. To put a package somewhere shared instead, pass a directory:

```console
$ sudo archix install --path lua/modules/dev rust-analyzer
```

If the path isn't a valid directory, the command fails and nothing is installed.

archix only ever edits its own data files. It never rewrites your Lua. So `archix remove` removes packages that `archix install` added; a package you declared by hand in `packages` stays until you delete it from your file.

You can also skip the command and declare AUR packages in Lua directly, alongside official ones:

```lua title="lua/hosts/desktop.lua"
return {
  packages = { "firefox", "zen-browser-bin", "visual-studio-code-bin" },
}
```

## Clean chroot builds

Every AUR package is built in a clean chroot with devtools' `makechrootpkg`, at the host's locked snapshot date. The chroot contains only `base-devel` and the package's declared dependencies, all at locked versions.

This fixes two classic AUR problems:

- **Missing dependencies.** A PKGBUILD that forgot a dependency fails in the chroot instead of quietly building against whatever you have installed.
- **Unreproducible builds.** The same PKGBUILD commit at the same snapshot date builds against the same dependencies, on every machine.

### `build_on_host`

Some packages don't build in a chroot: they need hardware access, a running service, or files the chroot doesn't have. For those, set `build_on_host = true` on that package. archix then builds it on the host system instead of in the chroot. Use it as an escape hatch, not a default: a host build depends on the state of your machine.

## The build cache

Built packages are cached, keyed by PKGBUILD commit and snapshot date. If neither changed, archix reuses the built package instead of building again. That makes rollback, a second machine on the same lock, and re-running a failed switch fast.

The cache is also the first place archix looks when a locked AUR version is needed. If it's not there, archix rebuilds from the locked commit. If the commit can't be fetched any more, for example because the package was deleted from the AUR, archix offers the nearest version it can get. See [Missing versions](/docs/concepts/lockfile/#missing-versions).

## PKGBUILD review

AUR packages are code from strangers that runs on your machine. archix shows it to you before building:

- on first install, the full PKGBUILD,
- on updates, the diff since the commit you last approved.

```console
$ sudo archix install zen-browser-bin
:: Review zen-browser-bin (aur, cd6266c)
   PKGBUILD   112 lines
:: Proceed with build? [Y/n]
```

Review is skipped when the lock already contains that exact commit. You, or whoever committed the lock, already approved it. That's what lets a second machine install from your lock without asking about every package.

Review is configurable globally and per package in your config, for example to always show the diff for packages you don't fully trust.

## Local PKGBUILDs

Besides the AUR, archix builds packages from local PKGBUILDs. They go through the same chroot build, cache and lock as AUR packages, so your own packaging work becomes part of the declared system.

## How installs run

archix reads package state through libalpm and runs every transaction through the `pacman` binary. It doesn't reimplement pacman, so hooks, `pacman.conf` options and the local database behave as you expect.
