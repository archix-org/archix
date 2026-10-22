#!/bin/bash
# DKMS with two kernels, linux and linux-lts: every deployment gets a UKI and entry per kernel and the module built for both, each entry boots and loads its own build, booting the older deployment's entries rolls both back across an update (2026-06-01 to 2026-10-10), and gc and a rebuilding rollback handle both kernels' entries.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
stage=$(cat "$state/stage" 2>/dev/null || echo 1)
config=/etc/archix/init.lua
lock=/etc/archix/archix.lock
ax() { archix --host archix-vm "$@"; }
say() { echo "VMTEST: $*" > /dev/console; }
diag() {
  say "diagnostics: /archix/deploy holds $(ls -A /archix/deploy | tr '\n' ' '), entries $(ls /efi/loader/entries | tr '\n' ' ')"
  say "diagnostics: dkms $(dkms status 2>&1 | tr '\n' '|')"
  say "diagnostics: pacman.log $(tail -8 /var/log/pacman.log | tr '\n' '|')"
}
fail() { say "FAIL: $*"; diag; echo failed > "$state/stage"; sync; systemctl poweroff; exit 1; }
current() { ax generations | awk '$1 == "*" { print $2 }'; }
deployments() { ls /archix/deploy | grep -E '^[0-9]+$' | sort -n | tr '\n' ' '; }
remember() { echo "$2" > "$state/$1"; }
recall() { cat "$state/$1"; }
next() { echo "$1" > "$state/stage"; sync; }
# systemd-boot names the entry it started in LoaderEntrySelected: 4 attribute bytes, then archix-N.conf or archix-N-linux-lts.conf in UTF-16.
selected=$(tail -c +5 /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | tr -d '\000')
booted=$(echo "$selected" | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
# Deployment $1's kernel of package $2: the modules directory that holds a vmlinuz, -lts or not.
kernel_of() {
  local k v
  for k in /archix/deploy/"$1"/usr/lib/modules/*/vmlinuz; do
    v=$(basename "$(dirname "$k")")
    case $v in
      *-lts) [ "$2" != linux-lts ] || echo "$v" ;;
      *) [ "$2" != linux ] || echo "$v" ;;
    esac
  done
}
built_module() { modinfo -F version /archix/deploy/"$1"/usr/lib/modules/"$2"/updates/dkms/v4l2loopback.ko* 2> /dev/null; }
entry_file() {
  case $2 in
    linux) ls /efi/loader/entries/archix-"$1".conf /efi/loader/entries/archix-"$1"+*.conf 2> /dev/null | head -1 ;;
    *) ls /efi/loader/entries/archix-"$1-$2".conf 2> /dev/null ;;
  esac
}
# Both kernels of deployment $1 have their own entry, whose UKI is built from that kernel, and the module DKMS built for it at version $2.
both_kernels() {
  local n=$1 want=$2 pkg kver entry uki
  for pkg in linux linux-lts; do
    kver=$(kernel_of "$n" "$pkg")
    [ -n "$kver" ] || fail "deployment $n should have $pkg"
    entry=$(entry_file "$n" "$pkg")
    [ -n "$entry" ] || fail "deployment $n should have an entry for $pkg: $(ls /efi/loader/entries | tr '\n' ' ')"
    uki=$(sed -nE 's#^(efi|uki) /#/efi/#p' "$entry")
    [ -f "$uki" ] || fail "$entry starts $uki, which is missing"
    ukify inspect "$uki" | grep -A6 '^\.uname' | grep -q "$kver" || fail "$entry's UKI should be built from $kver: $(ukify inspect "$uki" | grep -A6 '^\.uname' | tr '\n' '|')"
    if [ -n "$want" ]; then
      [ "$(built_module "$n" "$kver")" = "$want" ] || fail "deployment $n should have v4l2loopback $want built for $kver, found '$(built_module "$n" "$kver")'"
    fi
  done
}
# The module DKMS built for the running kernel, at the version the deployment locks, loaded.
dkms_module() {
  local want=$1 have
  have=$(modinfo -F version v4l2loopback 2>/dev/null)
  [ "$have" = "$want" ] || fail "v4l2loopback $want should be built by DKMS for $(uname -r), found '${have:-nothing}'"
  case $(modinfo -n v4l2loopback) in */updates/dkms/*) ;; *) fail "v4l2loopback should come from DKMS, not $(modinfo -n v4l2loopback)" ;; esac
  modprobe v4l2loopback || fail "v4l2loopback $want should load on $(uname -r)"
  grep -q '^v4l2loopback ' /proc/modules || fail "v4l2loopback should be loaded"
}
running() { case $(uname -r) in *-lts) echo linux-lts ;; *) echo linux ;; esac; }
boots() {
  [ "$booted" = "$1" ] || fail "should have booted deployment $1, not $booted ($selected)"
  [ "$(running)" = "$2" ] || fail "deployment $1's $2 entry should boot $2, not $(uname -r)"
  [ "$(uname -r)" = "$(kernel_of "$1" "$2")" ] || fail "deployment $1 should run its own $2, $(kernel_of "$1" "$2"), not $(uname -r)"
}
say "stage $stage: selected $selected, mounted $(cat /run/archix/deployment), current $(current), deployments $(deployments), kernel $(uname -r)"
[ "$booted" = "$(cat /run/archix/deployment)" ] || fail "the initramfs mounted deployment $(cat /run/archix/deployment), but systemd-boot started $selected"

case $stage in
  1)
    image=$(current)
    both_kernels "$image" ""
    say "PASS: the image's deployment $image has a UKI and entry for linux and for linux-lts"
    sed -i 's|"systemd-ukify" }, -- packages|"systemd-ukify", "v4l2loopback-dkms" }, -- packages|' "$config"
    ax switch --noconfirm > /dev/console 2>&1 || fail "the switch adding a DKMS module"
    remember old "$(current)"
    for headers in linux-headers linux-lts-headers; do
      grep -q "^$headers = " "$lock" || fail "archix should declare $headers for the DKMS module by itself: $(grep -- '-headers = ' "$lock" | tr '\n' '|')"
    done
    both_kernels "$(recall old)" 0.15.3
    say "PASS: deployment $(recall old) has both kernels' headers and v4l2loopback 0.15.3 built for each"
    next 2
    systemctl reboot
    ;;
  2)
    boots "$(recall old)" linux
    dkms_module 0.15.3
    say "PASS: deployment $(recall old)'s linux entry loads its own build"
    bootctl set-oneshot "archix-$(recall old)-linux-lts.conf" || fail "bootctl set-oneshot"
    next 3
    systemctl reboot
    ;;
  3)
    boots "$(recall old)" linux-lts
    dkms_module 0.15.3
    say "PASS: deployment $(recall old)'s linux-lts entry boots $(uname -r) and loads its own build"
    ax update --to 2026-10-10 --noconfirm > /dev/console 2>&1 || fail "archix update --to 2026-10-10"
    ax switch --noconfirm > /dev/console 2>&1 || fail "the 2026-10-10 update"
    remember new "$(current)"
    both_kernels "$(recall new)" 0.15.4
    [ "$(kernel_of "$(recall new)" linux)" != "$(kernel_of "$(recall old)" linux)" ] || fail "the update should change linux"
    say "update: linux $(kernel_of "$(recall old)" linux) to $(kernel_of "$(recall new)" linux), linux-lts $(kernel_of "$(recall old)" linux-lts) to $(kernel_of "$(recall new)" linux-lts)"
    both_kernels "$(recall old)" 0.15.3
    say "PASS: the update built v4l2loopback 0.15.4 for both new kernels, leaving deployment $(recall old)'s builds as they were"
    next 4
    systemctl reboot
    ;;
  4)
    boots "$(recall new)" linux
    dkms_module 0.15.4
    bootctl set-oneshot "archix-$(recall new)-linux-lts.conf" || fail "bootctl set-oneshot"
    next 5
    systemctl reboot
    ;;
  5)
    boots "$(recall new)" linux-lts
    dkms_module 0.15.4
    say "PASS: both of deployment $(recall new)'s entries boot their kernel and load v4l2loopback 0.15.4"
    bootctl set-oneshot "archix-$(recall old)-linux-lts.conf" || fail "bootctl set-oneshot"
    next 6
    systemctl reboot
    ;;
  6)
    boots "$(recall old)" linux-lts
    [ "$(current)" = "$(recall old)" ] || fail "booting deployment $(recall old)'s entry should make it current"
    dkms_module 0.15.3
    say "PASS: rolled back across the update on linux-lts: $(uname -r) loads v4l2loopback 0.15.3"
    bootctl set-oneshot "archix-$(recall old).conf" || fail "bootctl set-oneshot"
    next 7
    systemctl reboot
    ;;
  7)
    boots "$(recall old)" linux
    dkms_module 0.15.3
    say "PASS: rolled back across the update on linux: $(uname -r) loads v4l2loopback 0.15.3"
    ax rollback "$(recall new)" --noconfirm > /dev/console 2>&1 || fail "rolling forward to deployment $(recall new)"
    next 8
    systemctl reboot
    ;;
  8)
    old=$(recall old) new=$(recall new)
    boots "$new" linux
    sed -i 's|"v4l2loopback-dkms" }, -- packages|"v4l2loopback-dkms", "tree" }, -- packages|' "$config"
    ax switch --noconfirm > /dev/console 2>&1 || fail "the switch adding tree"
    [ -d "/archix/deploy/$old" ] && fail "keep = 2 should drop deployment $old: $(deployments)"
    ls /efi/loader/entries | grep -q "^archix-$old[-+.]" && fail "both of deployment $old's entries should go with it: $(ls /efi/loader/entries | tr '\n' ' ')"
    for entry in /efi/loader/entries/archix-*.conf; do
      uki=$(sed -nE 's#^(efi|uki) /#/efi/#p' "$entry")
      [ -f "$uki" ] || fail "after gc, $entry starts $uki, which is gone"
    done
    for uki in /efi/EFI/archix/*.efi; do
      grep -qr "/EFI/archix/$(basename "$uki")" /efi/loader/entries || fail "gc left $uki, which no entry starts"
    done
    for n in $(deployments); do both_kernels "$n" ""; done
    say "PASS: gc dropped deployment $old with both its entries and their UKIs, and every kept deployment keeps one per kernel"
    ax rollback "$old" --noconfirm > /dev/console 2>&1 || fail "rebuilding generation $old"
    [ "$(current)" = "$old" ] || fail "the rebuilt deployment $old should be current"
    both_kernels "$old" 0.15.3
    say "PASS: rebuilding generation $old built v4l2loopback 0.15.3 for both its kernels and wrote both entries"
    next 9
    systemctl reboot
    ;;
  9)
    boots "$(recall old)" linux
    dkms_module 0.15.3
    bootctl set-oneshot "archix-$(recall old)-linux-lts.conf" || fail "bootctl set-oneshot"
    next 10
    systemctl reboot
    ;;
  10)
    boots "$(recall old)" linux-lts
    dkms_module 0.15.3
    say "PASS: both entries of the rebuilt deployment $(recall old) boot their kernel and load v4l2loopback 0.15.3"
    next done
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
