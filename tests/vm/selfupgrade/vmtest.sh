#!/bin/bash
# archix upgrading itself with deployments: the older build (package 1) switches to this tree's (package 2) through a switch killed once halfway, the new deployment boots, the older deployment's entry boots its older archix, which reads the newer one's records, and each rebuilds a dropped generation the other recorded.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
stage=$(cat "$state/stage" 2>/dev/null || echo 1)
config=/etc/archix/init.lua
lock=/etc/archix/archix.lock
repo=/srv/archix-repo
records=/var/lib/archix/generations
ax() { archix --host archix-vm "$@"; }
say() { echo "VMTEST: $*" > /dev/console; }
diag() {
  say "diagnostics: /archix/deploy holds $(ls -A /archix/deploy | tr '\n' ' ')"
  say "diagnostics: generations $(ax generations 2>&1 | tr '\n' '|')"
  say "diagnostics: pacman.log $(tail -8 /var/log/pacman.log | tr '\n' '|')"
}
fail() { say "FAIL: $*"; diag; echo failed > "$state/stage"; sync; systemctl poweroff; exit 1; }
current() { ax generations | awk '$1 == "*" { print $2 }'; }
deployments() { ls /archix/deploy | grep -E '^[0-9]+$' | sort -n | tr '\n' ' '; }
remember() { echo "$2" > "$state/$1"; }
recall() { cat "$state/$1"; }
next() { echo "$1" > "$state/stage"; sync; }
# systemd-boot names the entry it started in LoaderEntrySelected: 4 attribute bytes, then archix-N.conf in UTF-16.
booted=$(tr -d '\000' < /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
version() { pacman -Q archix | awk '{print $2}'; }
# A package's version in a deployment that isn't booted, from that deployment's own pacman database.
deployed() {
  local db; db=$(mktemp -d)
  ln -s "/archix/deploy/$1/pacman" "$db/local"
  pacman --dbpath "$db" -Q "$2" 2>/dev/null | awk '{print $2}'
  rm -rf "$db"
}
# The record format a generation's stored record names; 0 when it names none.
format_of() { grep -o '"format": [0-9]*' "$records/$1/generation.json" 2>/dev/null | awk '{print $2}' | grep . || echo 0; }
record_sum() { sha256sum "$records/$1/generation.json" | cut -c1-16; }
boot_check_ran() {
  [ "$(systemctl show -p Result --value archix-boot.service)" = success ] || fail "archix-boot.service should have run cleanly: $(journalctl -b -u archix-boot.service | tail -3 | tr '\n' '|')"
}
# Reads everything the running archix can: generations and status must not fail on records another version wrote.
reads_records() {
  ax generations > "$state/generations.out" 2>&1 || fail "archix $(version) listing generations: $(tail -5 "$state/generations.out" | tr '\n' '|')"
  ax status > "$state/status.out" 2>&1 || fail "archix $(version) status: $(tail -5 "$state/status.out" | tr '\n' '|')"
  grep -q "warning: skipping" "$state/generations.out" && fail "archix $(version) skipped a record: $(grep skipping "$state/generations.out" | tr '\n' '|')"
  say "archix $(version) status: $(head -4 "$state/status.out" | tr '\n' '|')"
}
nap() { read -rt 1 <> <(:) || :; }
kill_comm() { local p c; for p in /proc/[0-9]*; do read -r c < "$p/comm" 2>/dev/null && [ "$c" = "$1" ] && kill -9 "${p#/proc/}" 2>/dev/null; done; }
say "stage $stage: booted $booted, mounted $(cat /run/archix/deployment), current $(current), deployments $(deployments), archix $(version)"
[ "$booted" = "$(cat /run/archix/deployment)" ] || fail "the initramfs mounted deployment $(cat /run/archix/deployment), but systemd-boot started archix-$booted"

case $stage in
  1)
    [ "$(version)" = 1-1 ] || fail "the image should run archix 1, built from $(cat /srv/archix-next/old-commit)"
    remember old "$(current)"
    say "archix 1 is $(cat /srv/archix-next/old-commit); its deployment is $(recall old)"
    cp /srv/archix-next/archix-2-1-x86_64.pkg.tar.zst "$repo/" && repo-add -q "$repo/archix-self.db.tar.gz" "$repo/archix-2-1-x86_64.pkg.tar.zst" || fail "publishing archix 2"
    ax update --to 2026-10-10 --noconfirm > /dev/console 2>&1 || fail "locking archix 2"
    grep -q '^archix = .*version = "2-1"' "$lock" || fail "the lock should hold archix 2-1: $(grep '^archix' "$lock")"
    kept=$(deployments)
    start=$(wc -l < /var/log/pacman.log)
    setsid archix --host archix-vm switch --noconfirm > "$state/killed.log" 2>&1 < /dev/null &
    pid=$!
    for _ in $(seq 1800); do
      tail -n +"$((start + 1))" /var/log/pacman.log | grep -q '\[ALPM\] upgraded archix (1-1 -> 2-1)' && break
      kill -0 "$pid" 2> /dev/null || fail "the switch finished before pacman upgraded archix: $(tail -5 "$state/killed.log" | tr '\n' '|')"
      nap
    done
    kill -9 "$pid"; kill_comm archix; kill_comm pacman
    nap
    say "killed the switch once pacman had upgraded archix in the staging root: $(tail -2 "$state/killed.log" | tr '\n' '|')"
    [ "$(current)" = "$(recall old)" ] && [ "$(deployments)" = "$kept" ] || fail "the killed switch committed: deployments before $kept, now $(deployments), current $(current)"
    [ "$(version)" = 1-1 ] || fail "the killed switch changed the running archix"
    ax switch --noconfirm > /dev/console 2>&1 || fail "archix 1 switching to archix 2 after the kill"
    remember new "$(current)"
    ls -d /archix/deploy/.staging-* > /dev/null 2>&1 && fail "the switch should clear the killed one's staging tree"
    [ "$(deployed "$(recall new)" archix)" = 2-1 ] || fail "deployment $(recall new) should have archix 2-1"
    cmp -s "/archix/deploy/$(recall new)/usr/lib/archix/archix-static" "/archix/deploy/$(recall old)/usr/lib/archix/archix-static" && fail "the new deployment's initramfs binary should be archix 2's"
    remember old-record "$(record_sum "$(recall old)")"
    say "PASS: archix 1 switched to archix 2 (deployment $(recall new)) after a switch killed halfway, which left nothing behind"
    next 2
    systemctl reboot
    ;;
  2)
    new=$(recall new) old=$(recall old)
    [ "$booted" = "$new" ] || fail "deployment $new should boot by default"
    [ "$(version)" = 2-1 ] || fail "deployment $new should run archix 2"
    boot_check_ran
    reads_records
    say "records: generation $old has format $(format_of "$old"), generation $new (written by archix 1's switch) format $(format_of "$new")"
    say "PASS: the new deployment boots archix 2, which reads the generations and state archix 1 wrote"
    sed -i 's|"archix" }, -- packages|"archix", "tree" }, -- packages|' "$config"
    ax switch --noconfirm > /dev/console 2>&1 || fail "archix 2 switching"
    newer=$(current)
    remember newer "$newer"
    [ "$(format_of "$newer")" = 1 ] || fail "generation $newer, written by archix 2, should name record format 1: $(head -c 300 "$records/$newer/generation.json")"
    [ "$(deployments)" = "$old $new $newer " ] || fail "keep = 3 should keep deployments $old $new $newer: $(deployments)"
    remember newer-record "$(record_sum "$newer")"
    bootctl set-oneshot "archix-$old.conf" || fail "bootctl set-oneshot"
    next 3
    systemctl reboot
    ;;
  3)
    old=$(recall old) new=$(recall new) newer=$(recall newer)
    [ "$booted" = "$old" ] || fail "should have booted deployment $old's entry"
    [ "$(version)" = 1-1 ] || fail "deployment $old should run its own archix 1"
    [ "$(current)" = "$old" ] || fail "booting deployment $old's entry should make it current"
    boot_check_ran
    reads_records
    [ "$(record_sum "$newer")" = "$(recall newer-record)" ] || fail "archix 1 rewrote generation $newer's record"
    say "PASS: the older deployment's entry boots archix 1, which reads generation $newer's format $(format_of "$newer") record and leaves it as it was"
    # Only two deployments, so the switch drops both of archix 2's.
    sed -i 's|deployments = { keep = 3 }|deployments = { keep = 2 }|; s|"archix" }, -- packages|"archix", "jq" }, -- packages|' "$config"
    ax switch --noconfirm > /dev/console 2>&1 || fail "archix 1 switching to a deployment with jq"
    remember jq "$(current)"
    [ ! -d "/archix/deploy/$newer" ] && [ ! -d "/archix/deploy/$new" ] || fail "keep = 2 should drop deployments $new and $newer: $(deployments)"
    ax rollback "$newer" --noconfirm > /dev/console 2>&1 || fail "archix 1 rebuilding generation $newer, which archix 2 recorded"
    [ "$(current)" = "$newer" ] || fail "the rebuilt deployment $newer should be current"
    [ "$(deployed "$newer" archix)" = 2-1 ] && [ -n "$(deployed "$newer" tree)" ] || fail "the rebuilt deployment $newer should have archix 2 and tree"
    [ "$(record_sum "$newer")" = "$(recall newer-record)" ] || fail "rebuilding generation $newer changed its stored record"
    say "deployment $newer's own copy of its record, written by archix 1's rebuild, has format $(grep -o '"format": [0-9]*' "/archix/deploy/$newer/generation.json" || echo none)"
    say "PASS: archix 1 rebuilt generation $newer, recorded by archix 2, with archix 2 in it, and left the stored record as it was"
    next 4
    systemctl reboot
    ;;
  4)
    newer=$(recall newer) new=$(recall new)
    [ "$booted" = "$newer" ] || fail "the rebuilt deployment $newer should boot"
    [ "$(version)" = 2-1 ] || fail "deployment $newer should run archix 2"
    boot_check_ran
    reads_records
    [ "$(format_of "$newer")" = 1 ] || fail "generation $newer's stored record should still name format 1"
    command -v tree > /dev/null || fail "the rebuilt deployment should have tree"
    say "PASS: the deployment archix 1 rebuilt boots archix 2, which reads every record"
    [ -d "/archix/deploy/$new" ] && fail "deployment $new should have been dropped"
    ax rollback "$new" --noconfirm > /dev/console 2>&1 || fail "archix 2 rebuilding generation $new, which archix 1 recorded"
    [ "$(current)" = "$new" ] || fail "the rebuilt deployment $new should be current"
    [ "$(deployed "$new" archix)" = 2-1 ] && [ -z "$(deployed "$new" tree)" ] || fail "the rebuilt deployment $new should have archix 2 and no tree"
    say "PASS: archix 2 rebuilt generation $new from archix 1's record"
    next 5
    systemctl reboot
    ;;
  5)
    new=$(recall new)
    [ "$booted" = "$new" ] && [ "$(version)" = 2-1 ] || fail "the rebuilt deployment $new should boot archix 2"
    boot_check_ran
    reads_records
    command -v tree > /dev/null && fail "deployment $new has no tree"
    say "PASS: the deployment archix 2 rebuilt from archix 1's record boots"
    next done
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
