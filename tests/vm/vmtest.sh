#!/bin/bash
# Runs at every boot of the VM test, walking through its stages and reporting on the console.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
stage=$(cat "$state/stage" 2>/dev/null || echo plain)
say() { echo "VMTEST: $*" > /dev/console; }
fail() { say "FAIL: $*"; systemctl poweroff; exit 1; }
current() { archix generations | awk '$1 == "*" { print $2 }'; }
deployments() { ls /archix/deploy 2>/dev/null | sort -n | tr '\n' ' '; }
listing() { ls -A /archix/deploy | tr '\n' ' '; }
next() { echo "$1" > "$state/stage"; sync; }
# Entries in the root's own /usr, which the deployment's bind hides.
hidden_usr() { mkdir -p /run/vmtest-root && mount --bind / /run/vmtest-root && ls -A /run/vmtest-root/usr | wc -l; umount /run/vmtest-root; }
# What pacman -Qkk finds wrong in deployment $1's /usr, /opt and /boot, the trees a staged deployment shares with it.
findings() { archix-check-deployment "$1" | grep -E ': /(usr|opt|boot)/' || true; }
efivar=/sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f
# systemd-boot names the entry it started in LoaderEntrySelected: 4 attribute bytes, then archix-N.conf in UTF-16.
booted=$(tr -d '\000' < "$efivar" 2>/dev/null | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
running=$(cat /run/archix/deployment 2>/dev/null)
say "stage $stage: booted entry $booted, mounted deployment $running, current $(current), deployments $(deployments)"
[ "$booted" = "$running" ] || fail "the initramfs mounted deployment $running, but systemd-boot started archix-$booted"
config=/etc/archix/init.lua
loader=/efi/loader/loader.conf

case $stage in
  plain)
    [ -e /archix ] && fail "the image should boot as a plain install, not laid out yet"
    systemctl is-active -q archix-protected.service || fail "the ProtectSystem=strict service should run before the first switch"
    pid=$(systemctl show -P MainPID archix-protected.service)
    archix --host archix-vm switch --noconfirm > /dev/console 2>&1 || fail "the first switch on the booted system"
    [ "$(current)" = 2 ] && [ "$(deployments)" = "1 2 " ] || fail "the first switch should lay out deployment 1 and build 2: $(deployments)"
    for _ in 1 2 3 4 5; do sleep 1; done
    systemctl is-active -q archix-protected.service && [ "$(systemctl show -P MainPID archix-protected.service)" = "$pid" ] \
      || fail "the ProtectSystem=strict service lost its /usr during the first layout: $(journalctl -u archix-protected.service -b --no-pager | tail -5)"
    [ "$(hidden_usr)" -gt 0 ] || fail "the first switch pruned the live /usr, which running services may still use"
    findmnt -no SOURCE /usr | grep -q "archix/deploy/1/usr" || fail "the running system should be bound to deployment 1: $(findmnt -no SOURCE /usr)"
    touch /boot/archix-vmtest 2> /dev/null && fail "/boot should be read-only once laid out"
    say "PASS: the first switch on a booted system laid it out with a ProtectSystem=strict service running, and pruned nothing live"
    next 1
    systemctl reboot
    ;;
  1)
    [ "$(current)" = 2 ] || fail "the first switch should make deployment 2 current"
    [ "$(deployments)" = "1 2 " ] || fail "the first switch should lay out deployment 1 and build 2: $(deployments)"
    findmnt -no SOURCE /usr | grep -q "archix/deploy/2/usr" || fail "/usr should be deployment 2's: $(findmnt -no SOURCE /usr)"
    touch /usr/archix-vmtest 2> /dev/null && fail "/usr should be read-only"
    touch /boot/archix-vmtest 2> /dev/null && fail "/boot should be read-only"
    [ "$(hidden_usr)" = 0 ] || fail "the first boot after the layout should have emptied the root's own /usr, which deployments hide"
    systemctl is-active -q archix-protected.service || fail "the ProtectSystem=strict service should run on deployment 2"
    say "PASS: the first switch laid the system out as deployments 1 and 2, 2 boots with a read-only /usr and /boot, and its boot pruned what the binds hide"
    # The first 4 bytes are the EFI attributes; bit 0 is NON_VOLATILE, which would let a later direct firmware boot read a stale entry.
    attrs=$(od -An -tu4 -N4 "$efivar" 2>/dev/null | tr -d ' ')
    [ -n "$attrs" ] && [ $((attrs & 1)) = 0 ] || fail "LoaderEntrySelected should be volatile, but its attributes are ${attrs:-missing}"
    systemctl is-active -q archix-marker.service || fail "deployment 2 should start the marker"
    sed -i '/-- marker/d' "$config"
    archix --host archix-vm install tree --noconfirm > /dev/console 2>&1 || fail "archix install tree"
    [ "$(current)" = 3 ] || fail "installing tree should make deployment 3 current"
    [ -x /archix/deploy/3/usr/bin/tree ] || fail "deployment 3 should have tree"
    [ -e /archix/deploy/2/usr/bin/tree ] && fail "the switch changed deployment 2"
    [ "$(deployments)" = "2 3 " ] || fail "keep = 2 should keep only the booted deployment 2 and the new 3: $(deployments)"
    command -v tree > /dev/null && fail "tree should arrive with the next boot, not now"
    systemctl is-active -q archix-marker.service || fail "the running system should keep the marker until the next boot"
    grep -qx "preferred archix-3.conf" "$loader" && grep -qx "default archix-2.conf" "$loader" \
      || fail "loader.conf should prefer deployment 3 and fall back to 2: $(cat "$loader")"
    say "PASS: a switch built deployment 3 beside the running one, which it left alone"
    next 2
    systemctl reboot
    ;;
  2)
    [ "$booted" = 3 ] && [ "$(current)" = 3 ] || fail "the restart should boot deployment 3"
    command -v tree > /dev/null || fail "deployment 3 should have tree"
    systemctl is-enabled -q archix-marker.service && fail "deployment 3 drops the marker"
    say "PASS: deployment 3 booted with tree and without the marker"
    bootctl set-oneshot archix-2.conf || fail "bootctl set-oneshot archix-2.conf"
    next 3
    systemctl reboot
    ;;
  3)
    [ "$booted" = 2 ] || fail "should have booted deployment 2's entry"
    [ "$(current)" = 2 ] || fail "booting deployment 2's entry should make it current"
    grep -qx "default archix-2.conf" "$loader" && ! grep -q "^preferred" "$loader" || fail "deployment 2 should be the default now: $(cat "$loader")"
    command -v tree > /dev/null && fail "deployment 2 has no tree"
    systemctl is-active -q archix-marker.service || fail "deployment 2 should start the marker"
    say "PASS: booting an older entry made its deployment current, with nothing reinstalled"
    archix rollback 3 --noconfirm > /dev/console 2>&1 || fail "archix rollback 3"
    [ "$(current)" = 3 ] || fail "rolling back to deployment 3 should make it current"
    grep -qx "default archix-3.conf" "$loader" || fail "rolling back to deployment 3 should make it the default: $(cat "$loader")"
    [ "$(deployments)" = "2 3 " ] || fail "a rollback to a kept deployment builds nothing: $(deployments)"
    next 4
    systemctl reboot
    ;;
  4)
    [ "$booted" = 3 ] || fail "the rollback should boot deployment 3"
    command -v tree > /dev/null || fail "deployment 3 should have tree"
    say "PASS: a rollback to a kept deployment repointed it and its boot entry"
    findings 2 > "$state/baseline-2"
    # What a crash between a rollback to 2 writing the boot default and repointing current leaves: loader.conf names 2, current is still 3.
    sed -i -e '/^preferred /d' -e 's/^default .*/default archix-2.conf/' "$loader"
    grep -qx "default archix-2.conf" "$loader" || fail "loader.conf should name deployment 2: $(cat "$loader")"
    [ "$(current)" = 3 ] || fail "current should still be 3"
    next 4b
    sync -f /efi
    say "cutting the power between a rollback's boot default and its current"
    echo 1 > /proc/sys/kernel/sysrq
    echo b > /proc/sysrq-trigger
    ;;
  4b)
    [ "$booted" = 2 ] || fail "after a crash between the rollback's two writes, deployment 2's entry should boot, not $booted"
    [ "$(current)" = 2 ] || fail "booting deployment 2 should have made it current after the crash"
    grep -qx "default archix-2.conf" "$loader" || fail "deployment 2 should stay the default: $(cat "$loader")"
    findings 2 | diff "$state/baseline-2" - > "$state/diff-2" || fail "deployment 2's files changed: $(head -c 600 "$state/diff-2")"
    systemctl is-active -q archix-marker.service || fail "deployment 2 should start the marker"
    say "PASS: a crash between a rollback's boot default and its current left deployment 2 booting, whole, and current"
    archix secure-boot setup > /dev/console 2>&1 || fail "archix secure-boot setup"
    sed -i 's|boot = { loader = "systemd-boot" }|boot = { loader = "systemd-boot", secure_boot = true, protected_entry = true }|' "$config"
    archix --host archix-vm switch --noconfirm > /dev/console 2>&1 || fail "switch with Secure Boot"
    next 5
    systemctl reboot
    ;;
  5)
    bootctl status 2> /dev/null | grep -q "Secure Boot: enabled (user)" || fail "Secure Boot should be enforcing"
    say "PASS: booted a signed entry with Secure Boot enforcing"
    systemctl is-failed -q archix-boot-complete.service && fail "archix-boot-complete.service failed: $(journalctl -u archix-boot-complete.service -b --no-pager | tail -5)"
    grep -qx "default archix-$(current).conf" "$loader" && ! grep -q "^preferred" "$loader" \
      || fail "once deployment $(current) booted fine, it should be the default and nothing preferred: $(cat "$loader")"
    say "PASS: a deployment that booted fine became systemd-boot's default"
    archix secure-boot check > /dev/console 2>&1 || fail "secure-boot check should pass"
    archix secure-boot check --anticheat > /dev/console 2>&1 && fail "the anti-cheat check can't pass with a self-signed kernel"
    [ -f "/efi/loader/entries/archix-$(current)-protected.conf" ] || fail "protected_entry should write a protected entry"
    sed -i 's|secure_boot = true|secure_boot = false|' "$config"
    before=$(current) kept=$(deployments)
    archix --host archix-vm install jq --noconfirm > /dev/console 2>&1 && fail "an unsigned switch must be refused under Secure Boot"
    [ "$(current)" = "$before" ] || fail "the refused switch changed the current deployment"
    [ "$(deployments)" = "$kept" ] || fail "the refused switch left a deployment behind: $(deployments)"
    ls -d /archix/deploy/.staging-* > /dev/null 2>&1 && fail "the refused switch left its staging tree: $(listing)"
    sed -i 's|secure_boot = false|secure_boot = true|' "$config"
    say "PASS: secure-boot setup, check, the protected entry, and refusing unsigned entries"
    echo "$before" > "$state/before"
    echo "$kept" > "$state/kept"
    next 6
    archix --host archix-vm install jq --noconfirm > /dev/console 2>&1 &
    for _ in $(seq 3000); do ls -d /archix/deploy/.staging-* > /dev/null 2>&1 && break; sleep 0.2; done
    ls -d /archix/deploy/.staging-* > /dev/null 2>&1 || fail "the switch never started staging"
    sleep 1
    say "cutting the power while the switch stages its deployment"
    echo 1 > /proc/sys/kernel/sysrq
    echo b > /proc/sysrq-trigger
    ;;
  6)
    before=$(cat "$state/before")
    [ "$booted" = "$before" ] && [ "$(current)" = "$before" ] || fail "after a power cut mid-switch, deployment $before should still boot and be current"
    [ "$(deployments)" = "$(cat "$state/kept")" ] || fail "the cut switch committed a deployment: $(listing); deployments before $(cat "$state/kept"), now $(deployments)"
    archix --host archix-vm install jq --noconfirm > /dev/console 2>&1 || fail "the switch after the power cut"
    [ "$(current)" -gt "$before" ] || fail "installing jq should make a newer deployment"
    ls -d /archix/deploy/.staging-* > /dev/null 2>&1 && fail "the next switch should clear the cut switch's staging tree: $(listing)"
    say "PASS: a power cut mid-switch left deployment $before booting, and the next switch cleaned up after it"
    good=$booted
    echo "$good" > "$state/good"
    # An init that exits at once makes the kernel panic ("Attempted to kill init"), and panic=3 reboots it.
    sed -i 's|"console=ttyS0" }|"console=ttyS0", "init=/usr/bin/false", "panic=3" }|' "$config"
    archix --host archix-vm switch --noconfirm > /dev/console 2>&1 || fail "switch to a deployment that can't boot"
    bad=$(current)
    echo "$bad" > "$state/bad"
    [ -f "/efi/loader/entries/archix-$bad+3.conf" ] || fail "the new entry should count three boot tries: $(ls /efi/loader/entries)"
    grep -qx "preferred archix-$bad.conf" "$loader" && grep -qx "default archix-$good.conf" "$loader" \
      || fail "loader.conf should prefer deployment $bad and fall back to $good: $(cat "$loader")"
    say "PASS: deployment $bad's entry gets three boot tries; rebooting into a kernel that panics"
    next 7
    systemctl reboot
    ;;
  7)
    good=$(cat "$state/good") bad=$(cat "$state/bad")
    ls /efi/loader/entries > /dev/console
    [ "$booted" = "$good" ] || fail "after three failed boots of deployment $bad, systemd-boot should start $good, not $booted"
    [ "$(current)" = "$good" ] || fail "archix should make deployment $good current after $bad failed to boot"
    [ -f "/efi/loader/entries/archix-$bad+0-3.conf" ] || fail "deployment $bad's entry should be marked bad: $(ls /efi/loader/entries)"
    grep -q "generation $bad didn't finish booting" /var/lib/archix/notes || fail "the login note should say $bad didn't boot: $(cat /var/lib/archix/notes)"
    grep -q "^preferred" "$loader" && fail "falling back to $good should drop preferred: $(cat "$loader")"
    say "PASS: deployment $bad failed to boot three times, systemd-boot fell back to $good, and archix made it current"
    sed -i 's|, "init=/usr/bin/false", "panic=3" }| }|' "$config"
    # A deployment with a UKI of its own, which keep = 1 then drops; a copy is kept to try booting once it's revoked.
    sed -i 's|"console=ttyS0" }|"console=ttyS0", "archix.revoketest" }|' "$config"
    archix --host archix-vm switch --noconfirm > /dev/console 2>&1 || fail "switch to a deployment with a UKI of its own"
    dropped=$(current)
    own=$(ls /efi/loader/entries/archix-"$dropped".conf /efi/loader/entries/archix-"$dropped"+*.conf 2> /dev/null | head -1)
    revoked=$(sed -nE 's#^(efi|uki) /#/efi/#p' "$own")
    cp "$revoked" /var/tmp/revoked.efi || fail "copying $revoked"
    # An entry archix didn't write, standing in for Microsoft's: revoking must leave it in dbx.
    kek=/var/lib/sbctl/keys/KEK
    hash-to-efi-sig-list /usr/lib/systemd/boot/efi/linuxx64.efi.stub /var/tmp/foreign.esl > /dev/null || fail "hashing the stand-in"
    sign-efi-sig-list -a -k "$kek/KEK.key" -c "$kek/KEK.pem" dbx /var/tmp/foreign.esl /var/tmp/foreign.auth > /dev/null || fail "signing the stand-in"
    efi-updatevar -a -f /var/tmp/foreign.auth dbx || fail "appending the stand-in to dbx"
    foreign=$(od -An -tx1 -j44 -N32 /var/tmp/foreign.esl | tr -d ' \n')
    hash-to-efi-sig-list /var/tmp/revoked.efi /var/tmp/revoked.esl > /dev/null || fail "hashing the copy"
    target=$(od -An -tx1 -j44 -N32 /var/tmp/revoked.esl | tr -d ' \n')
    say "efivarfs before revoking: $(stat -f -c 'blocks=%b free=%f size=%S' /sys/firmware/efi/efivars)"
    sed -i 's|, "archix.revoketest" }| }|; s|protected_entry = true }|protected_entry = true, revoke_unused = true },\n  deployments = { keep = 1 }|' "$config"
    archix --host archix-vm switch --noconfirm > /dev/console 2>&1 || fail "switch with deployments.keep = 1"
    [ "$(deployments)" = "$good $(current) " ] || fail "keep = 1 should leave only the booted and the current deployment: $(deployments)"
    archix generations | grep -E "^ +$dropped " | grep -q record || fail "deployment $dropped's generation should stay as a record: $(archix generations)"
    [ -e "$revoked" ] && fail "dropping deployment $dropped should take $revoked off the ESP"
    dbx=$(efi-readvar -v dbx)
    echo "$dbx" | grep -qi "$foreign" || fail "the entry archix didn't write should still be in dbx: $dbx"
    echo "$dbx" | grep -qi "$target" || fail "dbx should list the dropped UKI $target: $dbx"
    say "efivarfs after revoking: $(stat -f -c 'blocks=%b free=%f size=%S' /sys/firmware/efi/efivars)"
    archix secure-boot revoke --dry-run 2>&1 | grep -q "nothing to revoke" || fail "revoke should find nothing left"
    say "PASS: keep = 1 dropped the unpinned deployments, and revoke_unused put the dropped UKI in dbx but kept the entry archix didn't write"
    mkdir -p /efi/EFI/revoketest
    cp /var/tmp/revoked.efi /efi/EFI/revoketest/revoked.efi
    printf 'title revoked\nefi /EFI/revoketest/revoked.efi\n' > /efi/loader/entries/revoketest.conf
    echo "reboot-on-error yes" >> "$loader"
    current > "$state/before-revoked"
    bootctl set-oneshot revoketest.conf || fail "bootctl set-oneshot revoketest.conf"
    next 8
    systemctl reboot
    ;;
  8)
    selected=$(tail -c +5 "$efivar" | tr -d '\000')
    [ "$selected" = revoketest.conf ] && fail "the firmware should refuse the revoked UKI"
    [ "$(current)" = "$(cat "$state/before-revoked")" ] || fail "the refused boot shouldn't change the current deployment"
    bootctl status 2> /dev/null | grep -q "Secure Boot: enabled (user)" || fail "Secure Boot should be enforcing"
    say "PASS: the firmware refused the revoked UKI and archix's entry booted instead ($selected)"
    rm -rf /efi/EFI/revoketest /efi/loader/entries/revoketest.conf
    sed -i '/^reboot-on-error/d' "$loader"
    entries=$(ls /efi/loader/entries)
    archix eject --noconfirm > /dev/console 2>&1 && fail "eject should refuse a system laid out as deployments"
    [ "$(ls /efi/loader/entries)" = "$entries" ] || fail "the refused eject changed the boot entries: $(ls /efi/loader/entries)"
    [ -e /etc/systemd/system/archix-boot.service ] || fail "the refused eject removed archix-boot.service"
    say "PASS: eject refuses a system that boots through deployments and changes nothing"
    next done
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
