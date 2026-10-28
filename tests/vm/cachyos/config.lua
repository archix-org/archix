local function cachyos(name, arch)
  return { name = name, servers = { "https://mirror.cachyos.org/repo/" .. arch .. "/$repo" }, before_official = true }
end

return {
  packages = { "base", "linux-cachyos", "mkinitcpio", "systemd-ukify", "cachyos-keyring", "htop", "jq" }, -- packages
  hostname = "archix-vm",
  archive_url = "@ARCHIVE_URL@",
  custom_repos = {
    cachyos("cachyos-v3", "$arch_v3"),
    cachyos("cachyos-core-v3", "$arch_v3"),
    cachyos("cachyos-extra-v3", "$arch_v3"),
    cachyos("cachyos", "$arch"),
  },
  kernel_params = { "root=PARTUUID=@ROOT_UUID@", "rw", "console=ttyS0" },
  boot = { loader = "systemd-boot", kernel = "linux-cachyos" },
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
