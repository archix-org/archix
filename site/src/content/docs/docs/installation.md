---
title: Installation
description: Install archix from the AUR on Arch Linux, CachyOS or EndeavourOS, and check that your machine meets the requirements.
sidebar:
  order: 2
---

archix ships as two AUR packages:

- `archix` builds from source.
- `archix-bin` installs a prebuilt binary.

Both install the same tool. Pick `archix-bin` if you'd rather not compile Rust on a fresh machine.

## Requirements

- **An Arch-based system on x86_64.** Arch Linux, CachyOS and EndeavourOS are supported. CachyOS is the primary target, so its repos get special care (see [Third-party repos](/docs/guides/third-party-repos/)).
- **systemd-boot**, if you want boot entries per generation and boot-time rollback. archix writes one entry per generation, plus a rescue entry. GRUB and Limine are not supported. On those bootloaders archix still manages your system and `archix rollback` still works from a running system, but you don't get generation boot entries.
- **btrfs on `/`**, recommended. With a btrfs root, archix snapshots before every switch and rolls back by booting the snapshot. On ext4 and other filesystems, rollback reinstalls the old generation's packages and files from the cache instead. Both work; btrfs is faster and has fewer ways to fail. See [Generations and rollback](/docs/concepts/generations/).
- **sbctl keys**, optional. If you've enrolled Secure Boot keys with `sbctl`, archix signs every generation's unified kernel image (UKI).
- **base-devel and git** for the bootstrap below. AUR builds later run in a clean chroot with devtools' `makechrootpkg`.

## Bootstrap from the AUR

You don't have an AUR helper yet, or you're about to replace the one you have. Build archix the way you'd build yay for the first time:

```console
$ sudo pacman -S --needed base-devel git
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

Then run `sudo archix init` to capture your system. The [quick start](/docs/quick-start/) walks through it.

## Updates to archix itself

After the first switch, archix is a package in your lock like any other. It updates through its own lock: when you run `archix update` and `archix switch`, the new archix version is resolved, built and installed in the same transaction as everything else. If an archix update turns out bad, rolling back the generation rolls back archix too.

You don't need to declare archix in `packages`, and you shouldn't update it with `makepkg` again.

## The static rescue binary

Every [GitHub release](https://github.com/archix-org/archix/releases) includes a static `archix` binary. It is a rescue tool, not an install path. It has no dependencies, so it runs from a live USB against a mounted system:

```console
# mount /dev/nvme0n1p2 /mnt
# ./archix rollback --root /mnt
```

You should never need it. Every generation already carries a static archix in its initramfs and gets an `archix rescue` boot entry, which repairs a broken system without a live USB. The release binary is the last resort when the machine can't boot any of its own entries. See [Safety](/docs/concepts/safety/#rescue-mode).
