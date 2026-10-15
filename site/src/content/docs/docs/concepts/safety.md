---
title: Safety and recovery
description: How archix prepares before it applies, journals every step, manages files without losing your edits, and recovers from failed or interrupted switches without a live USB.
sidebar:
  order: 5
---

The goal is simple: no failure during a switch, including pulling the power cord halfway through, should ever need a live USB. This page explains how archix gets there.

## Prepare, then apply

`archix switch` runs in two phases.

**Prepare** does everything that can fail without touching the system:

- evaluates the config and renders every managed file,
- resolves the lock with pacman itself, so dependencies are consistent, and shows new AUR build recipes for review,
- offers to adopt packages that are newer than the lock,
- checks that every managed file can replace what's on disk now, keeping a backup ([below](#managed-files-backups-and-hand-edits)),
- lists the changes and asks you to confirm,
- stores every managed file's contents, and downloads files declared with a `url`, checking each against its `sha256`,
- builds every AUR package in a clean chroot,
- downloads every package and checks it against the lock.

If anything fails here, a typo, a network error, an AUR build that won't compile, a declined prompt, archix stops and the packages, files and services are exactly as they were. Most problems surface in this phase.

Packages are downloaded to `/var/cache/archix/pkg`, and every file is checked against the `sha256` in the lock before it's used. Files already in pacman's own cache are reused. A file that doesn't match its checksum stops the switch here. The same goes for files declared with [`archix.fetchurl`](/docs/reference/lua-api/#archixfetchurl):

```console
error: https://downloads.example.org/hello/1.2.0/hello-x86_64 has sha256 0c1f3a5e7b9d2f4a6c8e0b1d3f5a7c9e2b4d6f8a0c1e3b5d7f9a2c4e6b8d0f1a, but the config says 5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03
```

**Apply** changes the system, in a fixed order of steps:

```console
$ sudo archix switch
upgrade    linux 7.2.8.arch1-1 -> 7.2.9.arch1-1
upgrade    mesa 1:25.3.0-1 -> 1:25.3.1-1
boot entry for the new generation
apply 3 changes? [Y/n]
:: installing packages
:: removing packages
:: setting install reasons
:: updating users and groups
:: stopping and disabling services
:: writing managed files
:: reloading systemd, sysctl, udev and locales
:: enabling and starting services
:: verifying the result
:: writing the boot entry
:: making the new generation current
switched to generation 42 (snapshot 2026-10-15)
```

Services are stopped and disabled while their unit files still exist, and enabled and started only after `systemctl daemon-reload` has seen the new ones.

archix also checks pacman before it starts. If pacman is running, or a `db.lck` is left over with no pacman running, `switch` refuses and tells you why, rather than fight another package manager.

## The journal

Before apply starts, archix writes `/var/lib/archix/journal.json`, fsynced and renamed into place. It records the target generation, the package set from before the switch, the exact transaction, and which of the steps above are done. Each step is marked done as it completes.

Every step is idempotent. A step doesn't mean "upgrade mesa from A to B"; it means "make the system match the target". Running a step twice is harmless, which is what makes it safe to continue an interrupted switch from wherever it stopped.

When every step is done, the journal is deleted. A journal on disk always means a switch is unfinished.

## Atomic writes

Managed files are written to a temporary file and renamed into place, with their mode and owner already set. A rename is atomic, so a file is either the old version or the new one, never half of each.

The two things that decide what "current" means only change at the very end, after the verify step has confirmed the result:

- the boot entry for the new generation, written in the second-to-last step;
- the current-generation pointer, replaced atomically in the last step.

Until then, the previous generation is current, and a reboot lands in it.

On btrfs, archix also takes a read-only snapshot of `/` right before apply. It's a manual recovery point, not something rollback uses; see [btrfs snapshots](/docs/concepts/generations/#btrfs-snapshots).

## Managed files, backups and hand edits

Everything archix manages on disk, from `files` and `units` to `sysctl`, `hostname`, `locales` and dotfiles, becomes a managed file. archix is careful never to lose a file it didn't write:

