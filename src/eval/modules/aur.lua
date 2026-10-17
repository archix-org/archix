local t = archix.types

return {
  options = {
    aur = {
      url = archix.option {
        type = t.str,
        default = "https://aur.archlinux.org",
        description = "The AUR to look packages up in and clone build recipes from.",
      },
      review = archix.option {
        type = t.bool,
        default = true,
        description = "Show each AUR build recipe entering the lock (the PKGBUILD, or a diff) and ask before trusting it.",
      },
      build_on_host = archix.option {
        type = t.list_of(t.str),
        default = {},
        description = "AUR packages built directly on the host with makepkg instead of in a clean chroot.",
      },
      skip_check = archix.option {
        type = t.list_of(t.str),
        default = {},
        description = "AUR packages (by pkgbase) built without running their test suite (makepkg --nocheck), for recipes whose check() fails against the locked snapshot.",
      },
    },
    missing_versions = archix.option {
      type = t.enum { "ask", "newer", "older" },
      default = "ask",
      description = "When a locked AUR version can't be built any more: ask (defaulting to the nearest newer version), or always take the nearest newer or older one.",
    },
  },
}
