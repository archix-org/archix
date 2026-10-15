local t = archix.types

return {
  options = {
    packages = archix.option {
      type = t.list_of(t.str),
      default = {},
      description = "Packages to install: from the configured repositories, else from the AUR. Their dependencies are installed too and recorded in archix.lock.",
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
    third_party_fallback = archix.option {
      type = t.enum { "ask", "fallback", "fail" },
      default = "ask",
      description = "When a locked package from a custom repo is gone: ask (the answer defaults to falling back), fall back without asking, or fail. Falling back tries the repo's current version, then the AUR, then the official repos.",
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
