#!/bin/bash
# In the runner VM: switches the target disk (vdb) through dm-log-writes (log on vdc), then replays the log onto a pristine
# copy (vdd) and checks every flush point as a power cut there would leave it.
set -u
state=/var/lib/archix-powercut
mkdir -p "$state"
say() { echo "VMTEST: $*" > /dev/console; }
fail() { say "FAIL: $*"; systemctl poweroff; exit 1; }
[ -e "$state/started" ] && { say "FAIL: the runner rebooted"; systemctl poweroff; exit 1; }
touch "$state/started"
target=/dev/vdb log=/dev/vdc pristine=/dev/vdd
read -r esp_start esp_size root_start root_size < <(sfdisk -J "$target" | python -c "
import json, sys
p = json.load(sys.stdin)['partitiontable']['partitions']
print(p[0]['start'], p[0]['size'], p[1]['start'], p[1]['size'])")
fs=$(blkid -p -o value -s TYPE -O $((root_start * 512)) "$target")
opts=; [ "$fs" = btrfs ] && opts=subvol=@
printf '%s\n' "$esp_start $esp_size $root_start $root_size $fs $opts" > "$state/layout"
linear() { dmsetup create "$1" --table "0 $3 linear $4 $2"; }

dmsetup create logw --table "0 $(blockdev --getsz "$target") log-writes $target $log" || fail "creating the log-writes device"
linear tesp "$esp_start" "$esp_size" /dev/mapper/logw
linear troot "$root_start" "$root_size" /dev/mapper/logw
mkdir -p /target
mount ${opts:+-o $opts} /dev/mapper/troot /target && mount /dev/mapper/tesp /target/efi || fail "mounting the target"
dmsetup message logw 0 mark begin
DEPLOY_MARK="dmsetup message logw 0 mark" ROOT=/target ESP=/target/efi /usr/local/lib/archix-deploy/deploy-switch -Su > /dev/console 2>&1 \
  || fail "the switch on the target failed"
upgrades=$(grep -c . /target/var/lib/prefetched-upgrades)
umount -R /target
dmsetup message logw 0 mark end
dmsetup remove tesp troot logw
say "PASS: switched the target through dm-log-writes; $upgrades packages upgraded"

# What each deployment looks like when the switch completes, to compare every replayed state against.
linear froot "$root_start" "$root_size" "$target"
mount ${opts:+-o $opts} /dev/mapper/froot /target || fail "mounting the finished target"
mkdir -p "$state/baselines"
for gen in $(ls /target/archix/deploy | grep -E '^[0-9]+$'); do
  /usr/local/lib/archix-deploy/check-deployment "$gen" /target > "$state/baselines/$gen"
done
umount /target && dmsetup remove froot
say "baselines: $(for f in "$state"/baselines/*; do echo -n "deployment ${f##*/} $(wc -l < "$f") findings; "; done)"

say "replaying every flush point"
replay-log --log "$log" --replay "$pristine" --check flush --fsck /usr/local/lib/archix-powercut/check-point > "$state/replay.log" 2>&1
status=$?
points=$(cat "$state/points" 2> /dev/null || echo 0)
tail -5 "$state/replay.log" > /dev/console
[ "$status" = 0 ] || fail "replay stopped at point $points: $(tail -3 "$state/check.log")"
say "PASS: $points flush points replayed, each one fscked and booting a complete deployment"
say "OK"
systemctl poweroff
