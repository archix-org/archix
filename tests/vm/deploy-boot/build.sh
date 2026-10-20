#!/usr/bin/env bash
# Builds /work/disk.img: Arch laid out as deployments 1 and 2 (current), an uncommitted .staging-3 and an entry for each, booted through archix's hook in a $INITRAMFS initramfs.
set -euo pipefail
archive=${ARCHIVE:-https://archive.archlinux.org}
date=${DATE:-2026-10-03}
fs=${FS:-ext4}
packages=(base linux mkinitcpio systemd-ukify)
[ "$fs" = btrfs ] && packages+=(btrfs-progs)
img=/work/disk.img
rm -f "$img" /work/vars.fd
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
trap 'gpgconf --homedir /mnt/etc/pacman.d/gnupg --kill all 2> /dev/null; umount -R /mnt 2>/dev/null || true; losetup -d "$esp" "$root" 2>/dev/null || true' EXIT
mkfs.fat -F 32 -n ESP "$esp" > /dev/null
rootflags=
if [ "$fs" = btrfs ]; then
  mkfs.btrfs -q -L archixroot "$root"
  mount "$root" /mnt && btrfs -q subvolume create /mnt/@ && umount /mnt
  mount -o subvol=@ "$root" /mnt
  rootflags=" rootflags=subvol=@"
else
  mkfs.ext4 -q -L archixroot "$root"
  mount "$root" /mnt
fi
mkdir -p /mnt/efi
mount "$esp" /mnt/efi

grep -v '^NoExtract' /etc/pacman.conf > /tmp/pacman.conf
echo "Server = $archive/repos/${date//-//}/\$repo/os/\$arch" > /tmp/mirrorlist
sed 's|/etc/pacman.d/mirrorlist|/tmp/mirrorlist|' /tmp/pacman.conf > /tmp/pacstrap.conf
for attempt in 1 2 3; do pacstrap -C /tmp/pacstrap.conf -K /mnt "${packages[@]}" > /dev/null && break; [ "$attempt" = 3 ] && exit 1; sleep 20; done
echo "root=PARTUUID=${root_uuid,,} ${ROOT_OPTS:-ro}${rootflags} console=ttyS0" > /mnt/etc/kernel/cmdline
# With `ro` on the command line, systemd-remount-fs makes the root writable only through an fstab entry.
echo "PARTUUID=${root_uuid,,} / $fs rw${rootflags:+,subvol=@} 0 1" >> /mnt/etc/fstab
echo "UUID=$(blkid -s UUID -o value "$esp") /efi vfat defaults 0 2" >> /mnt/etc/fstab

install -Dm755 /work/archix-static /mnt/usr/lib/archix/archix-static
install -Dm755 /work/archix-static /mnt/usr/bin/archix
cp -r /work/files/. /mnt/
mkdir -p /mnt/etc/systemd/system/sysinit.target.wants
ln -s /etc/systemd/system/archix-deploy-activate.service /mnt/etc/systemd/system/sysinit.target.wants/archix-deploy-activate.service
if [ "${INITRAMFS:-systemd}" = busybox ]; then
  sed -i 's/^HOOKS=.*/HOOKS=(base udev autodetect microcode modconf kms keyboard keymap consolefont block filesystems fsck)/' /mnt/etc/mkinitcpio.conf
fi
grep '^HOOKS=' /mnt/etc/mkinitcpio.conf
install -Dm755 /vm/deploy-boot/vmtest.sh /mnt/usr/local/bin/archix-vm-test
printf '[Unit]\nDescription=archix deployment boot test\nAfter=multi-user.target\n\n[Service]\nType=oneshot\nExecStart=/usr/local/bin/archix-vm-test\nStandardOutput=journal+console\nTimeoutSec=0\n\n[Install]\nWantedBy=multi-user.target\n' \
  > /mnt/etc/systemd/system/archix-vm-test.service
arch-chroot /mnt systemctl enable archix-vm-test > /dev/null 2>&1

# Live state that deployment 2's /var changes must create, replace or leave alone.
mkdir -p /mnt/var/lib/archix-vm /mnt/var/lib/dkms
echo state > /mnt/var/lib/archix-vm/kept
echo 'logged live' >> /mnt/var/log/pacman.log

deploy=/mnt/archix/deploy
mkdir -p "$deploy"
if [ "$fs" = btrfs ]; then btrfs -q subvolume create "$deploy/1"; else mkdir "$deploy/1"; fi
for tree in usr etc opt boot; do mv "/mnt/$tree" "$deploy/1/$tree"; mkdir "/mnt/$tree"; done
mv /mnt/var/lib/pacman/local "$deploy/1/pacman"
mkdir /mnt/var/lib/pacman/local "$deploy/1/dkms"
for tree in usr etc opt boot; do mount --bind "$deploy/1/$tree" "/mnt/$tree"; done
mount --bind "$deploy/1/pacman" /mnt/var/lib/pacman/local
arch-chroot /mnt mkinitcpio -P > /dev/null 2>&1
arch-chroot /mnt ukify build --linux /boot/vmlinuz-linux --initrd /boot/initramfs-linux.img --cmdline @/etc/kernel/cmdline --output /boot/archix.efi > /dev/null
arch-chroot /mnt bootctl --esp-path=/efi --no-variables install > /dev/null 2>&1
lsinitcpio "$deploy/1/boot/initramfs-linux.img" | grep -E 'archix'
for tree in usr etc opt boot var/lib/pacman/local; do umount "/mnt/$tree"; done

copy() {
  if [ "$fs" = btrfs ]; then
    btrfs -q subvolume snapshot "$deploy/1" "$1"
  else
    mkdir "$1"
    cp -al "$deploy/1/usr" "$deploy/1/opt" "$1/"
    cp -a "$deploy/1/etc" "$deploy/1/boot" "$deploy/1/pacman" "$deploy/1/dkms" "$1/"
  fi
}
copy "$deploy/2"
copy "$deploy/.staging-3"
for n in 1 2 .staging-3; do echo "${n#.staging-}" > "$deploy/$n/etc/archix-vm-deployment"; done
upper=$deploy/2/var-upper
mkdir -p "$upper/lib/archix-vm" "$upper/log"
echo seed > "$upper/lib/archix-vm/seed"
chown 0:12 "$upper/lib/archix-vm/seed"
chmod 640 "$upper/lib/archix-vm/seed"
mknod "$upper/lib/archix-vm/kept" c 0 0
echo 'logged in staging' > "$upper/log/pacman.log"
ln -s deploy/2 /mnt/archix/current

mkdir -p /mnt/efi/EFI/archix /mnt/efi/loader/entries
cp "$deploy/1/boot/archix.efi" /mnt/efi/EFI/archix/shared.efi
mkdir -p /mnt/var/lib/archix/boot/entries
for n in 1 2 3; do
  printf 'title archix %s\nefi /EFI/archix/shared.efi\n' "$n" > "/mnt/efi/loader/entries/archix-$n.conf"
  cp "/mnt/efi/loader/entries/archix-$n.conf" /mnt/var/lib/archix/boot/entries/
done
printf 'timeout 2\nconsole-mode keep\ndefault archix-2.conf\n' > /mnt/efi/loader/loader.conf
sync -f /mnt/efi
sync -f /mnt
ls -la /mnt/archix /mnt/archix/deploy /mnt/efi/loader/entries
