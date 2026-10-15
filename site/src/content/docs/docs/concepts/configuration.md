---
title: Configuration
description: How an archix config is laid out, how modules, imports and priorities merge into one typed configuration, and what Lua may and may not do.
sidebar:
  order: 1
---

An archix config is a directory of Lua 5.4 files. archix evaluates it into one merged, type-checked configuration for the current host, then makes the system match it.

## Layout

The layout follows neovim: an entry point at the top, and modules under `lua/`.

```
/etc/archix
├── init.lua
├── archix.lock
└── lua
    ├── hosts
    │   ├── desktop.lua
    │   └── laptop.lua
    └── modules
        ├── common.lua
        ├── gaming.lua
        └── desktop
            └── init.lua
```

- `init.lua` is the entry point. It usually does nothing but import other modules.
- `lua/hosts/<host>.lua` holds what's specific to one machine.
- `lua/modules/...` holds what you share between machines.
- `archix.lock` is generated. Commit it next to `init.lua`.

Module names are dotted paths under `lua/`. `"modules.common"` loads `lua/modules/common.lua`, and `"modules.desktop"` loads `lua/modules/desktop/init.lua` if there's no `lua/modules/desktop.lua`.

## Choosing the host

One config can describe all your machines. archix picks the host by the machine's hostname and exposes it as `archix.hostname`, so `init.lua` can import the right file:

```lua title="init.lua"
return {
  imports = {
    "modules.common",
    "hosts." .. archix.hostname,
  },
}
```

Pass `--host` to evaluate as a different machine. That's how you check or lock the laptop's config from the desktop:

```console
$ archix --host laptop diff
$ archix --host laptop lock
```

## Modules

Every Lua file you import is a module. A module returns a table with up to three keys:

```lua title="lua/modules/gaming.lua"
local t = archix.types

return {
  imports = { "modules.common" },
  options = {
    gaming = {
      enable = archix.option { type = t.bool, default = false, description = "Install Steam and friends" },
    },
  },
  config = function(cfg)
    if not cfg.gaming.enable then return {} end
    return {
      repos = { "core", "extra", "multilib" },
      packages = { "steam", "gamemode", "mangohud" },
    }
  end,
}
```

- `imports` lists other modules, by name or inline as tables or functions.
- `options` declares new options. Each one has a type, and optionally a default and a description.
- `config` sets options. It's a table, or a function that receives the merged config.

Most modules only set options, so there's a shortcut. A table with none of those three keys is treated as `config`:

```lua title="lua/hosts/desktop.lua"
return {
  gaming = { enable = true },
  packages = { "obs-studio" },
}
```

A bare function is treated as `config = function(cfg) ... end`.

A module that uses any of the three keys can't have other fields. archix rejects `{ imports = {...}, packages = {...} }` and tells you to put the settings under `config`, because otherwise it couldn't tell a setting from a typo.

### Imports

Each module is loaded once, however many times it's imported. That makes it safe for `gaming.lua` and `desktop.lua` to both import `modules.common`.

`require` also works, for plain Lua helpers that aren't modules:

```lua title="lua/lib/util.lua"
local M = {}
function M.when(cond, value) if cond then return value end return {} end
return M
```

```lua
local util = require("lib.util")
```

A require cycle is an error that names the chain.

### Options and types

Options make the config checkable. Every key you set must be declared somewhere, and every value must fit its type. Typos and wrong types are errors that name the file:

```console
$ archix eval
error: `programs.kity`: option does not exist (set in lua/hosts/desktop.lua); did you mean `programs.kitty`?
error: `gaming.enable`: expected bool, got string "yes" in lua/hosts/laptop.lua
```

