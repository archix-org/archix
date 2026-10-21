local t = archix.types

return {
  options = {
    hooks = archix.option {
      type = t.attrs_of(t.submodule {
        run = archix.option { type = t.lines, description = "The bash script to run; it runs with set -e and pipefail." },
        user = archix.option {
          type = t.nullable(t.str),
          default = archix.null,
          description = "Run as this user, in their home; root by default. At boot nobody is logged in, so there's no session bus; a switch with nothing to build gives them theirs if they're logged in.",
        },
        after = archix.option { type = t.list_of(t.str), default = {}, description = "Hooks that must run before this one." },
      }),
      default = {},
      description = "Scripts run at the first boot of the deployment every switch or rollback commits, like NixOS activation scripts: early, once its users exist and its /var changes are applied, before networking, D-Bus and logins. A switch with nothing to build runs them right away. They run once per switch, so make them idempotent; a failing hook is reported on the console and at the next login, runs again at the next boot, and doesn't undo the switch.",
    },
  },
}
