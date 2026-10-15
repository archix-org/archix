---
title: Lua API
description: Reference for the archix global in Lua configs, covering option declarations, types, priorities, escape hatches, app modules and format writers.
sidebar:
  order: 3
---

archix configs are Lua 5.4. Every module sees a global `archix` table with the functions below. Shared helpers can live in any module and be loaded with `require`.

## Module shape

A module returns one of:

- a table with any of `imports`, `options` and `config`, and no other keys;
- a table without those keys, which is treated as `config`;
- a function, which is treated as `config = function(cfg) ... end`.

```lua
return {
  imports = { "modules.common", "hosts." .. archix.hostname },
  options = { ... },
  config = function(cfg) return { ... } end,
}
```

| Key | Value |
| --- | --- |
| `imports` | A list of module names (`"modules.common"` loads `lua/modules/common.lua` or `lua/modules/common/init.lua`), or inline module tables and functions. Each named module is loaded once. |
| `options` | A nested table whose leaves are `archix.option { ... }` declarations. |
| `config` | A table of definitions, or `function(cfg)` returning one. `cfg` is the final merged config, read-only. Returning `nil` contributes nothing. |

See [Configuration](/docs/concepts/configuration/) for how modules merge.

## `archix.option`

```lua
archix.option { type = t, default = v, description = "..." }
```

Declares an option. Put it under `options`, at the path the option should have:

```lua
local t = archix.types

return {
  options = {
    editor = {
      name = archix.option { type = t.str, default = "nvim", description = "The default editor" },
    },
  },
}
```

| Field | Required | Description |
| --- | --- | --- |
| `type` | yes | An `archix.types` value. |
| `default` | no | The value used when nothing sets the option, at priority 1500. Checked against `type` when declared. |
| `description` | no | Shown by `archix options`. |

Any other field is an error. Declaring the same option in two modules is an error, and so is declaring an option at a path that already has options under it.

## Types

All types live in `archix.types`. Values are type-checked after merging, and errors name the file each value came from.

| Type | Accepts | Merging several definitions |
| --- | --- | --- |
| `bool` | `true`, `false` | Must agree |
| `int` | Lua integers. `12.0` is a float, not an int. | Must agree |
| `float` | Numbers, integers included | Must agree |
| `str` | Strings | Must agree |
| `path` | Strings that are absolute (`/etc/...`), `~` or start with `~/` | Must agree |
| `lines` | Strings | Joined with newlines, in definition order |
| `any` | Anything | Tables merge per key, lists concatenate, scalars must agree |
| `enum { "a", "b" }` | One of the listed strings | Must agree |
| `list_of(t)` | Lists of `t` | Concatenated in definition order |
| `attrs_of(t)` | Tables with string keys, values of `t` | Merged per key; each key merges like an option of type `t` |
| `nullable(t)` | `archix.null` or a `t` | All null, or merged as `t`. Mixing null and a value is a conflict. |
| `submodule { ... }` | Tables with the declared options | Merged per option, with each option's default |

