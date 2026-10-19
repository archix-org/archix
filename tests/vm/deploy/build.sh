#!/usr/bin/env bash
# Builds /work/disk.img for the deployment prototype: an ESP and an ext4 or btrfs root (mounted with BTRFS_OPTS), Arch pacstrapped at $DATE and laid out as deployment 1.
set -euo pipefail
archive=${ARCHIVE:-https://archive.archlinux.org}
date=${DATE:-2026-10-03}
fs=${FS:-ext4}
proto=/vm/deploy/proto
packages=(base linux mkinitcpio systemd-ukify python arch-install-scripts libcap ${EXTRA_PACKAGES:-})
[ "$fs" = btrfs ] && packages+=(btrfs-progs)
img=/work/disk.img
rm -f "$img" /work/vars.fd
truncate -s "${SIZE:-12G}" "$img"
sfdisk --quiet "$img" << SFDISK
label: gpt
size=600M, type=uefi
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
  mount -o "subvol=@${BTRFS_OPTS:+,$BTRFS_OPTS}" "$root" /mnt
  rootflags=" rootflags=subvol=@${BTRFS_OPTS:+,$BTRFS_OPTS}"
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
cp /tmp/pacman.conf /mnt/etc/pacman.conf
echo "Server = ${VM_ARCHIVE:-$archive}/repos/${date//-//}/\$repo/os/\$arch" > /mnt/etc/pacman.d/mirrorlist
ln -sf /run/systemd/resolve/stub-resolv.conf /mnt/etc/resolv.conf
printf '[Match]\nName=en*\n\n[Network]\nDHCP=yes\n' > /mnt/etc/systemd/network/20-wired.network
echo "root=PARTUUID=${root_uuid,,} rw${rootflags} console=ttyS0" > /mnt/etc/kernel/cmdline
echo "UUID=$(blkid -s UUID -o value "$esp") /efi vfat defaults 0 2" >> /mnt/etc/fstab

install -Dm755 "$proto/mount-deployment" /mnt/usr/local/lib/archix-deploy/mount-deployment
install -Dm644 "$proto/archix-deploy-mount.service" /mnt/usr/local/lib/archix-deploy/archix-deploy-mount.service
install -Dm644 "$proto/archix-deploy.install" /mnt/etc/initcpio/install/archix-deploy
install -Dm755 "$proto/activate-deployment" /mnt/usr/local/lib/archix-deploy/activate-deployment
install -Dm644 "$proto/archix-deploy-activate.service" /mnt/etc/systemd/system/archix-deploy-activate.service
for tool in deploy-switch flatten check-deployment; do install -Dm755 "$proto/$tool" "/mnt/usr/local/lib/archix-deploy/$tool"; done
install -Dm755 "${TEST_SCRIPT:-/vm/deploy/vmtest.sh}" /mnt/usr/local/bin/archix-proto-test
if [ -n "${POWERCUT:-}" ]; then
  install -Dm755 /work/replay-log /mnt/usr/local/bin/replay-log
  for tool in invariant check-point; do install -Dm755 "/vm/deploy/powercut/$tool" "/mnt/usr/local/lib/archix-powercut/$tool"; done
  echo "${REPLAY_POINTS:-}" > /mnt/etc/archix-powercut-points
fi
printf '[Unit]\nDescription=archix deployment prototype test\nAfter=multi-user.target network-online.target\nWants=network-online.target\n\n[Service]\nType=oneshot\nExecStart=/usr/local/bin/archix-proto-test\nStandardOutput=journal+console\nTimeoutSec=0\n\n[Install]\nWantedBy=multi-user.target\n' \
  > /mnt/etc/systemd/system/archix-proto-test.service
echo 'HOOKS+=(archix-deploy)' > /mnt/etc/mkinitcpio.conf.d/archix-deploy.conf
arch-chroot /mnt systemctl enable systemd-networkd systemd-resolved archix-proto-test archix-deploy-activate > /dev/null 2>&1

# The deployment layout: the versioned trees move into deployment 1, and their old places become mountpoints.
deploy=/mnt/archix/deploy/1
mkdir -p /mnt/archix/deploy
if [ "$fs" = btrfs ]; then btrfs -q subvolume create "$deploy"; else mkdir "$deploy"; fi
for tree in usr etc opt boot; do mv "/mnt/$tree" "$deploy/$tree"; mkdir "/mnt/$tree"; done
mv /mnt/var/lib/pacman/local "$deploy/pacman"
mkdir /mnt/var/lib/pacman/local
ln -s deploy/1 /mnt/archix/current

# A view of deployment 1 to build its initramfs and UKI in.
for tree in usr etc opt boot; do mount --bind "$deploy/$tree" "/mnt/$tree"; done
mount --bind "$deploy/pacman" /mnt/var/lib/pacman/local
arch-chroot /mnt mkinitcpio -P > /dev/null 2>&1
arch-chroot /mnt ukify build --linux /boot/vmlinuz-linux --initrd /boot/initramfs-linux.img --cmdline @/etc/kernel/cmdline --output /boot/archix.efi > /dev/null
arch-chroot /mnt bootctl --esp-path=/efi --no-variables install > /dev/null 2>&1
mkdir -p /mnt/efi/EFI/archix /mnt/efi/loader/entries
cp "$deploy/boot/archix.efi" /mnt/efi/EFI/archix/1.efi
printf 'title archix deployment 1\nefi /EFI/archix/1.efi\n' > /mnt/efi/loader/entries/archix-1.conf
printf 'timeout 2\nconsole-mode keep\ndefault archix-1.conf\n' > /mnt/efi/loader/loader.conf
# For an offline switch: the next snapshot's databases and package files, already in the image.
if [ -n "${PREFETCH_TO:-}" ]; then
  sed "s|^Include = /etc/pacman.d/mirrorlist|Server = $archive/repos/${PREFETCH_TO//-//}/\$repo/os/\$arch|" /mnt/etc/pacman.conf > /mnt/root/prefetch.conf
  arch-chroot /mnt pacman --config /root/prefetch.conf -Sy --needed --noconfirm archlinux-keyring > /dev/null
  arch-chroot /mnt pacman --config /root/prefetch.conf -Suw --noconfirm > /dev/null
  arch-chroot /mnt pacman -Qu | tee /mnt/var/lib/prefetched-upgrades
  rm /mnt/root/prefetch.conf
fi
lsinitcpio "$deploy/boot/initramfs-linux.img" | grep -E 'archix-deploy|mount-deployment'
ls -la /mnt/archix /mnt/archix/deploy/1 /mnt/efi/loader/entries
