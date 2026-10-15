return {
  home = { user = "hm-user" },
  programs = {
    git = {
      enable = true,
      settings = { user = { name = "John Doe", email = "user@example.org" } },
      signing = {
        signer = "path-to-ssh",
        format = "ssh",
        key = "ssh-ed25519 AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        sign_by_default = true,
      },
    },
  },
}
