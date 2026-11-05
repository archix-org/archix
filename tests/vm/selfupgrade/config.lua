-- In the deployments model's syntax, as the last build of it reads it: tests/vm/selfupgrade/test.py upgrades that system to this tree's archix-git.
return {
  packages = { "base", "linux", "mkinitcpio", "systemd-ukify", "archix-git" },
  hostname = "archix-vm",
  archive_url = "@ARCHIVE_URL@",
  custom_repos = { { name = "archix-self", servers = { "file:///srv/archix-repo" }, sig_level = "Optional TrustAll" } },
  kernel_params = { "root=PARTUUID=@ROOT_UUID@", "rw", "console=ttyS0" },
  boot = { loader = "systemd-boot" },
  deployments = { keep = 3 },
  files = {
    ["/opt/archix-vmtest/bin/archix-backdoor"] = { source = "archix-backdoor", mode = "0755" },
    ["/etc/systemd/network/20-wired.network"] = { text = "[Match]\nName=en*\n\n[Network]\nDHCP=yes" },
  },
  units = {
    ["archix-backdoor.service"] = "[Unit]\nDescription=archix VM test driver's root shell\nWants=network-online.target\n\n[Service]\nExecStart=/opt/archix-vmtest/bin/archix-backdoor\n\n[Install]\nWantedBy=multi-user.target",
  },
  services = {
    ["archix-backdoor.service"] = true,
    ["systemd-networkd.service"] = true,
    ["systemd-resolved.service"] = true,
  },
}
