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

Config functions run again until the merged config stops changing, so one function can read what another produced. A config that's still changing after 32 rounds is an error that names the options involved.

Reading `cfg` is checked too. A path that isn't an option is an error with a did-you-mean suggestion, and so is reading an option that has no value and no default. A null option reads as `nil`.

```lua
return function(cfg)
  if cfg.programs.zsh.enable then
    return { programs = { zsh = { init_content = 'eval "$(zoxide init zsh)"' } } }
  end
end
```

archix's own modules, which declare every option in the [options reference](/docs/reference/options/), are loaded before `init.lua`. You never import them.

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

Use it when you want to process a file's content in Lua. To install a file as it is, `source` in `files` or `home.files` is simpler.

```lua
local motd = archix.read_file("files/motd")

return {
  files = { ["/etc/motd"] = { text = motd .. "Host: " .. archix.hostname } },
}
```

## `archix.home_directory`

```lua
archix.home_directory(cfg) -- returns a string
```

The home directory of `home.user`, for config functions that need an absolute path in it: `home.directory` if set, else `users.<user>.home`, else `/home/<user>`. It's an error when `home.user` isn't set.

```lua
return function(cfg)
  return { programs = { zsh = { session_variables = { NOTES = archix.home_directory(cfg) .. "/notes" } } } }
end
```

## `archix.fetchurl`

```lua
archix.fetchurl { url = "https://...", sha256 = "..." } -- returns a file spec
```

Builds a file that's downloaded from `url` and checked against `sha256`, for [`files`](/docs/reference/options/#files) and [`home.files`](/docs/reference/options/#homefiles). It checks the table while the config is evaluated, and returns a copy of it, so other fields such as `mode`, `owner` or `executable` pass through.

```lua
return {
  files = {
    ["/usr/local/bin/hello"] = archix.fetchurl {
      url = "https://downloads.example.org/hello/1.2.0/hello-x86_64",
      sha256 = "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03",
      mode = "0755",
    },
  },
}
```

| Field | Required | Description |
| --- | --- | --- |
| `url` | yes | Where to download the file. `https://`, `http://` and `file://` URLs work. |
| `sha256` | yes | The file's sha256, as 64 hex digits. |

A missing `url` is an error, `archix.fetchurl needs { url = ..., sha256 = ... }`, and so is a `sha256` that isn't 64 hex digits: ``archix.fetchurl: sha256 must be 64 hex digits; `archix prefetch https://...` prints it``. You don't compute the hash yourself: [`archix prefetch <url>`](/docs/reference/cli/#prefetch) downloads the file and prints the whole `archix.fetchurl { ... }` line to paste.

Evaluating the config never downloads anything; it only records the URL and the hash. The download happens at `archix switch`, and the hash is what's locked.

## `archix.program`

```lua
archix.program "name" { file = ".config/name/config", format = "toml", package = "pkg" }
```

Defines a thin app module in one line. The result is a module: return it from a file, or put it in `imports`. archix's own alacritty, foot and mpv modules are built this way.

| Field | Required | Description |
| --- | --- | --- |
| `file` | yes | Where the config file is written, relative to `home.user`'s home. |
| `format` | yes | The name of the [format writer](#format-writers) that renders `settings`, such as `toml`, `json`, `yaml`, `ini`, `key_value`, `kconfig` or `kdl`. An unknown name is an error. |
| `package` | no | The package installed when the program is enabled. Defaults to the program's name. |

```lua title="lua/modules/fuzzel.lua"
return archix.program "fuzzel" { file = ".config/fuzzel/fuzzel.ini", format = "ini" }
```

It declares four options under `programs.<name>`:

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `enable` | `bool` | `false` | Install and configure the program. |
| `package` | `null or str` | `package`, or the name | Set to `archix.null` to skip installing it. |
| `settings` | `any` | `{}` | The file's contents, rendered by the writer. |
| `extra_config` | `lines` | `""` | Text appended to the file. |

```lua
programs = { fuzzel = { enable = true, settings = { main = { font = "Inter:size=11" } } } }
```

Like every program module, it needs `home.user`. Defining a program archix already ships, or one another module defines, is an error, because each option can only be declared once.

## Format writers

Writers turn a Lua table into the text of a config file. They live in `archix.formats`, and each one mirrors the Nix generator Home Manager uses for the same format, so ported modules render the same bytes. Use them for `text` in `files` and `home.files`, or by name in `archix.program`.

| Function | Output |
| --- | --- |
| `toml(t)` | TOML. Nested tables become `[a.b]` sections, lists of tables become `[[a]]`, null values are left out. |
| `json(t)` | JSON as `jq .` prints it: two-space indent, sorted keys, final newline. |
| `yaml(t)` | YAML, with keys and strings double-quoted. |
| `ini(t, opts)` | INI like Home Manager's `toINI`: top-level scalars first, then one section per table. A list repeats its key. |
| `systemd(t)` | A systemd unit: one section per table, like `ini`. |
| `git_ini(t)` | git config: nested tables become `[section "sub"]`, keys are tab-indented and strings quoted. |
| `key_value(t, opts)` | `key value` lines, as in kitty.conf. |
| `kconfig(t)` | KDE's kconfig: one group per table, nested tables become `[Group][Subgroup]`. |
| `kdl(t)` | KDL: tables become nodes with children, lists become arguments. |

`ini` takes `separator` (default `"="`) and `list_separator`, which joins a list into one value instead of repeating the key. `key_value` takes `separator` (default `" "`) and `value`, a function that renders each value.

```lua
local f = archix.formats

return {
  home = {
    files = {
      [".config/kdeglobals"] = { text = f.kconfig { General = { ColorScheme = "BreezeDark" } } },
      [".config/niri/config.kdl"] = { text = f.kdl { layout = { gaps = 16 }, ["spawn-at-startup"] = { "waybar" } } },
      [".config/tool/tool.conf"] = { text = f.key_value({ theme = "dark", width = 80 }, { separator = " = " }) },
    },
  },
}
```

Keys are written in sorted order, so the same table always renders the same bytes. Numbers in `ini`, `systemd`, `key_value` and `git_ini` follow Nix's `toString`, as Home Manager does: `0.95` becomes `0.950000`. Write the value as a string, `"0.95"`, to keep it short.

A few helpers used by the ported modules are there too:

| Function | Returns |
| --- | --- |
| `nix_string(v)` | Nix's `toString`: integers as they are, floats with six decimals, `true` as `"1"` and `false` as `""`. |
| `yes_no(b)` | `"yes"` or `"no"`. |
| `shell_arg(s)` | `s` quoted for a shell, like Nix's `escapeShellArg`: bare when it's safe, single-quoted otherwise. |
| `wrap_lines(items, width)` | A list of lines, packing `items` space-separated into lines no longer than `width`. |

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
