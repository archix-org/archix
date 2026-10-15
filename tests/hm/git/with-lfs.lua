-- Home Manager's test runs with an old stateVersion, where signing.format defaults to "openpgp"; archix has no such default.
return {
  home = { user = "hm-user" },
  programs = {
    git = {
      enable = true,
      settings = { user = { name = "John Doe", email = "user@example.org" } },
      lfs = { enable = true, skip_smudge = true },
      signing = { format = "openpgp" },
    },
  },
}
