#!/bin/bash
# A year of weekly switches, each with a package bump and a new UKI, and a reboot: the ESP, generation records and archix's state stay bounded, dbx is untouched while revocation is off and stops at archix's 128-entry cap once on, and every invariant holds every week.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
week=$(cat "$state/stage" 2>/dev/null || echo 1)
weeks=52
config=/etc/archix/init.lua
dbx=/sys/firmware/efi/efivars/dbx-d719b2cb-3d3a-4596-a3bc-dad00e67656f
kek=/var/lib/sbctl/keys/KEK
ax() { archix --host archix-vm "$@"; }
say() { echo "VMTEST: $*" > /dev/console; }
diag() {
  say "diagnostics: /archix/deploy holds $(ls -A /archix/deploy | tr '\n' ' '), entries $(ls /efi/loader/entries | tr '\n' ' '), UKIs $(ls /efi/EFI/archix | tr '\n' ' ')"
  say "diagnostics: generations $(ax generations 2>&1 | tail -6 | tr '\n' '|')"
}
fail() { say "FAIL: week $week: $*"; diag; echo failed > "$state/stage"; sync; systemctl poweroff; exit 1; }
current() { ax generations | awk '$1 == "*" { print $2 }'; }
deployments() { ls /archix/deploy | grep -E '^[0-9]+$' | sort -n | tr '\n' ' '; }
remember() { echo "$2" > "$state/$1"; }
recall() { cat "$state/$1" 2> /dev/null; }
# systemd-boot names the entry it started in LoaderEntrySelected: 4 attribute bytes, then archix-N.conf in UTF-16.
booted=$(tr -d '\000' < /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
# archix's own entries in dbx, by the owner GUID it writes them under.
ours_in_dbx() { [ -e "$dbx" ] || { echo 0; return; }; od -An -tx1 -v "$dbx" | tr -d ' \n' | grep -o c6825be8dae8eb40b968e918df35dec1 | wc -l; }
le32() { printf "\\x$(printf %02x $(($1 & 255)))\\x$(printf %02x $((($1 >> 8) & 255)))\\x$(printf %02x $((($1 >> 16) & 255)))\\x$(printf %02x $((($1 >> 24) & 255)))"; }
# Appends $1 made-up SHA-256 entries under archix's owner GUID to dbx, signed with sbctl's KEK, as if archix had revoked that many images before.
seed_dbx() {
  local n=$1 i
  {
    printf '\x26\x16\xc4\xc1\x4c\x50\x92\x40\xac\xa9\x41\xf9\x36\x93\x43\x28'
    le32 $((28 + 48 * n)); le32 0; le32 48
    for i in $(seq "$n"); do printf '\xc6\x82\x5b\xe8\xda\xe8\xeb\x40\xb9\x68\xe9\x18\xdf\x35\xde\xc1'; head -c 32 /dev/urandom; done
  } > /var/tmp/seed.esl
  sign-efi-sig-list -a -k "$kek/KEK.key" -c "$kek/KEK.pem" dbx /var/tmp/seed.esl /var/tmp/seed.auth > /dev/null || fail "signing the seeded dbx entries"
  chattr -i "$dbx" 2> /dev/null
  efi-updatevar -a -f /var/tmp/seed.auth dbx || fail "appending the seeded entries to dbx"
}
complete() { local t; for t in usr etc opt boot pacman dkms; do [ -d "/archix/deploy/$1/$t" ] || return 1; done; }
findings() { /opt/archix-vmtest/bin/archix-check-deployment "$1" | grep -E ': /(usr|opt|boot)/' || true; }
max() { [ "$1" -gt "$2" ] && echo "$1" || echo "$2"; }
# Everything that must hold every week.
invariants() {
  local n entry uki ukis entries esp_mb biggest
  [ "$booted" = "$(current)" ] || fail "the new deployment $(current) should boot, not $booted"
  complete "$(current)" || fail "current names deployment $(current), which isn't complete"
  ls -d /archix/deploy/.staging-* > /dev/null 2>&1 && fail "a staging tree is left: $(ls -A /archix/deploy | tr '\n' ' ')"
  # The rescue entry names no deployment; it has to stay whole with exactly one UKI however many switches refresh it.
  rescue=$(sed -nE 's#^(efi|uki) /#/efi/#p' /efi/loader/entries/archix-rescue.conf 2> /dev/null)
  [ -n "$rescue" ] && [ -f "$rescue" ] || fail "the rescue entry or its UKI is missing: $(ls /efi/loader/entries)"
  [ "$(ls /efi/EFI/archix-rescue | wc -l)" = 1 ] || fail "EFI/archix-rescue should hold one UKI: $(ls /efi/EFI/archix-rescue)"
  for entry in /efi/loader/entries/archix-[0-9]*.conf; do
    n=$(basename "$entry" | grep -o '^archix-[0-9]*' | cut -d- -f2)
    complete "$n" || fail "$(basename "$entry") names deployment $n, which isn't a complete committed deployment"
    uki=$(sed -nE 's#^(efi|uki) /#/efi/#p' "$entry")
    [ -f "$uki" ] || fail "$(basename "$entry") starts $uki, which is missing"
  done
  for uki in /efi/EFI/archix/*.efi; do
    grep -qr "/EFI/archix/$(basename "$uki")" /efi/loader/entries || fail "$uki is on the ESP but no entry starts it"
  done
  for n in $(deployments); do
    [ -f "/var/lib/archix/boot/entries/archix-$n.conf" ] || fail "deployment $n has no recorded entry"
  done
  ukis=$(ls /efi/EFI/archix | wc -l) entries=$(ls /efi/loader/entries | grep -c '^archix-')
  [ "$ukis" -le "$entries" ] || fail "$ukis UKIs for $entries entries"
  [ "$entries" -le 3 ] || fail "deployments.keep = 2 should leave at most 3 entries, not $entries"
  esp_mb=$(du -sm /efi | cut -f1)
  biggest=$(( $(ls -l /efi/EFI/archix | awk 'NR > 1 { print $5 }' | sort -n | tail -1) / 1048576 + 1 ))
  [ "$(ls /efi/EFI/Linux | grep -c '^archix-fallback-')" = 1 ] || fail "the ESP should hold one fallback UKI: $(ls /efi/EFI/Linux | tr '\n' ' ')"
  [ -f "/efi/EFI/Linux/archix-fallback-$(current).efi" ] || fail "the fallback UKI should boot the current deployment $(current): $(ls /efi/EFI/Linux | tr '\n' ' ')"
  # Three entries' UKIs and the fallback's copy of one.
  [ "$esp_mb" -le $((4 * biggest + 8)) ] || fail "the ESP holds $esp_mb MiB, more than four UKIs of $biggest MiB"
  remember esp-max "$(max "$esp_mb" "$(recall esp-max || echo 0)")"
  [ "$(ours_in_dbx)" -le 128 ] || fail "archix has $(ours_in_dbx) entries in dbx, more than its cap of 128"
  if [ $((week % 4)) = 0 ] || [ "$week" -gt "$weeks" ]; then
    for n in $(deployments); do
      if [ -f "$state/baseline-$n" ]; then
        findings "$n" | diff "$state/baseline-$n" - > /dev/null || fail "deployment $n's files changed since it was committed: $(findings "$n" | head -c 400)"
      else
        findings "$n" > "$state/baseline-$n"
      fi
    done
  fi
}
records() { ls /var/lib/archix/generations | grep -cE '^[0-9]+$'; }
say "week $week: booted $booted, current $(current), deployments $(deployments), records $(records), ESP $(du -sm /efi | cut -f1) MiB, /var/lib/archix $(du -sk /var/lib/archix | cut -f1) KiB, archix entries in dbx $(ours_in_dbx), variable store $(stat -f -c '%f' /sys/firmware/efi/efivars) bytes free"
[ "$week" = failed ] && { systemctl poweroff; exit 0; }
[ "$week" = done ] && { systemctl poweroff; exit 0; }
invariants
for n in $(deployments); do [ -f "$state/baseline-$n" ] || findings "$n" > "$state/baseline-$n"; done

# Records: kept by count and age; every one is younger than keep_days = 30 until week 14 sets it to 0.
if [ "$week" -le 14 ]; then
  [ "$(records)" = "$(current)" ] || fail "with keep_days = 30, every record of the $(current) generations should stay, as documented: $(records)"
else
  [ "$(records)" -le 10 ] || fail "with generations.keep = 10 and keep_days = 0, at most 10 records should stay, not $(records)"
fi
if [ "$week" -lt 40 ]; then
  [ "$(ours_in_dbx)" = 0 ] || fail "with revoke_unused off, archix should never write dbx: $(ours_in_dbx) entries"
fi
if [ "$week" -gt "$weeks" ]; then
  kib=$(du -sk /var/lib/archix | cut -f1)
  say "after $weeks weeks: ESP at most $(recall esp-max) MiB, $(records) records, /var/lib/archix $kib KiB ($(wc -l < /var/lib/archix/boot/signed) signed images recorded, $(ls /var/lib/archix/boot | grep -c '^dbx-before') dbx backups), archix entries in dbx $(ours_in_dbx), cap reached in week $(recall capped)"
  [ "$kib" -lt 65536 ] || fail "/var/lib/archix grew to $kib KiB"
  [ -n "$(recall capped)" ] || fail "the dbx cap should have been reached"
  [ "$(ours_in_dbx)" = 128 ] || fail "dbx should hold exactly archix's 128 entries at the cap, not $(ours_in_dbx)"
  say "PASS: $weeks weekly switches kept the ESP, records and archix's state bounded, every invariant held each week, and dbx stopped at the cap"
  echo done > "$state/stage"; sync
  say "OK"
  systemctl poweroff
  exit 0
fi

case $week in
  14)
    sed -i 's|generations = { keep = 10, keep_days = 30 }|generations = { keep = 10, keep_days = 0 }|' "$config"
    say "from now on generations.keep_days = 0"
    ;;
  27)
    archix secure-boot setup > /dev/console 2>&1 || fail "archix secure-boot setup"
    sed -i 's|boot = { loader = "systemd-boot" }|boot = { loader = "systemd-boot", secure_boot = true }|' "$config"
    say "from now on UKIs are signed and Secure Boot enforces, with revocation off"
    ;;
  40)
    bootctl status 2> /dev/null | grep -q "Secure Boot: enabled (user)" || fail "Secure Boot should be enforcing"
    pending=$(archix secure-boot revoke --dry-run 2>&1 | sed -n 's/^\([0-9]*\) boot images\? archix signed.*/\1/p')
    pending=${pending:-0}
    [ "$pending" -gt 0 ] || fail "twelve weeks of signed UKIs should have left some unused but unrevoked"
    # Two weeks to the cap: OVMF's variable store keeps every superseded copy of dbx, so a longer approach runs into archix's 64 KiB free-space margin first.
    seed_dbx $((128 - pending - 2))
    say "$pending signed images wait for revocation; seeded dbx with $((128 - pending - 2)) of archix's entries, so the cap comes in two weeks"
    sed -i 's|secure_boot = true }|secure_boot = true, revoke_unused = true }|' "$config"
    ;;
esac
cp "/srv/churn-pkgs/archix-churn-$((week + 1))-1-any.pkg.tar.zst" /srv/churn-repo/ \
  && repo-add -q /srv/churn-repo/churn.db.tar.gz "/srv/churn-repo/archix-churn-$((week + 1))-1-any.pkg.tar.zst" || fail "publishing archix-churn $((week + 1))"
sed -i "s|\"archix.week=[0-9]*\"|\"archix.week=$week\"|" "$config"
ax update --to 2026-10-10 --noconfirm > "$state/update.log" 2>&1 || fail "locking archix-churn $((week + 1)): $(tail -5 "$state/update.log" | tr '\n' '|')"
before=$(current)
ax switch --noconfirm > "$state/switch.log" 2>&1
status=$?
[ "$(current)" -gt "$before" ] || fail "the week's switch should commit a deployment (exit $status): $(tail -8 "$state/switch.log" | tr '\n' '|')"
if grep -q "more than the 128 it allows itself" "$state/switch.log"; then
  [ -n "$(recall capped)" ] || remember capped "$week"
  say "week $week: revocation refused at the cap, the switch committed anyway (exit $status): $(grep -o 'that would make [0-9]* of archix.s entries' "$state/switch.log")"
elif [ "$status" != 0 ]; then
  fail "the week's switch failed: $(tail -8 "$state/switch.log" | tr '\n' '|')"
elif [ "$week" -ge 40 ]; then
  grep -q "^revoked " "$state/switch.log" || fail "with revoke_unused on, the switch should revoke the UKI it dropped: $(tail -8 "$state/switch.log" | tr '\n' '|')"
fi
[ "$week" -lt 40 ] || [ -n "$(recall capped)" ] || [ "$week" -lt 42 ] || fail "the cap should have been reached by week 42"
echo $((week + 1)) > "$state/stage"; sync
systemctl reboot
