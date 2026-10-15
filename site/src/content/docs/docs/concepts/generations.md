---
title: Generations and rollback
description: What a generation contains, how archix rolls back from a running system or from the boot menu, and how old generations are cleaned up.
sidebar:
  order: 3
---

Every `archix switch` that changes something creates a generation: a numbered, complete record of what the system looked like after that switch. Rolling back means making the system match an older generation again.

## What a generation contains

Generations live in `/var/lib/archix/generations/<n>/generation.json`, and `/var/lib/archix/current` points at the current one. Each one records:

- **The packages.** The snapshot date and every package's exact version, file and sha256, including the sha256 of each AUR package it built.
- **The managed files, users, groups and services.** File contents are stored once by sha256 in `/var/lib/archix/blobs`, so identical files across generations cost nothing extra. Files downloaded with [`archix.fetchurl`](/docs/reference/lua-api/#archixfetchurl), AppImages included, are stored the same way. Every switch that records a generation stores the contents it needs, including one with nothing else to apply, so rolling back to any kept generation finds its files.
- **The Flatpak apps.** The remote, ref and commit each declared app was pinned to.
- **The settings it was applied with.** `undeclared_packages`, `ignored_packages`, the generation limits and the boot settings, so rollback and `gc` work without evaluating your config.
- **The config's git commit**, if your config directory is a git repo, and whether the tree had uncommitted changes.
- **Its btrfs snapshot**, on a btrfs root.

A switch that only changes settings, such as `ignored_packages` or a boot option, still creates a generation, so turning on Secure Boot gives you newly signed entries. A switch that changes nothing prints `nothing to do: the system matches generation 42 (snapshot 2026-10-15)` and creates none. A switch with nothing to apply but a new generation to record, such as the first one after `archix init` when archix doesn't manage booting, prints `switched to generation 1 (snapshot 2026-10-09); no package changes were needed`.

```console
$ archix generations
    40  2026-10-02 18:40 UTC  snapshot 2026-10-02  210 packages  config 8c1f0e2a7b
    41  2026-10-09 09:12 UTC  snapshot 2026-10-09  212 packages  config 3f2a9c1e07
*   42  2026-10-15 08:55 UTC  snapshot 2026-10-15  214 packages  config 5d7b21c4e9+uncommitted
```

`*` marks the current generation. A generation only becomes current in the last step of a switch, after everything else succeeded, by atomically replacing the `current` link. An interrupted switch never becomes current.

## Rolling back from a running system

```console
$ sudo archix rollback
downgrade  linux 7.2.9.arch1-1 -> 7.2.8.arch1-1
downgrade  mesa 1:25.3.1-1 -> 1:25.3.0-1
roll back to generation 41 (snapshot 2026-10-09) with 2 changes? [Y/n]
:: installing packages
...
:: making the new generation current
rolled back to generation 41 (snapshot 2026-10-09); archix.lock is unchanged, so the next `archix switch` returns to the config
```

This makes the system match the previous generation: its exact package versions are reinstalled, from the cache when they're there and from the Arch Linux Archive otherwise, and its files, users, groups and services are applied again. Files come from the store, so a file declared with a `url` comes back even when the URL is gone. It goes through the same journal as a switch, so an interrupted rollback is as recoverable as an interrupted switch. See [Safety](/docs/concepts/safety/).

Flatpak apps follow last, once the generation is current. Each app is pinned back to the commit the generation recorded, and apps the newer generation added are uninstalled:

```console
flatpak    com.spotify.Client 81c2d4e6f0a3 -> 5f8806197a2b
flatpak    remove org.gimp.GIMP
```

Old commits are downloaded from their remote, so this part needs the network and a remote that still has the commit. If it fails, the failure is listed like a service that didn't start, and the rest of the rollback stands. Apps archix never managed, and `--user` apps, are never touched.

To go to a specific generation, name it. That works forwards too:

```console
$ sudo archix rollback 40
$ sudo archix rollback 42
```

Naming the current generation repairs it. `archix rollback 42` with 42 current prints `generation 42 is current; putting back whatever differs from it`, then reinstalls drifted packages and rewrites changed files, users and services. That's also how you finish after [rescue mode](/docs/concepts/safety/#rescue-mode).

`rollback` doesn't evaluate your config, so it works even when the config is broken. It refuses while a switch is unfinished; `archix recover --rollback` is the way back from that.

Rollback changes the system, not your config. Your `init.lua` and `archix.lock` still describe the newer generation, so the next `archix switch` will apply them again. That's deliberate: archix never rewrites your config behind your back. Before switching again, fix the config, revert the lock in git, or move it back with `archix update --to`.

## Rolling back from the boot menu

When archix manages booting, every generation gets its own boot entry. When a generation won't boot, or boots to a broken desktop, pick an older one in systemd-boot:

```
  Arch Linux (archix, generation 42)
› Arch Linux (archix, generation 41)
  Arch Linux (archix, generation 40)
  Arch Linux (archix rescue, generation 42)
  Arch Linux (archix rescue, generation 41)
```

The entry starts generation 41's kernel with `archix.generation=41` on its command line. Early in boot, before most of the system starts, `archix-boot.service` sees that 42 is current and rolls back to 41 from the cache, exactly like `archix rollback 41`:

```
archix: booted generation 41, but 42 is current; rolling back to 41
```

You land in generation 41, and it stays current until your next switch. This works the same on every filesystem.

Flatpak commits come from their remote, and the network isn't up that early in boot, so the boot service leaves Flatpak apps as they are and says so:

```
archix: Flatpak apps aren't pinned at boot; run `sudo archix rollback 41` once you're online
```

Naming the current generation puts back whatever still differs, Flatpak apps included.

### Older kernels keep booting

An older entry needs its kernel's modules, which pacman deletes when it upgrades the kernel. archix keeps a hardlinked copy of each entry's modules in `/var/lib/archix/kernels`, and the boot service puts the booted kernel's modules back before anything loads them.

### When pacman itself is broken

Rolling back at boot needs a working pacman and glibc to reinstall packages. If a bad update broke those, boot the generation's **archix rescue** entry instead. Rescue mode restores the core packages from the cache without pacman, in the initramfs, before the system starts. [Safety](/docs/concepts/safety/#rescue-mode) has the details.

If a switch was interrupted, the boot service only reports it and changes nothing; run `sudo archix recover`.

## Boot entries

Set `boot.loader = "systemd-boot"` (`archix init` offers it when systemd-boot is installed) and archix writes a boot entry for each generation. Your `kernel_params` must contain `root=`. archix declares the packages it needs for this, the `boot.kernel` package, `mkinitcpio` and `systemd-ukify`, plus `sbctl` with `boot.secure_boot = true`, so you don't list them yourself.

- Each entry is a unified kernel image (UKI) built with `ukify` from a fresh `mkinitcpio` initramfs, at `/boot/EFI/Linux/archix-42.efi`. Its title comes from its embedded os-release: "Arch Linux (archix, generation 42)", with `IMAGE_VERSION=42`.
- The entry is written in the step before the generation becomes current, after the result has been verified, so a switch that fails never gets one.
- Only the newest [`boot.entries`](/docs/reference/options/) generations (5 by default) keep an entry, and fewer if the ESP runs short of space. `gc` prunes the rest.
- The newest two generations also get a rescue entry, `archix-42-rescue.efi`, titled "Arch Linux (archix rescue, generation 42)". Rescue entries need the static binary at `/usr/lib/archix/archix-static`, which the AUR packages install; see [Installation](/docs/installation/#the-static-rescue-binary).

GRUB and Limine are not supported. With them, rollback from a running system still works, but archix doesn't write generation boot entries.

### Secure Boot

archix signs every UKI with `sbctl` when `boot.secure_boot` is `true`, or by default whenever `sbctl` has keys. It also signs systemd-boot itself, so you can keep Secure Boot enforcing without lanzaboote or a hand-written hook. Turning Secure Boot on counts as a settings change, so the next switch writes a newly signed entry.

Booting older generations, Secure Boot enforcing and the rescue entry are tested by booting archix-managed generations in a QEMU VM with OVMF firmware in CI.

## btrfs snapshots

On a btrfs root, archix takes a read-only snapshot of `/` right before applying each switch, in `/.archix-snapshots/<generation>-<time>`. Turn it off with `boot.snapshots = false`.

These snapshots are manual recovery points. Rollback, at the command line or from the boot menu, never restores them; it reinstalls packages and files the same way on every filesystem. A snapshot is there for when you want something back by hand: copy a file out of it, or, from a live USB, make a writable snapshot of it and boot that as your root. It only covers the subvolume mounted at `/`, not nested ones such as a separate `/home`. Each snapshot is deleted with its generation.

## Limits and `gc`

Generations cost disk space, mostly in the package cache. archix cleans up after itself:

| What | Default |
| --- | --- |
| Generations kept | The current one, the newest 10, and every one younger than 30 days |
| Boot entries | The newest 5, and fewer if the ESP runs out of space |
| Rescue entries | The newest 2 |
| Package cache, managed-file contents (downloads included), snapshot databases | Only what kept generations need |
| Preserved kernel modules | Only the kernels of kept boot entries |
| btrfs snapshots | Deleted with their generation |
| When | `gc` runs after every switch |

Set [`generations.keep`](/docs/reference/options/) and `generations.keep_days` to change the limits; `archix init` asks about them. To clean up by hand, for example after lowering a limit:

```console
$ sudo archix gc
deleted generations 30, 31
freed 812.4 MiB: 46 cached package files, 3 snapshot databases
```

`gc` never removes the current generation. A generation that has been collected can't be rolled back to, because its package files may be gone. If you want to keep one longer, raise the limits before it ages out.
