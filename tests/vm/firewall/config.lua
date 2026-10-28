return {
  packages = { "base", "linux", "mkinitcpio", "systemd-ukify", "openssh" },
  hostname = "archix-vm",
  archive_url = "@ARCHIVE_URL@",
  kernel_params = { "root=PARTUUID=@ROOT_UUID@", "rw", "console=ttyS0" },
  boot = { loader = "systemd-boot" },
  firewall = { enable = true, allowed_services = { "ssh", "probe-lan" }, service_ports = { ["probe-lan"] = { tcp = { 2201 }, sources = "lan" } } }, -- firewall
  files = {
    ["/opt/archix-vmtest/bin/archix-vmtest"] = { source = "vmtest.sh", mode = "0755" },
    ["/etc/systemd/network/20-wired.network"] = { text = "[Match]\nName=en*\n\n[Network]\nDHCP=yes" },
    ["/etc/systemd/network/10-probe.network"] = { text = "[Match]\nMACAddress=52:54:00:12:34:77\n\n[Network]\nAddress=192.168.77.2/24\nAddress=198.51.100.2/24\nAddress=fd00:77::2/64\nAddress=2001:db8::2/64\nIPv6AcceptRA=no" },
    ["/etc/ssh/sshd_config.d/50-vmtest.conf"] = { text = "Port 22\nPort 2201\nPort 2202" },
  },
  units = {
    ["archix-vmtest.service"] = "[Unit]\nDescription=archix VM test\nAfter=multi-user.target archix-boot.service network-online.target sshd.service\nWants=network-online.target\n\n[Service]\nType=oneshot\nExecStart=/opt/archix-vmtest/bin/archix-vmtest\nStandardOutput=journal+console\nTimeoutSec=0\n\n[Install]\nWantedBy=multi-user.target",
  },
  services = {
    ["archix-vmtest.service"] = true,
    ["sshd.service"] = true,
    ["systemd-networkd.service"] = true,
    ["systemd-resolved.service"] = true,
  },
}
