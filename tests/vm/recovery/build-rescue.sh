#!/usr/bin/env bash
# Builds /work/rescue.img for the recovery suite: plain Arch with the tools the Arch ISO has, on an ESP and root of its own, booted by systemd-boot as the ISO is, which runs recover.sh at boot.
set -euo pipefail
img=/work/rescue.img mnt=/rescue
rm -f "$img"
truncate -s 6G "$img"
sfdisk --quiet "$img" <<SFDISK
label: gpt
size=300M, type=uefi
type=linux
SFDISK
part() { sfdisk -J "$img" | python -c "import json,sys; p=json.load(sys.stdin)['partitiontable']['partitions'][$1]; print(p['start']*512, p['size']*512)"; }
read -r esp_start esp_size < <(part 0)
read -r root_start root_size < <(part 1)
esp=$(losetup --show -f -o "$esp_start" --sizelimit "$esp_size" "$img")
root=$(losetup --show -f -o "$root_start" --sizelimit "$root_size" "$img")
trap 'umount -R "$mnt" 2>/dev/null || true; losetup -d "$esp" "$root" 2>/dev/null || true' EXIT
mkfs.fat -F 32 -n RESCUE "$esp" > /dev/null
mkfs.ext4 -q -L rescueroot "$root"
mkdir -p "$mnt"
mount "$root" "$mnt"
mkdir -p "$mnt/efi"
mount -o umask=0077 "$esp" "$mnt/efi"
# The package set build-image.sh pacstrapped with, at the same snapshot.
for attempt in 1 2 3; do pacstrap -C /tmp/pacstrap.conf -K "$mnt" base linux mkinitcpio systemd-ukify arch-install-scripts btrfs-progs dosfstools > /dev/null && break; [ "$attempt" = 3 ] && exit 1; sleep 20; done
echo "root=UUID=$(blkid -s UUID -o value "$root") rw console=ttyS0" > "$mnt/etc/kernel/cmdline"
install -Dm755 /vm/recovery/recover.sh "$mnt/usr/local/bin/archix-recover"
printf '[Unit]\nDescription=archix recovery test\nAfter=multi-user.target\n\n[Service]\nType=oneshot\nExecStart=/usr/local/bin/archix-recover\nStandardOutput=journal+console\nTimeoutSec=0\n\n[Install]\nWantedBy=multi-user.target\n' > "$mnt/etc/systemd/system/archix-recover.service"
arch-chroot "$mnt" systemctl enable archix-recover.service > /dev/null 2>&1
kernel=$(basename "$(dirname "$(ls "$mnt"/usr/lib/modules/*/vmlinuz | head -1)")")
mkdir -p "$mnt/efi/EFI/Linux"
arch-chroot "$mnt" mkinitcpio -k "$kernel" -S autodetect -U /efi/EFI/Linux/rescue.efi > /dev/null
bootctl --esp-path="$mnt/efi" --no-variables install > /dev/null
printf 'timeout 0\nconsole-mode keep\n' > "$mnt/efi/loader/loader.conf"
ls "$mnt/efi/EFI/Linux"
