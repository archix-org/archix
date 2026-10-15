---
title: Coming from NixOS
description: Map NixOS concepts to archix, understand what works differently, and port an existing NixOS and Home Manager config.
sidebar:
  order: 5
---

You already know the ideas behind archix: a declarative config, a lockfile, generations and rollback, a typed module system with priorities. This page maps what you know onto archix, then covers what's different and how to port a config.

## Concept map

| NixOS | archix |
| --- | --- |
| `configuration.nix` | `init.lua` |
| Modules with `imports`, `options`, `config` | Modules with `imports`, `options`, `config` |
| `flake.lock` | `archix.lock` |
| `nixos-rebuild switch` | `archix switch` |
| `nixos-rebuild switch --rollback` | `archix rollback` |
| `nix-env --list-generations` | `archix generations` |
| `nixos-rebuild dry-activate` | `archix diff` |
| `nix flake update` | `archix update` |
| `nix eval` of a config | `archix eval` |
| `nixos-option`, the options search | `archix options` |
| `lib.mkDefault` | `archix.default(v)` |
| `lib.mkForce` | `archix.force(v)` |
| `lib.mkOverride n` | `archix.override(n, v)` |
| `null` | `archix.null` |
| `lib.mkOption { type = ...; }` | `archix.option { type = ... }` |
| `types.listOf`, `attrsOf`, `nullOr`, `enum`, `submodule` | `archix.types.list_of`, `attrs_of`, `nullable`, `enum`, `submodule` |
| `builtins.readFile ./x` | `archix.read_file("x")` |
| `pkgs.fetchurl { url; sha256; }` | `archix.fetchurl { url = ..., sha256 = ... }`, as a file in `files` or `home.files` |
| `nix-prefetch-url` | `archix prefetch` |
| `services.flatpak` with nix-flatpak's `packages` | `flatpak.apps`, locked to commits in `archix.lock` |
| `networking.hostName` per host | `archix.hostname`, from the machine |
| Home Manager `programs.*` | `programs.*`, ported from Home Manager |
| `home.file` | `home.files` |
| `environment.etc` | `files` |
| `systemd.services`, `systemd.user.services` | `units`, `services`, `home.systemd` |
| `users.users` | `users` |
| `nix-collect-garbage` | `archix gc` |
| nixpkgs | The Arch repos, the AUR and third-party repos |
| Generations in the boot menu | Generations in the systemd-boot menu |

The priorities use the same numbers: option defaults are 1500, `default` is 1000, plain values 100, `force` 50. If you've reasoned about `mkOverride` before, the same reasoning applies.

## What's different

### No derivations

archix installs Arch's binary packages. There's no store, no derivations, no build closure to evaluate, and nothing to wait for while nixpkgs catches up. The filesystem is a normal FHS layout, so binaries from the internet, language toolchains and proprietary apps just work.

The flip side: you can't override how a package is built from your config, the way `overrideAttrs` does. If you need a custom build, publish it to the AUR or to a repo of your own declared in `custom_repos`. archix locks those like everything else, and builds AUR packages in a clean chroot.

### A lock of versions, not of inputs

`flake.lock` pins the source of nixpkgs, and everything follows from evaluation. `archix.lock` pins outputs: an Arch Linux Archive date, plus the exact version and checksum of every installed package. The effect is the same, reproducible installs, but the lock is also readable as a list of what's on the machine. One lockfile covers all hosts, and each host can sit on its own date.

### Lua, not Nix

Lua is eager. Where Nix gets `config.foo` references from laziness, archix re-runs function modules until the merged config stops changing:

```nix
# NixOS
config = lib.mkIf config.my.desktop.enable { environment.systemPackages = [ pkgs.firefox ]; };
```

```lua
-- archix
config = function(cfg)
  if not cfg.my.desktop.enable then return {} end
  return { packages = { "firefox" } }
end,
```

