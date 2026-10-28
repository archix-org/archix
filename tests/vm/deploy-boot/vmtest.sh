#!/bin/bash
# Runs at every boot of the deployment boot VM: checks the mounted deployment, /usr/local and its activation, then boots the next entry, last deployment 1's entry edited on the ESP; reports on the console.
set -u
state=/var/lib/archix-vm-stage
stage=$(cat "$state" 2> /dev/null || echo 1)
say() { echo "VMTEST: $*" > /dev/console; }
fail() { say "FAIL: $*"; systemctl poweroff; exit 1; }
booted=$(cat /run/archix/deployment 2> /dev/null)
selected=$(tail -c +5 /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2> /dev/null | tr -d '\000')
say "stage $stage: entry $selected, deployment $booted, /usr from $(findmnt -n -o SOURCE /usr)"

check_deployment() {
  [ "$booted" = "$1" ] || fail "deployment $1 should boot from entry $selected, not ${booted:-none}"
  [ "$(cat /etc/archix-vm-deployment)" = "$1" ] || fail "/etc should be deployment $1's"
  for at in /usr /etc /opt /boot /var/lib/pacman/local /var/lib/dkms; do
    findmnt -n -o SOURCE "$at" | grep -q "archix/deploy/$1/" || fail "$at should come from deployment $1: $(findmnt -n -o SOURCE "$at")"
  done
  for at in /usr /opt; do
    touch "$at/.archix-vm-rw" 2> /dev/null && fail "$at should be read-only"
  done
  touch /etc/.archix-vm-rw && rm /etc/.archix-vm-rw || fail "/etc should be writable once the root is"
  pacman -Q linux > /dev/null || fail "pacman should read deployment $1's database"
  ls /var/lib/archix/activated | grep -q "^$1-" || fail "deployment $1 should be activated"
  [ "$(cut -d- -f1 /var/lib/archix/booted-deployment)" = "$1" ] || fail "the booted deployment should be recorded"
  systemctl is-active -q archix-deploy-activate.service || fail "activation: $(systemctl status archix-deploy-activate.service | tail -5)"
}

usr_local_shared() {
  findmnt -n -o SOURCE /usr/local | grep -q '/var/usrlocal\]$' || fail "/usr/local should be /var/usrlocal: $(findmnt -n -o SOURCE /usr/local)"
  [ "$(cat /run/archix/usr-local)" = mounted ] || fail "deploy-mount should record /usr/local as mounted: $(cat /run/archix/usr-local)"
  grep -qx shared /usr/local/archix-vm-shared || fail "/usr/local should show the shared tree"
  echo "$1" >> /usr/local/archix-vm-written || fail "/usr/local should be writable"
}

refused_note() {
  grep -qF "$1" /run/archix/entry-refused || fail "deploy-mount should record why it passed over archix-1.conf: $(cat /run/archix/entry-refused 2>&1)"
  rm -f /var/lib/archix/notes
  archix boot-check > /dev/console 2>&1 || fail "archix boot-check"
  grep -F "systemd-boot started the boot entry archix-1.conf, but archix booted the current deployment 2 instead: " /var/lib/archix/notes | grep -qF "$1" \
    || fail "the login notes should say why: $(cat /var/lib/archix/notes 2>&1)"
  [ "$(readlink /archix/current)" = deploy/2 ] || fail "current should stay deployment 2: $(readlink /archix/current)"
}

case $stage in
  1)
    check_deployment 2
    [ "$(cat /var/lib/archix-vm/seed)" = seed ] || fail "deployment 2's /var file should be applied"
    [ "$(stat -c %a:%g /var/lib/archix-vm/seed)" = 640:12 ] || fail "with its mode and group: $(stat -c %a:%g /var/lib/archix-vm/seed)"
    [ "$(cat /var/lib/archix-vm/kept)" = state ] || fail "a whiteout must not delete live state"
    grep -q 'logged live' /var/log/pacman.log && ! grep -q 'logged in staging' /var/log/pacman.log || fail "pacman.log must stay the live one"
    usr_local_shared 1
    say "PASS: entry archix-2.conf booted deployment 2 and activated its /var changes, with /var/usrlocal on /usr/local"
    echo 2 > "$state"
    bootctl set-oneshot archix-1.conf || fail "bootctl set-oneshot"
    systemctl reboot
    ;;
  2)
    check_deployment 1
    findmnt /usr/local > /dev/null && fail "a boot of deployment 1, older than the current 2, should leave /usr/local out"
    grep -q '^skipped .*older than the current 2' /run/archix/usr-local || fail "deploy-mount should say why /usr/local is out: $(cat /run/archix/usr-local)"
    [ -z "$(ls -A /usr/local)" ] || fail "deployment 1's own /usr/local should be empty: $(ls -A /usr/local)"
    touch /usr/local/.archix-vm-rw 2> /dev/null && fail "deployment 1's own /usr/local should be read-only"
    say "PASS: entry archix-1.conf booted deployment 1, without the shared /usr/local"
    echo 3 > "$state"
    bootctl set-oneshot archix-3.conf || fail "bootctl set-oneshot"
    systemctl reboot
    ;;
  3)
    [ "$selected" = archix-3.conf ] || fail "entry archix-3.conf should have started this boot"
    check_deployment 2
    usr_local_shared 3
    [ "$(cat /usr/local/archix-vm-written)" = "$(printf '1\n3')" ] || fail "what the first boot wrote to /usr/local should still be there: $(cat /usr/local/archix-vm-written)"
    say "PASS: the entry of an uncommitted staging tree booted the current deployment, with /var/usrlocal on /usr/local"
    printf 'options %s archix.vmtest=edited\n' "$(cat /proc/cmdline)" >> /efi/loader/entries/archix-1.conf
    sync -f /efi
    echo 4 > "$state"
    bootctl set-oneshot archix-1.conf || fail "bootctl set-oneshot"
    systemctl reboot
    ;;
  4)
    [ "$selected" = archix-1.conf ] || fail "entry archix-1.conf should have started this boot"
    grep -qw archix.vmtest=edited /proc/cmdline || fail "the edited entry's options should be this boot's command line: $(cat /proc/cmdline)"
    check_deployment 2
    refused_note "isn't the one archix built /EFI/archix/shared.efi with"
    say "PASS: deployment 1's entry edited to pass another command line booted the current deployment, with a login note"
    sed 's|^efi .*|efi /EFI/archix/other.efi|' /var/lib/archix/boot/entries/archix-1.conf > /efi/loader/entries/archix-1.conf
    sync -f /efi
    echo 5 > "$state"
    bootctl set-oneshot archix-1.conf || fail "bootctl set-oneshot"
    systemctl reboot
    ;;
  5)
    [ "$selected" = archix-1.conf ] || fail "entry archix-1.conf should have started this boot"
    check_deployment 2
    refused_note "it started /EFI/archix/other.efi, but archix wrote it to start /EFI/archix/shared.efi"
    cp /var/lib/archix/boot/entries/archix-1.conf /efi/loader/entries/archix-1.conf
    sync -f /efi
    say "PASS: deployment 1's entry edited to start another UKI booted the current deployment, with a login note"
    echo done > "$state"
    say "OK"
    systemctl poweroff
    ;;
  *) systemctl poweroff ;;
esac
