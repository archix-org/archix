---
title: Flatpak and AppImages
description: Declare Flatpak apps locked to exact commits and AppImages locked by hash, then update them, roll them back and remove them like the rest of the system.
sidebar:
  order: 4
---

Some apps are easiest to get outside the repos and the AUR: as a Flatpak from Flathub, or as an AppImage from the developer's releases page. archix manages both the same way it manages packages. You declare them, they're locked, and every generation records them, so `archix rollback` takes them back too.

- **Flatpak apps** are locked to an exact commit in `archix.lock`.
- **AppImages** are locked by their sha256, like any other file downloaded with [`archix.fetchurl`](/docs/reference/lua-api/#archixfetchurl).

## Flatpak apps

### Declare them

List app ids in [`flatpak.apps`](/docs/reference/options/#flatpakapps):

```lua title="lua/hosts/desktop/init.lua"
return {
  flatpak = {
    apps = { "org.mozilla.firefox", "com.spotify.Client" },
  },
}
```

Apps are installed system-wide, from Flathub unless you say otherwise. Declaring any app also declares the `flatpak` package, so you don't list it in `packages`.

For another remote, declare it in `flatpak.remotes` and prefix the app id with its name. Flathub stays available alongside it:

```lua title="lua/hosts/desktop/init.lua"
return {
  flatpak = {
    apps = { "org.mozilla.firefox", "myremote:org.example.App" },
    remotes = { myremote = { url = "https://flatpak.example.org/repo/example.flatpakrepo" } },
  },
}
```

To take most apps from another remote, set `flatpak.remote` to its name; ids without a prefix then come from there. `switch` adds the remotes your apps use, with `flatpak remote-add --system --if-not-exists`, and never removes one.

### Switch

```console
$ sudo archix switch
flatpak    install com.spotify.Client (5f8806197a2b)
flatpak    install org.mozilla.firefox (0bb3a9e40c1f)
boot entry for the new generation
apply 3 changes? [Y/n]
:: installing packages
...
:: making the new generation current
switched to generation 44 (snapshot 2026-10-09)
```

Before it asks, `switch` locks each new app to its remote's current commit and writes it to `archix.lock`; the short commit is shown in brackets. After you confirm and the new generation is current, it adds the remotes your apps use, installs the apps and pins each one to its locked commit with `flatpak update --commit=`. Pinning holds even when the remote already has something newer, so a second machine with the same lock gets the same build.

Only `switch` adds remotes, and only after you confirm; `diff` and `lock` never do. So when flatpak isn't installed yet, or an app's remote hasn't been added, there's nothing to ask for a commit, and the app is listed with the reason:

```console
flatpak    install org.mozilla.firefox: flatpak isn't installed yet
```

The same switch installs the `flatpak` package and adds the remote, then locks the app, writes it to `archix.lock` and installs it.

A Flatpak step that fails, such as a remote that can't be reached, doesn't undo the switch. The new generation is current, the failure is listed, and archix exits non-zero, as with a service that doesn't start. See [Hard and soft failures](/docs/concepts/safety/#hard-and-soft-failures).

### The lock

Each app is one line in the host's `flatpak` table:

```toml title="archix.lock"
[hosts.desktop.flatpak]
"com.spotify.Client" = { remote = "flathub", ref = "app/com.spotify.Client/x86_64/stable", commit = "5f8806197a2b3c4d6e8f0a1b2c3d4e5f60718293a4b5c6d7e8f9012a3b4c5d6e" }
"org.mozilla.firefox" = { remote = "flathub", ref = "app/org.mozilla.firefox/x86_64/stable", commit = "0bb3a9e40c1f8d2e4b6a7c9d1e3f5a7b9c0d2e4f6a8b1c3d5e7f9a0b2c4d6e8f" }
```

Commit it with the rest of the lock. `archix lock` keeps these commits, and only locks apps that are new. See [The lockfile](/docs/concepts/lockfile/#flatpak-apps-commits).

### Update

Flatpak apps move forward with everything else. `archix update` checks every app on its remote, moves it to the remote's current commit, and prints a line for each app whose commit changed:

```console
$ sudo archix update
resolving 88 declared packages against the 2026-10-15 snapshot
snapshot 2026-10-09 -> 2026-10-15
~ mesa 1:25.3.0-1 -> 1:25.3.1-1
212 packages locked
wrote /etc/archix/archix.lock
flatpak com.spotify.Client at 81c2d4e6f0a3
```

An app whose remote can't be reached keeps its locked commit, with `warning: couldn't check org.mozilla.firefox on flathub; keeping its locked commit`.

Nothing changes until you switch. `archix diff` shows what will:

```console
$ archix diff
snapshot 2026-10-15: 212 packages
upgrade    mesa 1:25.3.0-1 -> 1:25.3.1-1
flatpak    com.spotify.Client 5f8806197a2b -> 81c2d4e6f0a3
```

Updating an app with `flatpak update` yourself moves it away from the lock, and `diff` shows the way back, from the installed commit to the locked one. The next switch pins it to the locked commit again. To keep the newer version, run `archix update` instead.

### Roll back

Each generation records the commits it pinned. `archix rollback` pins the older ones again, and uninstalls apps the newer generation added:

```console
$ sudo archix rollback
flatpak    com.spotify.Client 81c2d4e6f0a3 -> 5f8806197a2b
downgrade  mesa 1:25.3.1-1 -> 1:25.3.0-1
roll back to generation 44 (snapshot 2026-10-09) with 2 changes? [Y/n]
...
rolled back to generation 44 (snapshot 2026-10-09); archix.lock is unchanged, so the next `archix switch` returns to the config
```

Old commits come from the remote, so rolling back Flatpak apps needs the network, and a remote that still has the commit. Rolling back from the boot menu happens before the network is up, so it skips Flatpak apps and says so: ``archix: Flatpak apps aren't pinned at boot; run `sudo archix rollback 44` once you're online``. Running that pins them. See [Generations and rollback](/docs/concepts/generations/).

### What gets removed

Take an app out of `flatpak.apps` and the next switch uninstalls it, then runs `flatpak uninstall --unused` to remove runtimes nothing needs any more:

```console
flatpak    remove org.gimp.GIMP
```

archix only removes apps it manages: the ones in the current generation. Apps you installed system-wide by hand stay, and `diff` lists them on a line of their own:

```console
flatpak apps archix doesn't manage: com.valvesoftware.Steam
```

To manage one, add it to `flatpak.apps`; it's locked to its remote's current commit, and the next switch pins it there. Apps installed with `flatpak install --user` are never touched, listed or locked.

### Runtimes aren't locked

Only apps are locked. Each app's runtime, such as `org.freedesktop.Platform`, follows the branch the app's metadata asks for, at that branch's current commit whenever flatpak installs or updates it. Rolling an app back gives you its old commit, not necessarily the runtime it was first installed with. Runtimes on a stable branch only get compatible updates, so in practice that's rarely noticeable.

### Existing apps and `archix init`

`archix init` declares the system-wide apps already installed, with any remote other than Flathub, and locks each one to the commit it's installed at. The first `archix diff` stays empty, and the first switch changes no app. The wizard's Packages step lists them, `✓ 2 Flatpak apps, locked to their installed commits: com.spotify.Client, org.mozilla.firefox`. Apps whose remote has been deleted can't be locked, so init leaves them out, and archix never touches them. The Packages step shows `! Flatpak apps whose remote is gone, left alone: org.example.App`, and `--defaults` prints:

```
  ! their remote is gone, so archix leaves these Flatpak apps alone: org.example.App
```

## AppImages

### Declare them

An AppImage is a single file, so it's declared by URL and sha256. `archix prefetch` downloads it and prints both:

```console
$ archix prefetch https://example.org/releases/Obsidian-1.9.14.AppImage
archix.fetchurl { url = "https://example.org/releases/Obsidian-1.9.14.AppImage", sha256 = "3b9f2c4e8a1d6f0b7c5e2a9d4f1b8c6e3a0d7f5b2c9e4a1d8f6b3c0e7a5d2f9b" }
```

Put the `url` and `sha256` in an [`appimages`](/docs/reference/options/#appimages) entry, keyed by the command you want to run it with:

```lua title="lua/hosts/desktop/init.lua"
return {
  appimages = {
    obsidian = {
      url = "https://example.org/releases/Obsidian-1.9.14.AppImage",
      sha256 = "3b9f2c4e8a1d6f0b7c5e2a9d4f1b8c6e3a0d7f5b2c9e4a1d8f6b3c0e7a5d2f9b",
      name = "Obsidian",
      categories = { "Office" },
    },
  },
}
```

`archix.fetchurl` passes other fields through, so the pasted line works as the entry too: `obsidian = archix.fetchurl { url = ..., sha256 = ..., categories = { "Office" } }`.

### Switch

```console
$ sudo archix switch
...
install    fuse2 2.9.9-5
create     /opt/archix/appimages/obsidian.AppImage
create     /usr/local/bin/obsidian
create     /usr/local/share/applications/archix-obsidian.desktop
boot entry for the new generation
apply 5 changes? [Y/n]
downloading https://example.org/releases/Obsidian-1.9.14.AppImage
downloading fuse2-2.9.9-5-x86_64.pkg.tar.zst
:: installing packages
...
switched to generation 46 (snapshot 2026-10-15)
```

Each entry becomes three managed files:

| File | What it is |
| --- | --- |
| `/opt/archix/appimages/obsidian.AppImage` | The AppImage, mode `0755`. |
| `/usr/local/bin/obsidian` | A link to it, so `obsidian` runs from your `PATH`. |
| `/usr/local/share/applications/archix-obsidian.desktop` | The menu entry, named `name` (the key by default), with `icon` and `categories` if you set them. `desktop = false` leaves it out. |

archix also declares `fuse2`, which AppImages need to run.

The download happens after you confirm, before anything changes, and an AppImage whose hash differs from the config stops the switch: `<url> has sha256 <actual>, but the config says <sha256>`. Big files are copied from the store into place, not read into memory.

### Update, roll back and remove

To move to a new release, run `archix prefetch` on its URL and replace `url` and `sha256`. `archix diff` shows `update     /opt/archix/appimages/obsidian.AppImage` without downloading anything, because it only compares hashes.

The AppImage is stored with the generation like every managed file, so `archix rollback` puts the old one back from the store, even when the old release has been taken down. `gc` deletes it once no kept generation uses it.

Take the entry out of `appimages`, and the next switch deletes all three files. `fuse2` is no longer declared, so it goes too unless something else needs it.

## Which one to use

- **A package in the repos or the AUR:** use that. It's locked to the snapshot with everything else, and builds against the same libraries.
- **A Flatpak:** for sandboxed apps, or apps whose upstream only ships a Flatpak. You get exact commits, and updates come with `archix update`.
- **An AppImage:** when it's the only build there is. Its hash is in your config, so it changes only when you change it.
