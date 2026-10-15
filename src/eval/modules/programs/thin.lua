return {
  imports = {
    archix.program "alacritty" { file = ".config/alacritty/alacritty.toml", format = "toml" },
    archix.program "foot" { file = ".config/foot/foot.ini", format = "ini" },
    archix.program "mpv" { file = ".config/mpv/mpv.conf", format = "ini" },
  },
}
