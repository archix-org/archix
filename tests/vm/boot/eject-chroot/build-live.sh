#!/usr/bin/env bash
# Run by build-image.sh with the boot image's plain root at /mnt: builds eject-chroot's live system, /work/live.img (an ext4 root labelled archixlive, the plain root without archix, with the Arch ISO's arch-chroot, btrfs-progs and dosfstools) and the kernel and initramfs QEMU boots it with, as a USB stick's own kernel boots.
set -euo pipefail
img=/work/live.img live=/live
rm -f "$img" /work/live-vmlinuz /work/live-initramfs.img
truncate -s 4G "$img"
mkfs.ext4 -q -L archixlive "$img"
mkdir -p "$live"
mount -o loop "$img" "$live"
trap 'umount -R "$live" 2>/dev/null || true' EXIT
# The ESP mounted at /mnt/efi stays out; so does everything of archix's, as a live USB has none of it.
cp -ax /mnt/. "$live"/
rm -rf "$live"/etc/archix "$live"/usr/bin/archix "$live"/usr/lib/archix "$live"/var/lib/archix "$live"/var/cache/archix "$live"/usr/local/share/archix-vmtest "$live"/opt/archix-vmtest/bin/archix-check-deployment
rm -f "$live"/etc/systemd/system/multi-user.target.wants/archix-protected.service "$live"/etc/systemd/system/archix-protected.service "$live"/etc/kernel/cmdline
echo archiso > "$live"/etc/hostname
: > "$live"/etc/fstab
for attempt in 1 2 3; do pacstrap -C /tmp/pacstrap.conf "$live" arch-install-scripts btrfs-progs dosfstools > /dev/null && break; [ "$attempt" = 3 ] && exit 1; sleep 20; done
kernel=$(basename "$(dirname "$(ls "$live"/usr/lib/modules/*/vmlinuz | head -1)")")
arch-chroot "$live" mkinitcpio -k "$kernel" -S autodetect -g /boot/initramfs-live.img > /dev/null
cp "$live/usr/lib/modules/$kernel/vmlinuz" /work/live-vmlinuz
cp "$live/boot/initramfs-live.img" /work/live-initramfs.img
if grep -rl archix "$live"/etc/pacman.d/hooks "$live"/etc/mkinitcpio.conf "$live"/etc/mkinitcpio.conf.d 2> /dev/null; then
  echo "the live system has archix in its pacman or mkinitcpio config" >&2
  exit 1
fi
ls -la /work/live.img /work/live-vmlinuz /work/live-initramfs.img
