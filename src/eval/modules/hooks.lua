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
      description = "Scripts run as root at the end of every switch or revert that applies, like NixOS activation scripts: once packages, users, files and services are in place, on the running system. They run once per switch, so make them idempotent; a failing hook is reported in the switch's output and at the next login, and doesn't undo the switch.",
    },
  },
}
