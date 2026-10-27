#!/bin/bash
# In the runner VM, PHASE=record: boots the target disk (vdb) as its initramfs and early units would, switches it to the prefetched update, rolls back to the kept deployment, rebuilds the dropped generation and boots each, all with the real archix through dm-log-writes (log on vdc), and writes the baselines to vde; PHASE=replay: replays that log onto the pristine copy (vdd), checking every flush point or POINTS evenly spaced prefixes through check-point; PHASE=compare: logs one switch and its first boot twice with each of the power-cut and the plain release build, and compares their I/O.
set -u
state=/var/lib/archix-powercut
lib=/usr/local/lib/archix-powercut
mkdir -p "$state"
# Each boot's settings come on their own disk, so one runner image serves every phase.
head -c 65536 /dev/vdf | tr -d '\000' > /etc/archix-powercut.conf
. /etc/archix-powercut.conf
say() { echo "VMTEST: $*" > /dev/console; }
# What a failed replay is read by: its check log and the consoles of the boots that failed, on vdg, which run.sh unpacks into the work dir's out/.
keep_logs() {
  [ "$PHASE" = replay ] && [ -d "$state" ] || return 0
  rm -rf "$state/out" && mkdir -p "$state/out"
  cp "$state"/check.log "$state"/replay.log "$state"/boot-*.log "$state"/case-*.log "$state/out/" 2> /dev/null
  for f in real-boots point-times reorder-points slow-once; do [ ! -e "$state/$f" ] || cp "$state/$f" "$state/out/$f.txt"; done
  tar -C "$state/out" -cf /dev/vdg . 2> /dev/null
}
fail() { say "FAIL: $*"; keep_logs; systemctl poweroff; exit 1; }
[ -e "$state/started" ] && fail "the runner rebooted"
touch "$state/started"
target=/dev/vdb log=/dev/vdc pristine=/dev/vdd export=/dev/vde
read -r esp_start esp_size root_start root_size < <(sfdisk -J "$pristine" | python -c "
import json, sys
p = json.load(sys.stdin)['partitiontable']['partitions']
print(p[0]['start'], p[0]['size'], p[1]['start'], p[1]['size'])")
fs=$(blkid -p -o value -s TYPE -O $((root_start * 512)) "$pristine")
opts=; [ "$fs" = btrfs ] && opts=subvol=@
echo "$esp_start $esp_size $root_start $root_size $fs $opts" > "$state/layout"
linear() { dmsetup create "$1" --table "0 $3 linear $4 $2"; }
started=$SECONDS
markdir=$state/mark
# pivot_root, which archix's staging uses, refuses a chroot whose parent mount is shared, as systemd makes the runner's.
mount --make-rprivate /
mkdir -p "$markdir" /target
install -m755 "$lib/mark" "$markdir/mark"
mark() { "$markdir/mark" "$1" || fail "logging the mark $1"; }
current() { readlink /target/archix/current | sed 's|^deploy/||'; }
deployments() { ls /target/archix/deploy | grep -E '^[0-9]+$' | sort -n | tr '\n' ' '; }
tail_of() { tail -6 "$1" | tr '\n' '|'; }
entry=
# systemd-boot 260's LoaderFeatures: boot counting, uki entries and `preferred`.
features=$(((1 << 4) | (1 << 16) | (1 << 19)))
# The archix the target runs: its own, or with PLAIN set the plain release build bound over it, which reads systemd-boot's variables from the files EFIVARS holds instead of the test hooks.
PLAIN= EFIVARS=
in_target() {
  if [ -n "$EFIVARS" ]; then
    chroot /target /usr/bin/env -i PATH=/usr/local/sbin:/usr/local/bin:/usr/bin HOME=/root TERM=dumb LANG=C.UTF-8 ARCHIX_DETACHED=1 ARCHIX_TEST_MARK=/run/powercut/mark "$@"
  else
    chroot /target /usr/bin/env -i PATH=/usr/local/sbin:/usr/local/bin:/usr/bin HOME=/root TERM=dumb LANG=C.UTF-8 ARCHIX_DETACHED=1 \
      ARCHIX_TEST_MARK=/run/powercut/mark ARCHIX_TEST_ENTRY="$entry" ARCHIX_TEST_CMDLINE= \
      ${COUNTING:+ARCHIX_TEST_LOADER_FEATURES=$features} ${BREAK:+ARCHIX_TEST_BREAK=$BREAK} "$@"
  fi
}
efivar() { { printf '\x06\x00\x00\x00'; printf '%s\0' "$2" | iconv -t UTF-16LE; } > "$1"; }
# A boot of the target: systemd-boot picks and counts an entry, archix's initramfs step mounts its deployment, then activation, archix-boot, systemd-bless-boot and archix-boot-complete.
boot_target() {
  [ "$fs" = btrfs ] && btrfs device scan --forget > /dev/null 2>&1
  mount ${opts:+-o $opts} /dev/mapper/troot /target || fail "mounting the target's root"
  mount -t tmpfs tmpfs /target/run && mount -t tmpfs tmpfs /target/tmp
  mkdir /target/run/powercut && mount --bind "$markdir" /target/run/powercut
  mount /dev/mapper/tesp /target/efi || fail "mounting the target's ESP"
  local name counted
  name=$("$lib/invariant" pick /target/efi)
  [ -n "$name" ] || fail "systemd-boot finds no entry on the target: $(ls /target/efi/loader/entries | tr '\n' ' ')"
  entry=$(sed -E 's/\+[0-9]+(-[0-9]+)?(\.(conf|efi))$/\2/' <<< "$name")
  local gen=${entry#archix-}
  gen=${gen#fallback-}
  gen=${gen%%[-.]*}
  mark "boot-$gen"
  if [[ $name =~ ^(.*)\+([0-9]+)(-([0-9]+))?\.conf$ ]] && [ "${BASH_REMATCH[2]}" -gt 0 ]; then
    counted="${BASH_REMATCH[1]}+$((BASH_REMATCH[2] - 1))-$((${BASH_REMATCH[4]:-0} + 1)).conf"
    mv "/target/efi/loader/entries/$name" "/target/efi/loader/entries/$counted"
    name=$counted
  fi
  efivar "$state/selected" "$entry"
  ${PLAIN:-archix} deploy-mount /target --efivar "$state/selected" --run-dir /target/run/archix > "$state/boot.log" 2>&1 || fail "archix's initramfs step: $(tail_of "$state/boot.log")"
  booted=$(cat /target/run/archix/deployment)
  [ "$booted" = "$gen" ] || fail "systemd-boot starts $name, but archix mounted deployment $booted"
  mount -t proc proc /target/proc && mount -t sysfs sys /target/sys && mount -t devtmpfs dev /target/dev && mount -t devpts devpts /target/dev/pts
  if [ -n "$PLAIN" ]; then
    mount --bind "$PLAIN" /target/usr/bin/archix
    efivar "$EFIVARS/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f" "$entry"
    mount --bind "$EFIVARS" /target/sys/firmware/efi/efivars
  elif [ -n "$EFIVARS" ]; then
    efivar "$EFIVARS/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f" "$entry"
    mount --bind "$EFIVARS" /target/sys/firmware/efi/efivars
  fi
  mark "activate-$gen"
  in_target archix deploy-activate >> "$state/boot.log" 2>&1 || fail "activation of deployment $booted: $(tail_of "$state/boot.log")"
  mark "activated-$gen"
  mkdir -p "$state/var"
  "$lib/var-manifest" /target "$gen" > "$state/var/$gen"
  in_target archix boot-check >> "$state/boot.log" 2>&1 || fail "archix boot-check: $(tail_of "$state/boot.log")"
  mark "bless-$gen"
  # systemd-bless-boot with archix's drop-in: archix renames the entry and syncs it under its new name, then systemd's own rename finds that done; without it (BREAK=bless-unsynced) systemd renames it and syncs only the directory and the filesystem.
  if [ "$name" != "$entry" ]; then
    [ "$BREAK" = bless-unsynced ] || in_target archix bless >> "$state/boot.log" 2>&1 || fail "archix bless: $(tail_of "$state/boot.log")"
    if [ -e "/target/efi/loader/entries/$name" ]; then
      mv "/target/efi/loader/entries/$name" "/target/efi/loader/entries/$entry"
      python -c 'import os, sys; os.fsync(os.open(sys.argv[1], os.O_RDONLY))' /target/efi/loader/entries
      sync -f /target/efi
    fi
  fi
  in_target archix boot-complete >> "$state/boot.log" 2>&1 || fail "archix boot-complete: $(tail_of "$state/boot.log")"
  [ -s /target/var/lib/archix-powercut-hook ] || fail "the hook didn't run at deployment $gen's first boot: $(tail_of "$state/boot.log")"
  say "booted $name: deployment $booted; current $(current), deployments $(deployments)"
}
shutdown_target() {
  local p
  for p in /proc/[0-9]*; do [[ $(readlink "$p/root") == /target* ]] && kill -9 "${p#/proc/}" 2> /dev/null; done
  umount -R /target || fail "unmounting the target: $(findmnt -R -n -o TARGET /target | tr '\n' ' ')"
}
# What each deployment's files are at a step boundary, which every replayed point between two boundaries is checked against.
baseline() {
  mkdir -p "$state/baselines/$1"
  local n
  for n in $(deployments); do "$lib/findings" "$n" /target > "$state/baselines/$1/$n"; done
}
run() {
  local what=$1
  shift
  in_target archix --host archix-vm "$@" > "$state/run.log" 2>&1 || fail "$what: $(tail_of "$state/run.log")"
}
start_log() {
  : > "$markdir/marks"
  dmsetup create logw --table "0 $(blockdev --getsz "$target") log-writes $target $log" || fail "creating the log-writes device"
  linear tesp "$esp_start" "$esp_size" /dev/mapper/logw
  linear troot "$root_start" "$root_size" /dev/mapper/logw
}
stop_log() {
  dmsetup remove tesp troot
  read -r _ _ _ logged used < <(dmsetup status logw)
  dmsetup remove logw
  dmesg | grep -qi 'log-writes.*\(error\|full\|space\)' && fail "dm-log-writes lost writes: $(dmesg | grep -i log-writes | tail -3 | tr '\n' '|')"
}

if [ "$PHASE" = compare ]; then
  # One switch and its first boot, logged twice with each build from the same pristine disk, both reading systemd-boot's variables from files.
  EFIVARS=/run/powercut-efivars
  mkdir -p "$EFIVARS"
  { printf '\x06\x00\x00\x00'; python -c "import struct, sys; sys.stdout.buffer.write(struct.pack('<Q', $features))"; } > "$EFIVARS/LoaderFeatures-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
  for run_name in powercut-1 plain-1 powercut-2 plain-2; do
    PLAIN=; [[ $run_name == plain-* ]] && PLAIN=/usr/local/bin/archix-plain
    dd if="$pristine" of="$target" bs=4M status=none || fail "restoring the target"
    start_log
    boot_target
    run "the switch to the update ($run_name)" switch --noconfirm
    shutdown_target
    boot_target
    shutdown_target
    stop_log
    "$lib/io-tokens" "$log" "$esp_start" "$esp_size" > "$state/tokens-$run_name" || fail "reading the log of $run_name"
    say "logged $run_name: $logged writes, $(cut -d' ' -f1 "$state/tokens-$run_name" | sort | uniq -c | tr -s ' ' | tr '\n' ' ')"
  done
  "$lib/io-compare" "$state"/tokens-{powercut-1,powercut-2,plain-1,plain-2} > "$state/compare" 2>&1
  verdict=$?
  tar -C "$state" -cf "$export" compare tokens-{powercut-1,powercut-2,plain-1,plain-2} || fail "exporting the tokens"
  while read -r line; do say "$line"; done < "$state/compare"
  [ "$verdict" = 0 ] || fail "the power-cut build's I/O differs from the plain release build's beyond what two runs of one build differ by"
  say "OK"
  systemctl poweroff
  exit 0
fi

if [ "$PHASE" = record ]; then
  start_log
  boot_target
  base=$booted
  [ "$(current)" = "$base" ] || fail "deployment $base booted, but current is $(current)"
  mark stage-0
  baseline 0
  say "switching deployment $base to the prefetched update"
  run "the switch to the update" switch --noconfirm
  new=$(current)
  [ "$new" != "$base" ] || fail "the switch made no new deployment: $(tail_of "$state/run.log")"
  say "PASS: the switch committed deployment $new ($(grep -c '^upgrade' "$state/run.log") upgrades)"
  shutdown_target
  boot_target
  [ "$booted" = "$new" ] || fail "deployment $new should boot after its switch, not $booted"
  mark stage-1
  baseline 1
  run "the rollback to the kept deployment $base" rollback "$base" --noconfirm
  [ "$(current)" = "$base" ] || fail "rolling back to deployment $base should make it current, not $(current)"
  say "PASS: rolled back to the kept deployment $base by repointing"
  mark stage-2
  baseline 2
  dropped=$((base - 1))
  [ ! -d "/target/archix/deploy/$dropped" ] || fail "keep = 2 should have dropped deployment $dropped: $(deployments)"
  run "the rebuild of generation $dropped" rollback "$dropped" --noconfirm
  [ "$(current)" = "$dropped" ] || fail "rebuilding generation $dropped should make it current, not $(current)"
  shutdown_target
  boot_target
  [ "$booted" = "$dropped" ] || fail "the rebuilt deployment $dropped should boot, not $booted"
  say "PASS: rebuilt the dropped generation $dropped from deployment $base and booted it"
  mark stage-3
  baseline 3
  shutdown_target
  mark end
  stop_log
  say "PASS: logged $logged writes ($((used / 2048)) MiB) and $(wc -l < "$markdir/marks") marks in $((SECONDS - started)) s"
  for k in 0 1 2 3; do say "baseline $k: $(for f in "$state/baselines/$k"/*; do echo -n "deployment ${f##*/} $(($(wc -l < "$f") - 1)) findings; "; done)"; done
  "$lib/point-table" "$log" "$esp_start" "$esp_size" > "$state/table" || fail "reading the log"
  echo "$logged $used $(wc -l < "$state/table") $(awk '$2 == 1' "$state/table" | wc -l) $(sha256sum < "$state/table" | cut -c1-16)" > "$state/logged"
  tar -C "$state" -cf "$export" baselines var table logged || fail "writing the baselines out"
  say "RECORDED: $(cat "$state/logged")"
  say "OK"
  systemctl poweroff
  exit 0
fi

tar -C "$state" -xf "$export" || fail "reading the logged run's baselines"
read -r _ _ _ _ log_id < "$state/logged"
if [ -n "$POINTS" ]; then
  entries=$(replay-log --log "$log" --num-entries)
  "$lib/point-table" "$log" "$esp_start" "$esp_size" $((entries / POINTS + 1)) > "$state/table" || fail "reading the log"
  mode="$POINTS evenly spaced prefixes of $entries entries"
else
  "$lib/point-table" "$log" "$esp_start" "$esp_size" > "$state/table.replay" || fail "reading the log"
  cmp -s "$state/table" "$state/table.replay" || fail "the point table read now isn't the one the logged run wrote"
  mode="flush points"
fi
total=$(wc -l < "$state/table")
esp_points=$(awk '$2 == 1' "$state/table" | wc -l)
say "replaying log $log_id: $total points ($mode), $esp_points after ESP writes, each of those booted for real, and every ${SAMPLE:-no}th other point; seed $SEED${SHARD:+; shard $SHARD checks every ${SHARD#*/}th point}"
if [ -z "$POINTS" ] && [ "${SUBSETS:-0}" -gt 0 ]; then
  # What the ESP holds before each flush window, which check-point's subsets of the window's writes start from.
  dd if="$pristine" of="$state/esp-before" bs=1M iflag=skip_bytes,count_bytes skip=$((esp_start * 512)) count=$((esp_size * 512)) conv=sparse status=none || fail "copying the ESP"
  say "reordering: up to $SUBSETS subsets of the ESP writes before each of those $esp_points flushes, seed $SEED, every ${REORDER_SAMPLE:-no}th booted for real"
fi
modprobe -q kvm_amd || modprobe -q kvm_intel
[ -e /dev/kvm ] || fail "no KVM in the runner for the real boots; the host needs nested virtualisation"
# What a real boot takes on this machine: the pristine image's, deployment 3's first boot, whose archix units' time every replayed state's is judged against.
# The pristine image is the logged run's starting disk, the same for every shard, so a boot of it that crashed (gate 40429132019: PID 1 in the initrd segfaulted on a page of zeros in libsystemd-core) is the runner's and gets one more try.
secs=$("$lib/esp-case" pristine) || {
  cp /dev/shm/boot.log "$state/boot-pristine-1.log"
  say "the pristine image didn't boot for real in $secs s, so it boots once more: $(grep -a -i 'segfault\|panic\|freezing\|failed\|emergency' /dev/shm/boot.log | tail -4 | tr -d '\r' | tr '\n' '|' | cut -c1-600)"
  secs=$("$lib/esp-case" pristine)
} || { cp /dev/shm/boot.log "$state/boot-pristine.log"; fail "the pristine image didn't boot for real in $secs s: $(grep -a -i 'archix\|failed\|emergency' /dev/shm/boot.log | tail -6 | tr -d '\r' | tr '\n' '|')"; }
cp /dev/shm/boot.log "$state/boot-pristine.log"
real=$(grep -a -o 'POWERCUT-BOOTED.*' /dev/shm/boot.log | tail -1 | tr -d '\r')
[[ $real == *" esp=rw "*" altered=0 "* && $real == *" failed=" ]] || fail "the pristine image's real boot isn't clean: $real $(grep -a -o 'POWERCUT-[A-Z]*.*' /dev/shm/boot.log | tr -d '\r' | tr '\n' '|' | cut -c1-5000)"
took=$(grep -o ' boot=[0-9]*' <<< "$real" | cut -d= -f2)
units=$(grep -o ' archix=[0-9]*' <<< "$real" | cut -d= -f2)
qkk=$(grep -o ' qkk=[0-9]*' <<< "$real" | cut -d= -f2)
[ -n "$units" ] && [ -n "$qkk" ] || fail "the pristine image's report has no archix units' or pacman -Qkk time: $real"
echo "${took:-$secs}" > "$state/baseline-boot"
echo "$units" > "$state/baseline-units"
echo "$qkk" > "$state/baseline-qkk"
say "baseline: the pristine image boots for real in ${took:-$secs} s to its report, $secs s with it, $units ms of it in archix's units, and its pacman -Qkk in $qkk ms ($real; $(grep -a -o 'POWERCUT-UNITS.*' /dev/shm/boot.log | tail -1 | tr -d '\r')); a replayed state's archix units may take ${BOOT_RATIO}x that or $BOOT_FLOOR s more, whichever is longer, scaled by how much longer its pacman -Qkk takes, unless a second boot of it and of the pristine image says otherwise${BOOT_CEILING:+, and its boot at most $BOOT_CEILING s}"
# Each way the ESP can be mounted, with a file on it whose FAT chain runs into a free cluster, as a power cut can leave one; then what fsck's repair of current's entry or UKI leaves.
cases=0
for c in passno-2 passno-0 gpt-auto drop-entry drop-uki empty-uki only-fallback; do
  [ -z "$SHARD" ] || [ "$((cases % ${SHARD#*/}))" = "$((${SHARD%/*} - 1))" ] || { cases=$((cases + 1)); continue; }
  cases=$((cases + 1))
  [ -z "$CASES" ] || [[ ,$CASES, == *,$c,* ]] || continue
  "$lib/esp-case" "$c" > "$state/case.out" 2>&1 || fail "ESP case $c: $(tail -3 "$state/case.out" | tr '\n' '|')"
  say "ESP case $c: $(tail -1 "$state/case.out")"
  echo "$c" >> "$state/cases"
done
replay_started=$SECONDS
stop() {
  # replay-log's "Fsck errored out on entry N" only says check-point failed after entry N.
  say "replay log: $(grep -v 'Fsck errored out' "$state/replay.log" | tail -3 | tr '\n' '|')"
  grep PROBLEM "$state/check.log" | head -4 | while read -r line; do say "${line:0:2000}"; done
  fail "$(tail -1 "$state/check.log") (seed $SEED, log $log_id)"
}
if [ -n "$POINTS" ]; then
  pos=0
  while read -r p _; do
    replay-log --log "$log" --replay "$pristine" --start-entry "$pos" --limit $((p - pos)) >> "$state/replay.log" 2>&1 || stop
    pos=$p
    "$lib/check-point" || stop
  done < "$state/table"
else
  replay-log --log "$log" --replay "$pristine" --check flush --fsck "$lib/check-point" >> "$state/replay.log" 2>&1 || stop
  # The log's tail after its last flush, as a power cut at the very end leaves it.
  [ "$(cat "$state/points")" = "$total" ] || "$lib/check-point" || stop
fi
[ "$(cat "$state/points")" = "$total" ] || fail "went through $(cat "$state/points") points of $total"
say "steps: $(grep -v ', subset [0-9]*,' "$state/check.log" | grep -o '^point [0-9]* ([^,)]*' | sed 's/^point [0-9]* (//' | uniq -c | tr -s ' ' | tr '\n' ' ')"
while read -r p step esp real; do say "real boot at point $p ($step$([ "$esp" = 1 ] && echo ', ESP')): ${real#POWERCUT-BOOTED }"; done < "$state/real-boots"
# Records of an ESP subset are tagged POINT/SUBSET.
count() { if [ -e "$state/$1" ]; then grep -vc '^[0-9]*/' "$state/$1"; else echo 0; fi; }
subset_count() { if [ -e "$state/$1" ]; then grep -c '^[0-9]*/' "$state/$1"; else echo 0; fi; }
lines() { if [ -e "$state/$1" ]; then wc -l < "$state/$1"; else echo 0; fi; }
checked=$(lines checked)
say "PASS: $total points, $checked of them checked, in $((SECONDS - replay_started)) s ($mode), $esp_points after ESP writes; $(count real-boots) booted for real; $(count activation-points) cut during a first boot, whose next activation matched a whole one; fsck notes: $(count vfat-notes) ESP, $(count root-fsck-notes) root; $(count repaired) ESP repairs checked; $(count leftover-points) points leave leftovers the next switch clears; $(lines stale-devices) device-mapper devices a check couldn't remove"
if [ -s "$state/reorder-points" ]; then
  say "PASS: reordered $(awk '{ n += $4 } END { print n }' "$state/reorder-points") ESP subsets in $(awk '{ s += $5 } END { print s }' "$state/reorder-points") s at $(wc -l < "$state/reorder-points") flush points (seed $SEED, windows of up to $(sort -n -k2 "$state/reorder-points" | tail -1 | cut -d' ' -f2) ESP writes); $(subset_count real-boots) booted for real; fsck notes: $(subset_count vfat-notes) ESP; $(subset_count repaired) ESP repairs checked; $(subset_count leftover-points) leave leftovers the next switch clears"
fi
spread() { grep -o " $1=[0-9]*" "$state/real-boots" 2> /dev/null | cut -d= -f2 | sort -n | awk -v u="$2" '{ a[NR] = $1; s += $1 } END { if (NR) printf "%d %s mean, %d %s median, %d %s max", s / NR, u, a[int((NR + 1) / 2)], u, a[NR], u; else printf "none" }'; }
say "timing: point checks $(awk '$3 == 0 { n++; s += $4 } END { printf "%d without a real boot, %.1f s each", n, n ? s / n : 0 }' "$state/point-times"), $(awk '$3 == 1 { n++; s += $4 } END { printf "%d with one, %.1f s each", n, n ? s / n : 0 }' "$state/point-times"); real boots $(spread secs s), $(spread boot s) to the report, against the baseline's $(cat "$state/baseline-boot") s; archix's units $(spread archix ms) against the baseline's $(cat "$state/baseline-units") ms; pacman -Qkk $(spread qkk ms) against the baseline's $(cat "$state/baseline-qkk") ms"
[ ! -s "$state/slow-once" ] || while read -r line; do say "slow once: $line"; done < "$state/slow-once"
if [ -s "$state/orphans" ]; then
  while read -r n line; do say "orphan at $n points: $line"; done < <(cut -d' ' -f2- "$state/orphans" | sort | uniq -c)
  fail "$(count orphans) points and $(subset_count orphans) ESP subsets leave orphans after the next boot and gc"
fi
# What this shard covered, which the release gate sums over the shards of one log.
esp_booted=$(awk '$3 == 1 && $2 == 1' "$state/point-times" | wc -l)
sampled=$(awk '$3 == 1 && $2 == 0' "$state/point-times" | wc -l)
sample_due=$(awk -v s="${SAMPLE:-0}" '$2 == 0 && s > 0 && NR % s == 0' "$state/table" | wc -l)
say "SUMMARY: {\"log\": \"$log_id\", \"fs\": \"$fs\", \"seed\": \"$SEED\", \"shard\": \"${SHARD:-1/1}\", \"points_total\": $total, \"esp_points_total\": $esp_points, \"sampled_points_total\": $sample_due, \"subsets_total\": $(cat "$state/subsets-seen" 2> /dev/null || echo 0), \"cases_total\": 7, \"points_checked\": $checked, \"esp_points_booted\": $esp_booted, \"sampled_points_booted\": $sampled, \"subsets_checked\": $(cat "$state/subsets" 2> /dev/null || echo 0), \"subsets_booted\": $(subset_count real-boots), \"real_boots\": $(lines real-boots), \"cases_checked\": $(lines cases)}"
say "OK"
systemctl poweroff
