return {
  imports = {
    {
      programs = {
        starship = {
          settings = {
            add_newline = false,
            format = "$line_break$package$line_break$character",
            scan_timeout = 10,
            character = { success_symbol = "➜", error_symbol = "➜" },
            package = { disabled = true },
            memory_usage = { threshold = -1 },
            aws = { style = "bold blue" },
            battery = { charging_symbol = "⚡️", display = { { threshold = 10, style = "bold red" } } },
          },
        },
      },
    },
    {
      programs = {
        starship = { settings = { aws = { disabled = true }, battery = { display = { { threshold = 30, style = "bold yellow" } } } } },
      },
    },
  },
  config = { home = { user = "hm-user" }, programs = { starship = { enable = true } } },
}
