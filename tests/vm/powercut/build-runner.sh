#!/usr/bin/env bash
# Builds /work/runner.img: plain Arch booted by systemd-boot, with dm-log-writes' replay-log, archix for the initramfs step, QEMU and OVMF for nested real boots and the power-cut scripts, run at boot by archix-powercut.service.
set -euo pipefail
archive=${ARCHIVE:-https://archive.archlinux.org}
date=2026-10-10
img=/work/runner.img
rm -f "$img"
truncate -s 6G "$img"
sfdisk --quiet "$img" << SFDISK
label: gpt
size=300M, type=uefi
type=linux
SFDISK
part() { sfdisk -J "$img" | python -c "import json,sys; p=json.load(sys.stdin)['partitiontable']['partitions'][$1]; print(p['start']*512, p['size']*512, p['uuid'])"; }
read -r esp_start esp_size _ < <(part 0)
read -r root_start root_size root_uuid < <(part 1)
esp=$(losetup --show -f -o "$esp_start" --sizelimit "$esp_size" "$img")
root=$(losetup --show -f -o "$root_start" --sizelimit "$root_size" "$img")
trap 'gpgconf --homedir /mnt/etc/pacman.d/gnupg --kill all 2> /dev/null; umount -R /mnt 2> /dev/null || true; losetup -d "$esp" "$root" 2> /dev/null || true' EXIT
mkfs.fat -F 32 "$esp" > /dev/null
mkfs.ext4 -q "$root"
mount "$root" /mnt
mkdir -p /mnt/boot
mount "$esp" /mnt/boot
grep -v '^NoExtract' /etc/pacman.conf | sed 's|/etc/pacman.d/mirrorlist|/tmp/runner-mirrorlist|' > /tmp/runner-pacman.conf
echo "Server = $archive/repos/${date//-//}/\$repo/os/\$arch" > /tmp/runner-mirrorlist
for attempt in 1 2 3; do
  pacstrap -C /tmp/runner-pacman.conf /mnt base linux mkinitcpio dosfstools btrfs-progs python qemu-base edk2-ovmf > /dev/null && break
  [ "$attempt" = 3 ] && exit 1
  sleep 20
done
kernel=$(basename "$(dirname "$(ls /mnt/usr/lib/modules/*/vmlinuz | head -1)")")
arch-chroot /mnt mkinitcpio -k "$kernel" -S autodetect -g /boot/initramfs-linux.img > /dev/null 2>&1
bootctl --esp-path=/mnt/boot --no-variables install > /dev/null 2>&1
printf 'default runner.conf\ntimeout 0\n' > /mnt/boot/loader/loader.conf
printf 'title runner\nlinux /vmlinuz-linux\ninitrd /initramfs-linux.img\noptions root=PARTUUID=%s rw console=ttyS0 quiet\n' "${root_uuid,,}" > /mnt/boot/loader/entries/runner.conf
install -Dm755 /work/replay-log /mnt/usr/local/bin/replay-log
install -Dm755 /work/archix-static /mnt/usr/local/bin/archix
install -Dm755 /vm/deploy/proto/check-deployment /mnt/usr/local/bin/archix-check-deployment
for tool in check-point invariant findings mark point-table var-manifest; do install -Dm755 "/vm/powercut/$tool" "/mnt/usr/local/lib/archix-powercut/$tool"; done
install -Dm755 /vm/powercut/vmtest.sh /mnt/usr/local/bin/archix-powercut
printf 'POINTS=%s\nBREAK=%s\nCOUNTING=%s\nSAMPLE=%s\n' "${REPLAY_POINTS:-}" "${BREAK:-}" "${COUNTING-1}" "${SAMPLE-20}" > /mnt/etc/archix-powercut.conf
printf '[Unit]\nDescription=archix power-cut replay\nAfter=multi-user.target\n\n[Service]\nType=oneshot\nExecStart=/usr/local/bin/archix-powercut\nStandardOutput=journal+console\nTimeoutSec=0\n\n[Install]\nWantedBy=multi-user.target\n' \
  > /mnt/etc/systemd/system/archix-powercut.service
arch-chroot /mnt systemctl enable archix-powercut.service > /dev/null 2>&1
