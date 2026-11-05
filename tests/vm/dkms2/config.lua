-- tests/vm/dkms2/test.py: linux and linux-lts, with v4l2loopback-dkms added by the test.
return {
  packages = { "base", "linux", "linux-lts", "mkinitcpio", "systemd-ukify", "sbctl", "efitools", "e2fsprogs", "git" },
  hostname = "archix-vm",
  archive_url = "@ARCHIVE_URL@",
  kernel_params = { "root=PARTUUID=@ROOT_UUID@", "rw", "console=ttyS0" },
  boot = { loader = "systemd-boot" },
  rollback = { enable = true },
  allow_no_password_login = true,
  files = {
    ["/opt/archix-vmtest/bin/archix-backdoor"] = { source = "archix-backdoor", mode = "0755" },
    ["/etc/systemd/network/20-wired.network"] = { text = "[Match]\nName=en*\n\n[Network]\nDHCP=yes" },
    ["/etc/archix-vmtest"] = { text = "0" },
  },
  units = {
    ["archix-backdoor.service"] = "[Unit]\nDescription=archix VM test driver's root shell\nWants=network-online.target\n\n[Service]\nExecStart=/opt/archix-vmtest/bin/archix-backdoor\n\n[Install]\nWantedBy=multi-user.target",
  },
  -- The driver's shell runs every command; a restart would cut it off mid-switch.
  restart_if_changed = { ["archix-backdoor.service"] = false },
  services = {
    ["archix-backdoor.service"] = true,
    ["systemd-networkd.service"] = true,
    ["systemd-resolved.service"] = true,
  },
}
