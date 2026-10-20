#!/bin/bash
# Runs at every boot of the deployment boot VM: checks the mounted deployment and its activation, then boots the next entry; reports on the console.
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
  touch /usr/.archix-vm-rw && rm /usr/.archix-vm-rw || fail "/usr should be writable once the root is"
  pacman -Q linux > /dev/null || fail "pacman should read deployment $1's database"
  [ -e "/var/lib/archix/activated/$1" ] || fail "deployment $1 should be activated"
  [ "$(cat /var/lib/archix/booted-deployment)" = "$1" ] || fail "the booted deployment should be recorded"
  systemctl is-active -q archix-deploy-activate.service || fail "activation: $(systemctl status archix-deploy-activate.service | tail -5)"
}

case $stage in
  1)
    check_deployment 2
    [ "$(cat /var/lib/archix-vm/seed)" = seed ] || fail "deployment 2's /var file should be applied"
    [ "$(stat -c %a:%g /var/lib/archix-vm/seed)" = 640:12 ] || fail "with its mode and group: $(stat -c %a:%g /var/lib/archix-vm/seed)"
    [ "$(cat /var/lib/archix-vm/kept)" = state ] || fail "a whiteout must not delete live state"
    grep -q 'logged live' /var/log/pacman.log && ! grep -q 'logged in staging' /var/log/pacman.log || fail "pacman.log must stay the live one"
    say "PASS: entry archix-2.conf booted deployment 2 and activated its /var changes"
    echo 2 > "$state"
    bootctl set-oneshot archix-1.conf || fail "bootctl set-oneshot"
    systemctl reboot
    ;;
  2)
    check_deployment 1
    say "PASS: entry archix-1.conf booted deployment 1"
    echo 3 > "$state"
    bootctl set-oneshot archix-3.conf || fail "bootctl set-oneshot"
    systemctl reboot
    ;;
  3)
    [ "$selected" = archix-3.conf ] || fail "entry archix-3.conf should have started this boot"
    check_deployment 2
    say "PASS: the entry of an uncommitted staging tree booted the current deployment"
    echo done > "$state"
    say "OK"
    systemctl poweroff
    ;;
  *) systemctl poweroff ;;
esac
