return {
  packages = { "base", "git", "neovim" },
  users = { me = { groups = { "wheel" }, shell = "/usr/bin/zsh" } },
  home = { user = "me" },
  programs = {
    kitty = { enable = true, font = { name = "JetBrainsMono Nerd Font", size = 12 } },
    git = { enable = true, settings = { user = { name = "Me", email = "me@example.com" } } },
    zsh = { enable = true, autosuggestion = { enable = true } },
    starship = { enable = true },
  },
}
