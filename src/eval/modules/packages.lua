local t = archix.types

return {
  options = {
    packages = archix.option {
      type = t.list_of(t.str),
      default = {},
      description = "Packages from the official repositories. Their dependencies are installed too and recorded in archix.lock.",
    },
    repos = archix.option {
      type = t.list_of(t.enum { "core", "extra", "multilib", "core-testing", "extra-testing", "multilib-testing" }),
      default = { "core", "extra" },
      description = "Official repositories packages are resolved from, in priority order.",
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
