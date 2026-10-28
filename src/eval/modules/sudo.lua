local t = archix.types

return {
  options = {
    sudo = {
      enable = archix.option {
        type = t.bool,
        default = false,
        description = "Install sudo and write /etc/sudoers.d/archix (mode 0440) into the next deployment, checked with `visudo -c` while it's built; a rule visudo refuses fails the switch with nothing changed.",
      },
      wheel = archix.option { type = t.bool, default = true, description = "Let members of wheel run any command as any user." },
      wheel_needs_password = archix.option { type = t.bool, default = true, description = "Whether wheel members type their password (false gives them NOPASSWD)." },
      extra_rules = archix.option {
        type = t.list_of(t.str),
        default = {},
        description = "More sudoers lines, such as \"alice ALL=(ALL) NOPASSWD: /usr/bin/systemctl\".",
      },
    },
  },
  config = function(cfg)
    if cfg.sudo.enable then return { packages = { "sudo" } } end
    return {}
  end,
}
