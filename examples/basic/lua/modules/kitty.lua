local t = archix.types

return {
  options = {
    programs = {
      kitty = {
        enable = archix.option { type = t.bool, default = false, description = "Install and configure kitty" },
        font = {
          name = archix.option { type = t.str, default = "monospace" },
          size = archix.option { type = t.int, default = 12 },
        },
      },
    },
  },
  config = function(cfg)
    if not cfg.programs.kitty.enable then return {} end
    return { packages = { "kitty" } }
  end,
}