There's no `mkIf`, `mkMerge`, `mkBefore` or `mkAfter`. Use plain `if`, return a table, and rely on definition order: a module's own config, then its imports in order. Lists concatenate in that order. See [Configuration](/docs/concepts/configuration/#function-config-and-fixpoint-rounds).

Evaluation is still pure. The sandbox has no `io` or `os`, `pairs` iterates in sorted order, and the only escape hatches are `archix.hostname` and `archix.read_file`.

### Mutable passwords

archix never manages passwords. That matches NixOS's default of `users.mutableUsers = true`: users and groups are declared, passwords are set with `passwd`. When archix creates a user, it reminds you to run `passwd`. There's no equivalent of `hashedPassword`.

Encrypted secrets, agenix- or sops-style, are not supported.

### pacman still works

On NixOS, imperative changes mostly don't stick. On archix they're allowed. `pacman -Syu` and `yay` keep working, a pacman hook records what they did, and the next switch asks whether to adopt the change into the lock or revert it. With the default `undeclared_packages = "remove"`, the next switch offers to declare packages you installed with pacman or yay, and removes them if you say no, so the declarative guarantee holds; you just aren't forbidden from experimenting. See [pacman and yay](/docs/concepts/pacman-and-yay/).

### One machine at a time

`archix switch` applies to the machine it runs on. You can evaluate, diff and lock another host with `--host`, but remote switch over ssh, like `nixos-rebuild --target-host`, is not supported.

### Bootloaders

systemd-boot only. GRUB and Limine are not supported. Each generation gets a UKI, signed with sbctl if you have keys, so Secure Boot works without lanzaboote. Picking an older generation in the boot menu rolls the system back to it early in boot, by reinstalling that generation's packages and files from the cache rather than switching a store path, so it works on any filesystem.

### No store, so rollback reinstalls

NixOS rolls back by pointing at an older closure that's still in the store. archix has no store: a rollback reinstalls the older generation's exact package files from its cache, then puts its files, users and services back. That's why `gc` keeps the package files of every kept generation, and why a collected generation can't be rolled back to.

## Porting a config

The NixOS config ports by hand, module by module. These tips make it go faster.

**Start with `archix init`, not a translation.** Let onboarding capture your current Arch install into a working config first. Then bring over the structure and options from your NixOS config, one module at a time, running `archix diff` after each.

**Keep your module structure.** If your NixOS config has `hosts/` and `modules/`, the same split works in `lua/hosts/` and `lua/modules/`. `imports = [ ./common.nix ]` becomes `imports = { "modules.common" }`.

**Translate package names.** Most names match between nixpkgs and Arch, but not all. `pkgs.python3` is `python`, and many nixpkgs sets like `pythonPackages.requests` are `python-requests`. `archix search` finds the Arch name. Anything only in the AUR is still just a name in `packages`.

**Home Manager config ports almost directly.** archix's `programs.*` modules take their option names from Home Manager, in snake_case, and `settings` passes through, so most `programs.<name>` blocks become Lua tables with the same keys. Like Home Manager's per-user config, they apply to one user, which you name with `home.user`:

```nix
programs.kitty = {
  enable = true;
  settings = { font_size = 11; confirm_os_window_close = 0; };
};
```

```lua
users = { me = { groups = { "wheel" } } },
home = { user = "me" },
programs = {
  kitty = {
    enable = true,
    settings = { font_size = 11, confirm_os_window_close = 0 },
  },
},
```

**Your own options port too.** A custom NixOS module with `options.my.thing = mkOption { ... }` becomes:

```lua
local t = archix.types

return {
  options = {
    my = {
      thing = archix.option { type = t.nullable(t.str), default = archix.null, description = "What it does" },
    },
  },
}
```

**Leave services for last.** Packages and dotfiles are easy wins. Services, `/etc` files and kernel parameters touch the boot path, so port them once the rest is stable, and keep the previous generation's boot entry around while you do. See [Dotfiles and apps](/docs/guides/dotfiles-and-apps/) for the ported `programs.*` modules.
