#!/bin/bash
# The rescue suite's guest side: every deployment broken at once, then a lost root password, each repaired from the rescue entry through `archix rescue` alone, which tests/vm/rescue/drive.py types at its login and prompt; then under Secure Boot with root locked, wrong and empty passwords get no prompt or shell, a wheel user's does, and a LUKS volume a TPM opens under this Secure Boot state asks the rescue for its passphrase.
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
  [ "$(grep -E '^archix-([0-9]+[.+-]|rescue)' <<< "$ids" | tail -1)" = archix-rescue.conf ] || fail "the rescue entry should sort after every deployment's: $(tr '\n' ' ' <<< "$ids")"
  [ "$(bootctl list --json=short | jq -r '.[] | select(.isDefault) | .id')" != archix-rescue.conf ] || fail "the rescue entry must never be the default"
  grep -q rescue /efi/loader/loader.conf && fail "loader.conf should never name the rescue entry: $(tr '\n' '|' < /efi/loader/loader.conf)"
  return 0
}
rescue_source() { jq -r .source /var/lib/archix/boot/rescue.json; }
systemd_ok() { head -c 4 "/archix/deploy/$1/usr/lib/systemd/systemd" | grep -q ELF; }
root_hash() { grep '^root:' "/archix/deploy/$1/etc/shadow" | cut -d: -f2; }
# Runs a shell command with /archix writable, which the boot binds read-only.
in_store() { unshare -m sh -c "mount -o remount,bind,rw /archix && $1"; }
root_pass='rescue root pass' admin_pass='rescue admin pass' luks_pass=archix-vmtest-passphrase
# The script steps that log in to the rescue as user $1 with password $2.
login() { printf '%s\t%s\n' 'rescue login (root or a wheel user): ' "$1" 'password: ' "$2" "logged in as $1" ''; }
volume=/dev/vdb
cryptsetup_bin=$(command -v systemd-cryptsetup || echo /usr/lib/systemd/systemd-cryptsetup)

