return {
  packages = { "base", "linux", "mkinitcpio", "systemd-ukify", "tpm2-tss", "tpm2-tools", "python-cryptography", "binutils" },
  hostname = "archix-vm",
  archive_url = "@ARCHIVE_URL@",
  kernel_params = { "root=PARTUUID=@ROOT_UUID@", "rw", "console=ttyS0" },
  boot = { loader = "systemd-boot" },
  deployments = { keep = 3 },
  files = {
    ["/opt/archix-vmtest/bin/archix-vmtest"] = { source = "vmtest.sh", mode = "0755" },
    ["/etc/systemd/network/20-wired.network"] = { text = "[Match]\nName=en*\n\n[Network]\nDHCP=yes" },
    -- ["/etc/crypttab"] = { text = "data /dev/vdb - tpm2-device=auto,headless=true,nofail" },
  },
  units = {
    ["archix-vmtest.service"] = "[Unit]\nDescription=archix VM test\nAfter=multi-user.target archix-boot.service network-online.target cryptsetup.target\nWants=network-online.target\n\n[Service]\nType=oneshot\nExecStart=/opt/archix-vmtest/bin/archix-vmtest\nStandardOutput=journal+console\nTimeoutSec=0\n\n[Install]\nWantedBy=multi-user.target",
  },
  services = {
    ["archix-vmtest.service"] = true,
    ["systemd-networkd.service"] = true,
    ["systemd-resolved.service"] = true,
  },
}
