local t = archix.types

local app = t.submodule {
  url = archix.option { type = t.str, description = "Where to download the AppImage." },
  sha256 = archix.option { type = t.str, description = "Its sha256; `archix prefetch <url>` prints it." },
  name = archix.option { type = t.nullable(t.str), default = archix.null, description = "The name in app menus; the attribute name by default." },
  icon = archix.option { type = t.nullable(t.str), default = archix.null, description = "An icon name or path for the menu entry." },
  categories = archix.option { type = t.list_of(t.str), default = {}, description = "Menu categories, such as Office or Development." },
  desktop = archix.option { type = t.bool, default = true, description = "Add an entry to app menus." },
}

return {
  options = {
    appimages = archix.option {
      type = t.attrs_of(app),
      default = {},
      description = "AppImages to download, keyed by command name: installed to /opt/archix/appimages, linked as /usr/bin/<name> and added to app menus in /usr/share/applications, all inside the deployment, so a rollback takes them back together; a name a package's /usr/bin file has is refused. Declares fuse2, which AppImages need.",
    },
  },
  config = function(cfg)
    local files = {}
    local any = false
    for name, image in pairs(cfg.appimages) do
      if not name:match("^[%w._-]+$") or name:match("^%.") then
        error("appimages: " .. name .. " isn't a valid command name")
      end
      any = true
      -- The command and menu entry go into the deployment's /usr, which `files` refuses; system.render writes them.
      files["/opt/archix/appimages/" .. name .. ".AppImage"] = { url = image.url, sha256 = image.sha256, mode = "0755" }
    end
    if not any then return {} end
    return { files = files, packages = { "fuse2" } }
  end,
}
