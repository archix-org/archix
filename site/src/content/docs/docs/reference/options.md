---
title: Options reference
description: The core archix options, packages, repos, programs and files, and how to list every option available on your system.
sidebar:
  order: 2
---

This page documents every option archix ships: packages and repositories, the AUR, third-party repos, generations, the system (files, users, services and more), booting, your home directory and the `programs.*` modules. Your installed archix is still the authoritative list, because `archix options` is generated from the declarations themselves, and it includes options your own modules declare.

All of these modules are built in and loaded before your `init.lua`. You never import them.

## Listing every option

```console
$ archix options
```

Each entry shows the option's path, type, default, description and the module that declared it:

```
repos
    type: list of one of "core", "extra", "multilib", "core-testing", "extra-testing", "multilib-testing"
    default: ["core", "extra"]
    Official repositories packages are resolved from, in priority order.
    declared in <archix>/packages.lua
```

Options declared by your own modules appear in the same list, with your file in `declared in`. In paths, `<name>` stands for any key of an `attrs_of` option, and `*` for any element of a list.

For tooling, `--json` prints an array of objects:

```console
$ archix options --json | jq '.[] | select(.path == "packages")'
{
  "path": "packages",
  "type": "list of str",
  "default": [],
  "description": "Packages to install: from the configured repositories, else from the AUR. Their dependencies are installed too and recorded in archix.lock.",
  "declared_in": "<archix>/packages.lua"
}
```

Options with no default, such as a `custom_repos` entry's `name`, have `"default": null` in JSON and no `default:` line in the text output. Give such an option a value whenever you set its parent; this page marks them *required*. A module that reads one while it's unset fails with `has no value and no default`.

