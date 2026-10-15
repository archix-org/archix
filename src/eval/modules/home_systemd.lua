-- Ported from Home Manager's modules/systemd.nix (MIT); see tests/hm/PORTED for the commit.
local suffixes = {
  services = "service",
  timers = "timer",
  sockets = "socket",
  paths = "path",
  targets = "target",
  slices = "slice",
  mounts = "mount",
  automounts = "automount",
}

return {
  config = function(cfg)
    local files = {}
    for kind, units in pairs(cfg.home.systemd) do
      local suffix = suffixes[kind]
      if suffix == nil then error("home.systemd: unknown unit kind " .. kind) end
      for name, sections in pairs(units) do
        local unit = name .. "." .. suffix
        files[".config/systemd/user/" .. unit] = { text = archix.formats.systemd(sections) }
        local install = sections.Install
        local wanted = install ~= nil and install.WantedBy or nil
        if type(wanted) == "string" then wanted = { wanted } end
        for _, target in ipairs(wanted or {}) do
          files[".config/systemd/user/" .. target .. ".wants/" .. unit] = {
            link = archix.home_directory(cfg) .. "/.config/systemd/user/" .. unit,
          }
        end
      end
    end
    return { home = { files = files } }
  end,
}
