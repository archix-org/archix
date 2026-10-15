local t = archix.types

local text_file = t.submodule {
  text = archix.option { type = t.nullable(t.lines), default = archix.null, description = "The file's contents." },
  source = archix.option {
    type = t.nullable(t.str),
    default = archix.null,
    description = "A file in the config directory to copy, as a path relative to it.",
  },
  url = archix.option { type = t.nullable(t.str), default = archix.null, description = "Download the file from here; needs sha256. archix.fetchurl { url, sha256 } builds both." },
  sha256 = archix.option { type = t.nullable(t.str), default = archix.null, description = "The sha256 the download from url must have; `archix prefetch <url>` prints it." },
  link = archix.option { type = t.nullable(t.str), default = archix.null, description = "Make the file a symlink to this target." },
  mode = archix.option { type = t.str, default = "0644" },
  owner = archix.option { type = t.str, default = "root" },
  group = archix.option { type = t.str, default = "root" },
}

return {
  options = {
    files = archix.option {
      type = t.attrs_of(text_file),
      default = {},
      description = "Files archix writes, keyed by absolute path; give one of text, source, link or url. Files archix replaces are backed up and restored when they leave the config.",
    },
    units = archix.option { type = t.attrs_of(t.lines), default = {}, description = "systemd system units, keyed by file name such as foo.service." },
    user_units = archix.option { type = t.attrs_of(t.lines), default = {}, description = "systemd user units for every user, keyed by file name." },
    services = archix.option {
      type = t.attrs_of(t.bool),
      default = {},
      description = "System units to enable and start (true) or disable and stop (false).",
    },
    user_services = archix.option { type = t.attrs_of(t.bool), default = {}, description = "User units to enable (true) or disable (false) for every user." },
    sysctl = archix.option { type = t.attrs_of(t.any), default = {}, description = "Kernel settings applied with sysctl, written to /etc/sysctl.d/90-archix.conf." },
    udev_rules = archix.option { type = t.attrs_of(t.lines), default = {}, description = "udev rules, keyed by file name without .rules." },
    modprobe = archix.option { type = t.attrs_of(t.lines), default = {}, description = "modprobe.d snippets, keyed by file name without .conf." },
    kernel_modules = archix.option { type = t.list_of(t.str), default = {}, description = "Kernel modules to load at boot." },
    kernel_params = archix.option { type = t.list_of(t.str), default = {}, description = "Kernel command line parameters, written to /etc/kernel/cmdline for boot entries." },
    hostname = archix.option { type = t.nullable(t.str), default = archix.null },
    timezone = archix.option { type = t.nullable(t.str), default = archix.null, description = "A zoneinfo name such as Europe/Istanbul." },
    locales = archix.option { type = t.list_of(t.str), default = {}, description = "Locales to generate, such as en_US.UTF-8; the first becomes LANG." },
    keymap = archix.option { type = t.nullable(t.str), default = archix.null, description = "The console keymap." },
    users = archix.option {
      type = t.attrs_of(t.submodule {
        uid = archix.option { type = t.nullable(t.int), default = archix.null },
        group = archix.option { type = t.nullable(t.str), default = archix.null, description = "Primary group; by default one named after the user." },
        groups = archix.option { type = t.list_of(t.str), default = {}, description = "Supplementary groups." },
        shell = archix.option { type = t.path, default = "/bin/bash" },
        home = archix.option { type = t.nullable(t.path), default = archix.null },
        description = archix.option { type = t.str, default = "" },
        system = archix.option { type = t.bool, default = false, description = "A system user: low uid, no home directory." },
      }),
      default = {},
      description = "Users archix creates and keeps in step; ones it created are removed (home kept) when they leave the config. Passwords are never managed: set them with passwd.",
    },
    groups = archix.option {
      type = t.attrs_of(t.submodule {
        gid = archix.option { type = t.nullable(t.int), default = archix.null },
        system = archix.option { type = t.bool, default = false },
      }),
      default = {},
      description = "Groups archix creates and keeps in step.",
    },
  },
}