"Must agree" means two different values at the same priority are a conflict, and archix tells you to `archix.force` the one that should win. Only the definitions with the best priority take part in merging; see [Priorities](#priorities).

`submodule` is most useful inside `attrs_of`, for named entries that share a shape:

```lua
local t = archix.types

return {
  options = {
    my = {
      sites = archix.option {
        type = t.attrs_of(t.submodule {
          root = archix.option { type = t.path },
          port = archix.option { type = t.int, default = 80 },
        }),
        default = {},
        description = "Static sites to serve",
      },
    },
  },
}
```

```lua
my = { sites = { blog = { root = "/srv/blog" }, docs = { root = "/srv/docs", port = 8080 } } }
```

`blog.port` gets its default of 80.

## Priorities

Lower numbers win. Only definitions at the winning priority are merged; the rest are dropped, lists included.

| Function | Priority | Use |
| --- | --- | --- |
| *(declared default)* | 1500 | |
| `archix.default(v)` | 1000 | A value a host can replace without forcing |
| *(plain value)* | 100 | |
| `archix.force(v)` | 50 | Win over plain values |
| `archix.override(n, v)` | `n` | Any priority |

```lua
programs = { kitty = { settings = { font_size = archix.force(11) } } }
repos = archix.override(10, { "core", "extra" })
```

Wrapping a table of options applies the priority to every option inside it. A wrapper must wrap a whole option value or a table of options, not one list element, and wrappers can't be nested. `override` needs an integer priority and a non-nil value.

## `archix.null`

The null value, for `nullable` options. Lua tables can't hold `nil`, so `{ x = nil }` leaves `x` unset; `{ x = archix.null }` sets it to null. In `cfg`, a null option reads as `nil`. `archix.null` itself is read-only.

## `archix.hostname`

A string: the host being evaluated. It's the machine's hostname, or the value of `--host`.

```lua
imports = { "hosts." .. archix.hostname }
```

## `archix.read_file`

```lua
archix.read_file("relative/path") -- returns a string
```

Reads a file inside the config directory and returns its content. The path is relative to the directory containing `init.lua`. Absolute paths, and paths that resolve outside the config directory, are errors. This keeps evaluation pure: the config can only depend on files that are part of it.

```lua
files = { ["~/.config/nvim/init.lua"] = archix.read_file("dotfiles/nvim/init.lua") }
```

## `archix.program`

```lua
archix.program "name" { package = "pkg", file = "~/path", format = "fmt" }
```

Defines a thin app module in one line. The result is a module: return it from a file, or put it in `imports`.

| Field | Description |
| --- | --- |
| `package` | The package installed when the program is enabled. |
| `file` | Where the config file is written. |
| `format` | The writer used for `settings`: `toml`, `json`, `yaml`, `ini`, `kdl`, `key-value` or `kconfig`. |

```lua title="lua/modules/foot.lua"
return archix.program "foot" { package = "foot", file = "~/.config/foot/foot.ini", format = "ini" }
```

It declares `programs.<name>.enable` and `programs.<name>.settings`:

```lua
programs = { foot = { enable = true, settings = { main = { pad = "8x8" } } } }
```

## Format writers

Writers turn a Lua table into the content of a config file. Use them as values in `files`, and archix uses them for `programs.<name>.settings`.

| Function | Format |
| --- | --- |
| `archix.toml { ... }` | TOML |
| `archix.json { ... }` | JSON |
| `archix.yaml { ... }` | YAML |
| `archix.ini { ... }` | INI, one section per top-level key |
| `archix.kdl { ... }` | KDL |
| `archix.kconfig { ... }` | KDE's kconfig, one group per top-level key |

There's also a `key-value` format, for flat `key=value` files, available as `format = "key-value"` in `archix.program`.

```lua
files = {
  ["~/.config/alacritty/alacritty.toml"] = archix.toml { window = { opacity = 0.9 } },
  ["~/.config/kdeglobals"] = archix.kconfig { General = { ColorScheme = "BreezeDark" } },
}
```

Keys are written in sorted order, so the same table always renders the same bytes.

## Config values

Config tables are checked before merging:

- Keys must be strings, or list indices.
- A table is either a list (`{ "a", "b" }`) or a record (`{ a = 1 }`), never both. Lists can't have holes.
- `{}` is an empty list or an empty record; the option's type decides.
- `nil` isn't a value. Use `archix.null`.
- Functions, userdata and `archix.option` declarations can't be config values.

## The Lua environment

| Available | Not available |
| --- | --- |
| `table`, `string`, `utf8`, `math`, `coroutine` | `io`, `os`, `debug`, `package` |
| `require`, for modules under `lua/` | `load`, `loadfile`, `dofile`, `collectgarbage` |
| `pairs`, iterating keys in sorted order | `math.random`, `math.randomseed` |
| `print`, writing `trace:` lines to stderr | |

`pairs` is sorted so that evaluation is reproducible; plain Lua's iteration order changes from run to run. `print` is for debugging and doesn't affect the result:

```console
$ archix eval > /dev/null
trace: kitty enabled	true
```