- **A file archix replaces** is moved to `/var/lib/archix/backups/<path>` first. When the file leaves your config, the original comes back.
- **A directory archix replaces** with a file or a link, such as an existing `~/.config/nvim` that a [live link](/docs/guides/dotfiles-and-apps/#live-links-for-app-written-configs) takes over, is moved to the same place, contents and all. When the entry leaves your config, the link goes and the directory comes back.

Backups and restores are renames. When the backup and the original are on different filesystems, such as a separate `/home` partition or a btrfs `@home` subvolume, archix copies instead, keeping modes, owners and symlinks, and then deletes the source.
- **A file archix takes over unchanged**, such as `/etc/fstab` after `archix init` captured it, is copied to the same place when archix first manages it. Unmanaging it later restores it instead of deleting it.
- **A managed file you edited by hand** is saved to `/var/lib/archix/edited/<time>/<path>` before archix overwrites it, with a warning. `archix diff` flags it beforehand:

  ```console
  update     /etc/issue (edited outside archix; your version gets saved)
  ```

- **A file archix created** is deleted when it leaves your config, but only if it still has the contents archix wrote.

archix only replaces a directory it has never managed, and only once. It refuses when an earlier backup of that path still exists, `/home/me/.config/nvim is a directory and an earlier backup of it exists; move one of them away`, and when archix managed a file at that path in the current generation: `/home/me/.config/nvim exists and is not a file or symlink; archix won't replace it`. Both `switch` and `rollback` check this before they change anything, so a refusal is a plain error. Move the directory or the old backup aside and run the command again.

Users and groups archix created are removed when they leave the config, and their home directories are kept. archix never touches passwords: a new user gets a hint instead, ``created user me; set a password with `passwd me` ``.

archix is strict about what *it* manages, and only that. It doesn't revert changes to `/etc` files it never managed, and it doesn't disable services it never managed. Doing that blindly could, for example, delete users that packages create. `archix init` captures the files and services you'd changed, which makes them managed.

Files archix takes over unchanged are backed up even when a switch has nothing else to do, as with the first switch after `archix init`, which usually prints only `switched to generation 1 (snapshot 2026-10-09); no package changes were needed`. Every switch that records a generation, that one included, also stores the contents of every file the generation manages in `/var/lib/archix/blobs`, so a later rollback to it never finds them missing.

### What init captures and never captures

`archix init` looks for the `/etc` files you changed: package config files edited since they were installed (it compares them with the checksums in pacman's database), and files nobody owns in well-known directories such as `/etc/systemd/system`, `/etc/sudoers.d`, `/etc/sysctl.d`, `/etc/modprobe.d`, `/etc/udev/rules.d`, `/etc/X11/xorg.conf.d`, `/etc/pacman.d/hooks` and `/etc/polkit-1/rules.d`. It sorts what it finds into three groups:

- **Secrets** are never shown or copied, only counted: `· 3 files that hold secrets stay out of the config and aren't listed`. That covers SSH host keys, NetworkManager connections, wpa_supplicant, iwd, WireGuard, OpenVPN and PPP configs, `/etc/ssl/private`, `/etc/cryptsetup-keys.d`, `krb5.keytab`, `smbpasswd`, `ipsec.secrets`, and any path containing `secret`, `password`, `passwd`, `token`, `credential`, `keyfile`, `.key`, `.pem`, `.p12` or `.pfx`.
- **Worth managing** files are pre-selected: `/etc/fstab`, `/etc/crypttab`, `/etc/mkinitcpio.conf`, `/etc/sudoers`, `/etc/pacman.d/mirrorlist`, `/etc/makepkg.conf`, `/etc/hosts`, `sshd_config`, the systemd, NetworkManager, bluetooth and display manager configs, and everything hand-written in the directories above.
- **The rest**, mostly files that package install scripts change, such as `/etc/shells`, are off by default.

init's Config files step shows them all in one checklist, `Config files archix manages`, each tagged with why it was found: `recommended` for the ones worth managing, then `changed from its package's version` or `not from any package`. The recommended ones start ticked. `space` toggles a file, `a` and `n` select all or none, and `r` goes back to the recommended set. `--defaults` takes the recommended ones. Chosen files are copied into `files/<host>/` and declared with their mode, owner and group.

It never captures files that archix or other tools own: `/etc/pacman.conf`, `/etc/passwd`, `/etc/shadow`, `/etc/group`, `/etc/gshadow` and their backups, `/etc/machine-id`, `/etc/resolv.conf`, `/etc/mtab`, `/etc/ld.so.cache`, `/etc/kernel/cmdline`, pacman's keyring, the extracted CA certificates, and archix's own files.

Since init only declares what's already there, the first switch after it changes nothing, with one exception: dotfiles you chose to import. The first switch backs up the originals and replaces them with links into the config, and init's Result section lists them under `! the first switch would still change these:`.

## Hard and soft failures

Not every failure during apply means the same thing.

**Hard failures** stop the switch: a pacman error, a file that can't be written, a user that can't be created, a verify step that finds the system doesn't match the target. The journal stays open, and archix tells you to run `archix recover`:

```console
:: installing packages
error: installing packages failed; the switch is unfinished, run `archix recover`: pacman exited with exit status: 1
```

**Soft failures** don't: a service that fails to start, a reload such as `sysctl --system` that fails, or a Flatpak app that can't be installed, pinned or removed. Flatpak apps are synced after the new generation becomes current, outside the journal. The new generation becomes current, archix lists what failed and exits non-zero, like NixOS does. A broken unit is usually a config problem you fix and switch again, and undoing the whole generation for it would be worse.

```console
:: enabling and starting services
:: verifying the result
:: writing the boot entry
:: making the new generation current
switched to generation 43 (snapshot 2026-10-15)
failed: systemctl enable --now nginx.service: Job for nginx.service failed because the control process exited with error code.
error: the new generation is current, but 1 step failed (listed above)
```

## Unfinished switches

archix never acts on an unfinished switch automatically. Guessing wrong after a crash could make things worse, so a human decides.

- `archix-boot.service` reports the unfinished switch at boot and changes nothing.
- `switch`, `upgrade`, `install`, `remove`, `rollback`, `lock`, `update` and `gc` refuse to run, and say which switch is unfinished. Read-only commands such as `diff` and `status` keep working, so you can look around first.
- pacman and yay keep working. The pacman hook adds a warning after each transaction.
- You're told at login, by a message from `/etc/profile.d/archix.sh` and a desktop notification.

```console
$ archix status
the switch to generation 43 (snapshot 2026-10-15) (started at unix time 1792051200) was interrupted while installing packages
run `archix recover` to deal with it
current generation: 42 (snapshot 2026-10-15)
desktop is locked to snapshot 2026-10-15 (214 packages)
12 changes pending; see `archix diff`
```

## `archix recover`

`recover` shows what was interrupted and at which step, then offers three ways out:

- **continue** reruns the steps that hadn't completed and finishes the switch.
- **rollback** returns to the package set from before the switch, and undoes its changes to files, users and services. Packages come from pacman's cache or the Arch Linux Archive, and anything the transaction touched is reinstalled.
- **abandon** changes nothing. It shows how the installed packages differ from the switch's target, runs `pacman -Qk` on the packages the switch touched, then closes the journal, so you see the real state of the system and fix it by hand.

```console
$ sudo archix recover
the switch to generation 43 (snapshot 2026-10-15) (started at unix time 1792051200) was interrupted while installing packages
[c]ontinue it, [r]oll back, [a]bandon (close the journal without changes), or [q]uit? c
finished the interrupted switch to generation 43 (snapshot 2026-10-15)
```

To skip the prompt, pass the choice:

```console
$ sudo archix recover --continue
$ sudo archix recover --rollback
$ sudo archix recover --abandon
```

Pick continue if the interruption was something outside archix, like a power cut. Pick rollback if you don't trust the new package set. Pick abandon if you want to inspect and fix things by hand; afterwards `archix diff` shows how the system differs from the lock, and the next `archix switch` reinstalls whatever the lock needs. Rollback and abandon both delete the generation the interrupted switch had created, since it never became current.

A recovery rollback is journaled too. If it gets interrupted in turn, `archix recover` picks it up the same way.

## Interrupted pacman transactions

Power loss in the middle of a pacman transaction is the classic way to break Arch: a stale lock file, half-extracted packages, a local database that disagrees with the disk.

`recover` handles this before anything else. It first makes sure pacman isn't running; if it is, it stops and asks you to wait. Then it removes the stale `/var/lib/pacman/db.lck`. If the install step was the one interrupted, continuing reinstalls every package of the transaction, even ones pacman already reports as installed, so half-extracted ones are replaced. The verify step then checks the result. Prepare already downloaded everything to `/var/cache/archix/pkg`, so continuing needs no network.

## Rescue mode

Some breakage stops pacman itself: a half-installed glibc, a deleted libalpm. Then nothing that relies on pacman can repair the system, including rollback.

For that, when archix manages booting, the initramfs carries a static archix, and the newest two generations get a rescue entry:

```
  Arch Linux (archix, generation 43)
  Arch Linux (archix, generation 42)
› Arch Linux (archix rescue, generation 43)
  Arch Linux (archix rescue, generation 42)
```

Booting it adds `archix.rescue` to the kernel command line. Before switching to the real root, the initramfs runs `archix rescue`, which unpacks the current generation's core packages straight from the cache, without pacman: glibc, pacman and its libraries, systemd, bash, coreutils, the kernel, archix itself and the rest of the core set. It keeps config files you edited, saving the package's version as `.pacnew`, and rewrites the packages' entries in pacman's database. Then the system boots, and rescue's last line tells you how to finish with the regular tools:

```
36 packages restored; boot normally, then run `sudo archix rollback 43` to put everything else back
```

Rolling back to the current generation puts back whatever differs from it, packages, files and services alike. If a switch was interrupted, run `sudo archix recover` instead.

That's what makes a broken pacman repairable without a live USB. It works with both systemd-based and busybox initramfs images. The rescue hook copies the static binary that the `archix` and `archix-bin` AUR packages install at `/usr/lib/archix/archix-static`; rescue entries are only built when that file exists. If you installed archix another way, build it yourself from the source tree:

```console
$ cargo build --release --target x86_64-unknown-linux-musl
$ sudo install -Dm755 target/x86_64-unknown-linux-musl/release/archix /usr/lib/archix/archix-static
```

In CI, a QEMU VM with OVMF firmware deletes libalpm to break pacman, boots the rescue entry, and checks that pacman works again.

### From a live USB

If the machine can't boot any of its own entries, the same static binary works from a live USB against the mounted system:

```console
# mount /dev/nvme0n1p2 /mnt
# mount /dev/nvme0n1p1 /mnt/boot
# /mnt/usr/lib/archix/archix-static rescue --root /mnt
```

`--generation N` restores another generation's core packages, and `--package NAME` adds packages beyond the core set. You shouldn't need it often. It's a fallback for accidents the built-in boot entries can't cover, not a normal recovery path.
