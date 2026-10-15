#!/usr/bin/env bash
# Builds /work/disk.img: GPT with an ESP and an ext4 root, Arch pacstrapped at the test snapshot, archix generation 1.
set -euo pipefail
img=/work/disk.img
rm -f "$img" /work/vars.fd
truncate -s 8G "$img"
sfdisk --quiet "$img" <<SFDISK
label: gpt
size=600M, type=uefi
type=linux
SFDISK

part() { sfdisk -J "$img" | python -c "import json,sys; p=json.load(sys.stdin)['partitiontable']['partitions'][$1]; print(p['start']*512, p['size']*512, p['uuid'])"; }
read -r esp_start esp_size _ < <(part 0)
read -r root_start root_size root_uuid < <(part 1)
esp=$(losetup --show -f -o "$esp_start" --sizelimit "$esp_size" "$img")
root=$(losetup --show -f -o "$root_start" --sizelimit "$root_size" "$img")
trap 'umount -R /mnt 2>/dev/null || true; losetup -d "$esp" "$root" 2>/dev/null || true' EXIT
mkfs.fat -F 32 -n ESP "$esp" >/dev/null
mkfs.ext4 -q -L archixroot "$root"
mount "$root" /mnt
mkdir -p /mnt/boot
mount "$esp" /mnt/boot

grep -v '^NoExtract' /etc/pacman.conf > /tmp/pacman.conf
pacstrap -C /tmp/pacman.conf -K /mnt base linux mkinitcpio systemd-ukify sbctl >/dev/null
cp /tmp/pacman.conf /mnt/etc/pacman.conf
cp /etc/pacman.d/mirrorlist /mnt/etc/pacman.d/mirrorlist
install -Dm755 /work/archix /mnt/usr/bin/archix
install -Dm755 /work/archix-static /mnt/usr/lib/archix/archix-static
bootctl --esp-path=/mnt/boot --no-variables install >/dev/null
printf 'timeout 2\nconsole-mode keep\n' > /mnt/boot/loader/loader.conf

mkdir -p /mnt/etc/archix
sed "s/@ROOT_UUID@/${root_uuid,,}/" /vm/config.lua > /mnt/etc/archix/init.lua
install -Dm755 /vm/vmtest.sh /mnt/etc/archix/vmtest.sh
arch-chroot /mnt archix --host archix-vm update --to 2026-10-10 --noconfirm
arch-chroot /mnt archix --host archix-vm switch --noconfirm
ls /mnt/boot/EFI/Linux
