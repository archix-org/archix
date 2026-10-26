#!/bin/bash
# The recovery suite's archix side: two deployments, then the ESP wiped and the current one damaged; the rescue disk (recover.sh) brings the kept one back, which then boots and switches again.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
stage=$(cat "$state/stage" 2>/dev/null || echo 1)
config=/etc/archix/init.lua
ax() { archix --host archix-vm "$@"; }
say() { echo "VMTEST: $*" > /dev/console; }
fail() { say "FAIL: $*"; echo failed > "$state/stage"; sync; systemctl poweroff; exit 1; }
current() { ax generations | awk '$1 == "*" { print $2 }'; }
deployments() { ls /archix/deploy | grep -E '^[0-9]+$' | sort -n | tr '\n' ' '; }
next() { echo "$1" > "$state/stage"; sync; }
remember() { echo "$2" > "$state/$1"; }
recall() { cat "$state/$1"; }
booted=$(tr -d '\000' < /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
mark() { sed -i "s|\[\"/etc/archix-vmtest-mark\"\] = { text = \"[0-9]*\" }|[\"/etc/archix-vmtest-mark\"] = { text = \"$1\" }|" "$config"; }
switch_to() {
  local before; before=$(current)
  mark "$1"
  ax switch --noconfirm > "$state/switch.log" 2>&1 || fail "the switch to mark $1: $(tail -8 "$state/switch.log" | tr '\n' '|')"
  [ "$(current)" -gt "$before" ] || fail "the switch to mark $1 should commit a deployment: $(tail -5 "$state/switch.log" | tr '\n' '|')"
}
say "stage $stage: booted $booted, mounted $(cat /run/archix/deployment), current $(current), deployments $(deployments), mark $(cat /etc/archix-vmtest-mark), entries $(ls /efi/loader/entries 2>/dev/null | tr '\n' ' ')"
[ "$booted" = "$(cat /run/archix/deployment)" ] || fail "the initramfs mounted deployment $(cat /run/archix/deployment), but systemd-boot started archix-$booted"

case $stage in
  1)
    remember kept "$(current)"
    switch_to 2
    remember broken "$(current)"
    say "PASS: deployment $(recall broken) committed beside $(recall kept)"
    next 2
    systemctl reboot
    ;;
  2)
    b=$(recall broken) k=$(recall kept)
    [ "$booted" = "$b" ] && grep -qx 2 /etc/archix-vmtest-mark || fail "deployment $b should boot with mark 2"
    # A lost ESP: no loader, no entries, no UKIs, no fallback, no systemd-boot.
    rm -rf /efi/* || fail "wiping the ESP"
    sync /efi
    # Current's init replaced by a new file, so the kept deployment's hardlink to the old one stays as it was.
    unshare -m sh -c "mount -o remount,bind,rw /archix && printf 'damaged\n' > /archix/deploy/$b/usr/lib/systemd/systemd.new && mv /archix/deploy/$b/usr/lib/systemd/systemd.new /archix/deploy/$b/usr/lib/systemd/systemd && sync /archix/deploy/$b/usr/lib/systemd" || fail "damaging deployment $b"
    head -c 4 "/archix/deploy/$k/usr/lib/systemd/systemd" | grep -q ELF || fail "deployment $k's systemd should be untouched"
    [ -z "$(ls -A /efi)" ] || fail "the ESP should be empty: $(ls -A /efi | tr '\n' ' ')"
    say "PASS: wiped the ESP and replaced deployment $b's /usr/lib/systemd/systemd; current is $(current)"
    next 3
    say "broken"
    systemctl poweroff
    ;;
  3)
    k=$(recall kept)
    [ "$booted" = "$k" ] && [ "$(current)" = "$k" ] || fail "after the recovery deployment $k should boot and be current"
    grep -qx 1 /etc/archix-vmtest-mark || fail "deployment $k should have mark 1, not $(cat /etc/archix-vmtest-mark)"
    [ -f "/efi/EFI/Linux/archix-fallback-$k.efi" ] || fail "the recovery should write deployment $k's fallback UKI: $(find /efi -type f | tr '\n' ' ')"
    grep -q "archix-$k" /efi/loader/loader.conf || fail "loader.conf should name deployment $k: $(tr '\n' '|' < /efi/loader/loader.conf)"
    systemctl --failed --no-legend | grep -q archix && fail "no archix unit should fail: $(systemctl --failed --no-legend | tr '\n' '|')"
    ax status > "$state/status.log" 2>&1 || fail "archix status: $(tail -5 "$state/status.log" | tr '\n' '|')"
    say "PASS: the recovered deployment $k booted from the entry the rescue wrote; status: $(head -4 "$state/status.log" | tr '\n' '|')"
    switch_to 3
    say "PASS: the recovered system switched to deployment $(current)"
    next 4
    systemctl reboot
    ;;
  4)
    [ "$booted" = "$(current)" ] && [ "$booted" -gt "$(recall broken)" ] && grep -qx 3 /etc/archix-vmtest-mark || fail "the switch after the recovery should boot, with mark 3"
    say "PASS: the switch after the recovery booted deployment $booted"
    next done
    say "OK"
    systemctl poweroff
    ;;
  *)
    say "nothing to do at stage $stage"
    systemctl poweroff
    ;;
esac
