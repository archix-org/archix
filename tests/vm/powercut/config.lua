return {
  packages = { "base", "linux", "mkinitcpio", "systemd-ukify", @EXTRA_PACKAGES@ }, -- packages
  hostname = "archix-vm",
  archive_url = "@ARCHIVE_URL@",
  kernel_params = { "root=PARTUUID=@ROOT_UUID@", "rw", "console=ttyS0" },
  boot = { loader = "systemd-boot" },
  files = {
    ["/etc/systemd/network/20-wired.network"] = { text = "[Match]\nName=en*\n\n[Network]\nDHCP=yes" },
    -- What a real boot of a replayed state reports before powering off: the entry systemd-boot started, the deployment archix mounted, and pacman's missing files.
    ["/usr/local/bin/archix-powercut-booted"] = { mode = "0755", text = [[#!/bin/sh
entry=$(tr -d '\000' < /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f | tr -cd '[:alnum:]+.-')
echo "POWERCUT-BOOTED entry=$entry deployment=$(cat /run/archix/deployment) current=$(readlink /archix/current) missing=$(pacman -Qk 2>&1 | grep -c 'No such file') failed=$(systemctl --failed --no-legend --plain | awk '{ print $1 }' | paste -sd, -)" > /dev/console
systemctl poweroff
]] },
  },
  units = {
    ["archix-powercut-booted.service"] = "[Unit]\nDescription=archix power-cut boot report\nAfter=multi-user.target archix-boot.service\n\n[Service]\nType=oneshot\nExecStart=/usr/local/bin/archix-powercut-booted\n\n[Install]\nWantedBy=multi-user.target",
  },
  services = {
    ["systemd-networkd.service"] = true,
    ["archix-powercut-booted.service"] = true,
  },
  -- Deferred to each new deployment's first boot, so activation replays it.
  hooks = {
    powercut = { run = "cat /etc/hostname /usr/lib/os-release > /var/lib/archix-powercut-hook" },
  },
}
