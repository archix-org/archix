local t = archix.types

return {
  options = {
    generations = {
      keep = archix.option {
        type = t.int,
        default = 10,
        description = "Number of newest generation records gc always keeps; `archix revert` applies a kept record live, in place, its packages from archix's cache and the archive.",
      },
      keep_days = archix.option {
        type = t.int,
        default = 30,
        description = "gc also keeps the record of every generation younger than this many days.",
      },
    },
  },
}
