---
title: Installation
description: Install archix from the AUR on Arch Linux, CachyOS or EndeavourOS, and check that your machine meets the requirements.
sidebar:
  order: 2
---

archix ships as two AUR packages:

- `archix` builds from source.
- `archix-bin` installs a prebuilt binary.

Both install the same tool, plus the static `archix` binary that [rescue mode](/docs/concepts/safety/#rescue-mode) puts in the initramfs. Pick `archix-bin` if you'd rather not compile Rust on a fresh machine.

## Requirements

- **An Arch-based system on x86_64.** Arch Linux, CachyOS and EndeavourOS are supported. CachyOS is the primary target, so its repos get special care (see [Third-party repos](/docs/guides/third-party-repos/)).
- **systemd-boot**, if you want a boot entry per generation, rollback from the boot menu and rescue entries. Entries are unified kernel images (UKIs) built with `mkinitcpio` and `ukify` (from `systemd-ukify`). GRUB and Limine are not supported: on those bootloaders archix still manages your system and `archix rollback` still works from a running system, but you don't get generation boot entries.
- **sbctl keys**, optional. If `sbctl` has keys, archix signs every UKI and systemd-boot itself, so Secure Boot stays on.
- **Any filesystem.** Rollback reinstalls a generation's exact packages and files from the cache, which works the same on ext4, btrfs or anything else. On a btrfs root, archix also takes a read-only snapshot of `/` before every switch, as an extra recovery point you can use by hand. See [Generations and rollback](/docs/concepts/generations/#btrfs-snapshots).
- **base-devel and git** for the bootstrap below, and **devtools** for AUR builds later, which run in a clean chroot with `makechrootpkg`.

## Bootstrap from the AUR

You don't have an AUR helper yet, or you're about to replace the one you have. Build archix the way you'd build yay for the first time:

```console
$ sudo pacman -S --needed base-devel git devtools
$ git clone https://aur.archlinux.org/archix.git && cd archix && makepkg -si
```

For the prebuilt binary, clone `archix-bin` instead:

```console
$ git clone https://aur.archlinux.org/archix-bin.git && cd archix-bin && makepkg -si
```

If you already have yay or paru, `yay -S archix` works too. Once archix has made its first switch, it replaces them.

Check that it runs:

```console
$ archix --version
archix 1.0.0
```

Then run `sudo archix init` to capture your system into a config and a lock that match it exactly, so the first `archix diff` is empty. The [quick start](/docs/quick-start/) walks through it.

## Updates to archix itself

archix is an AUR package on your system, so `archix init` declares it in `installed.lua` like your other explicitly installed packages, and locks it by AUR commit. From then on it updates like everything else: `archix update` and `archix switch`, or `archix -Syu`, build the new version in a clean chroot and install it in the same transaction as the rest. If an archix update turns out bad, rolling back the generation rolls back archix too.

Don't update it with `makepkg` again.

## The static rescue binary

The package installs a statically linked archix at `/usr/lib/archix/archix-static`. When archix manages booting and that binary is present, archix adds an mkinitcpio hook that copies it into the initramfs, and gives the newest two generations an `archix rescue` boot entry. That entry repairs a system pacman can't repair, without a live USB.

The same binary also works from a live USB against the mounted system, since it has no dependencies:

```console
# mount /dev/nvme0n1p2 /mnt
# /mnt/usr/lib/archix/archix-static rescue --root /mnt
```

You should rarely need that. See [Safety](/docs/concepts/safety/#rescue-mode).
