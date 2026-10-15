local t = archix.types

return {
  options = {
    hooks = archix.option {
      type = t.attrs_of(t.submodule {
        run = archix.option { type = t.lines, description = "The bash script to run; it runs with set -e and pipefail." },
        user = archix.option {
          type = t.nullable(t.str),
          default = archix.null,
          description = "Run as this user, in their home with their session bus when they're logged in; root by default.",
        },
        after = archix.option { type = t.list_of(t.str), default = {}, description = "Hooks that must run before this one." },
      }),
      default = {},
      description = "Scripts run at the end of every switch and rollback, after files are written and services started, like NixOS activation scripts. They run every time, so make them idempotent; a failing hook is reported without undoing the switch.",
    },
  },
}