say "stage $stage: entry $selected, deployment $booted, current $(current), deployments $(deployments), rescue from $(rescue_source)"
case $stage in
  1)
    [ "$booted" = "$(current)" ] || fail "the current deployment should boot first"
    remember older "$booted"
    check_menu
    first=$(deployments | cut -d' ' -f1)
    [ "$(rescue_source)" = "$first" ] || fail "the image's switch should build the rescue entry from deployment $first, the system it laid out and ran on, as nothing had booted from an entry"
    ls /var/cache/pacman/pkg | grep -qE '^systemd-[0-9]' || fail "the image should keep systemd's package, which the repair restores from"
    # The rescue opens only for root or a wheel user with a password.
    useradd -m -G wheel admin || fail "useradd admin"
    printf 'root:%s\nadmin:%s\n' "$root_pass" "$admin_pass" | chpasswd || fail "chpasswd"
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
    grep -qE '^admin:\$y\$' /etc/shadow || fail "the switch should have carried admin's password into deployment $booted"
    # Every deployment's init replaced by a new file with garbage in it, so none of them can boot.
    for n in $(deployments); do
      in_store "printf 'broken\n' > /archix/deploy/$n/usr/lib/systemd/systemd.new && mv /archix/deploy/$n/usr/lib/systemd/systemd.new /archix/deploy/$n/usr/lib/systemd/systemd" || fail "breaking deployment $n"
      systemd_ok "$n" && fail "deployment $n's systemd should be broken"
    done
    sync
    say "PASS: broke /usr/lib/systemd/systemd in deployments $(deployments)"
    {
      login root "$root_pass"
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
    # And the newer deployment's boot entry gone, which `use` builds again through its own `archix rollback` in a chroot.
    rm -f "/efi/loader/entries/archix-$(recall newer).conf" "/efi/loader/entries/archix-$(recall newer)+"*.conf
    sync
    {
      login admin "$admin_pass"
      printf '%s\t%s\n' 'rescue> ' 'password root --all' 'new password for root: ' 'rescued pass' 'again: ' 'rescued pass'
      for n in $(deployments); do printf '%s\t\n' "set root's password in deployment $n"; done
      printf '%s\t%s\n' 'rescue> ' "use $(recall newer)" "deployment $(recall newer) has no boot entry; building it" '' \
        "deployment $(recall newer) is current and the boot default with a new entry" '' 'rescue> ' 'reboot'
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
    ls /efi/loader/entries | grep -qE "^archix-$n[.+]" || fail "the rescue should have built deployment $n's entry again: $(ls /efi/loader/entries | tr '\n' ' ')"
    check_menu
    say "PASS: with root's password lost, admin of wheel logged in to the rescue, which reset root's password in every deployment, and deployment $n booted normally"
    archix secure-boot setup > /dev/console 2>&1 || fail "archix secure-boot setup"
    sed -i 's|boot = { loader = "systemd-boot" }|boot = { loader = "systemd-boot", secure_boot = true }|' "$config"
    ax switch --noconfirm > "$state/switch-sb.log" 2>&1 || fail "the switch with Secure Boot: $(tail -8 "$state/switch-sb.log" | tr '\n' '|')"
    remember signed "$(current)"
    next 5
    systemctl reboot
    ;;
  5)
    [ "$booted" = "$(recall signed)" ] || fail "deployment $(recall signed) should boot"
    bootctl status 2> /dev/null | grep -q "Secure Boot: enabled (user)" || fail "Secure Boot should be enforcing"
    check_menu
    uki=$(sed -n 's|^efi /|/efi/|p' /efi/loader/entries/archix-rescue.conf)
    sbctl verify "$uki" 2>&1 | grep -q "is signed" || fail "the rescue UKI should be signed: $(sbctl verify "$uki" 2>&1)"
    inspected=$(ukify inspect "$uki")
    grep -q 'archix.rescue' <<< "$inspected" || fail "the rescue UKI should carry its command line: $inspected"
    grep -q pcrsig <<< "$inspected" && fail "the rescue UKI must carry no signed PCR policy"
    # A volume a TPM opens under this Secure Boot state alone, PCR 7, which the signed rescue UKI shares.
    printf %s "$luks_pass" | cryptsetup luksFormat --type luks2 --batch-mode "$volume" - || fail "luksFormat"
    PASSWORD=$luks_pass systemd-cryptenroll --tpm2-device=auto --tpm2-pcrs=7 "$volume" > /dev/console 2>&1 || fail "systemd-cryptenroll"
    "$cryptsetup_bin" attach vmtest-tpm "$volume" - tpm2-device=auto,headless=true > /dev/console 2>&1 || fail "the TPM alone should open the volume in this boot"
    "$cryptsetup_bin" detach vmtest-tpm || cryptsetup close vmtest-tpm
    passwd -l root > /dev/null || fail "passwd -l root"
    passwd -S root | grep -q ' L ' || fail "root should be locked: $(passwd -S root)"
    sync
    say "PASS: Secure Boot enforces, the rescue UKI is signed with its command line inside and no PCR policy, root is locked, and the TPM alone opens $volume under PCR 7"
    # Nothing at the rescue opens for a wrong or empty password, and three tries end it.
    {
      printf '%s\t\n' '!rescue> ' '![rescue ' '![rootfs '
      printf '%s\t%s\n' 'rescue login (root or a wheel user): ' admin 'password: ' 'not the password' 'wrong user or password' '' \
        'rescue login (root or a wheel user): ' root 'password: ' '<enter>' 'wrong user or password' '' \
        'rescue login (root or a wheel user): ' admin 'password: ' '<enter>' 'wrong user or password' '' \
        '3 wrong logins' '' 'the machine restarts in 15 s' ''
    } | script
    to_rescue 6
    ;;
  6)
    [ "$booted" = "$(recall signed)" ] || fail "after the closed rescue, deployment $(recall signed) should boot, not ${booted:-none}"
    case $selected in archix-rescue*) fail "the rescue entry should have been a one-shot: $selected" ;; esac
    say "PASS: with root locked, a wrong and an empty password got no prompt or shell, and three tries restarted the machine"
    {
      login admin "$admin_pass"
      printf '%s\t%s\n' 'rescue> ' 'shell' '[rescue ' 'echo SHELL-$((40 + 2)); exit' 'SHELL-42' '' \
        'rescue> ' "open $volume vmtest" "$volume is encrypted: its passphrase opens it here" '' \
        "Enter passphrase for $volume" 'not the passphrase' 'No key available with this passphrase' '' \
        "Enter passphrase for $volume" "$luks_pass" "opened $volume as /dev/mapper/vmtest" '' 'rescue> ' 'reboot'
    } | script
    to_rescue 7
    ;;
  7)
    [ "$booted" = "$(recall signed)" ] || fail "deployment $(recall signed) should boot after the rescue"
    passwd -S root | grep -q ' L ' || fail "root should still be locked: $(passwd -S root)"
    say "PASS: admin's password opened the rescue and its shell under Secure Boot, and the TPM-bound volume asked for its passphrase, refused a wrong one and opened with the right one"
    next done
    say "OK"
    systemctl poweroff
    ;;
  *)
    say "nothing to do at stage $stage"
    systemctl poweroff
    ;;
esac
