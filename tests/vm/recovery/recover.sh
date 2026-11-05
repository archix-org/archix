#!/bin/bash
# Run at boot on the recovery suite's rescue disk, plain Arch as on a live USB: the documented recovery of an archix disk whose ESP is gone and whose root is damaged, with the static archix the disk itself carries, then power off.
set -u
say() { echo "VMTEST: rescue: $*" > /dev/console; }
fail() { say "FAIL: $*"; umount -R /run/top 2> /dev/null; systemctl poweroff; exit 1; }
log=/var/tmp/recover.log
step() { "$@" > "$log" 2>&1 || fail "\`$*\` failed: $(tail -12 "$log" | tr '\n' '|')"; }
root=$(blkid -L archixroot) || fail "no filesystem labelled archixroot"
say "booted from $(findmnt -no SOURCE /), the archix root is $root"
# 1. The archix binary from the disk itself: its root's static build, read from the top level, read-only.
mkdir -p /run/top
step mount -o ro,subvolid=5 "$root" /run/top
cp /run/top/@/usr/lib/archix/archix-static /usr/local/bin/archix || fail "the root has no static archix"
kept=$(cat /run/top/@varlib/archix-vmtest/kept) || fail "the test's record of the snapshot to keep is missing"
umount /run/top
# 2. Into the root with its layout, binds and ESP, as it boots (archix chroot mounts them all): put systemd-boot back, list the snapshots and keep the one from before the damage.
in_root() { archix chroot "$root" env -u JOURNAL_STREAM bash -c "$*"; }
step in_root "archix snapshot list"
say "snapshots: $(tr '\n' '|' < "$log")"
step in_root "findmnt -n -o FSTYPE /efi | grep -qx vfat && bootctl --esp-path=/efi install && archix rollback $kept --noconfirm; status=\$?; ls -R /efi/loader; exit \$status"
say "rollback: $(tr '\n' '|' < "$log")"
grep -q '^archix-current' "$log" || fail "the keep wrote no current entry on the ESP"
say "recovered"
systemctl poweroff
