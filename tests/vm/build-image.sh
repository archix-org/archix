#!/usr/bin/env bash
# Builds /work/disk.img: GPT with an ESP and an ext4 root (FS=btrfs: btrfs with an @ subvolume, as CachyOS installs it), Arch pacstrapped at the test snapshot, archix generation 1.
set -euo pipefail
suite=${SUITE:-boot}
archive=${ARCHIVE:-https://archive.archlinux.org}
dir=/vm date=2026-10-10 size=8G packages=(base linux mkinitcpio systemd-ukify sbctl) archix=/work/archix
# The rollback suite starts at an older snapshot and rolls glibc back, so archix itself must not need the build host's glibc.
if [ "$suite" = rollback ]; then
  dir=/vm/rollback date=2026-06-01 size=16G packages=(base linux mkinitcpio systemd-ukify devtools) archix=/work/archix-static
fi
fs=${FS:-ext4}
[ "$fs" = btrfs ] && packages+=(btrfs-progs)
img=/work/disk.img
rm -f "$img" /work/vars.fd
truncate -s "$size" "$img"
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
if [ "$fs" = btrfs ]; then
  mkfs.btrfs -q -L archixroot "$root"
  mount "$root" /mnt && btrfs -q subvolume create /mnt/@ && umount /mnt
  mount -o subvol=@ "$root" /mnt
else
  mkfs.ext4 -q -L archixroot "$root"
  mount "$root" /mnt
fi
mkdir -p /mnt/boot
mount "$esp" /mnt/boot

grep -v '^NoExtract' /etc/pacman.conf > /tmp/pacman.conf
# pacstrap at the test's snapshot, so generation 1 isn't newer than its own lock.
echo "Server = $archive/repos/${date//-//}/\$repo/os/\$arch" > /tmp/mirrorlist
sed 's|/etc/pacman.d/mirrorlist|/tmp/mirrorlist|' /tmp/pacman.conf > /tmp/pacstrap.conf
for attempt in 1 2 3; do pacstrap -C /tmp/pacstrap.conf -K /mnt "${packages[@]}" >/dev/null && break; [ "$attempt" = 3 ] && exit 1; sleep 20; done
cp /tmp/pacman.conf /mnt/etc/pacman.conf
cp /tmp/mirrorlist /mnt/etc/pacman.d/mirrorlist
install -Dm755 "$archix" /mnt/usr/bin/archix
install -Dm755 /work/archix-static /mnt/usr/lib/archix/archix-static
bootctl --esp-path=/mnt/boot --no-variables install >/dev/null
# The ESP in fstab, as on any real install: early boot units must not wait on it in a cycle.
echo "UUID=$(blkid -s UUID -o value "$esp") /boot vfat defaults 0 2" >> /mnt/etc/fstab
printf 'timeout 2\nconsole-mode keep\n' > /mnt/boot/loader/loader.conf

mkdir -p /mnt/etc/archix
rootflags= progs=; [ "$fs" = btrfs ] && rootflags='"rootflags=subvol=@", ' progs='"btrfs-progs", '
sed -e "s/@ROOT_UUID@/${root_uuid,,}/" -e "s|\"rw\", |\"rw\", $rootflags|" -e "s|{ \"base\", |{ \"base\", $progs|" -e "s|@ARCHIVE_URL@|$archive|" "$dir/config.lua" > /mnt/etc/archix/init.lua
install -Dm755 "$dir/vmtest.sh" /mnt/etc/archix/vmtest.sh
[ -d "$dir/pkgbuilds" ] && cp -r "$dir/pkgbuilds" /mnt/etc/archix/
arch-chroot /mnt archix --host archix-vm update --to "$date" --noconfirm
arch-chroot /mnt archix --host archix-vm switch --noconfirm
ls /mnt/boot/EFI/Linux
# Inside the VM the host-side mirror has another address.
sed -i "s|$archive|${VM_ARCHIVE:-$archive}|" /mnt/etc/archix/init.lua /mnt/etc/pacman.d/mirrorlist /mnt/var/lib/archix/archive-url
