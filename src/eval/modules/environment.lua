local t = archix.types

return {
  options = {
    environment = {
      variables = archix.option {
        type = t.attrs_of(t.any),
        default = {},
        description = "Environment variables for every user: login shells get them from /etc/profile.d/archix-environment.sh, and systemd user services and graphical sessions from /etc/environment.d/90-archix.conf, from the next boot on. Lists are joined with ':'.",
      },
      shell_aliases = archix.option {
        type = t.attrs_of(t.str),
        default = {},
        description = "Shell aliases for every user's login shells, from /etc/profile.d/archix-aliases.sh, from the next boot on; programs.zsh.shell_aliases covers every interactive zsh.",
      },
    },
  },
}
