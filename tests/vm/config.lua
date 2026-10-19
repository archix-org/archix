return {
  packages = { "base", "linux", "mkinitcpio", "systemd-ukify", "sbctl", "efitools" },
  hostname = "archix-vm",
  archive_url = "@ARCHIVE_URL@",
  kernel_params = { "root=PARTUUID=@ROOT_UUID@", "rw", "console=ttyS0" },
  boot = { loader = "systemd-boot", esp = "/boot" },
  files = {
    ["/usr/local/bin/archix-vmtest"] = { source = "vmtest.sh", mode = "0755" },
    ["/etc/systemd/network/20-wired.network"] = { text = "[Match]\nName=en*\n\n[Network]\nDHCP=yes" },
  },
  units = {
    ["archix-marker.service"] = "[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/usr/bin/true\n\n[Install]\nWantedBy=multi-user.target", -- marker
    ["archix-vmtest.service"] = "[Unit]\nDescription=archix VM test\nAfter=multi-user.target archix-boot.service network-online.target\nWants=network-online.target\n\n[Service]\nType=oneshot\nExecStart=/usr/local/bin/archix-vmtest\nStandardOutput=journal+console\n\n[Install]\nWantedBy=multi-user.target",
  },
  services = {
    ["archix-vmtest.service"] = true,
    ["systemd-networkd.service"] = true,
    ["systemd-resolved.service"] = true,
    -- Stage 1 drops the marker, so booting generation 1's entry has to start it again during the boot-time rollback.
    ["archix-marker.service"] = true, -- marker
  },
}
