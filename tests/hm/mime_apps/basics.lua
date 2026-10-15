return {
  home = { user = "hm-user" },
  mime_apps = {
    enable = true,
    associations = {
      added = { ["image/png"] = { "foo1.desktop", "foo2.desktop", "foo3.desktop" }, ["image/jpeg"] = "foo4.desktop" },
      removed = { ["image/png"] = "foo5.desktop" },
    },
    default_applications = { ["image/png"] = { "default1.desktop", "default2.desktop" } },
    default_application_packages = { { desktop = "test.desktop", mime_types = { "image/png", "image/svg+xml" } } },
  },
}
