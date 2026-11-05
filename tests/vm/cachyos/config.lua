-- tests/vm/cachyos/test.py: CachyOS's repos ahead of Arch's, booted by Limine until the test moves it to systemd-boot.
local function cachyos(name, arch)
  return { name = name, servers = { "https://mirror.cachyos.org/repo/" .. arch .. "/$repo" }, before_official = true }
end

return {
  packages = { "base", "linux-cachyos", "mkinitcpio", "systemd-ukify", "cachyos-keyring", "htop", "jq", "sbctl", "e2fsprogs", "git", "limine", "efibootmgr" },
  hostname = "archix-vm",
  archive_url = "@ARCHIVE_URL@",
  custom_repos = {
    cachyos("cachyos-v3", "$arch_v3"),
    cachyos("cachyos-core-v3", "$arch_v3"),
    cachyos("cachyos-extra-v3", "$arch_v3"),
    cachyos("cachyos", "$arch"),
  },
  kernel_params = { "root=PARTUUID=@ROOT_UUID@", "rw", "console=ttyS0" },
  boot = { loader = "limine", kernel = "linux-cachyos" },
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
  restart_if_changed = { ["archix-backdoor.service"] = false },
  services = {
    ["archix-backdoor.service"] = true,
    ["systemd-networkd.service"] = true,
    ["systemd-resolved.service"] = true,
  },
}
