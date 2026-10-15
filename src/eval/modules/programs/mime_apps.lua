-- Ported from Home Manager's modules/misc/xdg-mime-apps.nix (MIT); see tests/hm/PORTED for the commit.
local t = archix.types

local function as_list(v)
  if type(v) == "string" then return { v } end
  return v
end

local function joined(map)
  local out = {}
  for mime, apps in pairs(map) do out[mime] = table.concat(as_list(apps), ";") end
  return out
end

return {
  options = {
    mime_apps = {
      enable = archix.option { type = t.bool, default = false, description = "Manage ~/.config/mimeapps.list." },
      associations = {
        added = archix.option { type = t.attrs_of(t.any), default = {}, description = "Extra apps per MIME type: a desktop file name or a list of them." },
        removed = archix.option { type = t.attrs_of(t.any), default = {} },
      },
      default_applications = archix.option { type = t.attrs_of(t.any), default = {} },
      default_application_packages = archix.option {
        type = t.list_of(t.submodule {
          desktop = archix.option { type = t.str, description = "The package's desktop file name, such as firefox.desktop." },
          mime_types = archix.option { type = t.list_of(t.str) },
        }),
        default = {},
        description = "Apps that become the default for every MIME type they declare, after default_applications.",
      },
    },
  },
  config = function(cfg)
    local mime = cfg.mime_apps
    if not mime.enable then return {} end
    local defaults = {}
    for type_, apps in pairs(mime.default_applications) do
      defaults[type_] = {}
      for _, app in ipairs(as_list(apps)) do table.insert(defaults[type_], app) end
    end
    for _, package in ipairs(mime.default_application_packages) do
      for _, type_ in ipairs(package.mime_types) do
        defaults[type_] = defaults[type_] or {}
        table.insert(defaults[type_], package.desktop)
      end
    end
    local text = archix.formats.ini {
      ["Added Associations"] = joined(mime.associations.added),
      ["Removed Associations"] = joined(mime.associations.removed),
      ["Default Applications"] = joined(defaults),
    }
    return { home = { files = { [".config/mimeapps.list"] = { text = text } } } }
  end,
}
