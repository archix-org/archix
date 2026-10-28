return {
  packages = { "base", "linux", "mkinitcpio", "systemd-ukify", "age", "wireguard-tools", "networkmanager", "strace", "openssl" },
  hostname = "archix-vm",
  archive_url = "@ARCHIVE_URL@",
  kernel_params = { "root=PARTUUID=@ROOT_UUID@", "rw", "console=ttyS0" },
  boot = { loader = "systemd-boot" },
  files = {
    ["/opt/archix-vmtest/bin/archix-backdoor"] = { source = "archix-backdoor", mode = "0755" },
    ["/opt/archix-vmtest/bin/archix-leak-sweep"] = { source = "archix-leak-sweep", mode = "0755" },
    ["/etc/systemd/network/20-wired.network"] = { text = "[Match]\nName=en*\n\n[Network]\nDHCP=yes" },
    -- NetworkManager only reads its profiles here; networkd keeps the NIC.
    ["/etc/NetworkManager/conf.d/archix-vmtest.conf"] = { text = "[keyfile]\nunmanaged-devices=interface-name:en*\n" },
  },
  units = {
    ["archix-backdoor.service"] = "[Unit]\nDescription=archix VM test driver's root shell\nWants=network-online.target\n\n[Service]\nExecStart=/opt/archix-vmtest/bin/archix-backdoor\n\n[Install]\nWantedBy=multi-user.target",
  },
  services = {
    ["archix-backdoor.service"] = true,
    ["systemd-networkd.service"] = true,
    ["systemd-resolved.service"] = true,
    ["NetworkManager.service"] = true,
  },
}
