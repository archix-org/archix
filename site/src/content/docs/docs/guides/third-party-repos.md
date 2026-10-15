---
title: Third-party repos
description: How archix declares and locks CachyOS, chaotic-aur and other third-party repos, and what happens when a locked version can't be restored.
sidebar:
  order: 2
---

Many Arch-based systems install packages from repos that Arch doesn't run. CachyOS has its optimized `cachyos-v3`, `cachyos-core-v3` and `cachyos-extra-v3` repos and its kernels. EndeavourOS has `endeavouros`. Plenty of people add chaotic-aur for prebuilt AUR packages.

archix supports all of them. They need a little more care than the official repos, though, and on CachyOS that care matters for most of your system. This page explains why, and what archix does about it.

## Declaring a third-party repo

Third-party repos are declared in your config, like everything else. From the declaration, archix writes the repo's section in `/etc/pacman.conf` and imports its signing keys. You don't edit `pacman.conf` or run `pacman-key` by hand, and a fresh machine gets the same repos, with the same keys, on its first switch.

You rarely write the declaration yourself. During `archix init`, step 6 finds every third-party repo in your current `pacman.conf` and offers to declare it. On a CachyOS install, say yes: the CachyOS repos supply your kernel, and often your mesa, glibc and more.

To see the repo options your archix provides, with their types and descriptions:

```console
$ archix options | less
```

The official repos are a separate, simpler option: [`repos`](/docs/reference/options/#repos) picks which of `core`, `extra`, `multilib` and the testing repos you use. Those are always locked against the Arch Linux Archive, on CachyOS too.

## Three kinds of repo, three ways to lock

archix locks each kind of package with the strongest guarantee its source can give.

| | Official repos | AUR | Third-party repos |
| --- | --- | --- | --- |
| Examples | `core`, `extra`, `multilib` | any AUR package | `cachyos-v3`, `chaotic-aur`, `endeavouros` |
| Locked by | Arch Linux Archive snapshot date, plus exact version and sha256 | `aur.git` commit | Exact version and sha256 of the package file |
| Where the file comes from | The ALA, for any date and any version | Built in a clean chroot from the locked commit | A content-addressed copy archix keeps locally, or the live repo |
| Restorable on a fresh machine | Always | As long as the commit can be fetched and builds | Only while the repo still serves that version |
| When it can't be restored | Doesn't happen | Nearest-version prompt | The fallback chain, then the nearest-version prompt |

The official repos are the easy case. The Arch Linux Archive keeps every day of the repos and every package version ever published, so any lock can be restored anywhere.

The AUR is nearly as good. A PKGBUILD commit is permanent unless the package is deleted, and the build is redone from that commit at the locked snapshot date.

Third-party repos are the hard case.

## Why third-party versions can go missing

Third-party repos have no dated archive. Most of them, CachyOS included, keep only the current version of each package. When a new build lands, the old one is gone from the mirrors.

So archix keeps its own copy. Every package file it installs from a third-party repo is stored in a content-addressed cache, keyed by its checksum, and the lock records that checksum. On the machine that installed it, rollback and re-installs work exactly as they do for official packages: the file is right there.

The gap is a different machine, or the same machine after a reinstall. Its cache is empty. If the repo has moved on since you made the lock, the exact `linux-cachyos 6.17.2-2` in your lock doesn't exist anywhere archix can reach. No tool can fix that without an archive the repo doesn't provide. archix is upfront about it and gives you a predictable way forward.

## The fallback chain

When a locked third-party version can't be restored, archix falls back in this order:

1. **Another version from the same third-party repo.** Usually the current build. This keeps you on the same optimizations, for example still the `x86-64-v3` build.
2. **The AUR.** For packages that also exist there, such as most of chaotic-aur, archix builds the package from the AUR in a clean chroot.
3. **The official Arch repos.** For CachyOS rebuilds of official packages, such as `mesa` from `cachyos-extra-v3`, archix installs Arch's own build from the locked snapshot. You lose the CachyOS optimizations for that package but keep a working, locked system.

Whatever it picks is written back to the lock, so the next machine gets the same answer.

### Asking first

Falling back changes what you run, so by default archix asks every time. The prompt defaults to falling back:

```console
$ sudo archix switch
linux-cachyos 6.17.2-2 is no longer available from cachyos-core-v3.
Fall back to linux-cachyos 6.17.4-1 from cachyos-core-v3? [Y/n]
```

Press Enter and archix uses the fallback. Answer `n` and the switch stops in the prepare phase, with the system untouched, so you can decide what to do. One option is a [pin](/docs/concepts/lockfile/#pins) to a version that's still available.

You choose the default behaviour during onboarding, in step 6 of `archix init`, right after declaring the repos. "Ask every time" is the default. The setting lives in your config, so you can change it later and it applies to every machine that uses the config.

## Tips for CachyOS

- **Declare the repos during `archix init`.** On CachyOS, the third-party repos aren't extras. They supply the kernel and many core packages.
- **Switch the second machine soon after locking.** Third-party versions are most likely to still exist right after the lock was made. The longer you wait, the more packages hit the fallback chain.
- **Keep the default prompt at first.** Watching what falls back where tells you how much of your system depends on CachyOS-only builds.
- **Commit the lock after a fallback.** The chosen versions are written back, and committing them saves your other machines from the same question.