Types on this page are written the way `archix options` prints them. `null or str` is `types.nullable(types.str)`; set it to `archix.null` to clear it. See [Types](/docs/reference/lua-api/#types).

## Packages and repositories

### `packages`

| | |
| --- | --- |
| Type | `list of str` |
| Default | `[]` |
| Declared in | `<archix>/packages.lua` |

Packages to install: from the configured repositories, else from the AUR. Their dependencies are installed too and recorded in `archix.lock`.

List only what you want; dependencies are resolved and locked for you, and marked as dependencies in pacman's database. Names are checked offline against the sync databases and the AUR metadata, so a misspelled name fails before anything is downloaded. `archix install` adds names here through its own `installed.lua` file.

AUR package names go in the same list. archix looks each name up in your configured repos first, third-party repos included, and only then in the AUR. See [AUR packages](/docs/guides/aur/).

Like every list, `packages` concatenates across modules in definition order. Duplicates are harmless. Program modules add their own packages here, so `programs.kitty.enable = true` is enough to install kitty.

```lua
return {
  packages = { "git", "neovim", "ripgrep", "zen-browser-bin" },
}
```

To replace the list a shared module built up instead of adding to it, force it:

```lua
packages = archix.force { "base", "linux", "linux-firmware" },
```

### `repos`

| | |
| --- | --- |
| Type | `list of one of "core", "extra", "multilib", "core-testing", "extra-testing", "multilib-testing"` |
| Default | `["core", "extra"]` |
| Declared in | `<archix>/packages.lua` |

Official repositories packages are resolved from, in priority order.

All of them are locked against the Arch Linux Archive snapshot in `archix.lock`. Add `multilib` for Steam and Wine. The testing repos are for testing; packages there are locked like any other but change often.

The default only applies when no module sets `repos`. As soon as one does, the default is dropped, so always list every repo you want. `repos = { "multilib" }` on its own would leave you without `core` and `extra`:

```lua title="lua/hosts/desktop.lua"
return {
  repos = { "core", "extra", "multilib" },
}
```

If several modules set `repos`, their lists concatenate in definition order, and that order is the priority order. Duplicate entries are ignored, and an empty list is an error: packages need at least one repository.

Third-party repos such as CachyOS's are declared with [`custom_repos`](#custom_repos).

### `undeclared_packages`

| | |
| --- | --- |
| Type | `one of "remove", "warn"` |
| Default | `"remove"` |
| Declared in | `<archix>/packages.lua` |

What switch does with installed packages that aren't in the lock: remove them, or only list them.

With `"remove"`, a package you installed with `pacman -S` and never declared is removed on the next switch, so the system is exactly what the config says. With `"warn"`, archix keeps it and lists it in `archix diff` and `archix switch`.

`"warn"` is a good setting while you move an existing system into your config. Switch back to the default once nothing is listed any more. See [pacman and yay](/docs/concepts/pacman-and-yay/).

```lua title="lua/hosts/desktop.lua"
return {
  undeclared_packages = "warn",
}
```

### `ignored_packages`

| | |
| --- | --- |
| Type | `list of str` |
| Default | `[]` |
| Declared in | `<archix>/packages.lua` |

Installed packages archix leaves alone and never reports as undeclared.

Use it for packages you deliberately manage by hand, such as a driver you build yourself or a tool you test from a local checkout. An ignored package is never removed, whatever `undeclared_packages` says, and never shows up in `diff`. Like `packages`, the list concatenates across modules. `archix init` puts foreign packages that aren't on the AUR here.

```lua title="lua/modules/common.lua"
return {
  ignored_packages = { "nvidia-dkms-custom" },
}
```

Ignoring only affects packages that aren't in the lock. A package you also declare in `packages` is managed as usual.

## AUR

See [AUR packages](/docs/guides/aur/) for how AUR packages are locked, reviewed and built.

### `aur.url`

| | |
| --- | --- |
| Type | `str` |
| Default | `"https://aur.archlinux.org"` |
| Declared in | `<archix>/aur.lua` |

The AUR to look packages up in and clone build recipes from.

### `aur.review`

| | |
| --- | --- |
| Type | `bool` |
| Default | `true` |
| Declared in | `<archix>/aur.lua` |

Show each AUR build recipe entering the lock (the PKGBUILD, or a diff) and ask before trusting it.

A commit already in the lock is never shown again. `--noconfirm` skips the review for one run; `aur.review = false` skips it always.

### `aur.build_on_host`

| | |
| --- | --- |
| Type | `list of str` |
| Default | `[]` |
| Declared in | `<archix>/aur.lua` |

AUR packages built directly on the host with makepkg instead of in a clean chroot.

Their build dependencies are installed from the locked snapshot for the build, then removed. Use it for packages that don't build in a chroot.

```lua
aur = { build_on_host = { "some-driver-dkms" } },
```

### `missing_versions`

| | |
| --- | --- |
| Type | `one of "ask", "newer", "older"` |
| Default | `"ask"` |
| Declared in | `<archix>/aur.lua` |

When a locked AUR version can't be built any more: ask (defaulting to the nearest newer version), or always take the nearest newer or older one.

A version can't be built when its commit is gone from the package's history or its build fails. Whichever version is taken, the lock follows. `switch --accept-nearest newer|older` decides for one run.

## Third-party repositories

See [Third-party repos](/docs/guides/third-party-repos/) for a full walkthrough.

### `custom_repos`

| | |
| --- | --- |
| Type | `list of submodule` |
| Default | `[]` |
| Declared in | `<archix>/packages.lua` |

Third-party repositories. They have no dated archive, so archix keeps a copy of every package it installs from them.

Each entry has these fields:

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `name` | `str` | *required* | Repository name, as in `pacman.conf`. |
| `servers` | `list of str` | *required* | Server URLs, tried in order; `$repo` and `$arch` are expanded like pacman does. |
| `keys` | `list of str` | `[]` | Fingerprints of the repository's signing keys; archix imports and locally signs them. |
| `keyserver` | `str` | `"hkps://keyserver.ubuntu.com"` | Where the `keys` are fetched from. |
| `sig_level` | `str` | `"Required DatabaseOptional"` | The repository's `SigLevel` in `pacman.conf`. |
| `before_official` | `bool` | `false` | List the repository before core and extra, so its packages win (CachyOS's repos need this). |

A name that clashes with an official repo or another custom repo is an error. archix writes a block for each repo into `/etc/pacman.conf`, so plain pacman sees the same repos, and removes the block when the repo leaves the config.

```lua
custom_repos = {
  {
    name = "chaotic-aur",
    servers = { "https://cdn-mirror.chaotic.cx/$repo/$arch" },
    keys = { "3056513887B78AEB" },
  },
},
```

### `third_party_fallback`

| | |
| --- | --- |
| Type | `one of "ask", "fallback", "fail"` |
| Default | `"ask"` |
| Declared in | `<archix>/packages.lua` |

When a locked package from a custom repo is gone: ask (the answer defaults to falling back), fall back without asking, or fail. Falling back tries the repo's current version, then the AUR, then the official repos.

The lock follows whatever replaces the package, and an AUR replacement goes through review. With `"ask"`, `--noconfirm` falls back.

## Generations

### `generations.keep`

| | |
| --- | --- |
| Type | `int` |
| Default | `10` |
| Declared in | `<archix>/generations.lua` |

Number of newest generations gc always keeps.

### `generations.keep_days`

| | |
| --- | --- |
| Type | `int` |
| Default | `30` |
| Declared in | `<archix>/generations.lua` |

gc also keeps every generation younger than this many days.

`archix gc` runs after every switch. It always keeps the current generation, then deletes cached packages, file contents and snapshot databases no kept generation needs. See [Generations](/docs/concepts/generations/).

```lua
generations = { keep = 5, keep_days = 14 },
```

## System

Every setting in this section renders to files archix manages. Their contents are stored with each generation, so `archix rollback` restores them along with the packages. A file archix replaces is backed up and comes back when it leaves the config; a managed file edited by hand is saved before it's overwritten, and `diff` flags it beforehand. See [Safety](/docs/concepts/safety/).

### `files`

| | |
| --- | --- |
| Type | `attrs of submodule` |
| Default | `{}` |
| Declared in | `<archix>/system.lua` |

Files archix writes, keyed by absolute path; give one of `text`, `source` or `link`. Files archix replaces are backed up and restored when they leave the config.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `text` | `null or lines` | `null` | The file's contents. A final newline is added if missing. |
| `source` | `null or str` | `null` | A file in the config directory to copy, as a path relative to it. |
| `link` | `null or str` | `null` | Make the file a symlink to this target. |
| `mode` | `str` | `"0644"` | Octal permissions. |
| `owner` | `str` | `"root"` | |
| `group` | `str` | `"root"` | |

Give exactly one of `text`, `source` and `link`. `text` is a `lines` value, so several modules can each add lines to the same file. A `source` path can't leave the config directory.

archix doesn't write under `/usr` (except `/usr/local`), `/proc`, `/sys`, `/dev`, `/run`, `/tmp`, `/home` or `/boot`. Files in a home directory go in [`home.files`](#homefiles).

```lua
files = {
  ["/etc/motd"] = { text = "Managed by archix." },
  ["/etc/sudoers.d/wheel"] = { text = "%wheel ALL=(ALL:ALL) ALL", mode = "0440" },
  ["/etc/X11/xorg.conf.d/30-touchpad.conf"] = { source = "files/30-touchpad.conf" },
  ["/etc/resolv.conf"] = { link = "/run/systemd/resolve/stub-resolv.conf" },
},
```

### `units`

| | |
| --- | --- |
| Type | `attrs of lines` |
| Default | `{}` |
| Declared in | `<archix>/system.lua` |

systemd system units, keyed by file name such as `foo.service`. They're written to `/etc/systemd/system`. Declaring a unit doesn't enable it; list it in [`services`](#services) too.

```lua
units = {
  ["backup.service"] = [[
[Unit]
Description=Nightly backup

[Service]
Type=oneshot
ExecStart=/usr/local/bin/backup
]],
},
```

### `user_units`

| | |
| --- | --- |
| Type | `attrs of lines` |
| Default | `{}` |
| Declared in | `<archix>/system.lua` |

systemd user units for every user, keyed by file name. They're written to `/etc/systemd/user`. For units in one user's home, use [`home.systemd`](#homesystemd).

### `services`

| | |
| --- | --- |
| Type | `attrs of bool` |
| Default | `{}` |
| Declared in | `<archix>/system.lua` |

System units to enable and start (`true`) or disable and stop (`false`).

Units are stopped and disabled before their files go away, and enabled and started after `daemon-reload`. A unit that fails to start doesn't undo the switch: the generation still becomes current, the failures are listed, and archix exits non-zero.

```lua
services = { ["sshd.service"] = true, ["bluetooth.service"] = true, ["cups.service"] = false },
```

### `user_services`

| | |
| --- | --- |
| Type | `attrs of bool` |
| Default | `{}` |
| Declared in | `<archix>/system.lua` |

User units to enable (`true`) or disable (`false`) for every user, with `systemctl --global`.

```lua
user_services = { ["pipewire-pulse.socket"] = true },
```

### `users`

| | |
| --- | --- |
| Type | `attrs of submodule` |
| Default | `{}` |
| Declared in | `<archix>/system.lua` |

Users archix creates and keeps in step; ones it created are removed (home kept) when they leave the config. Passwords are never managed: set them with `passwd`.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `uid` | `null or int` | `null` | Picked by `useradd` when null. |
| `group` | `null or str` | `null` | Primary group; by default one named after the user. |
| `groups` | `list of str` | `[]` | Supplementary groups. |
| `shell` | `path` | `"/bin/bash"` | |
| `home` | `null or path` | `null` | The home directory; `/home/<name>` when null. |
| `description` | `str` | `""` | The GECOS field. |
| `system` | `bool` | `false` | A system user: low uid, no home directory. |

Declaring a user that already exists brings it in line with every field, defaults included: `groups` is the complete list of supplementary groups, and `shell` falls back to `/bin/bash`. Users that archix didn't create are never deleted.

```lua
users = {
  me = { groups = { "wheel", "video" }, shell = "/usr/bin/zsh", description = "Me" },
},
```

### `groups`

| | |
| --- | --- |
| Type | `attrs of submodule` |
| Default | `{}` |
| Declared in | `<archix>/system.lua` |

Groups archix creates and keeps in step.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `gid` | `null or int` | `null` | Picked by `groupadd` when null. |
| `system` | `bool` | `false` | A system group. |

```lua
groups = { media = {}, builders = { gid = 1500 } },
```

### `hostname`

| | |
| --- | --- |
| Type | `null or str` |
| Default | `null` |
| Declared in | `<archix>/system.lua` |

Written to `/etc/hostname`. Null leaves the file alone.

### `timezone`

| | |
| --- | --- |
| Type | `null or str` |
| Default | `null` |
| Declared in | `<archix>/system.lua` |

A zoneinfo name such as `Europe/Istanbul`. archix points `/etc/localtime` at it.

### `locales`

| | |
| --- | --- |
| Type | `list of str` |
| Default | `[]` |
| Declared in | `<archix>/system.lua` |

Locales to generate, such as `en_US.UTF-8`; the first becomes `LANG`.

archix writes `/etc/locale.gen` (the charset is taken from the name, `ISO-8859-1` without one) and `/etc/locale.conf`.

```lua
locales = { "en_US.UTF-8", "tr_TR.UTF-8" },
```

### `keymap`

| | |
| --- | --- |
| Type | `null or str` |
| Default | `null` |
| Declared in | `<archix>/system.lua` |

The console keymap, written to `/etc/vconsole.conf`.

### `sysctl`

| | |
| --- | --- |
| Type | `attrs of any` |
| Default | `{}` |
| Declared in | `<archix>/system.lua` |

Kernel settings applied with sysctl, written to `/etc/sysctl.d/90-archix.conf`. Booleans become `1` and `0`.

```lua
sysctl = { ["vm.swappiness"] = 10, ["net.ipv4.ip_forward"] = true },
```

### `kernel_modules`

| | |
| --- | --- |
| Type | `list of str` |
| Default | `[]` |
| Declared in | `<archix>/system.lua` |

Kernel modules to load at boot, written to `/etc/modules-load.d/archix.conf`.

### `modprobe`

| | |
| --- | --- |
| Type | `attrs of lines` |
| Default | `{}` |
| Declared in | `<archix>/system.lua` |

modprobe.d snippets, keyed by file name without `.conf`.

```lua
modprobe = { nvidia = "options nvidia_drm modeset=1" },
```

### `udev_rules`

| | |
| --- | --- |
| Type | `attrs of lines` |
| Default | `{}` |
| Declared in | `<archix>/system.lua` |

udev rules, keyed by file name without `.rules`. They're written to `/etc/udev/rules.d`.

```lua
udev_rules = {
  ["60-ioschedulers"] = [[ACTION=="add|change", KERNEL=="nvme[0-9]n[0-9]", ATTR{queue/scheduler}="none"]],
},
```

## Boot and kernel

archix can write a boot entry for every generation, so you can pick an older generation from the boot menu. Each entry is a unified kernel image, built with `ukify` from a fresh `mkinitcpio` initramfs, and placed in `EFI/Linux` on the ESP as `archix-<n>.efi`, where systemd-boot finds it. Booting an older entry rolls the system back to that generation. Install `mkinitcpio` and `systemd-ukify`, and `sbctl` for Secure Boot.

```lua
boot = { loader = "systemd-boot", microcode = { "amd-ucode.img" } },
kernel_params = { "root=UUID=0a1b2c3d-...", "rw", "quiet" },
```

### `kernel_params`

| | |
| --- | --- |
| Type | `list of str` |
| Default | `[]` |
| Declared in | `<archix>/system.lua` |

Kernel command line parameters, written to `/etc/kernel/cmdline` for boot entries.

With `boot.loader` set, this is the whole command line, so it must contain `root=`; archix refuses to switch without it. Each entry adds `archix.generation=<n>`.

### `boot.loader`

| | |
| --- | --- |
| Type | `null or one of "systemd-boot"` |
| Default | `null` |
| Declared in | `<archix>/boot.lua` |

The boot loader archix writes a boot entry per generation for; null leaves booting alone.

### `boot.esp`

| | |
| --- | --- |
| Type | `path` |
| Default | `"/boot"` |
| Declared in | `<archix>/boot.lua` |

Where the EFI system partition is mounted.

### `boot.kernel`

| | |
| --- | --- |
| Type | `str` |
| Default | `"linux"` |
| Declared in | `<archix>/boot.lua` |

The kernel package boot entries use, such as `linux-lts` or `linux-zen`. Each entry keeps a copy of its kernel's modules, so an older entry still boots after the kernel package was upgraded.

### `boot.microcode`

| | |
| --- | --- |
| Type | `list of str` |
| Default | `[]` |
| Declared in | `<archix>/boot.lua` |

Microcode images on the ESP loaded before the initramfs, such as `amd-ucode.img`. Paths are relative to `boot.esp`.

### `boot.entries`

| | |
| --- | --- |
| Type | `int` |
| Default | `5` |
| Declared in | `<archix>/boot.lua` |

How many of the newest generations keep a boot entry; fewer if the ESP runs out of room.

### `boot.secure_boot`

| | |
| --- | --- |
| Type | `null or bool` |
| Default | `null` |
| Declared in | `<archix>/boot.lua` |

Sign boot entries with sbctl; by default, whenever sbctl has keys.

When signing, archix also signs systemd-boot's own binary. Changing this records a new generation, so turning Secure Boot on re-signs.

### `boot.snapshots`

| | |
| --- | --- |
| Type | `bool` |
| Default | `true` |
| Declared in | `<archix>/boot.lua` |

On a btrfs root, take a read-only snapshot of `/` before every switch.

A snapshot is deleted along with its generation. It's a manual recovery point; booting an older generation works by reinstalling its packages, on any filesystem. This option applies whether or not `boot.loader` is set.

## Home

archix manages one user's home directory per machine. Set `home.user`, and declare the same user in [`users`](#users) so it exists before its files are written. Files in the home, and directories archix creates for them, are owned by that user.

```lua
users = { me = { groups = { "wheel" }, shell = "/usr/bin/zsh" } },
home = { user = "me" },
```

### `home.user`

| | |
| --- | --- |
| Type | `null or str` |
| Default | `null` |
| Declared in | `<archix>/home.lua` |

The user whose home archix manages: `home.files` and every `programs.*` module apply to them.

Setting `home.files`, or enabling a program, without `home.user` is an error.

### `home.directory`

| | |
| --- | --- |
| Type | `null or path` |
| Default | `null` |
| Declared in | `<archix>/home.lua` |

That user's home directory; defaults to `users.<user>.home`, else `/home/<user>`.

### `home.files`

| | |
| --- | --- |
| Type | `attrs of submodule` |
| Default | `{}` |
| Declared in | `<archix>/home.lua` |

Files in the user's home, keyed by path relative to it (a leading `~/` is allowed).

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `text` | `null or lines` | `null` | The file's contents, written as given. |
| `source` | `null or str` | `null` | A file or directory in the config directory, as a path relative to it. |
| `live` | `bool` | `false` | Symlink to `source` in the config directory instead of copying it, so apps that write their own config edit your repo. |
| `link` | `null or str` | `null` | Make the file a symlink to this target. |
| `executable` | `bool` | `false` | Write the file with mode `0755` instead of `0644`. |

Give exactly one of `text`, `source` and `link`. A `source` is copied, and a directory `source` needs `live = true`. Paths can't leave the home or the config directory. See [Dotfiles and apps](/docs/guides/dotfiles-and-apps/).

```lua
home = {
  user = "me",
  files = {
    [".config/nvim"] = { source = "home/.config/nvim", live = true },
    ["bin/hello"] = { text = "#!/bin/sh\necho hi\n", executable = true },
    [".config/user-dirs.dirs"] = { source = "home/user-dirs.dirs" },
  },
},
```

### `home.systemd`

| | |
| --- | --- |
| Type | `attrs of attrs of any` |
| Default | `{}` |
| Declared in | `<archix>/home.lua` |

The user's systemd units, by kind (`services`, `timers`, `sockets`, `paths`, `targets`) and name, as sections of keys; `Install.WantedBy` creates the `.wants` links.

`slices`, `mounts` and `automounts` work too. Units are written to `~/.config/systemd/user/<name>.<kind>`. A list value repeats the key.

```lua
home = {
  systemd = {
    services = {
      sync = { Unit = { Description = "Sync notes" }, Service = { ExecStart = "/usr/bin/rclone sync notes: %h/notes" } },
    },
    timers = {
      sync = { Timer = { OnCalendar = "hourly" }, Install = { WantedBy = { "timers.target" } } },
    },
  },
},
```

## Programs

Each `programs.<name>` module installs an app and writes its config into `home.user`'s home. They need `home.user`, and they're off until you set `enable = true`.

Every program has a `package` option, the package `enable` adds to [`packages`](#packages). Set it to `archix.null` to configure an app without installing it.

The modules for kitty, git, zsh, starship, fastfetch, `mime_apps` and user units are ported from [Home Manager](https://github.com/nix-community/home-manager), and render the same files Home Manager does. Option names are snake_case versions of Home Manager's: `mouseBindings` is `mouse_bindings`, `signByDefault` is `sign_by_default`. See [Dotfiles and apps](/docs/guides/dotfiles-and-apps/#3-ported-home-manager-modules).

### `programs.kitty`

Writes `~/.config/kitty/kitty.conf`, plus `quick-access-terminal.conf` and `diff.conf` when they have settings.

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `enable` | `bool` | `false` | Install and configure kitty. |
| `package` | `null or str` | `"kitty"` | |
| `font` | `null or submodule` | `null` | `name` (`str`, required) and `size` (`any`): points; 8 renders as 8, 8.5 as 8.500000, like Home Manager. |
| `settings` | `attrs of any` | `{}` | kitty.conf settings; booleans become yes/no. |
| `keybindings` | `attrs of str` | `{}` | `map` lines, keyed by shortcut. |
| `mouse_bindings` | `attrs of str` | `{}` | `mouse_map` lines. |
| `action_aliases` | `attrs of str` | `{}` | `action_alias` lines. |
| `environment` | `attrs of str` | `{}` | `env` lines. |
| `extra_config` | `lines` | `""` | Appended to kitty.conf. |
| `quick_access_terminal_config` | `attrs of any` | `{}` | `quick-access-terminal.conf` settings. |
| `diff_config.settings` | `attrs of any` | `{}` | `diff.conf` settings. |
| `diff_config.keybindings` | `attrs of str` | `{}` | `diff.conf` key mappings. |
| `diff_config.extra_config` | `lines` | `""` | Appended to `diff.conf`. |
| `shell_integration.mode` | `null or str` | `"no-rc"` | kitty's `shell_integration`; `no-rc` is always implied, null turns integration off. |
| `shell_integration.enable_zsh_integration` | `bool` | `true` | Load kitty's shell integration from `~/.zshrc` when `programs.zsh` is enabled. |

```lua
programs = {
  kitty = {
    enable = true,
    font = { name = "JetBrainsMono Nerd Font", size = 11 },
    settings = { background_opacity = "0.95", confirm_os_window_close = 0, enable_audio_bell = false },
    keybindings = { ["ctrl+shift+t"] = "new_tab_with_cwd" },
  },
},
```

### `programs.git`

Writes `~/.config/git/config`, and `ignore`, `attributes` and `allowed_signers` next to it when they have content.

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `enable` | `bool` | `false` | Install and configure git. |
| `package` | `null or str` | `"git"` | |
| `settings` | `any` | `{}` | git config as nested tables: `{ user = { name = ... } }`; deeper levels become `[section "sub"]`. |
| `signing` | `null or submodule` | `null` | Commit signing; fields below. |
| `signing.key` | `null or str` | `null` | Sets `user.signingKey`. |
| `signing.format` | `null or one of "openpgp", "ssh", "x509"` | `null` | Sets `gpg.format` and the matching signer program. |
| `signing.signer` | `null or str` | `null` | The signer program; `/usr/bin/gpg`, `/usr/bin/ssh-keygen` or `/usr/bin/gpgsm` by default, depending on `format`. |
| `signing.sign_by_default` | `null or bool` | `null` | Sets `commit.gpgSign` and `tag.gpgSign`. |
| `signing.allowed_signers` | `lines` | `""` | Written to `~/.config/git/allowed_signers` and set as `gpg.ssh.allowedSignersFile`. |
| `ignores` | `list of str` | `[]` | Lines of the global ignore file. |
| `attributes` | `list of str` | `[]` | Lines of the global attributes file. |
| `includes` | `list of submodule` | `[]` | `path` (`str`, required) and `condition` (`null or str`): an `[include]`, or an `[includeIf "<condition>"]`. |
| `lfs.enable` | `bool` | `false` | Install git-lfs and set up its filter. |
| `lfs.skip_smudge` | `bool` | `false` | Don't download LFS files on checkout. |

Values in `settings` win over the ones `signing` and `lfs` derive.

```lua
programs = {
  git = {
    enable = true,
    settings = {
      user = { name = "Me", email = "me@example.com" },
      init = { defaultBranch = "main" },
      url = { ["git@github.com:"] = { insteadOf = "https://github.com/" } },
    },
    signing = { format = "ssh", key = "/home/me/.ssh/id_ed25519.pub", sign_by_default = true },
    ignores = { ".direnv", "*.swp" },
    includes = { { path = "~/work/.gitconfig", condition = "gitdir:~/work/" } },
  },
},
```

### `programs.zsh`

Writes `~/.zshrc` and `~/.zshenv`, plus `~/.zprofile`, `~/.zlogin` and `~/.zlogout` when they have content. Plugins come from Arch's packages in `/usr/share/zsh/plugins`, installed for you when enabled. To make zsh your login shell, also set `users.<name>.shell = "/usr/bin/zsh"`.

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `enable` | `bool` | `false` | Install zsh and manage `~/.zshrc`. |
| `package` | `null or str` | `"zsh"` | |
| `enable_completion` | `bool` | `true` | Run `completion_init`. |
| `completion_init` | `lines` | `"autoload -U compinit && compinit"` | |
| `default_keymap` | `null or one of "emacs", "viins", "vicmd"` | `null` | |
| `cdpath` | `list of str` | `[]` | |
| `autocd` | `null or bool` | `null` | Sets or unsets `AUTO_CD`. |
| `autosuggestion.enable` | `bool` | `false` | Install and load zsh-autosuggestions. |
| `autosuggestion.strategy` | `list of str` | `["history"]` | `ZSH_AUTOSUGGEST_STRATEGY`. |
| `autosuggestion.highlight` | `null or str` | `null` | `ZSH_AUTOSUGGEST_HIGHLIGHT_STYLE`. |
| `syntax_highlighting.enable` | `bool` | `false` | Install and load zsh-syntax-highlighting. |
| `syntax_highlighting.highlighters` | `list of str` | `[]` | Highlighters besides `main`. |
| `syntax_highlighting.styles` | `attrs of str` | `{}` | `ZSH_HIGHLIGHT_STYLES` entries. |
| `syntax_highlighting.patterns` | `attrs of str` | `{}` | `ZSH_HIGHLIGHT_PATTERNS` entries. |
| `history_substring_search.enable` | `bool` | `false` | Install and load zsh-history-substring-search. |
| `history_substring_search.search_up_key` | `any` | `["^[[A"]` | A key or list of keys. |
| `history_substring_search.search_down_key` | `any` | `["^[[B"]` | A key or list of keys. |
| `history.size` | `int` | `10000` | `HISTSIZE`. |
| `history.save` | `null or int` | `null` | `SAVEHIST`. Defaults to size. |
| `history.path` | `null or str` | `null` | Defaults to `~/.zsh_history`. Relative paths are relative to the home. |
| `history.ignore_patterns` | `list of str` | `[]` | `HISTORY_IGNORE` patterns. |
| `history.append` | `bool` | `false` | `APPEND_HISTORY`. |
| `history.ignore_dups` | `bool` | `true` | `HIST_IGNORE_DUPS`. |
| `history.ignore_all_dups` | `bool` | `false` | `HIST_IGNORE_ALL_DUPS`. |
| `history.save_no_dups` | `bool` | `false` | `HIST_SAVE_NO_DUPS`. |
| `history.find_no_dups` | `bool` | `false` | `HIST_FIND_NO_DUPS`. |
| `history.ignore_space` | `bool` | `true` | `HIST_IGNORE_SPACE`. |
| `history.expire_duplicates_first` | `bool` | `false` | `HIST_EXPIRE_DUPS_FIRST`. |
| `history.extended` | `bool` | `false` | `EXTENDED_HISTORY`. |
| `history.share` | `bool` | `true` | `SHARE_HISTORY`. |
| `set_options` | `list of str` | `[]` | Extra `setopt` options. |
| `shell_aliases` | `attrs of str` | `{}` | |
| `shell_global_aliases` | `attrs of str` | `{}` | `alias -g` aliases. |
| `dir_hashes` | `attrs of str` | `{}` | Named directories, `hash -d`. |
| `session_variables` | `attrs of any` | `{}` | Exported in `~/.zshenv`. |
| `init_content` | `lines` | `""` | Extra lines for `~/.zshrc`; other modules add their shell integration here. |
| `env_extra` | `lines` | `""` | Appended to `~/.zshenv`. |
| `profile_extra` | `lines` | `""` | Contents of `~/.zprofile`. |
| `login_extra` | `lines` | `""` | Contents of `~/.zlogin`. |
| `logout_extra` | `lines` | `""` | Contents of `~/.zlogout`. |

```lua
programs = {
  zsh = {
    enable = true,
    autosuggestion = { enable = true },
    syntax_highlighting = { enable = true },
    history = { size = 50000, ignore_all_dups = true },
    shell_aliases = { ll = "ls -l", g = "git" },
    session_variables = { EDITOR = "nvim" },
    init_content = "bindkey '^R' history-incremental-search-backward",
  },
},
```

### `programs.starship`

Writes `~/.config/starship.toml` when `settings` isn't empty.

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `enable` | `bool` | `false` | Install and configure the starship prompt. |
| `package` | `null or str` | `"starship"` | |
| `settings` | `any` | `{}` | `starship.toml` contents. |
| `enable_zsh_integration` | `bool` | `true` | Start starship from `~/.zshrc` when `programs.zsh` is enabled. |

```lua
programs = {
  starship = {
    enable = true,
    settings = { add_newline = false, character = { success_symbol = "[>](bold green)" } },
  },
},
```

### `programs.fastfetch`

Writes `~/.config/fastfetch/config.jsonc` when `settings` isn't empty.

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `enable` | `bool` | `false` | Install and configure fastfetch. |
| `package` | `null or str` | `"fastfetch"` | |
| `settings` | `any` | `{}` | `config.jsonc` contents. |

### `programs.alacritty`, `programs.foot`, `programs.mpv`

These are thin modules, built with [`archix.program`](/docs/reference/lua-api/#archixprogram). `settings` passes straight through to the app's config file in its format.

| Program | File | Format |
| --- | --- | --- |
| `alacritty` | `~/.config/alacritty/alacritty.toml` | `toml` |
| `foot` | `~/.config/foot/foot.ini` | `ini` |
| `mpv` | `~/.config/mpv/mpv.conf` | `ini` |

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `enable` | `bool` | `false` | Install and configure the program. |
| `package` | `null or str` | the program's name | |
| `settings` | `any` | `{}` | The file's contents, written in the format above. |
| `extra_config` | `lines` | `""` | Text appended to the file. |

```lua
programs = {
  alacritty = { enable = true, settings = { window = { opacity = 0.9 }, font = { size = 11 } } },
  foot = { enable = true, settings = { main = { font = "JetBrains Mono:size=11", pad = "8x8" } } },
  mpv = { enable = true, settings = { hwdec = "auto-safe", ["keep-open"] = "yes" } },
},
```

### `mime_apps`

Default applications per MIME type. Writes `~/.config/mimeapps.list`. It lives at the top level, not under `programs`, and installs nothing.

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `enable` | `bool` | `false` | Manage `~/.config/mimeapps.list`. |
| `associations.added` | `attrs of any` | `{}` | Extra apps per MIME type: a desktop file name or a list of them. |
| `associations.removed` | `attrs of any` | `{}` | Apps removed per MIME type, in the same form. |
| `default_applications` | `attrs of any` | `{}` | Default apps per MIME type, in the same form. |
| `default_application_packages` | `list of submodule` | `[]` | Apps that become the default for every MIME type they declare, after `default_applications`. |
| `default_application_packages.*.desktop` | `str` | *required* | The package's desktop file name, such as `firefox.desktop`. |
| `default_application_packages.*.mime_types` | `list of str` | *required* | The MIME types it handles. |

```lua
mime_apps = {
  enable = true,
  default_applications = {
    ["application/pdf"] = "org.pwmt.zathura.desktop",
    ["x-scheme-handler/https"] = { "firefox.desktop", "chromium.desktop" },
  },
},
```
