#!/usr/bin/env bash
# Run by build-image.sh on the power-cut target: a generation 3 with tree added (so generation 2 is dropped, to be rebuilt), then archix.lock moved to TO_DATE with its packages in the cache, so the switch in the VM is offline.
set -euo pipefail
opts=; [ "$FS" = btrfs ] && opts=subvol=@
to=${TO_DATE:-2026-10-10}
# The image build's chroot sees the host's EFI variables and command line, which aren't the target's.
export ARCHIX_TEST_ENTRY= ARCHIX_TEST_CMDLINE= ARCHIX_TEST_LOADER_FEATURES=0
# pacstrap's gpg-agent keeps its keyring open, and the layout moved that into deployment 1, out of gpgconf's reach.
stop_leftovers() {
  local p
  for p in /proc/[0-9]*; do
    if [[ $(readlink "$p/root") == /mnt* || $(tr '\0' ' ' < "$p/cmdline" 2> /dev/null) == gpg-agent*/mnt* ]]; then kill -9 "${p#/proc/}" 2> /dev/null || true; fi
  done
}
# Boots the current deployment as the initramfs would, its trees bound over the root and recorded in /run, where gc finds it pinned.
remount() {
  stop_leftovers
  umount -R /mnt || { ps -eo pid,args; exit 1; }
  mount ${opts:+-o $opts} "$ROOT_DEV" /mnt
  mount -t tmpfs tmpfs /mnt/run
  /work/archix-static deploy-mount /mnt --efivar /nonexistent --run-dir /mnt/run/archix > /dev/null
  mount "$ESP_DEV" /mnt/efi
  mount -t proc proc /mnt/proc && mount -t sysfs sys /mnt/sys && mount -t devtmpfs dev /mnt/dev && mount -t tmpfs tmpfs /mnt/tmp
  mkdir -p /mnt/run/systemd/resolve && cp /etc/resolv.conf /mnt/run/systemd/resolve/stub-resolv.conf
}
in_target() { chroot /mnt archix --host archix-vm "$@" --noconfirm > /dev/null; }
remount
sed -i 's|"systemd-ukify", |"systemd-ukify", "tree", |' /mnt/etc/archix/init.lua
in_target switch
remount
in_target update --to "$to"
python - /mnt/etc/archix/archix.lock /mnt/var/cache "$ARCHIVE" << 'PY'
import hashlib, os, sys, tomllib, urllib.request
lock_path, cache, archive = sys.argv[1:4]
lock = tomllib.load(open(lock_path, "rb"))["hosts"]["archix-vm"]
date = lock["date"].replace("-", "/") if isinstance(lock["date"], str) else lock["date"].strftime("%Y/%m/%d")
dest = os.path.join(cache, "archix/pkg")
os.makedirs(dest, exist_ok=True)
fetched = 0
for name, pkg in lock["packages"].items():
    file = pkg["file"]
    if any(os.path.isfile(os.path.join(cache, d, file)) for d in ("archix/pkg", "pacman/pkg")):
        continue
    for url in (f"{archive}/repos/{date}/{pkg['repo']}/os/x86_64/{file}", f"{archive}/packages/{name[0]}/{name}/{file}"):
        try:
            data = urllib.request.urlopen(url).read()
        except OSError:
            continue
        if hashlib.sha256(data).hexdigest() != pkg["sha256"]:
            continue
        open(os.path.join(dest, file), "wb").write(data)
        open(os.path.join(dest, file + ".sig"), "wb").write(urllib.request.urlopen(url + ".sig").read())
        fetched += 1
        break
    else:
        sys.exit(f"couldn't prefetch {file}")
print(f"prefetched {fetched} packages for {lock['date']}")
PY
stop_leftovers
ls /mnt/archix/deploy /mnt/efi/loader/entries
cat /mnt/efi/loader/loader.conf
