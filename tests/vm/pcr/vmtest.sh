#!/bin/bash
# Signed PCR 11 policies on a swtpm TPM: a LUKS2 volume enrolled with systemd's PCR public key unlocks with no passphrase under every UKI archix signs with that key, an older kept deployment's included, and after `archix secure-boot rotate-pcr-key` only under UKIs built with the new key.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
stage=$(cat "$state/stage" 2>/dev/null || echo 1)
config=/etc/archix/init.lua
volume=/dev/vdb
pass=archix-vmtest-passphrase
public=/etc/systemd/tpm2-pcr-public-key.pem
private=/etc/systemd/tpm2-pcr-private-key.pem
ax() { archix --host archix-vm "$@"; }
say() { echo "VMTEST: $*" > /dev/console; }
diag() {
  say "diagnostics: cryptsetup unit $(journalctl -b -u systemd-cryptsetup@data.service --no-pager 2>&1 | tail -8 | tr '\n' '|')"
  say "diagnostics: /run/systemd $(ls /run/systemd | grep -i tpm2 | tr '\n' ' '), PCR 11 $(tpm2_pcrread sha256:11 2>&1 | tail -1)"
  say "diagnostics: tokens $(cryptsetup luksDump "$volume" 2>&1 | sed -n '/^Tokens:/,/^Digests:/p' | tr '\n' '|')"
}
fail() { say "FAIL: $*"; diag; echo failed > "$state/stage"; sync; systemctl poweroff; exit 1; }
current() { ax generations | awk '$1 == "*" { print $2 }'; }
deployments() { ls /archix/deploy | grep -E '^[0-9]+$' | sort -n | tr '\n' ' '; }
remember() { echo "$2" > "$state/$1"; }
recall() { cat "$state/$1"; }
next() { echo "$1" > "$state/stage"; sync; }
# systemd-boot names the entry it started in LoaderEntrySelected: 4 attribute bytes, then archix-N.conf in UTF-16.
booted=$(tr -d '\000' < /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
# The UKI deployment $1's normal entry starts.
uki_of() {
  local entry; entry=$(ls /efi/loader/entries/archix-"$1".conf /efi/loader/entries/archix-"$1"+*.conf 2> /dev/null | head -1)
  sed -nE 's#^(efi|uki) /#/efi/#p' "$entry"
}
# The public key a UKI's signed policy names, from its .pcrpkey section.
uki_key() { objcopy -O binary --only-section=.pcrpkey "$(uki_of "$1")" /run/vmtest-pcrpkey 2> /dev/null && sha256sum < /run/vmtest-pcrpkey | cut -c1-16; }
key_sum() { sha256sum < "$1" | cut -c1-16; }
unlocked() { systemctl is-active -q systemd-cryptsetup@data.service && [ -b /dev/mapper/data ]; }
# headless=true in crypttab: systemd-cryptsetup never asks for a passphrase, so an open volume was opened by the TPM.
assert_unlocked() {
  unlocked || fail "$1: the volume should unlock through the TPM with no passphrase"
  mkdir -p /run/vmtest-data && mount -o ro /dev/mapper/data /run/vmtest-data || fail "$1: mounting the unlocked volume"
  [ "$(cat /run/vmtest-data/marker)" = "$(recall marker)" ] || fail "$1: the volume's marker isn't what stage 1 wrote"
  umount /run/vmtest-data
}
assert_locked() {
  unlocked && fail "$1: the volume should not unlock through the TPM"
  printf %s "$pass" | cryptsetup open "$volume" data-pass - || fail "$1: the passphrase should still open the volume"
  cryptsetup close data-pass
}
tpm2_tokens() { cryptsetup luksDump "$volume" | grep -c 'systemd-tpm2'; }
for _ in $(seq 60); do systemctl is-active -q systemd-cryptsetup@data.service && break; systemctl is-failed -q systemd-cryptsetup@data.service && break; [ -e /etc/crypttab ] || break; sleep 1; done
say "stage $stage: booted $booted, mounted $(cat /run/archix/deployment), current $(current), deployments $(deployments), volume $(systemctl is-active systemd-cryptsetup@data.service 2> /dev/null)"
[ "$booted" = "$(cat /run/archix/deployment)" ] || fail "the initramfs mounted deployment $(cat /run/archix/deployment), but systemd-boot started archix-$booted"

case $stage in
  1)
    [ -c /dev/tpmrm0 ] || fail "the VM should have a TPM"
    ukify genkey --pcr-private-key="$private" --pcr-public-key="$public" > /dev/console 2>&1 || fail "ukify genkey"
    printf %s "$pass" | cryptsetup luksFormat --type luks2 --batch-mode "$volume" - || fail "luksFormat"
    printf %s "$pass" | cryptsetup open "$volume" data-setup - || fail "opening the new volume"
    mkfs.ext4 -q /dev/mapper/data-setup && mkdir -p /run/vmtest-data && mount /dev/mapper/data-setup /run/vmtest-data || fail "making a filesystem on the volume"
    head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n' > /run/vmtest-data/marker
    cp /run/vmtest-data/marker "$state/marker"
    umount /run/vmtest-data && cryptsetup close data-setup
    PASSWORD=$pass systemd-cryptenroll --tpm2-device=auto --tpm2-public-key="$public" --tpm2-public-key-pcrs=11 "$volume" > /dev/console 2>&1 || fail "systemd-cryptenroll"
    [ "$(tpm2_tokens)" = 1 ] || fail "the volume should have one TPM2 token"
    sed -i 's|    -- \["/etc/crypttab"\]|    ["/etc/crypttab"]|' "$config"
    ax switch --noconfirm > /dev/console 2>&1 || fail "the switch with the PCR key in place"
    remember first "$(current)"
    [ "$(uki_key "$(recall first)")" = "$(key_sum "$public")" ] || fail "deployment $(recall first)'s UKI should carry the PCR public key"
    ukify inspect "$(uki_of "$(recall first)")" | grep -q pcrsig || fail "deployment $(recall first)'s UKI should carry a signed PCR policy"
    say "PASS: with systemd's PCR key in place the switch built a UKI with a signed policy for it, and the volume is enrolled against the key"
    next 2
    systemctl reboot
    ;;
  2)
    [ "$booted" = "$(recall first)" ] || fail "deployment $(recall first) should boot"
    assert_unlocked "deployment $(recall first)"
    say "PASS: deployment $(recall first) unlocked the volume through the TPM with no passphrase"
    sed -i 's|"console=ttyS0" }|"console=ttyS0", "archix.pcrtest" }|' "$config"
    ax switch --noconfirm > /dev/console 2>&1 || fail "the switch changing the command line"
    remember second "$(current)"
    [ "$(uki_of "$(recall second)")" != "$(uki_of "$(recall first)")" ] || fail "a new command line should make a new UKI"
    next 3
    systemctl reboot
    ;;
  3)
    [ "$booted" = "$(recall second)" ] || fail "deployment $(recall second) should boot"
    assert_unlocked "deployment $(recall second), a new UKI measuring differently"
    say "PASS: a switch that changed the UKI still unlocks the volume, with nothing re-sealed"
    bootctl set-oneshot "archix-$(recall first).conf" || fail "bootctl set-oneshot"
    next 4
    systemctl reboot
    ;;
  4)
    first=$(recall first)
    [ "$booted" = "$first" ] || fail "should have booted deployment $first's entry"
    if unlocked; then
      say "OBSERVED: the older kept deployment $first's UKI, signed with the same key, still unlocks the volume"
    else
      say "OBSERVED: the older kept deployment $first's UKI does not unlock the volume"
    fi
    assert_unlocked "the older deployment $first, signed with the same key"
    old_key=$(key_sum "$public")
    remember old-key "$old_key"
    PASSWORD=$pass ax secure-boot rotate-pcr-key "$volume" > /dev/console 2>&1 || fail "archix secure-boot rotate-pcr-key"
    [ "$(key_sum "$public")" != "$old_key" ] || fail "the PCR public key should be new"
    for n in $(deployments); do
      [ "$(key_sum "/archix/deploy/$n/etc/systemd/tpm2-pcr-public-key.pem")" = "$(key_sum "$public")" ] || fail "deployment $n should have the new key"
    done
    [ "$(tpm2_tokens)" = 1 ] || fail "re-enrolling should leave one TPM2 token: $(cryptsetup luksDump "$volume" | grep -A2 Tokens: | tr '\n' '|')"
    ax switch --noconfirm > "$state/rotated.log" 2>&1 || fail "the switch after rotating: $(tail -5 "$state/rotated.log" | tr '\n' '|')"
    grep -q "update     the PCR policy key" "$state/rotated.log" || fail "the switch should name the new key: $(tr '\n' '|' < "$state/rotated.log")"
    remember rotated "$(current)"
    [ "$(recall rotated)" != "$first" ] || fail "a new key should make a new deployment"
    [ "$(uki_key "$(recall rotated)")" = "$(key_sum "$public")" ] || fail "the new UKI should carry the new key"
    say "PASS: rotate-pcr-key put a new key in every deployment and re-enrolled the volume, and the next switch built a UKI for it"
    next 5
    systemctl reboot
    ;;
  5)
    [ "$booted" = "$(recall rotated)" ] || fail "deployment $(recall rotated) should boot"
    assert_unlocked "deployment $(recall rotated), built with the new key"
    say "PASS: the UKI built with the new key unlocks the volume"
    bootctl set-oneshot "archix-$(recall second).conf" || fail "bootctl set-oneshot"
    next 6
    systemctl reboot
    ;;
  6)
    [ "$booted" = "$(recall second)" ] || fail "should have booted deployment $(recall second)'s entry"
    assert_locked "deployment $(recall second), built with the old key"
    say "PASS: deployment $(recall second)'s UKI, built with the old key, no longer unlocks the volume; the passphrase still does"
    bootctl set-oneshot "archix-$(recall first).conf" || fail "bootctl set-oneshot"
    next 7
    systemctl reboot
    ;;
  7)
    [ "$booted" = "$(recall first)" ] || fail "should have booted deployment $(recall first)'s entry"
    assert_locked "deployment $(recall first), built with the old key"
    say "PASS: deployment $(recall first)'s UKI, built with the old key, no longer unlocks the volume either"
    next done
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
