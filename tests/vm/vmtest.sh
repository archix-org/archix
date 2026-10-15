#!/bin/bash
# Runs at every boot of the VM test, walking through its stages and reporting on the console.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
stage=$(cat "$state/stage" 2>/dev/null || echo 1)
say() { echo "VMTEST: $*" > /dev/console; }
fail() { say "FAIL: $*"; systemctl poweroff; exit 1; }
current() { archix generations | awk '$1 == "*" { print $2 }'; }
booted=$(sed -n 's/.*archix\.generation=\([0-9]*\).*/\1/p' /proc/cmdline)
say "stage $stage: booted generation $booted, current generation $(current)"
config=/etc/archix/init.lua

case $stage in
  1)
    [ "$(current)" = 1 ] || fail "generation 1 should be current"
    archix --host archix-vm install tree --noconfirm > /dev/console 2>&1 || fail "archix install tree"
    [ "$(current)" = 2 ] || fail "installing tree should make generation 2"
    bootctl set-oneshot archix-1.efi || fail "bootctl set-oneshot archix-1.efi"
    echo 2 > "$state/stage"
    say "PASS: generation 2 has a boot entry; booting generation 1's next"
    systemctl reboot
    ;;
  2)
    [ "$booted" = 1 ] || fail "should have booted generation 1's entry"
    [ "$(current)" = 1 ] || fail "booting generation 1's entry should make it current"
    pacman -Q tree > /dev/null 2>&1 && fail "the rollback at boot should have removed tree"
    say "PASS: booting an older entry rolled the system back to it"
    archix secure-boot setup > /dev/console 2>&1 || fail "archix secure-boot setup"
    sed -i 's|esp = "/boot" }|esp = "/boot", secure_boot = true, protected_entry = true }|' "$config"
    archix --host archix-vm switch --noconfirm > /dev/console 2>&1 || fail "switch with Secure Boot"
    echo 3 > "$state/stage"
    systemctl reboot
    ;;
  3)
    bootctl status 2> /dev/null | grep -q "Secure Boot: enabled (user)" || fail "Secure Boot should be enforcing"
    say "PASS: booted a signed entry with Secure Boot enforcing"
    archix secure-boot check > /dev/console 2>&1 || fail "secure-boot check should pass"
    archix secure-boot check --anticheat > /dev/console 2>&1 && fail "the anti-cheat check can't pass with a self-signed kernel"
    [ -f "/boot/EFI/Linux/archix-$(current)-protected.efi" ] || fail "protected_entry should write a protected entry"
    sed -i 's|secure_boot = true|secure_boot = false|' "$config"
    before=$(current)
    archix --host archix-vm install jq --noconfirm > /dev/console 2>&1 && fail "an unsigned switch must be refused under Secure Boot"
    [ "$(current)" = "$before" ] || fail "the refused switch changed the current generation"
    [ -e /var/lib/archix/journal.json ] && fail "the refused switch left a journal"
    pacman -Q jq > /dev/null 2>&1 && fail "the refused switch installed jq"
    sed -i 's|secure_boot = false|secure_boot = true|' "$config"
    say "PASS: secure-boot setup, check, the protected entry, and refusing unsigned entries"
    rm -f /usr/lib/libalpm.so*
    pacman -V > /dev/null 2>&1 && fail "pacman should be broken now"
    bootctl set-oneshot "archix-$(current)-rescue.efi" || fail "bootctl set-oneshot rescue"
    echo 4 > "$state/stage"
    systemctl reboot
    ;;
  4)
    pacman -V > /dev/null 2>&1 || fail "the rescue boot should have repaired pacman"
    say "PASS: the rescue entry restored pacman without pacman"
    echo done > "$state/stage"
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
