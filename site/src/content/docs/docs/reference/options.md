---
title: Options reference
description: The core archix options, packages, repos, programs and files, and how to list every option available on your system.
sidebar:
  order: 2
---

This page documents the core options you'll set in almost every config. archix declares many more, for services, users and groups, `/etc` files, kernel parameters, sysctl, udev, modprobe, third-party repos and every ported `programs.*` module. Your installed archix is the authoritative list, because it's generated from the declarations themselves.

## Listing every option

```console
$ archix options
```

Each entry shows the option's path, type, default, description and the module that declared it:

```
repos
    type: list of one of "core", "extra", "multilib", "core-testing", "extra-testing", "multilib-testing"
    default: ["core","extra"]
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
  "description": "Packages from the official repositories. Their dependencies are installed too and recorded in archix.lock.",
  "declared_in": "<archix>/packages.lua"
}
```

Options with no default have `"default": null` in JSON and no `default:` line in the text output.

## `packages`

| | |
| --- | --- |
| Type | `list of str` |
| Default | `[]` |
| Declared in | `<archix>/packages.lua` |

Packages from the official repositories. Their dependencies are installed too and recorded in `archix.lock`.

List only what you want; dependencies are resolved and locked for you, and marked as dependencies in pacman's database. Names are checked offline against the sync databases and the AUR metadata, so a misspelled name fails before anything is downloaded. AUR package names go in the same list. `archix install` adds names here through its own data file.

Like every list, `packages` concatenates across modules in definition order. Duplicates are harmless.

```lua
return {
  packages = { "git", "neovim", "ripgrep", "zen-browser-bin" },
}
```

To replace the list a shared module built up instead of adding to it, force it:

```lua
packages = archix.force { "base", "linux", "linux-firmware" },
```

## `repos`

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

If several modules set `repos`, their lists concatenate in definition order, and that order is the priority order.

Duplicate entries are ignored, and an empty list is an error: packages need at least one repository.

Third-party repos such as CachyOS's are declared separately; see [Third-party repos](/docs/guides/third-party-repos/).

## `programs.<name>`

Every app module, whether shipped with archix or defined with [`archix.program`](/docs/reference/lua-api/#archixprogram), lives under `programs.<name>`. They all have these two options.

### `programs.<name>.enable`

| | |
| --- | --- |
| Type | `bool` |
| Default | `false` |

Install the app's package and write its config file.

### `programs.<name>.settings`

A table passed straight through to the app's config file, rendered in the app's format. Any key the app understands works, including ones archix has never heard of.

```lua
return {
  programs = {
    kitty = {
      enable = true,
      settings = { font_size = 11, background_opacity = "0.95" },
    },
    foot = {
      enable = true,
      settings = { main = { font = "JetBrains Mono:size=11" } },
    },
  },
}
```

Modules ported from Home Manager add more options next to these, with Home Manager's names, types and defaults. Run `archix options` to see them. See [Dotfiles and apps](/docs/guides/dotfiles-and-apps/).

## `files`

Managed files, keyed by path. Each value is the file's content, usually produced by a [format writer](/docs/reference/lua-api/#format-writers). Paths starting with `~/` are written into a home directory, as that home's user.

```lua
return {
  files = {
    ["~/.config/foot/foot.ini"] = archix.ini {
      main = { font = "JetBrains Mono:size=11" },
    },
  },
}
```

archix records the hash of every file it writes. Changes made outside archix show up in `archix diff` and are never silently overwritten. See [Dotfiles and apps](/docs/guides/dotfiles-and-apps/).
