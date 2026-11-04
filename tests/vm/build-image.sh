#!/usr/bin/env bash
# Builds /work/disk.img: GPT with an ESP and an ext4 root (FS=btrfs: btrfs with an @ subvolume, as CachyOS installs it), Arch pacstrapped at the test snapshot with archix's binaries and a plain UKI, booting like a plain install; the tests' base makes the first switch on its first boot.
set -euo pipefail
archive=${ARCHIVE:-https://archive.archlinux.org}
dir=/vm/boot date=2026-10-10 size=8G packages=(base linux mkinitcpio systemd-ukify sbctl git e2fsprogs) archix=/work/archix
fs=${FS:-ext4}
[ "$fs" = btrfs ] && packages+=(btrfs-progs)
# LOADER=limine builds an image Limine boots from the ESP, with the kernel and initramfs there, as CachyOS installs it.
loader=${LOADER:-systemd-boot}
[ "$loader" != limine ] || packages+=(limine efibootmgr)
# IMG=/work/top.img with ROOT_AT_TOP=1 builds the power-cut suite's second image, its btrfs root on the top level rather than in @.
img=${IMG:-/work/disk.img}
rm -f "$img"
[ -n "${IMG:-}" ] || rm -f /work/vars.fd
truncate -s "$size" "$img"
# 1 GiB, as the Arch installation guide suggests.
sfdisk --quiet "$img" <<SFDISK
label: gpt
size=1G, type=uefi
type=linux
SFDISK

part() { sfdisk -J "$img" | python -c "import json,sys; p=json.load(sys.stdin)['partitiontable']['partitions'][$1]; print(p['start']*512, p['size']*512, p['uuid'])"; }
read -r esp_start esp_size _ < <(part 0)
read -r root_start root_size root_uuid < <(part 1)
esp=$(losetup --show -f -o "$esp_start" --sizelimit "$esp_size" "$img")
root=$(losetup --show -f -o "$root_start" --sizelimit "$root_size" "$img")
trap 'umount -R /mnt 2>/dev/null || true; losetup -d "$esp" "$root" 2>/dev/null || true' EXIT
mkfs.fat -F 32 -n ESP "$esp" >/dev/null
if [ "$fs" = btrfs ]; then
  mkfs.btrfs -q -L archixroot "$root"
  if [ -n "${ROOT_AT_TOP:-}" ]; then
    mount "$root" /mnt
  else
    mount "$root" /mnt && btrfs -q subvolume create /mnt/@ && umount /mnt
    mount -o subvol=@ "$root" /mnt
  fi
else
  mkfs.ext4 -q -L archixroot "$root"
  mount "$root" /mnt
fi
mkdir -p /mnt/efi
# bootctl won't trust a random seed on a world-readable ESP.
mount -o umask=0077 "$esp" /mnt/efi

grep -v '^NoExtract' /etc/pacman.conf > /tmp/pacman.conf
echo "Server = $archive/repos/${date//-//}/\$repo/os/\$arch" > /tmp/mirrorlist
sed 's|/etc/pacman.d/mirrorlist|/tmp/mirrorlist|' /tmp/pacman.conf > /tmp/pacstrap.conf
for attempt in 1 2 3; do pacstrap -C /tmp/pacstrap.conf -K /mnt "${packages[@]}" >/dev/null && break; [ "$attempt" = 3 ] && exit 1; sleep 20; done
cp /tmp/pacman.conf /mnt/etc/pacman.conf
cp /tmp/mirrorlist /mnt/etc/pacman.d/mirrorlist
# The filesystem package's resolv.conf is empty; the suites enable systemd-resolved, so point at its stub as an Arch install would.
ln -sf /run/systemd/resolve/stub-resolv.conf /mnt/etc/resolv.conf
install -Dm755 "$archix" /mnt/usr/bin/archix
install -Dm755 /work/archix-static /mnt/usr/lib/archix/archix-static
if [ "$loader" = limine ]; then
  install -Dm644 /mnt/usr/share/limine/BOOTX64.EFI /mnt/efi/EFI/BOOT/BOOTX64.EFI
  install -Dm644 /mnt/usr/share/limine/BOOTX64.EFI /mnt/efi/EFI/limine/BOOTX64.EFI
else
  bootctl --esp-path=/mnt/efi --no-variables install >/dev/null
  printf 'timeout 2\nconsole-mode keep\n' > /mnt/efi/loader/loader.conf
fi
# The ESP in fstab, as on any real install: early boot units must not wait on it in a cycle.
echo "UUID=$(blkid -s UUID -o value "$esp") /efi vfat defaults 0 2" >> /mnt/etc/fstab

mkdir -p /mnt/etc/archix
rootflags= progs=; [ "$fs" = btrfs ] && rootflags='"rootflags=subvol=@", ' progs='"btrfs-progs", '
[ -z "${ROOT_AT_TOP:-}" ] || rootflags=
extra=$(for package in ${TARGET_PACKAGES:-}; do printf '"%s", ' "$package"; done)
[ "$loader" != limine ] || progs+='"limine", "efibootmgr", '
sed -e "s/@ROOT_UUID@/${root_uuid,,}/" -e "s|loader = \"systemd-boot\"|loader = \"$loader\"|" -e "s|\"rw\", |\"rw\", $rootflags|" -e "s|{ \"base\", |{ \"base\", $progs|" -e "s|@ARCHIVE_URL@|$archive|" \
  -e "s|@EXTRA_PACKAGES@|$extra|" "$dir/config.lua" > /mnt/etc/archix/init.lua
# The guest runs the driver's root shell, which config.lua takes from here.
install -Dm755 /vm/driver/backdoor.sh /mnt/etc/archix/archix-backdoor
arch-chroot /mnt archix --host archix-vm update --to "$date" --noconfirm
# The tests lay the system out on their first boot, with services running, so the image boots like a plain Arch install: a UKI on the ESP, which systemd-boot lists by itself.
install -Dm755 /vm/driver/backdoor.sh /mnt/opt/archix-vmtest/bin/archix-backdoor
printf "[Unit]\nDescription=archix VM test driver's root shell\nWants=network-online.target\n\n[Service]\nExecStart=/opt/archix-vmtest/bin/archix-backdoor\n\n[Install]\nWantedBy=multi-user.target\n" > /mnt/etc/systemd/system/archix-backdoor.service
printf '[Match]\nName=en*\n\n[Network]\nDHCP=yes\n' > /mnt/etc/systemd/network/20-wired.network
# A service that reads /usr all the time from a read-only view of its own, as ProtectSystem=strict gives it.
printf '[Service]\nProtectSystem=strict\nExecStart=/bin/sh -c "while cat /usr/lib/os-release > /dev/null && ls /usr/bin > /dev/null; do sleep 0.2; done; exit 1"\n\n[Install]\nWantedBy=multi-user.target\n' > /mnt/etc/systemd/system/archix-protected.service
arch-chroot /mnt systemctl enable archix-backdoor.service archix-protected.service systemd-networkd.service systemd-resolved.service
sed -n 's/.*kernel_params = { \(.*\) },$/\1/p' /mnt/etc/archix/init.lua | tr -d '",' > /mnt/etc/kernel/cmdline
kernel=$(basename "$(dirname "$(ls /mnt/usr/lib/modules/*/vmlinuz | head -1)")")
if [ "$loader" = limine ]; then
  cp "/mnt/usr/lib/modules/$kernel/vmlinuz" /mnt/efi/vmlinuz-linux
  arch-chroot /mnt mkinitcpio -k "$kernel" -S autodetect -g /efi/initramfs-linux.img
  conf=$(printf 'timeout: 1\n\n/Arch Linux\n    protocol: linux\n    path: boot():/vmlinuz-linux\n    cmdline: %s\n    module_path: boot():/initramfs-linux.img\n' "$(cat /mnt/etc/kernel/cmdline)")
  printf '%s\n' "$conf" > /mnt/efi/limine.conf
  printf '%s\n' "$conf" > /mnt/efi/EFI/limine/limine.conf
  ls /mnt/efi /mnt/efi/EFI/limine
else
  mkdir -p /mnt/efi/EFI/Linux
  arch-chroot /mnt mkinitcpio -k "$kernel" -S autodetect -U /efi/EFI/Linux/arch-linux.efi
  ls /mnt/efi/EFI/Linux
fi
sed -i "s|$archive|${VM_ARCHIVE:-$archive}|" /mnt/etc/archix/init.lua /mnt/etc/pacman.d/mirrorlist
[ ! -f /mnt/var/lib/archix/archive-url ] || sed -i "s|$archive|${VM_ARCHIVE:-$archive}|" /mnt/var/lib/archix/archive-url
