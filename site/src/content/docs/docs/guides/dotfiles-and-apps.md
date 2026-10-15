---
title: Dotfiles and apps
description: Manage dotfiles and app configs with archix, from raw files and format writers to programs.* modules ported from Home Manager.
sidebar:
  order: 3
---

archix manages your home directory with the same switch that manages the system. Dotfiles become part of the generation, so a rollback takes your configs back too.

## Real files, with hashes

archix writes dotfiles as real files, not symlinks into a store. Apps see ordinary files they can read, watch and even write. archix records the hash of every file it writes.

That hash is how archix notices out-of-band edits. If you, or an app, change a managed file, `archix diff` shows it, and archix never silently overwrites the change on the next switch:

```console
$ archix diff
snapshot 2026-10-15: 1214 packages
the system matches the lock
file       ~/.config/foot/foot.ini (changed outside archix)
```

Move the change into your config, or let the switch put the declared version back.

## Live symlinks for app-written configs

Some files are written by apps on purpose. matugen regenerates color files from your wallpaper; settings dialogs save their state. Managing those as declared content would fight the app on every switch.

For those, archix keeps a live symlink instead: the file in your home links to a file in your config directory, the app writes through the link, and the change lands in your config repo where git can see it. Nothing is reported as drift, because the config is the file.

## Homes of every declared user

archix manages the home of every user declared in your config. It runs as root, but drops to each user before touching their home, so files are created with that user's ownership and permissions.

## Three layers

How much you write depends on how much help an app needs. archix gives you three layers, from raw to fully modelled.

### 1. Format writers

Any app, no module needed. Pick the writer for the app's config format, give it a Lua table, and assign the result to a path in `files`:

```lua title="lua/modules/foot.lua"
return {
  packages = { "foot" },
  files = {
    ["~/.config/foot/foot.ini"] = archix.ini {
      main = { font = "JetBrains Mono:size=11", pad = "8x8" },
      colors = { alpha = 0.95 },
    },
  },
}
```

renders:

```ini title="~/.config/foot/foot.ini"
[colors]
alpha=0.95

[main]
font=JetBrains Mono:size=11
pad=8x8
```

Keys come out sorted, because evaluation iterates tables in sorted order. That keeps the rendered file, and its hash, identical on every run.

Writers exist for `toml`, `json`, `yaml`, `ini`, `kdl`, `key-value` and `kconfig` (KDE's format):

```lua
files = {
  ["~/.config/alacritty/alacritty.toml"] = archix.toml { window = { opacity = 0.9 } },
  ["~/.config/kdeglobals"] = archix.kconfig { General = { ColorScheme = "BreezeDark" } },
}
```

For a file that's easier to keep as-is, read it from your config directory:

```lua
files = {
  ["~/.config/nvim/init.lua"] = archix.read_file("dotfiles/nvim/init.lua"),
}
```

### 2. `archix.program`

When you configure the same app on several machines, give it a module. `archix.program` defines one in a single line:

```lua title="lua/modules/foot.lua"
return archix.program "foot" { package = "foot", file = "~/.config/foot/foot.ini", format = "ini" }
```

Import it, and every host gets a `programs.foot` option:

```lua title="lua/hosts/laptop.lua"
return {
  programs = {
    foot = {
      enable = true,
      settings = {
        main = { font = "JetBrains Mono:size=10" },
        colors = { alpha = 0.9 },
      },
    },
  },
}
```

`enable` installs the package and writes the file. `settings` passes straight through to the writer, so any key the app understands works, including ones added in the app's latest release. Because it's a real option, priorities work: a shared module can set `settings` with `archix.default`, and a host can override one value with `archix.force`.

Only use `archix.program` for apps archix doesn't already ship a module for. Declaring an option twice is an error.

### 3. Ported Home Manager modules

Some apps need more than pass-through: options that interact, or values worth validating. For those, archix ships hand-written `programs.*` modules ported from [Home Manager](https://github.com/nix-community/home-manager) (MIT, credited), such as git, zsh, ssh, kitty, starship and fastfetch.

```lua title="lua/modules/common.lua"
return {
  programs = {
    kitty = {
      enable = true,
      settings = { font_size = 11, background_opacity = "0.95", confirm_os_window_close = 0 },
    },
  },
}
```

If you're coming from Home Manager, the option names, types and defaults are taken from Home Manager's own option definitions, so your config translates closely. `archix options` lists everything available on your system:

```console
$ archix options | grep -A3 '^programs.kitty.enable'
programs.kitty.enable
    type: bool
    default: false
    Whether to enable kitty.
```

Ports are checked, not just translated. Each module's Home Manager test cases, an input config plus the expected rendered file, are ported along with it, and a module only ships when archix renders byte-identical output for the same input. A scheduled job watches Home Manager upstream, and when a ported module or its tests change, the port is updated against the new tests. Because `settings` passes through, new app keys need no archix change at all.

## Which layer to use

- One machine, one file, no logic: a format writer in `files`.
- The same app on several machines, with per-host tweaks: `archix.program`, or the shipped module if there is one.
- An app archix ships a module for: use the module. It knows the app's defaults and catches invalid values.
- A file an app rewrites itself: a live symlink.
