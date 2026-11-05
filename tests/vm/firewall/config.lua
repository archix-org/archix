-- tests/vm/firewall/test.py: ssh allowed, and probe-lan on 2201 from the LAN only, until the test drops them; 2202 is never declared.
return {
  packages = { "base", "linux", "mkinitcpio", "systemd-ukify", "sbctl", "efitools", "e2fsprogs", "git", "openssh", "nftables" },
  hostname = "archix-vm",
  archive_url = "@ARCHIVE_URL@",
  kernel_params = { "root=PARTUUID=@ROOT_UUID@", "rw", "console=ttyS0" },
  boot = { loader = "systemd-boot" },
  rollback = { enable = true },
  firewall = { enable = true, allowed_services = { "ssh", "probe-lan" }, service_ports = { ["probe-lan"] = { tcp = { 2201 }, sources = "lan" } } },
  allow_no_password_login = true,
  files = {
    ["/opt/archix-vmtest/bin/archix-backdoor"] = { source = "archix-backdoor", mode = "0755" },
    ["/etc/systemd/network/20-wired.network"] = { text = "[Match]\nName=en*\n\n[Network]\nDHCP=yes" },
    ["/etc/archix-vmtest"] = { text = "0" },
    ["/etc/systemd/network/10-probe.network"] = { text = "[Match]\nMACAddress=52:54:00:12:34:77\n\n[Network]\nAddress=192.168.77.2/24\nAddress=198.51.100.2/24\nAddress=fd00:77::2/64\nAddress=2001:db8::2/64\nIPv6AcceptRA=no" },
    ["/etc/ssh/sshd_config.d/50-vmtest.conf"] = { text = "Port 22\nPort 2201\nPort 2202" },
  },
  units = {
    ["archix-backdoor.service"] = "[Unit]\nDescription=archix VM test driver's root shell\nWants=network-online.target\n\n[Service]\nExecStart=/opt/archix-vmtest/bin/archix-backdoor\n\n[Install]\nWantedBy=multi-user.target",
  },
  -- The driver's shell runs every command; a restart would cut it off mid-switch.
  restart_if_changed = { ["archix-backdoor.service"] = false },
  services = {
    ["archix-backdoor.service"] = true,
    ["sshd.service"] = true,
    ["systemd-networkd.service"] = true,
    ["systemd-resolved.service"] = true,
  },
}
