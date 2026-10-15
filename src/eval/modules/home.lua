local t = archix.types

return {
  options = {
    home = {
      user = archix.option {
        type = t.nullable(t.str),
        default = archix.null,
        description = "The user whose home archix manages: home.files and every programs.* module apply to them.",
      },
      directory = archix.option {
        type = t.nullable(t.path),
        default = archix.null,
        description = "That user's home directory; defaults to users.<user>.home, else /home/<user>.",
      },
      files = archix.option {
        type = t.attrs_of(t.submodule {
          text = archix.option { type = t.nullable(t.lines), default = archix.null },
          source = archix.option {
            type = t.nullable(t.str),
            default = archix.null,
            description = "A file or directory in the config directory, as a path relative to it.",
          },
          live = archix.option {
            type = t.bool,
            default = false,
            description = "Symlink to source in the config directory instead of copying it, so apps that write their own config edit your repo.",
          },
          url = archix.option { type = t.nullable(t.str), default = archix.null, description = "Download the file from here; needs sha256. archix.fetchurl { url, sha256 } builds both." },
          sha256 = archix.option { type = t.nullable(t.str), default = archix.null, description = "The sha256 the download from url must have; `archix prefetch <url>` prints it." },
          link = archix.option { type = t.nullable(t.str), default = archix.null, description = "Make the file a symlink to this target." },
          executable = archix.option { type = t.bool, default = false },
        }),
        default = {},
        description = "Files in the user's home, keyed by path relative to it (a leading ~/ is allowed).",
      },
      systemd = archix.option {
        type = t.attrs_of(t.attrs_of(t.any)),
        default = {},
        description = "The user's systemd units, by kind (services, timers, sockets, paths, targets) and name, as sections of keys; Install.WantedBy creates the .wants links.",
      },
    },
  },
}
