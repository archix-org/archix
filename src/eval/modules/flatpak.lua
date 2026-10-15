local t = archix.types

return {
  options = {
    flatpak = {
      apps = archix.option {
        type = t.list_of(t.str),
        default = {},
        description = "Flatpak app ids to install system-wide, such as org.mozilla.firefox; write remote:id for a remote other than flatpak.remote. Each is locked to a commit in archix.lock.",
      },
      remote = archix.option { type = t.str, default = "flathub", description = "The remote apps come from unless they name one." },
      remotes = archix.option {
        type = t.attrs_of(t.submodule {
          url = archix.option { type = t.str, description = "The remote's .flatpakrepo file or repository URL." },
          gpg_verify = archix.option { type = t.bool, default = true },
        }),
        default = { flathub = { url = "https://dl.flathub.org/repo/flathub.flatpakrepo" } },
        description = "Remotes archix adds system-wide, keyed by name.",
      },
    },
  },
  config = function(cfg)
    if #cfg.flatpak.apps == 0 then return {} end
    return { packages = { "flatpak" } }
  end,
}
