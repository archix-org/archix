return {
  packages = { "base", "linux", "mkinitcpio", "systemd-ukify", "sbctl", "efitools", "archix-churn" },
  hostname = "archix-vm",
  archive_url = "@ARCHIVE_URL@",
  custom_repos = { { name = "churn", servers = { "file:///srv/churn-repo" }, sig_level = "Optional TrustAll" } },
  kernel_params = { "root=PARTUUID=@ROOT_UUID@", "rw", "console=ttyS0", "archix.week=0" },
  boot = { loader = "systemd-boot" }, -- boot
  deployments = { keep = 2 },
  generations = { keep = 10, keep_days = 30 }, -- generations
  files = {
    ["/opt/archix-vmtest/bin/archix-vmtest"] = { source = "vmtest.sh", mode = "0755" },
    ["/etc/systemd/network/20-wired.network"] = { text = "[Match]\nName=en*\n\n[Network]\nDHCP=yes" },
  },
  units = {
    ["archix-vmtest.service"] = "[Unit]\nDescription=archix VM test\nAfter=multi-user.target archix-boot.service network-online.target\nWants=network-online.target\n\n[Service]\nType=oneshot\nExecStart=/opt/archix-vmtest/bin/archix-vmtest\nStandardOutput=journal+console\nTimeoutSec=0\n\n[Install]\nWantedBy=multi-user.target",
  },
  services = {
    ["archix-vmtest.service"] = true,
    ["systemd-networkd.service"] = true,
    ["systemd-resolved.service"] = true,
  },
}
