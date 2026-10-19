#!/bin/bash
# Measures an in-place pacman upgrade and rollback against an A/B /usr one on the same machine, printing METRIC lines.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
[ -f "$state/started" ] && { echo "VMTEST: FAIL: the measurement rebooted" > /dev/console; systemctl poweroff; exit 1; }
touch "$state/started"
say() { echo "VMTEST: $*" > /dev/console; }
fail() { say "FAIL: $*"; systemctl poweroff; exit 1; }
fs=$(findmnt -no FSTYPE /)
written() { sync; awk '{ print $7 * 512 }' /sys/block/vda/stat; }
used() { df -B1 --output=used / | tail -1 | tr -d ' '; }
inodes() { df --output=iused / | tail -1 | tr -d ' '; }

# Runs a step with cold caches and reports its wall, user and system seconds, bytes written, and the space and inodes it took.
measure() {
  local name=$1
  shift
  sync
  echo 3 > /proc/sys/vm/drop_caches
  local w0 u0 i0 times
  w0=$(written) u0=$(used) i0=$(inodes)
  local TIMEFORMAT="%R %U %S"
  times=$({ time "$@" > "/var/log/measure-$name.log" 2>&1; } 2>&1) || fail "$name failed: $(tail -5 "/var/log/measure-$name.log")"
  local w1 u1 i1
  w1=$(written) u1=$(used) i1=$(inodes)
  read -r real user sys <<< "$times"
  say "METRIC $fs $name wall=$real user=$user sys=$sys written=$((w1 - w0)) used=$((u1 - u0)) inodes=$((i1 - i0))"
}

same_as_before() {
  pacman -Q > "$state/now"
  diff -q "$state/before" "$state/now" > /dev/null || fail "$1: the packages differ from before: $(diff "$state/before" "$state/now" | head -5)"
  local broken
  broken=$(pacman -Qk 2>&1 | grep -c "missing file")
  [ "$broken" = 0 ] || fail "$1: $broken files are missing"
}

mirror=$(sed -n 's|^Server = \(.*\)/repos/.*|\1|p' /etc/pacman.d/mirrorlist | head -1)
echo "Server = $mirror/repos/2026/10/10/\$repo/os/\$arch" > /etc/pacman.d/mirrorlist
pacman -Sy --noconfirm --needed archlinux-keyring > /dev/console 2>&1 || fail "updating the keyring"
pacman -Suw --noconfirm > /dev/console 2>&1 || fail "downloading the upgrade"
pacman -Q > "$state/before"
pacman -Qu > "$state/upgrades"
say "METRIC $fs start upgrades=$(wc -l < "$state/upgrades") usr_bytes=$(du -sxb /usr | cut -f1) usr_entries=$(find /usr -xdev | wc -l) used=$(used) inodes=$(inodes)"

# What a rollback needs from the package cache today: the old versions' files.
old_files=()
while read -r name old _ new; do
  file=$(ls /var/cache/pacman/pkg/"$name-$old"-*.pkg.tar.zst 2> /dev/null | head -1)
  [ -n "$file" ] || fail "no cached file for $name $old"
  old_files+=("$file")
done < "$state/upgrades"
say "METRIC $fs cache old_versions=${#old_files[@]} bytes=$(du -cb "${old_files[@]}" | tail -1 | cut -f1)"

# Today's way: pacman replaces files in place; a rollback reinstalls the old versions and removes what the upgrade added.
rollback_in_place() {
  pacman -U --noconfirm "${old_files[@]}" || return 1
  local added
  added=$(comm -13 <(cut -d' ' -f1 "$state/before" | sort) <(pacman -Qq | sort))
  [ -z "$added" ] || pacman -Rdd --noconfirm $added
}
measure in-place-upgrade pacman -Su --noconfirm
measure in-place-rollback rollback_in_place
same_as_before in-place-rollback

# A/B: pacman upgrades a copy of /usr with a copy of its database, in a mount namespace where the copy is /usr, then both swap in at once.
clone_hardlinks() { cp -al /usr /usr.next; }
clone_reflinks() { cp -a --reflink=always /usr /usr.next; }
copy_db() { rm -rf /var/lib/archix-next-db && mkdir -p /var/lib/archix-next-db && cp -a /var/lib/pacman/local /var/lib/pacman/sync /var/lib/archix-next-db/; }
upgrade_copy() {
  unshare -m --propagation private sh -c 'mount --bind /usr.next /usr && exec pacman -Su --noconfirm --dbpath /var/lib/archix-next-db'
}
swap() { mv --exchange /usr /usr.next && mv --exchange /var/lib/pacman/local /var/lib/archix-next-db/local; }
a_b() {
  local clone=$1
  rm -rf /usr.next
  measure "a-b-$clone-clone" "clone_$clone"
  measure "a-b-$clone-db-copy" copy_db
  measure "a-b-$clone-upgrade" upgrade_copy
  measure "a-b-$clone-swap" swap
  say "METRIC $fs a-b-$clone kept used=$(used) inodes=$(inodes) usr_bytes=$(du -sxb /usr | cut -f1) both_bytes=$(du -sxbc /usr /usr.next | tail -1 | cut -f1)"
  measure "a-b-$clone-rollback" swap
  same_as_before "a-b-$clone-rollback"
  measure "a-b-$clone-drop" rm -rf /usr.next /var/lib/archix-next-db
}
if [ "$fs" = btrfs ]; then
  a_b reflinks
fi
a_b hardlinks

# A kill -9 partway: count what the live system is left with.
broken_now() { pacman -Qkk 2>&1 | grep -cE "missing file|checksum mismatch|Size mismatch"; }
clone_hardlinks
copy_db
upgrade_copy > /var/log/measure-a-b-killed.log 2>&1 &
sleep 30
pkill -9 -x pacman
wait
say "METRIC $fs a-b-killed live_broken_files=$(broken_now) live_packages_changed=$(pacman -Q | diff "$state/before" - | grep -c '^>')"
rm -rf /usr.next /var/lib/archix-next-db
pacman -Su --noconfirm > /var/log/measure-in-place-killed.log 2>&1 &
sleep 30
pkill -9 -x pacman
wait
rm -f /var/lib/pacman/db.lck
say "METRIC $fs in-place-killed live_broken_files=$(broken_now) live_packages_changed=$(pacman -Q | diff "$state/before" - | grep -c '^>')"
say "OK"
systemctl poweroff
