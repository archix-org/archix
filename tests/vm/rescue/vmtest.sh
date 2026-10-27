#!/bin/bash
# The rescue suite's guest side: every deployment broken at once, then a lost root password, each repaired from the rescue entry through `archix rescue` alone, which tests/vm/rescue/drive.py types at its prompt.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
stage=$(cat "$state/stage" 2> /dev/null || echo 1)
config=/etc/archix/init.lua
ax() { archix --host archix-vm "$@"; }
say() { echo "VMTEST: $*" > /dev/console; }
fail() { say "FAIL: $*"; echo failed > "$state/stage"; sync; systemctl poweroff; exit 1; }
next() { echo "$1" > "$state/stage"; sync; }
remember() { echo "$2" > "$state/$1"; }
recall() { cat "$state/$1"; }
current() { readlink /archix/current | cut -d/ -f2; }
deployments() { ls /archix/deploy | grep -E '^[0-9]+$' | sort -n | tr '\n' ' '; }
booted=$(cat /run/archix/deployment 2> /dev/null)
selected=$(tail -c +5 /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2> /dev/null | tr -d '\000')
# The steps drive.py types at the next rescue prompt, one "expect<TAB>send" per line on stdin.
script() { say "rescue script: $(base64 -w0)"; }
# Into the rescue at the next restart only, as from the boot menu.
to_rescue() {
  bootctl set-oneshot archix-rescue.conf || fail "bootctl set-oneshot archix-rescue.conf"
  next "$1"
  say "restarting into the rescue entry"
  systemctl reboot
  exit 0
}
check_menu() {
  [ -f /efi/loader/entries/archix-rescue.conf ] || fail "there should be a rescue entry: $(ls /efi/loader/entries | tr '\n' ' ')"
  local uki ids
  uki=$(sed -n 's|^efi /|/efi/|p' /efi/loader/entries/archix-rescue.conf)
  [ -f "$uki" ] || fail "the rescue entry's UKI $uki should be on the ESP"
  ids=$(bootctl list --json=short | jq -r '.[].id')
  [ "$(grep '^archix' <<< "$ids" | tail -1)" = archix-rescue.conf ] || fail "the rescue entry should sort after every deployment's: $(tr '\n' ' ' <<< "$ids")"
  [ "$(bootctl list --json=short | jq -r '.[] | select(.isDefault) | .id')" != archix-rescue.conf ] || fail "the rescue entry must never be the default"
  grep -q rescue /efi/loader/loader.conf && fail "loader.conf should never name the rescue entry: $(tr '\n' '|' < /efi/loader/loader.conf)"
  return 0
}
rescue_source() { jq -r .source /var/lib/archix/boot/rescue.json; }
systemd_ok() { head -c 4 "/archix/deploy/$1/usr/lib/systemd/systemd" | grep -q ELF; }
root_hash() { grep '^root:' "/archix/deploy/$1/etc/shadow" | cut -d: -f2; }
# Runs a shell command with /archix writable, which the boot binds read-only.
in_store() { unshare -m sh -c "mount -o remount,bind,rw /archix && $1"; }

