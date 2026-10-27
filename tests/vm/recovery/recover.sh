#!/bin/bash
# Run at boot on the recovery suite's rescue disk, plain Arch as on a live USB: the documented recovery of an archix disk whose ESP is gone and whose current deployment is damaged, then power off.
set -u
say() { echo "VMTEST: rescue: $*" > /dev/console; }
fail() { say "FAIL: $*"; umount -R /mnt 2> /dev/null; systemctl poweroff; exit 1; }
log=/var/tmp/recover.log
# Runs a step, failing with its output's tail.
step() { "$@" > "$log" 2>&1 || fail "\`$*\` failed: $(tail -12 "$log" | tr '\n' '|')"; }
# A command in the chroot as typed at its shell, where systemd's tools log to stderr: not as PID 1 of arch-chroot's namespace, nor with this unit's JOURNAL_STREAM.
in_root() { arch-chroot /mnt env -u JOURNAL_STREAM bash -c "$*"; }
root=$(blkid -L archixroot) || fail "no filesystem labelled archixroot"
esp=$(blkid -L ESP) || fail "no filesystem labelled ESP"
say "booted from $(findmnt -no SOURCE /), the archix root is $root, its ESP $esp"
# 1. Mount the root (on btrfs, its root subvolume) and the ESP where boot.esp says.
opts=()
[ "$(blkid -s TYPE -o value "$root")" = btrfs ] && opts=(-o subvol=@)
step mount "${opts[@]}" "$root" /mnt
at=$(cat /mnt/var/lib/archix/boot/esp) || fail "the root has no /var/lib/archix/boot/esp"
step mount "$esp" "/mnt$at"
say "deployments $(ls /mnt/archix/deploy | tr '\n' ' '), current $(readlink /mnt/archix/current), ESP files $(find "/mnt$at" -mindepth 1 | wc -l)"
# The VM test's own record of which deployment it damaged and which it kept; a user goes by archix verify below.
broken=$(cat /mnt/var/lib/archix-vmtest/broken) kept=$(cat /mnt/var/lib/archix-vmtest/kept)
# 2. Bind a deployment over the root with the static archix it carries, then look for the damage from inside it.
step "/mnt/archix/deploy/$kept/usr/lib/archix/archix-static" mount-deployment /mnt "$kept"
say "mount-deployment: $(tr '\n' '|' < "$log")"
step in_root archix generations
say "generations: $(tr '\n' '|' < "$log")"
in_root archix verify > "$log" 2>&1 && fail "archix verify should find deployment $broken damaged: $(tr '\n' '|' < "$log")"
grep -q "^deployment $kept: matches its packages" "$log" && grep -q "^deployment $broken: .* differ" "$log" || fail "archix verify should flag deployment $broken and pass $kept: $(tr '\n' '|' < "$log")"
say "verify: $(grep -v '^::' "$log" | tr '\n' '|')"
# 3. Put systemd-boot back, make the intact deployment current and write its boot entry and fallback.
# In a chroot bootctl skips its partition checks and takes the read-only /boot bind for an XBOOTLDR partition, so it's told the ESP is $BOOT too.
step in_root bootctl --esp-path="$at" --boot-path="$at" install
say "bootctl: $(tail -4 "$log" | tr '\n' '|')"
step in_root archix rollback "$kept" --noconfirm
say "rollback: $(tail -6 "$log" | tr '\n' '|')"
[ "$(readlink /mnt/archix/current)" = "deploy/$kept" ] || fail "current should be deploy/$kept, not $(readlink /mnt/archix/current)"
ls "/mnt$at/loader/entries" | grep -q "^archix-$kept[.+]" || fail "the ESP should have deployment $kept's entry: $(ls "/mnt$at/loader/entries" | tr '\n' ' ')"
say "the ESP now holds $(find "/mnt$at" -type f | sed "s|^/mnt$at/||" | sort | tr '\n' ' ')"
step umount -R /mnt
say "recovered"
systemctl poweroff
