local t = archix.types

return {
  options = {
    packages = archix.option {
      type = t.list_of(t.str),
      default = {},
      description = "Packages to install: from the configured repositories, else from the AUR. Their dependencies are installed too and recorded in archix.lock.",
    },
    prefer = archix.option {
      type = t.list_of(t.str),
      default = {},
      description = "Packages to use when a dependency could be met by several, such as which font satisfies ttf-font. They're installed as dependencies, not declared.",
    },
    unsafe_downgrades = archix.option {
      type = t.attrs_of(t.list_of(t.str)),
      default = {},
      description = "Versions per package that a downgrade mustn't cross without asking, on top of archix's own list (postgresql and mariadb major versions and a few more). A switch or rollback going back across one asks, defaulting to no; --noconfirm refuses unless --allow-unsafe-downgrade is given.",
    },
    patches = archix.option {
      type = t.attrs_of(t.list_of(t.str)),
      default = {},
      description = "Patch files for official packages, keyed by package name and relative to the config directory. archix rebuilds Arch's own recipe for the locked version with them, applied with patch -p1 before the recipe's prepare(), and adds a sub-release to the pkgrel. Recipes in pkgbuilds/<dir>/ need no option: declaring a package they build is enough.",
    },
    repos = archix.option {
      type = t.list_of(t.enum { "core", "extra", "multilib", "core-testing", "extra-testing", "multilib-testing" }),
      default = { "core", "extra" },
      description = "Official repositories packages are resolved from, in priority order.",
    },
    custom_repos = archix.option {
      type = t.list_of(t.submodule {
        name = archix.option { type = t.str, description = "Repository name, as in pacman.conf." },
        servers = archix.option {
          type = t.list_of(t.str),
          description = "Server URLs, tried in order; $repo and $arch are expanded like pacman does.",
        },
        keys = archix.option {
          type = t.list_of(t.str),
          default = {},
          description = "Fingerprints of the repository's signing keys; archix imports and locally signs them.",
        },
        keyserver = archix.option { type = t.str, default = "hkps://keyserver.ubuntu.com" },
        sig_level = archix.option { type = t.str, default = "Required DatabaseOptional" },
        before_official = archix.option {
          type = t.bool,
          default = false,
          description = "List the repository before core and extra, so its packages win (CachyOS's repos need this).",
        },
      }),
      default = {},
      description = "Third-party repositories. They have no dated archive, so archix keeps a copy of every package it installs from them.",
    },
    package_caches = archix.option {
      type = t.list_of(t.str),
      default = {},
      description = "Package caches tried before the archive: directories (file:///mnt/nas/archix) or HTTP servers holding what `archix cache push` copied. Repo packages are checked against the lock's sha256; AUR builds are taken as the cache's build records say, so list only caches you control.",
    },
    package_cache_push = archix.option {
      type = t.nullable(t.str),
      default = nil,
      description = "A package cache directory (file:///mnt/nas/archix) every switch copies this machine's new package files, signatures and AUR build records into, as `archix cache push` does, so other machines get its exact third-party versions and AUR builds. A failed copy only warns.",
    },
    lock_warn_after_days = archix.option {
      type = t.int,
      default = 30,
      description = "Warn when archix.lock's snapshot is older than this many days, since fixes released after it aren't installed; 0 never warns.",
    },
    archive_url = archix.option {
      type = t.str,
      default = "https://archive.archlinux.org",
      description = "The Arch Linux Archive, or a mirror of it with the same layout (repos/YYYY/MM/DD/...), that locks resolve and download from.",
    },
    archive_mirrors = archix.option {
      type = t.list_of(t.str),
      default = {},
      description = "Copies of the archive with its layout, tried in order when archive_url is down or lacks a file, for locking, downloads, rollbacks and recovery.",
    },
    third_party_fallback = archix.option {
      type = t.enum { "ask", "fallback", "fail" },
      default = "ask",
      description = "When a locked package from a custom repo is gone: ask (with --noconfirm, fail), fall back without asking, or fail. Falling back tries the repo's current version, then the AUR, then the official repos.",
    },
    undeclared_packages = archix.option {
      type = t.enum { "remove", "warn" },
      default = "remove",
      description = "What switch does with installed packages that aren't in the lock: remove them, or only list them.",
    },
    ignored_packages = archix.option {
      type = t.list_of(t.str),
      default = {},
      description = "Installed packages archix leaves alone and never reports as undeclared.",
    },
  },
}
