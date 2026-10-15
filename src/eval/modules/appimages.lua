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
      description = "AppImages to download, keyed by command name: installed to /opt/archix/appimages, linked into /usr/local/bin and added to app menus. Declares fuse2, which AppImages need.",
    },
  },
  config = function(cfg)
    local files = {}
    local any = false
    for name, image in pairs(cfg.appimages) do
      if not name:match("^[%w._-]+$") then
        error("appimages: " .. name .. " isn't a valid command name")
      end
      any = true
      local target = "/opt/archix/appimages/" .. name .. ".AppImage"
      files[target] = { url = image.url, sha256 = image.sha256, mode = "0755" }
      files["/usr/local/bin/" .. name] = { link = target }
      if image.desktop then
        local lines = { "[Desktop Entry]", "Type=Application", "Name=" .. (image.name or name), "Exec=" .. target .. " %U" }
        if image.icon then lines[#lines + 1] = "Icon=" .. image.icon end
        if #image.categories > 0 then lines[#lines + 1] = "Categories=" .. table.concat(image.categories, ";") .. ";" end
        files["/usr/local/share/applications/archix-" .. name .. ".desktop"] = { text = table.concat(lines, "\n") .. "\n" }
      end
    end
    if not any then return {} end
    return { files = files, packages = { "fuse2" } }
  end,
}
