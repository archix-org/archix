local t = archix.types

return {
  options = {
    generations = {
      keep = archix.option {
        type = t.int,
        default = 10,
        description = "Number of newest generations gc always keeps.",
      },
      keep_days = archix.option {
        type = t.int,
        default = 30,
        description = "gc also keeps every generation younger than this many days.",
      },
    },
  },
}
