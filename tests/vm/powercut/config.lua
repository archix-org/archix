return {
  packages = { "base", "linux", "mkinitcpio", "systemd-ukify", @EXTRA_PACKAGES@ }, -- packages
  hostname = "archix-vm",
  archive_url = "@ARCHIVE_URL@",
  kernel_params = { "root=PARTUUID=@ROOT_UUID@", "rw", "console=ttyS0" },
  boot = { loader = "systemd-boot", rescue_entry = false },
  files = {
    ["/etc/systemd/network/20-wired.network"] = { text = "[Match]\nName=en*\n\n[Network]\nDHCP=yes" },
    -- What a real boot of a replayed state reports before powering off: the seconds from the kernel's start to this report, before its own checks take any, the entry systemd-boot started, the deployment archix mounted, whether the ESP is still writable once a file with a broken FAT chain (esp-case's CHAIN.BIN) is read and whether systemd-fsck checked it (its result and how many lines fsck logged), the package files whose content pacman finds altered (modified backup files it leaves out), the failed units with their last journal lines, and the kernel's FAT and the ESP fsck's messages.
    ["/usr/local/bin/archix-powercut-booted"] = { mode = "0755", text = [[#!/bin/sh
boot=$(cut -d. -f1 /proc/uptime)
ls /efi > /dev/null
[ ! -e /efi/CHAIN.BIN ] || cat /efi/CHAIN.BIN > /dev/null 2>&1
esp=$(findmnt -no OPTIONS -t vfat /efi | cut -d, -f1)
unit=$(systemctl show -p Requires --value efi.mount | grep -o 'systemd-fsck@[^" ]*\.service' | sed 's/\\\\/\\/g' | head -1)
espfsck=none
[ -z "$unit" ] || espfsck="$(systemctl show -p Result --value "$unit"):$(journalctl -b -o cat --no-pager _SYSTEMD_UNIT="$unit" | grep -c .)"
entry=$(tr -d '\000' < /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f | tr -cd '[:alnum:]+.-')
altered=$(pacman -Qkk 2>&1 > /dev/null | grep -E '\((No such file or directory|Size mismatch|SHA256 checksum mismatch|Symlink path mismatch|Permissions mismatch|UID mismatch|GID mismatch)\)' \
  | grep -E ': /(usr|opt|boot|etc)/|No such file' | sed 's/ ([^)]*)$//' | sort -u)
[ -z "$altered" ] || echo "$altered" | sed 's/^/POWERCUT-ALTERED /' > /dev/console
failed=$(systemctl --failed --no-legend --plain | awk '{ print $1 }')
for unit in $failed; do journalctl -b -u "$unit" -o short-monotonic --no-pager | tail -25 | sed "s/^/POWERCUT-FAILED $unit: /" > /dev/console; done
{ journalctl -b -k -o cat --no-pager | grep -i 'fat-fs\|vfat' | tail -4; journalctl -b -u 'systemd-fsck@*' -o cat --no-pager | tail -4; } | sed 's/^/POWERCUT-KERNEL /' > /dev/console
echo "POWERCUT-BOOTED entry=$entry deployment=$(cat /run/archix/deployment) current=$(readlink /archix/current) esp=${esp:-unmounted} espfsck=$espfsck boot=$boot altered=$(printf '%s' "$altered" | grep -c .) failed=$(echo $failed | tr ' ' ,)" > /dev/console
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
