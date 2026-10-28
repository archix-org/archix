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
  on_change = archix.option {
    type = t.nullable(t.lines),
    default = archix.null,
    description = "A bash script run as root when a switch or rollback changed this file: for a file in /etc, /usr (but /usr/local), /opt or /boot, at the new deployment's first boot, before networking and before the hooks; for a file elsewhere, which is written right away, right after it's written.",
  },
  mutable = archix.option {
    type = t.nullable(t.bool),
    default = archix.null,
    description = "Write the file read-only (false) or writable (true); by default as mutable_files says.",
  },
  merge = archix.option {
    type = t.nullable(t.submodule {
      format = archix.option { type = t.enum { "ini", "kconfig", "json" }, description = "The file's format." },
      values = archix.option { type = t.attrs_of(t.any), default = {}, description = "Keys to set: sections (kconfig groups nest) of keys, or a JSON object deep-merged into the file." },
    }),
    default = archix.null,
    description = "Set only these keys in a file the app also writes, keeping the rest of it, like plasma-manager. Leaving the config leaves the keys as they are.",
  },
}

return {
  options = {
    files = archix.option {
      type = t.attrs_of(text_file),
      default = {},
      description = "Files archix writes, keyed by absolute path; give one of text, source, link, url or merge. Files in /etc, /usr, /opt and /boot go into the new deployment and take effect at the next boot; files elsewhere, /usr/local included, which every deployment shares, are written once the switch commits. Files archix replaces are backed up and restored when they leave the config.",
    },
    mutable_files = archix.option {
      type = t.bool,
      default = true,
      description = "Whether files archix writes stay writable. false writes them read-only like the Nix store, restores them on every switch, and marks merged kconfig keys [$i]; each file's mutable overrides it.",
    },
    capabilities = archix.option {
      type = t.attrs_of(t.str),
      default = {},
      description = "File capabilities, keyed by the path of a file a package installs (or the config declares) in /usr or /opt, in setcap's text form, such as cap_sys_nice+ep. They're set in the new deployment before it's committed, on a copy of the file so no other deployment changes, and `archix verify` checks them; a path that isn't there is refused. Dropping one gives the file back what its package or install script left it.",
    },
    units = archix.option { type = t.attrs_of(t.lines), default = {}, description = "systemd system units, keyed by file name such as foo.service." },
    user_units = archix.option { type = t.attrs_of(t.lines), default = {}, description = "systemd user units for every user, keyed by file name." },
    services = archix.option {
      type = t.attrs_of(t.bool),
      default = {},
      description = "System units to enable (true) or disable (false) in the next deployment; they start or stop at the next boot.",
    },
    masked_units = archix.option {
      type = t.list_of(t.str),
      default = {},
      description = "System units to mask, such as sleep.target, so nothing can start them: each becomes a link to /dev/null in /etc/systemd/system in the next deployment.",
    },
    default_target = archix.option {
      type = t.nullable(t.str),
      default = archix.null,
      description = "The target the next deployment boots into, such as multi-user.target for a machine without a desktop; by default systemd's, graphical.target.",
    },
    user_services = archix.option { type = t.attrs_of(t.bool), default = {}, description = "User units to enable (true) or disable (false) for every user in the next deployment; they start or stop at the next boot." },
    sysctl = archix.option { type = t.attrs_of(t.any), default = {}, description = "Kernel settings, written to /etc/sysctl.d/90-archix.conf and applied at the next boot." },
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
        hashed_password = archix.option {
          type = t.nullable(t.str),
          default = archix.null,
          description = "The password as a crypt(3) hash (`mkpasswd` prints a yescrypt one), or \"!\" to lock the account; set in the next deployment at every switch, so a password changed with passwd goes back to it, and archix diff says so. A locked or expired account stays so, and a password set in the rescue entry holds until this hash changes. Anyone who can read the config can try to crack it offline.",
        },
        initial_hashed_password = archix.option {
          type = t.nullable(t.str),
          default = archix.null,
          description = "A crypt(3) hash set only when archix creates the account; passwd changes it for good after that.",
        },
        ssh_authorized_keys = archix.option {
          type = t.list_of(t.str),
          default = {},
          description = "SSH public keys that may log in as the user, one .pub line each, written to /etc/ssh/authorized_keys.d/<user> in the next deployment; ~/.ssh/authorized_keys keeps working beside them. Declaring any installs openssh; enable sshd.service in services.",
        },
      }),
      default = {},
      description = "Users archix creates and keeps in step; ones it created are removed (home kept) when they leave the config. Passwords come from hashed_password or initial_hashed_password, else set them with passwd.",
    },
    allow_no_password_login = archix.option {
      type = t.bool,
      default = false,
      description = "Let a switch build a deployment in which neither root nor a wheel or sudo member has a password, which a switch otherwise refuses when the running system has one, as the rescue boot entry needs one to open.",
    },
    tmpfiles = archix.option {
      type = t.list_of(t.str),
      default = {},
      description = "tmpfiles.d lines, such as \"d /var/lib/foo 0750 foo foo -\", written to /etc/tmpfiles.d/archix.conf in the next deployment and applied when it boots. Lines touching archix's own paths are refused.",
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
  config = function(cfg)
    for _, user in pairs(cfg.users) do
      if #user.ssh_authorized_keys > 0 then return { packages = { "openssh" } } end
    end
    return {}
  end,
}
