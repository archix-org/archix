-- Ported from Home Manager's modules/programs/starship.nix (MIT); see tests/hm/PORTED for the commit.
local t = archix.types

return {
  options = {
    programs = {
      starship = {
        enable = archix.option { type = t.bool, default = false, description = "Install and configure the starship prompt." },
        package = archix.option { type = t.nullable(t.str), default = "starship" },
        settings = archix.option { type = t.any, default = {}, description = "starship.toml contents." },
        enable_zsh_integration = archix.option { type = t.bool, default = true },
      },
    },
  },
  config = function(cfg)
    local starship = cfg.programs.starship
    if not starship.enable then return {} end
    local result = { home = { files = {} } }
    if next(starship.settings) then
      result.home.files[".config/starship.toml"] = { text = archix.formats.toml(starship.settings) }
    end
    if starship.package ~= nil then result.packages = { starship.package } end
    if starship.enable_zsh_integration and cfg.programs.zsh.enable then
      result.programs = {
        zsh = { init_content = 'if [[ $TERM != "dumb" ]]; then\n  eval "$(starship init zsh)"\nfi\n' },
      }
    end
    return result
  end,
}
