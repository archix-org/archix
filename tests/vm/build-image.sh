#!/usr/bin/env bash
# Builds /work/disk.img for IMAGE_DIR's suite: GPT with an ESP and an ext4 root (FS=btrfs: btrfs with an @ subvolume, as CachyOS installs it), Arch pacstrapped at the test snapshot with archix's binary, booting like a plain install; the suite makes the first switch on its first boot.
set -euo pipefail
archive=${ARCHIVE:-https://archive.archlinux.org}
dir=${IMAGE_DIR:?IMAGE_DIR=/vm/SUITE names the suite whose image this builds} date=2026-10-10 size=8G packages=(base linux mkinitcpio systemd-ukify git e2fsprogs) archix=/work/archix
fs=${FS:-ext4}
# This container's /dev holds only the loop nodes that existed when it started, and a free device past them fails losetup while other runs hold the low ones.
for i in {0..255}; do [ -e /dev/loop$i ] || mknod -m 660 /dev/loop$i b 7 $i; done
# The suite's config.lua becomes /etc/archix/init.lua; its image.env may change the variables above and define before_pacstrap, after_pacstrap, install_archix and after_image.
[ ! -f "$dir/image.env" ] || . "$dir/image.env"
[ "$fs" = btrfs ] && packages+=(btrfs-progs)
img=/work/disk.img
rm -f "$img" /work/vars.fd
truncate -s "$size" "$img"
# 1 GiB, as the Arch installation guide suggests; image.env's partitions may add more after the root.
printf '%b' "${partitions:-label: gpt\nsize=1G, type=uefi\ntype=linux\n}" | sfdisk --quiet "$img"

part() { sfdisk -J "$img" | python -c "import json,sys; p=json.load(sys.stdin)['partitiontable']['partitions'][$1]; print(p['start']*512, p['size']*512, p['uuid'])"; }
read -r esp_start esp_size _ < <(part 0)
read -r root_start root_size root_uuid < <(part 1)
esp=$(losetup --show -f -o "$esp_start" --sizelimit "$esp_size" "$img")
root=$(losetup --show -f -o "$root_start" --sizelimit "$root_size" "$img")
trap 'umount -R /mnt 2>/dev/null || true; losetup -d "$esp" "$root" 2>/dev/null || true' EXIT
mkfs.fat -F 32 -n ESP "$esp" >/dev/null
if [ "$fs" = btrfs ]; then
  mkfs.btrfs -q -L archixroot "$root"
  mount "$root" /mnt && btrfs -q subvolume create /mnt/@ && umount /mnt
  mount -o subvol=@ "$root" /mnt
else
  mkfs.ext4 -q -L archixroot "$root"
  mount "$root" /mnt
fi
mkdir -p /mnt/efi
# bootctl won't trust a random seed on a world-readable ESP.
mount -o umask=0077 "$esp" /mnt/efi

grep -v '^NoExtract' /etc/pacman.conf > /tmp/pacman.conf
! declare -F before_pacstrap > /dev/null || before_pacstrap
echo "Server = $archive/repos/${date//-//}/\$repo/os/\$arch" > /tmp/mirrorlist
sed 's|/etc/pacman.d/mirrorlist|/tmp/mirrorlist|' /tmp/pacman.conf > /tmp/pacstrap.conf
for attempt in 1 2 3; do pacstrap -C /tmp/pacstrap.conf -K /mnt "${packages[@]}" >/dev/null && break; [ "$attempt" = 3 ] && exit 1; sleep 20; done
cp /tmp/pacman.conf /mnt/etc/pacman.conf
cp /tmp/mirrorlist /mnt/etc/pacman.d/mirrorlist
! declare -F after_pacstrap > /dev/null || after_pacstrap
# Defence in depth: the test systems' systemd never arms a hardware watchdog.
mkdir -p /mnt/etc/systemd/system.conf.d && printf '[Manager]\nRuntimeWatchdogSec=0\nRebootWatchdogSec=0\nKExecWatchdogSec=0\n' > /mnt/etc/systemd/system.conf.d/archix-no-watchdog.conf
# The filesystem package's resolv.conf is empty; the suites enable systemd-resolved, so point at its stub as an Arch install would.
ln -sf /run/systemd/resolve/stub-resolv.conf /mnt/etc/resolv.conf
if declare -F install_archix > /dev/null; then
  install_archix
else
  install -Dm755 "$archix" /mnt/usr/bin/archix
fi
bootctl --esp-path=/mnt/efi --no-variables install >/dev/null
printf 'timeout 2\nconsole-mode keep\n' > /mnt/efi/loader/loader.conf
# The ESP in fstab, as on any real install: early boot units must not wait on it in a cycle.
echo "UUID=$(blkid -s UUID -o value "$esp") /efi vfat defaults 0 2" >> /mnt/etc/fstab

mkdir -p /mnt/etc/archix
rootflags= progs=; [ "$fs" = btrfs ] && rootflags='"rootflags=subvol=@", ' progs='"btrfs-progs", '
extra=$(for package in ${TARGET_PACKAGES:-}; do printf '"%s", ' "$package"; done)
sed -e "s/@ROOT_UUID@/${root_uuid,,}/" -e "s|\"rw\", |\"rw\", $rootflags|" -e "s|{ \"base\", |{ \"base\", $progs|" -e "s|@ARCHIVE_URL@|$archive|" \
  -e "s|@EXTRA_PACKAGES@|$extra|" "$dir/config.lua" > /mnt/etc/archix/init.lua
# The guest runs the driver's root shell, which config.lua takes from here.
install -Dm755 /vm/driver/backdoor.sh /mnt/etc/archix/archix-backdoor
arch-chroot /mnt archix --host archix-vm update --to "$date" --noconfirm
# The suites make the first switch on its first boot, with services running, so the image boots like a plain Arch install.
install -Dm755 /vm/driver/backdoor.sh /mnt/opt/archix-vmtest/bin/archix-backdoor
printf "[Unit]\nDescription=archix VM test driver's root shell\nWants=network-online.target\n\n[Service]\nExecStart=/opt/archix-vmtest/bin/archix-backdoor\n\n[Install]\nWantedBy=multi-user.target\n" > /mnt/etc/systemd/system/archix-backdoor.service
printf '[Match]\nName=en*\n\n[Network]\nDHCP=yes\n' > /mnt/etc/systemd/network/20-wired.network
# A service that reads /usr all the time from a read-only view of its own, as ProtectSystem=strict gives it.
printf '[Service]\nProtectSystem=strict\nExecStart=/bin/sh -c "while cat /usr/lib/os-release > /dev/null && ls /usr/bin > /dev/null; do sleep 0.2; done; exit 1"\n\n[Install]\nWantedBy=multi-user.target\n' > /mnt/etc/systemd/system/archix-protected.service
arch-chroot /mnt systemctl enable archix-backdoor.service archix-protected.service systemd-networkd.service systemd-resolved.service
sed -n 's/.*kernel_params = { \(.*\) },$/\1/p' /mnt/etc/archix/init.lua | tr -d '",' > /mnt/etc/kernel/cmdline
# Each kernel's mkinitcpio preset writes a UKI to the ESP, which systemd-boot lists by itself, so pacman's own hooks rebuild it after a kernel or DKMS change as on a stock install; autodetect is skipped here, where it would keep only the build host's modules.
for preset in /mnt/etc/mkinitcpio.d/*.preset; do
  pkgbase=$(basename "$preset" .preset)
  printf 'ALL_kver="/boot/vmlinuz-%s"\nPRESETS=(default)\ndefault_uki="/efi/EFI/Linux/arch-%s.efi"\n' "$pkgbase" "$pkgbase" > "$preset"
  arch-chroot /mnt mkinitcpio -k "/boot/vmlinuz-$pkgbase" -S autodetect -U "/efi/EFI/Linux/arch-$pkgbase.efi"
done
rm -f /mnt/boot/initramfs-*.img
ls /mnt/efi/EFI/Linux
sed -i "s|$archive|${VM_ARCHIVE:-$archive}|" /mnt/etc/archix/init.lua /mnt/etc/pacman.d/mirrorlist
[ ! -f /mnt/var/lib/archix/archive-url ] || sed -i "s|$archive|${VM_ARCHIVE:-$archive}|" /mnt/var/lib/archix/archive-url
! declare -F after_image > /dev/null || after_image
