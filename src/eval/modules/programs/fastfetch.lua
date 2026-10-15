-- Ported from Home Manager's modules/programs/fastfetch.nix (MIT); see tests/hm/cases.toml for the commit.
local t = archix.types

return {
  options = {
    programs = {
      fastfetch = {
        enable = archix.option { type = t.bool, default = false, description = "Install and configure fastfetch." },
        package = archix.option { type = t.nullable(t.str), default = "fastfetch" },
        settings = archix.option { type = t.any, default = {}, description = "config.jsonc contents." },
      },
    },
  },
  config = function(cfg)
    local fastfetch = cfg.programs.fastfetch
    if not fastfetch.enable then return {} end
    local result = { home = { files = {} } }
    if next(fastfetch.settings) then
      result.home.files[".config/fastfetch/config.jsonc"] = { text = archix.formats.json(fastfetch.settings) }
    end
    if fastfetch.package ~= nil then result.packages = { fastfetch.package } end
    return result
  end,
}
