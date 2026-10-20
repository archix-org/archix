#!/usr/bin/env bash
# Builds /work/disk.img: GPT with an ESP and an ext4 root (FS=btrfs: btrfs with an @ subvolume, as CachyOS installs it), Arch pacstrapped at the test snapshot and laid out as deployments by archix's first switch, which the boot suite runs on its first boot instead.
set -euo pipefail
suite=${SUITE:-boot}
archive=${ARCHIVE:-https://archive.archlinux.org}
dir=/vm date=2026-10-10 size=8G packages=(base linux mkinitcpio systemd-ukify sbctl) archix=/work/archix
# The rollback suite starts at an older snapshot and rolls glibc back, so archix itself must not need the build host's glibc.
if [ "$suite" = rollback ]; then
  dir=/vm/rollback date=2026-06-01 size=16G packages=(base linux mkinitcpio systemd-ukify devtools) archix=/work/archix-static
elif [ "$suite" = atomic ]; then
  dir=/vm/atomic date=2026-06-01 size=24G packages=(base base-devel linux mkinitcpio systemd-ukify devtools firefox python rust git) archix=/work/archix-static
elif [ "$suite" = nvidia ]; then
  dir=/vm/nvidia date=2026-06-01 size=24G packages=(base linux mkinitcpio systemd-ukify devtools) archix=/work/archix-static
elif [ "$suite" = upgrade ]; then
  dir=/vm/upgrade packages=(base linux mkinitcpio systemd-ukify)
elif [ "$suite" = cachyos ]; then
  dir=/vm/cachyos date=2026-10-16 size=12G packages=(base linux-cachyos mkinitcpio systemd-ukify cachyos-keyring htop jq) archix=/work/archix-static
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
mkdir -p /mnt/efi
mount "$esp" /mnt/efi

grep -v '^NoExtract' /etc/pacman.conf > /tmp/pacman.conf
if [ "$suite" = cachyos ]; then
  # As on a CachyOS install: its live repos ahead of Arch's, x86-64-v3 builds allowed, and its signing key trusted.
  python - /tmp/pacman.conf <<'PY'
import re, sys
path = sys.argv[1]
text = open(path).read()
repos = "".join(f"[{r}]\nServer = https://mirror.cachyos.org/repo/x86_64_v3/$repo\n\n" for r in ("cachyos-v3", "cachyos-core-v3", "cachyos-extra-v3"))
repos += "[cachyos]\nServer = https://mirror.cachyos.org/repo/x86_64/$repo\n\n"
text = text.replace("[core]", repos + "[core]", 1)
text = re.sub(r"^#?Architecture = .*$", "Architecture = x86_64 x86_64_v3", text, count=1, flags=re.M)
open(path, "w").write(text)
PY
  keyring=$(mktemp -d)
  curl -sfo "$keyring/keyring.pkg.tar.zst" https://mirror.cachyos.org/repo/x86_64/cachyos/cachyos-keyring-20240331-1-any.pkg.tar.zst
  bsdtar -xf "$keyring/keyring.pkg.tar.zst" -C "$keyring" usr/share/pacman/keyrings
  pacman-key --init >/dev/null 2>&1
  pacman-key --populate-from "$keyring/usr/share/pacman/keyrings" --populate cachyos >/dev/null
fi
# pacstrap at the test's snapshot, so generation 1 isn't newer than its own lock.
echo "Server = $archive/repos/${date//-//}/\$repo/os/\$arch" > /tmp/mirrorlist
sed 's|/etc/pacman.d/mirrorlist|/tmp/mirrorlist|' /tmp/pacman.conf > /tmp/pacstrap.conf
for attempt in 1 2 3; do pacstrap -C /tmp/pacstrap.conf -K /mnt "${packages[@]}" >/dev/null && break; [ "$attempt" = 3 ] && exit 1; sleep 20; done
cp /tmp/pacman.conf /mnt/etc/pacman.conf
[ "$suite" = cachyos ] && arch-chroot /mnt pacman-key --populate archlinux cachyos >/dev/null
cp /tmp/mirrorlist /mnt/etc/pacman.d/mirrorlist
# The filesystem package's resolv.conf is empty; the suites enable systemd-resolved, so point at its stub as an Arch install would.
ln -sf /run/systemd/resolve/stub-resolv.conf /mnt/etc/resolv.conf
if [ "$suite" = upgrade ]; then
  # archix is a package from a local repo, so switches upgrade it and rollbacks downgrade it: 1 is the previous release, 2 and 3 this tree.
  useradd -r builder 2>/dev/null || true
  mkdir -p /mnt/srv/archix-repo /mnt/srv/archix-next
  for version in 1 2 3; do
    build=$(mktemp -d)
    binary=/work/archix-static; [ "$version" = 1 ] && binary=/work/old/archix-static
    sed "s/^pkgver=.*/pkgver=$version/" "$dir/PKGBUILD" > "$build/PKGBUILD"
    cp "$binary" "$build/archix"; cp "$binary" "$build/archix-static"
    chown -R builder "$build"
    runuser -u builder -- bash -c "cd $build && makepkg --nodeps >/dev/null"
    cp "$build/archix-$version-1-x86_64.pkg.tar.zst" /mnt/srv/archix-next/
  done
  cp /mnt/srv/archix-next/archix-1-1-x86_64.pkg.tar.zst /mnt/srv/archix-repo/
  repo-add -q /mnt/srv/archix-repo/archix-self.db.tar.gz /mnt/srv/archix-repo/archix-1-1-x86_64.pkg.tar.zst
  arch-chroot /mnt pacman -U --noconfirm /srv/archix-repo/archix-1-1-x86_64.pkg.tar.zst >/dev/null
else
  install -Dm755 "$archix" /mnt/usr/bin/archix
  install -Dm755 /work/archix-static /mnt/usr/lib/archix/archix-static
fi
bootctl --esp-path=/mnt/efi --no-variables install >/dev/null
# The ESP in fstab, as on any real install: early boot units must not wait on it in a cycle.
echo "UUID=$(blkid -s UUID -o value "$esp") /efi vfat defaults 0 2" >> /mnt/etc/fstab
printf 'timeout 2\nconsole-mode keep\n' > /mnt/efi/loader/loader.conf

mkdir -p /mnt/etc/archix
rootflags= progs=; [ "$fs" = btrfs ] && rootflags='"rootflags=subvol=@", ' progs='"btrfs-progs", '
sed -e "s/@ROOT_UUID@/${root_uuid,,}/" -e "s|\"rw\", |\"rw\", $rootflags|" -e "s|{ \"base\", |{ \"base\", $progs|" -e "s|@ARCHIVE_URL@|$archive|" "$dir/config.lua" > /mnt/etc/archix/init.lua
install -Dm755 "$dir/vmtest.sh" /mnt/etc/archix/vmtest.sh
install -Dm755 /vm/deploy/proto/check-deployment /mnt/usr/local/bin/archix-check-deployment
[ -n "${TO_DATE:-}" ] && echo "$TO_DATE" > /mnt/etc/archix-vmtest-to
[ -d "$dir/pkgbuilds" ] && cp -r "$dir/pkgbuilds" /mnt/etc/archix/
arch-chroot /mnt archix --host archix-vm update --to "$date" --noconfirm
if [ "$suite" = boot ]; then
  # The boot suite lays the system out on its first boot, with services running, so the image boots like a plain Arch install: a UKI on the ESP, which systemd-boot lists by itself.
  install -Dm755 "$dir/vmtest.sh" /mnt/usr/local/bin/archix-vmtest
  printf '[Match]\nName=en*\n\n[Network]\nDHCP=yes\n' > /mnt/etc/systemd/network/20-wired.network
  printf '[Unit]\nDescription=archix VM test\nAfter=multi-user.target archix-boot.service network-online.target\nWants=network-online.target\n\n[Service]\nType=oneshot\nExecStart=/usr/local/bin/archix-vmtest\nStandardOutput=journal+console\n\n[Install]\nWantedBy=multi-user.target\n' > /mnt/etc/systemd/system/archix-vmtest.service
  # A service that reads /usr all the time from a read-only view of its own, as ProtectSystem=strict gives it.
  printf '[Service]\nProtectSystem=strict\nExecStart=/bin/sh -c "while cat /usr/lib/os-release > /dev/null && ls /usr/bin > /dev/null; do sleep 0.2; done; exit 1"\n\n[Install]\nWantedBy=multi-user.target\n' > /mnt/etc/systemd/system/archix-protected.service
  arch-chroot /mnt systemctl enable archix-vmtest.service archix-protected.service systemd-networkd.service systemd-resolved.service
  sed -n 's/.*kernel_params = { \(.*\) },$/\1/p' /mnt/etc/archix/init.lua | tr -d '",' > /mnt/etc/kernel/cmdline
  kernel=$(basename "$(dirname "$(ls /mnt/usr/lib/modules/*/vmlinuz | head -1)")")
  mkdir -p /mnt/efi/EFI/Linux
  arch-chroot /mnt mkinitcpio -k "$kernel" -S autodetect -U /efi/EFI/Linux/arch-linux.efi
  ls /mnt/efi/EFI/Linux
  sed -i "s|$archive|${VM_ARCHIVE:-$archive}|" /mnt/etc/archix/init.lua /mnt/etc/pacman.d/mirrorlist
  [ ! -f /mnt/var/lib/archix/archive-url ] || sed -i "s|$archive|${VM_ARCHIVE:-$archive}|" /mnt/var/lib/archix/archive-url
  exit 0
fi
arch-chroot /mnt archix --host archix-vm switch --noconfirm
ls /mnt/archix/deploy /mnt/efi/loader/entries /mnt/efi/EFI/archix
# Inside the VM the host-side mirror has another address; each deployment has its own /etc.
sed -i "s|$archive|${VM_ARCHIVE:-$archive}|" /mnt/archix/deploy/*/etc/archix/init.lua /mnt/archix/deploy/*/etc/pacman.d/mirrorlist /mnt/var/lib/archix/archive-url
