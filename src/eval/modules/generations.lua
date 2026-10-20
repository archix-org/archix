local t = archix.types

return {
  options = {
    generations = {
      keep = archix.option {
        type = t.int,
        default = 10,
        description = "Number of newest generation records gc always keeps; a generation whose deployment is gone can still be rolled back to, rebuilt from the package cache.",
      },
      keep_days = archix.option {
        type = t.int,
        default = 30,
        description = "gc also keeps the record of every generation younger than this many days.",
      },
    },
    deployments = {
      keep = archix.option {
        type = t.int,
        default = 2,
        description = "How many deployments gc keeps, newest first, counting the booted, current and boot-default ones, which it never drops; each kept one boots from its own entry and rolls back instantly. 1 keeps the previous deployment only until the new one has booted.",
      },
    },
  },
}
