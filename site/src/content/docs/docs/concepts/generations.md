---
title: Generations and rollback
description: What a generation contains, how archix rolls back from a running system or from the boot menu, and how old generations are cleaned up.
sidebar:
  order: 3
---

Every successful `archix switch` creates a generation: a numbered, complete record of what the system looked like after that switch. Rolling back means making the system match an older generation again.

## What a generation contains

- **The lock.** Every package's exact version and file, for this host.
- **The rendered managed files.** Every `/etc` file, dotfile and unit archix wrote, stored content-addressed, so identical files across generations are stored once.
- **The package files it references.** Kept in the package cache for as long as the generation is kept, so rolling back never needs the network.
- **The config's git commit**, if your config directory is a git repo. You can always see which version of your config built a generation.

Each generation also keeps its own kernel and modules, and gets its own boot entry.

## Rolling back from a running system

```console
$ sudo archix rollback
```

This makes the system match the previous generation: its packages are reinstalled from the cache, its files are written back, and its services are enabled or disabled to match. It goes through the same prepare-and-apply steps as a switch, so an interrupted rollback is as recoverable as an interrupted switch. See [Safety](/docs/concepts/safety/).

Rollback changes the system, not your config. Your `init.lua` and `archix.lock` still describe the newer generation, so the next `archix switch` will apply them again. That's deliberate: archix never rewrites your config behind your back. Before switching again, fix the config, revert it in git, or [pin](/docs/concepts/lockfile/#pins) the package that caused trouble.

## Rolling back from the boot menu

When a generation won't boot, or boots to a broken desktop, pick an older one in systemd-boot:

```
  archix · generation 42 · Oct 15
› archix · generation 41 · Oct 09
  archix · generation 40 · Oct 02
  archix rescue · generation 42
```

What happens next depends on your filesystem.

### btrfs: the fast path

On a btrfs root, archix takes a snapshot before every apply. That snapshot is the generation's rollback point. Booting an older entry boots its snapshot, so the system is exactly as it was, instantly. Nothing is reinstalled.

This is why onboarding recommends btrfs.

### ext4 and everything else: the baseline

Without snapshots, archix still keeps everything a generation needs: its kernel image on the ESP, its kernel modules, and its packages and files in the cache. Booting an older entry starts that generation's kernel, then an early-boot service reinstalls the generation's packages and files from the cache before your desktop starts. You wait a little longer, then land in the old generation.

This path needs a working pacman and glibc to reinstall packages. If a bad update broke those, boot the generation's **archix rescue** entry instead. Rescue mode unpacks the core packages itself, without pacman, then hands over to the normal repair. That's what makes ext4 safe without a live USB. [Safety](/docs/concepts/safety/#rescue-mode) has the details.

## Boot entries and Secure Boot

archix supports systemd-boot. Each generation gets a unified kernel image (UKI) and a boot entry, and the newest successful generation is the default. The default entry only moves after a switch has fully succeeded, so a failed switch never leaves you booting into it.

If `sbctl` keys are present, every UKI is signed, so rollback works with Secure Boot enabled.

GRUB and Limine are not supported. With them, runtime rollback still works, but archix doesn't write generation boot entries.

## Limits and `gc`

Generations cost disk space, mostly in the package cache. archix cleans up after itself with these defaults:

| What | Default |
| --- | --- |
| Boot entries | 5, and fewer if the ESP runs out of space |
| Generations kept | The last 10, or the last 30 days, whichever keeps more |
| Package cache | Only files referenced by kept generations |
| btrfs snapshots | Cleaned up with their generation |
| When | `gc` runs after every switch |

Onboarding asks about the important ones, and you can change them in your config later.

To clean up by hand, for example after lowering a limit:

```console
$ sudo archix gc
```

`gc` only removes generations outside the limits, and never the current one. A generation that has been collected can't be rolled back to, because its package files may be gone. If you want to keep one indefinitely, raise the limits before it ages out.
