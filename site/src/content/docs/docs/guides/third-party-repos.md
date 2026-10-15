---
title: Third-party repos
description: How archix declares and locks CachyOS, chaotic-aur and other third-party repos, and what happens when a locked version can't be restored.
sidebar:
  order: 2
---

Many Arch-based systems install packages from repos that Arch doesn't run. CachyOS has its optimized `cachyos-v3`, `cachyos-core-v3` and `cachyos-extra-v3` repos and its kernels. EndeavourOS has `endeavouros`. Plenty of people add chaotic-aur for prebuilt AUR packages.

archix supports all of them. They need a little more care than the official repos, though, and on CachyOS that care matters for most of your system. This page explains why, and what archix does about it.

## Declaring a third-party repo

Third-party repos are declared in your config with [`custom_repos`](/docs/reference/options/), like everything else:

```lua title="lua/modules/cachyos.lua"
return {
  custom_repos = {
    {
      name = "cachyos-core-v3",
      servers = { "https://mirror.cachyos.org/repo/x86_64_v3/$repo" },
      keys = { "F3B607488DB35A47" },
      before_official = true,
    },
    {
      name = "cachyos",
      servers = { "https://mirror.cachyos.org/repo/$arch/$repo" },
      keys = { "F3B607488DB35A47" },
      before_official = true,
    },
  },
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `name` | | The repo's name, as in `pacman.conf`. It can't clash with an official repo. |
| `servers` | | Server URLs, tried in order. `$repo` and `$arch` are expanded as pacman does. |
| `keys` | `{}` | Fingerprints of the repo's signing keys. archix imports and locally signs any that pacman's keyring doesn't have yet, at switch. |
| `keyserver` | `"hkps://keyserver.ubuntu.com"` | Where those keys are fetched from. |
| `sig_level` | `"Required DatabaseOptional"` | The repo's `SigLevel`. |
| `before_official` | `false` | List the repo before `core` and `extra`, so its packages win. CachyOS's repos need this. |

From the declaration, archix writes the repo's section into `/etc/pacman.conf`, in a [block it manages](/docs/concepts/pacman-and-yay/#third-party-repos-in-pacmanconf), so plain pacman sees the same repos. Removing a repo from your config removes its section, and its packages. A fresh machine gets the same repos, with the same keys, on its first switch.

You rarely write the declaration yourself. `archix init` imports every third-party repo in your current `pacman.conf`: its servers, including the ones in `Include`d mirrorlists, its `SigLevel`, and whether it's listed before the official repos. On a CachyOS install, that's where your kernel, and often your mesa, glibc and more, come from.

`init` doesn't know which keys belong to which repo, so their signing keys stay in pacman's keyring, where they already are, and the declaration has no `keys`. That works on this machine. List the fingerprints under `keys` if you want archix to import them on other machines too.

The official repos are a separate, simpler option: [`repos`](/docs/reference/options/) picks which of `core`, `extra`, `multilib` and the testing repos you use. Those are always locked against the Arch Linux Archive, on CachyOS too. Repos resolve in `pacman.conf` order: the official ones at the lock's snapshot date, third-party ones at their live state.

## Three kinds of repo, three ways to lock

archix locks each kind of package with the strongest guarantee its source can give.

| | Official repos | AUR | Third-party repos |
| --- | --- | --- | --- |
| Examples | `core`, `extra`, `multilib` | any AUR package | `cachyos-v3`, `chaotic-aur`, `endeavouros` |
| Locked by | Arch Linux Archive snapshot date, plus exact version and sha256 | Build recipe commit | Exact version and sha256 of the package file |
| Where the file comes from | The ALA, for any date and any version | Built in a clean chroot from the locked commit | archix's own cached copy, or the repo's servers |
| Restorable on a fresh machine | Always | As long as the commit is there and builds | Only while the repo still serves that version |
| When it can't be restored | Doesn't happen | The nearest-version prompt | The fallback chain |

The official repos are the easy case. The Arch Linux Archive keeps every day of the repos and every package version ever published, so any lock can be restored anywhere.

The AUR is nearly as good. A recipe commit is permanent unless the package is deleted, and the build is redone from that commit at the locked snapshot date.

Third-party repos are the hard case.

## Why third-party versions can go missing

Third-party repos have no dated archive. Most of them, CachyOS included, keep only the current version of each package. When a new build lands, the old one is gone from the mirrors.

So archix keeps its own copy. Every package file it installs from a third-party repo stays in `/var/cache/archix/pkg`, checked against the sha256 in the lock, and `gc` keeps it for as long as a kept generation needs it. On the machine that installed it, rollback and reinstalls work exactly as they do for official packages: the file is right there.

The gap is a different machine, or the same machine after a reinstall. Its cache is empty. If the repo has moved on since you made the lock, the exact `linux-cachyos 6.17.2-2` in your lock doesn't exist anywhere archix can reach. No tool can fix that without an archive the repo doesn't provide. archix is upfront about it and gives you a predictable way forward.

## The fallback chain

When a locked third-party package is gone, not cached and not on the repo's servers, archix falls back in this order:

1. **The repo's current version.** This keeps you on the same optimizations, for example still the `x86-64-v3` build.
2. **The AUR.** For packages that also exist there, such as most of chaotic-aur, archix locks the AUR package, shows its recipe for review, and builds it in a clean chroot.
3. **The official repos.** For CachyOS rebuilds of official packages, such as `mesa` from `cachyos-extra-v3`, archix installs Arch's own build from the locked snapshot. You lose the CachyOS optimizations for that package but keep a working, locked system.

Whatever it picks is written back to the lock, so the next machine gets the same answer.

### Asking first

Falling back changes what you run, so by default archix asks. The prompt defaults to falling back:

```console
$ sudo archix switch
...
linux-cachyos 6.17.2-2 is gone from cachyos-core-v3
fall back to its current version, the AUR, or the official repos? [Y/n]
using linux-cachyos 6.17.4-1 from cachyos-core-v3
```

Press Enter and archix uses the fallback. Answer `n` and the switch stops before anything is installed: `aborted; nothing was changed`.

The [`third_party_fallback`](/docs/reference/options/) option sets the behaviour, and `archix init` asks about it when you have third-party repos:

| Value | Behaviour |
| --- | --- |
| `"ask"` | Ask each time, defaulting to falling back. `--noconfirm` falls back. This is the default. |
| `"fallback"` | Fall back without asking. |
| `"fail"` | Stop the switch: `linux-cachyos 6.17.2-2 is gone from cachyos-core-v3; third_party_fallback = "fail"`. |

The setting lives in your config, so it applies to every machine that uses the config.

## Tips for CachyOS

- **Let `archix init` import the repos.** On CachyOS, the third-party repos aren't extras. They supply the kernel and many core packages.
- **Add `keys` before you use the config on a second machine.** On the first one, the keys are already in pacman's keyring.
- **Switch the second machine soon after locking.** Third-party versions are most likely to still exist right after the lock was made. The longer you wait, the more packages hit the fallback chain.
- **Keep the default prompt at first.** Watching what falls back where tells you how much of your system depends on CachyOS-only builds.
- **Commit the lock after a fallback.** The chosen versions are written back, and committing them saves your other machines from the same question.
