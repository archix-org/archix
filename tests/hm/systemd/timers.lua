return {
  home = {
    user = "hm-user",
    systemd = {
      timers = {
        ["test-timer"] = {
          Unit = { Description = "A basic test timer" },
          Timer = { OnUnitActiveSec = "1h 30m" },
          Install = { WantedBy = { "timers.target" } },
        },
      },
    },
  },
}
