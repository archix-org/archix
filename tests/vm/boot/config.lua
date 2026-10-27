return {
  packages = { "base", "linux", "mkinitcpio", "systemd-ukify", "sbctl", "efitools" },
  hostname = "archix-vm",
  archive_url = "@ARCHIVE_URL@",
  kernel_params = { "root=PARTUUID=@ROOT_UUID@", "rw", "console=ttyS0" },
  boot = { loader = "systemd-boot" },
  files = {
    ["/usr/local/bin/archix-backdoor"] = { source = "archix-backdoor", mode = "0755" },
    ["/etc/systemd/network/20-wired.network"] = { text = "[Match]\nName=en*\n\n[Network]\nDHCP=yes" },
    -- Around archix's own archix.conf: with the stage's marker set, the ESP is read-only for archix bless and writable again for systemd-bless-boot.
    ["/etc/systemd/system/systemd-bless-boot.service.d/00-vmtest.conf"] = { text = "[Service]\nExecStartPre=/bin/sh -c 'if [ -e /var/lib/archix-vmtest/bless-fault ]; then ls /efi > /dev/null && mount -o remount,ro /efi; fi'\n" },
    ["/etc/systemd/system/systemd-bless-boot.service.d/zz-vmtest.conf"] = { text = "[Service]\nExecStartPre=/bin/sh -c 'if [ -e /var/lib/archix-vmtest/bless-fault ]; then mount -o remount,rw /efi; fi'\n" },
  },
  units = {
    ["archix-marker.service"] = "[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/usr/bin/true\n\n[Install]\nWantedBy=multi-user.target", -- marker
    ["archix-backdoor.service"] = "[Unit]\nDescription=archix VM test driver's root shell\nWants=network-online.target\n\n[Service]\nExecStart=/usr/local/bin/archix-backdoor\n\n[Install]\nWantedBy=multi-user.target",
  },
  services = {
    ["archix-backdoor.service"] = true,
    ["systemd-networkd.service"] = true,
    ["systemd-resolved.service"] = true,
    -- Stage 1 drops the marker, so booting an older deployment's entry must bring it back with that deployment's /etc.
    ["archix-marker.service"] = true, -- marker
  },
}