archix ships declarations for everything it manages, and you can declare your own, as `gaming.enable` above shows. The available types are `bool`, `int`, `float`, `str`, `path`, `lines`, `any`, `enum { ... }`, `list_of(t)`, `attrs_of(t)`, `nullable(t)` and `submodule { ... }`. The [Lua API reference](/docs/reference/lua-api/#types) describes each one.

Because declarations carry descriptions, archix can list them:

```console
$ archix options
gaming.enable
    type: bool
    default: false
    Install Steam and friends
    declared in lua/modules/gaming.lua
packages
    type: list of str
    default: []
    Packages from the official repositories. Their dependencies are installed too and recorded in archix.lock.
    declared in <archix>/packages.lua
...
```

### Function config and fixpoint rounds

`config = function(cfg)` receives the final merged config of every module, read-only. This is how one module reacts to another, like NixOS's `config.` references.

Lua isn't lazy, so archix gets there by repetition. It merges all static config, runs every function module against that result, merges again, and repeats until the output stops changing. If they haven't settled after 32 rounds, evaluation fails and lists the options that kept changing:

```console
error: config did not settle after 32 rounds; modules keep changing `packages`
```

That usually means a function adds something based on a value it also changes, such as appending to `packages` based on the length of `packages`.

A few rules keep this sound:

- `cfg` is read-only. Assigning to it is an error: return definitions instead.
- Reading an option that doesn't exist is an error, with a suggestion.
- Reading an option that has no value and no default is an error that names the reading module. Give the option a default, or set it.
- A function can return `nil` or `{}` to contribute nothing.

### Definition order

Definition order is pre-order: a module's own config comes first, then each of its imports in order, depth-first. With this `init.lua`:

```lua title="init.lua"
return {
  imports = { "modules.common", "hosts." .. archix.hostname },
  config = { packages = { "base" } },
}
```

the definitions are taken from `init.lua`, then `modules.common` and its imports, then `hosts.desktop` and its imports.

Order matters for lists. List options concatenate every definition in that order:

```lua
-- modules/common.lua
packages = { "git", "neovim" }
-- hosts/desktop.lua
packages = { "steam" }
```

```console
$ archix eval | jq .packages
[
  "base",
  "git",
  "neovim",
  "steam"
]
```

`lines` options join their definitions with newlines in the same order. `attrs_of` options merge per key. Scalars such as strings and numbers can't be combined: two different values at the same priority are a conflict.

## Priorities

When two modules set the same scalar to different values, archix doesn't guess:

```console
error: `programs.kitty.settings.font_size`: conflicting definitions:
    12 in lua/modules/common.lua
    11 in lua/hosts/laptop.lua
  use archix.force() on the one that should win
```

Every definition has a priority, and only the definitions with the lowest number survive. They're the same numbers NixOS uses:

| Definition | Priority |
| --- | --- |
| An option's declared default | 1500 |
| `archix.default(v)` | 1000 |
| A plain value | 100 |
| `archix.force(v)` | 50 |
| `archix.override(n, v)` | `n` |

So the laptop wins with:

```lua title="lua/hosts/laptop.lua"
return {
  programs = { kitty = { settings = { font_size = archix.force(11) } } },
}
```

Use `archix.default` in shared modules for values a host should be able to replace without forcing:

```lua title="lua/modules/common.lua"
return {
  programs = { kitty = { enable = true, settings = { font_size = archix.default(12) } } },
}
```

Wrapping a table applies the priority to every option inside it, so `archix.force { font_size = 11, background_opacity = "0.9" }` forces both. You can't nest wrappers.

Priorities apply to lists too, and that's how you replace a list instead of extending it. Only the lowest-numbered definitions are kept, so a forced list drops every plain one:

```lua
repos = archix.force { "core", "extra" }
```

To set a nullable option to null, use `archix.null`. Lua's `nil` can't be stored in a table, so `{ x = nil }` would simply not set `x`.

## Purity and escape hatches

Evaluation is pure: the same config gives the same result on every run and every machine with the same hostname. That's what makes `archix diff` trustworthy and lets you evaluate the laptop's config on the desktop.

To keep it that way, the Lua environment is restricted:

- The `table`, `string`, `utf8`, `math` and `coroutine` libraries are available. `io`, `os`, `debug`, `load`, `loadfile` and `dofile` are not, and neither is `math.random`.
- `pairs` iterates keys in sorted order. Lua normally seeds its hash order per run, which would make results differ between runs.
- `print` writes `trace:` lines to stderr, for debugging. It doesn't affect the result.

Two escape hatches are declared:

- `archix.hostname`, the host being evaluated.
- `archix.read_file("relative/path")`, which reads a file inside the config directory. Absolute paths and paths that leave the directory are errors.

```lua
local motd = archix.read_file("files/motd")
```

Anything with side effects, like running a command, belongs in an activation hook. Hooks run at switch time, after evaluation, so they can't make two evaluations disagree.

## Where the config lives

The config is read from `/etc/archix` by default. Override it per command with `--config <dir>`, or for a whole shell with the `ARCHIX_CONFIG` environment variable:

```console
$ archix --config ~/src/archix diff
$ export ARCHIX_CONFIG=~/src/archix
```

`archix init` offers to keep the config in a repo you own, with `/etc/archix` linked to it. That way you edit as yourself, and `sudo archix switch` still finds it.

`eval`, `options`, `lock` and `diff` don't need root. Only commands that change the system do.

### git

git is optional. When the config directory is a git repo, each generation records the commit it was built from, and archix warns when you switch with uncommitted changes. It never refuses to run because of git: you can switch a dirty tree, and the warning reminds you that the generation won't match any commit exactly.