say "stage $stage: entry $selected, deployment $booted, current $(current), deployments $(deployments), rescue from $(rescue_source)"
case $stage in
  1)
    [ "$booted" = "$(current)" ] || fail "the current deployment should boot first"
    remember older "$booted"
    check_menu
    [ "$(rescue_source)" = "$booted" ] || fail "the image's switch should build the rescue entry from deployment $booted, as nothing had booted"
    ls /var/cache/pacman/pkg | grep -qE '^systemd-[0-9]' || fail "the image should keep systemd's package, which the repair restores from"
    sed -i 's|\["/etc/archix-vmtest-mark"\] = { text = "1" }|["/etc/archix-vmtest-mark"] = { text = "2" }|' "$config"
    ax switch --noconfirm > "$state/switch.log" 2>&1 || fail "the switch: $(tail -8 "$state/switch.log" | tr '\n' '|')"
    [ "$(current)" -gt "$booted" ] || fail "the switch should commit a deployment"
    remember newer "$(current)"
    [ "$(rescue_source)" = "$booted" ] || fail "deployment $(current) hasn't booted, so the rescue should keep deployment $booted's kernel"
    check_menu
    say "PASS: the rescue entry sorts after the deployments, isn't the default, and keeps the kernel of the deployment that booted"
    next 2
    systemctl reboot
    ;;
  2)
    [ "$booted" = "$(recall newer)" ] || fail "deployment $(recall newer) should boot"
    # Every deployment's init replaced by a new file with garbage in it, so none of them can boot.
    for n in $(deployments); do
      in_store "printf 'broken\n' > /archix/deploy/$n/usr/lib/systemd/systemd.new && mv /archix/deploy/$n/usr/lib/systemd/systemd.new /archix/deploy/$n/usr/lib/systemd/systemd" || fail "breaking deployment $n"
      systemd_ok "$n" && fail "deployment $n's systemd should be broken"
    done
    sync
    say "PASS: broke /usr/lib/systemd/systemd in deployments $(deployments)"
    {
      printf '%s\t%s\n' 'rescue> ' 'list' "deployment $(recall newer)" '' 'rescue> ' 'check' 'the root and the ESP are clean' '' 'rescue> ' 'verify --repair'
      for n in $(deployments); do printf '%s\t\n' "deployment $n: repaired 1 files"; done
      printf '%s\t%s\n' 'rescue> ' "use $(recall older)" "deployment $(recall older) is current and the boot default" '' 'rescue> ' 'reboot'
    } | script
    to_rescue 3
    ;;
  3)
    o=$(recall older)
    [ "$booted" = "$o" ] && [ "$(current)" = "$o" ] || fail "after the rescue, deployment $o should boot by default and be current, not ${booted:-none} (current $(current))"
    case $selected in "archix-$o.conf" | "archix-$o+"*) ;; *) fail "the boot default should have started archix-$o.conf, not $selected" ;; esac
    for n in $(deployments); do systemd_ok "$n" || fail "the rescue should have repaired deployment $n's systemd"; done
    ax verify > "$state/verify.log" 2>&1 || fail "archix verify after the rescue: $(tr '\n' '|' < "$state/verify.log")"
    check_menu
    say "PASS: the rescue checked the disks, repaired every deployment from the package cache and made deployment $o current, which booted"
    # A root password nobody knows, in every deployment.
    lost=$(openssl passwd -6 "$(head -c 24 /dev/urandom | base64)")
    for n in $(deployments); do
      in_store "sed -i 's|^root:[^:]*:|root:$lost:|' /archix/deploy/$n/etc/shadow" || fail "losing deployment $n's root password"
    done
    sync
    {
      printf '%s\t%s\n' 'rescue> ' 'password root --all' 'new password for root: ' 'rescued pass' 'again: ' 'rescued pass'
      for n in $(deployments); do printf '%s\t\n' "set root's password in deployment $n"; done
      printf '%s\t%s\n' 'rescue> ' "use $(recall newer)" "deployment $(recall newer) is current and the boot default" '' 'rescue> ' 'reboot'
    } | script
    to_rescue 4
    ;;
  4)
    n=$(recall newer)
    [ "$booted" = "$n" ] && [ "$(current)" = "$n" ] || fail "deployment $n should boot and be current after the second rescue, not ${booted:-none} (current $(current))"
    for d in $(deployments); do
      hash=$(root_hash "$d")
      salt=$(cut -d'$' -f3 <<< "$hash")
      [ "$(openssl passwd -6 -salt "$salt" 'rescued pass')" = "$hash" ] || fail "deployment $d's root password should be the one set in the rescue: $hash"
    done
    passwd -S root | grep -q ' P ' || fail "root should have a usable password: $(passwd -S root)"
    check_menu
    say "PASS: the rescue reset root's password in every deployment, and deployment $n booted normally"
    next done
    say "OK"
    systemctl poweroff
    ;;
  *)
    say "nothing to do at stage $stage"
    systemctl poweroff
    ;;
esac
