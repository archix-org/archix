return {
  home = { user = "hm-user" },
  programs = {
    kitty = {
      enable = true,
      settings = { scrollback_lines = 10000, enable_audio_bell = false, update_check_interval = 0, background_opacity = 0.5 },
      font = { name = "DejaVu Sans", size = 8 },
      keybindings = { ["ctrl+c"] = "copy_or_interrupt", ["ctrl+f>2"] = "set_font_size 20" },
      mouse_bindings = {
        ["ctrl+left click"] = "ungrabbed mouse_handle_click selection link prompt",
        ["left click"] = "ungrabbed no-op",
      },
      action_aliases = {
        launch_tab = "launch --cwd=current --type=tab",
        launch_window = "launch --cwd=current --type=os-window",
      },
      environment = { LS_COLORS = "1" },
    },
  },
}
