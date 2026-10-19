#!/bin/bash
# Runs at every boot of the VM test, walking through its stages and reporting on the console.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
stage=$(cat "$state/stage" 2>/dev/null || echo 1)
say() { echo "VMTEST: $*" > /dev/console; }
fail() { say "FAIL: $*"; systemctl poweroff; exit 1; }
current() { archix generations | awk '$1 == "*" { print $2 }'; }
# systemd-boot names the entry it started in LoaderEntrySelected: 4 attribute bytes, then archix-N.conf in UTF-16.
booted=$(tr -d '\000' < /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
say "stage $stage: booted generation $booted, current generation $(current)"
config=/etc/archix/init.lua

case $stage in
  1)
    [ "$(current)" = 1 ] || fail "generation 1 should be current"
    # The first 4 bytes are the EFI attributes; bit 0 is NON_VOLATILE, which would let a later direct firmware boot read a stale entry.
    attrs=$(od -An -tu4 -N4 /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | tr -d ' ')
    [ -n "$attrs" ] && [ $((attrs & 1)) = 0 ] || fail "LoaderEntrySelected should be volatile, but its attributes are ${attrs:-missing}"
    say "PASS: systemd-boot's LoaderEntrySelected is volatile (attributes $attrs), so no later boot sees a stale entry"
    systemctl is-active -q archix-marker.service || fail "generation 1 should start the marker"
    sed -i '/-- marker/d' "$config"
    archix --host archix-vm install tree --noconfirm > /dev/console 2>&1 || fail "archix install tree"
    systemctl is-enabled -q archix-marker.service && fail "generation 2 drops the marker"
    [ "$(current)" = 2 ] || fail "installing tree should make generation 2"
    bootctl set-oneshot archix-1.conf || fail "bootctl set-oneshot archix-1.conf"
    echo 2 > "$state/stage"
    say "PASS: generation 2 has a boot entry; booting generation 1's next"
    systemctl reboot
    ;;
  2)
    [ "$booted" = 1 ] || fail "should have booted generation 1's entry"
    [ "$(current)" = 1 ] || fail "booting generation 1's entry should make it current"
    pacman -Q tree > /dev/null 2>&1 && fail "the rollback at boot should have removed tree"
    systemctl is-active -q archix-marker.service || fail "the boot-time rollback should start the marker again, without deadlocking"
    say "PASS: booting an older entry rolled the system back to it, services included"
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
    [ -f "/boot/loader/entries/archix-$(current)-protected.conf" ] || fail "protected_entry should write a protected entry"
    sed -i 's|secure_boot = true|secure_boot = false|' "$config"
    before=$(current)
    archix --host archix-vm install jq --noconfirm > /dev/console 2>&1 && fail "an unsigned switch must be refused under Secure Boot"
    [ "$(current)" = "$before" ] || fail "the refused switch changed the current generation"
    [ -e /var/lib/archix/journal.json ] && fail "the refused switch left a journal"
    pacman -Q jq > /dev/null 2>&1 && fail "the refused switch installed jq"
    sed -i 's|secure_boot = false|secure_boot = true|' "$config"
    say "PASS: secure-boot setup, check, the protected entry, and refusing unsigned entries"
    rescued=$(current)
    archix --host archix-vm install jq --noconfirm > /dev/console 2>&1 || fail "archix install jq"
    [ "$(current)" -gt "$rescued" ] || fail "installing jq should make a newer generation"
    rm -f /usr/lib/libalpm.so*
    pacman -V > /dev/null 2>&1 && fail "pacman should be broken now"
    bootctl set-oneshot "archix-$rescued-rescue.conf" || fail "bootctl set-oneshot rescue"
    echo "$rescued" > "$state/rescued"
    echo 4 > "$state/stage"
    systemctl reboot
    ;;
  4)
    pacman -V > /dev/null 2>&1 || fail "the rescue boot should have repaired pacman"
    grep -q "^generation $(cat "$state/rescued")," /var/lib/archix/last-rescue || fail "rescue should restore the generation whose entry was booted, not $(cat /var/lib/archix/last-rescue)"
    [ "$(current)" = "$(cat "$state/rescued")" ] || fail "booting an older rescue entry should roll back to it"
    say "PASS: an older generation's rescue entry restored that generation without pacman"
    good=$(current)
    echo "$good" > "$state/good"
    # An init that exits at once makes the kernel panic ("Attempted to kill init"), and panic=3 reboots it.
    sed -i 's|"console=ttyS0" }|"console=ttyS0", "init=/usr/bin/false", "panic=3" }|' "$config"
    archix --host archix-vm switch --noconfirm > /dev/console 2>&1 || fail "switch to a generation that can't boot"
    bad=$(current)
    echo "$bad" > "$state/bad"
    [ -f "/boot/loader/entries/archix-$bad+3.conf" ] || fail "the new entry should count three boot tries: $(ls /boot/loader/entries)"
    grep -qx "preferred archix-$bad.conf" /boot/loader/loader.conf && grep -qx "default archix-$good.conf" /boot/loader/loader.conf \
      || fail "loader.conf should prefer generation $bad and fall back to $good: $(cat /boot/loader/loader.conf)"
    say "PASS: generation $bad's entry gets three boot tries; rebooting into a kernel that panics"
    echo 5 > "$state/stage"
    systemctl reboot
    ;;
  5)
    good=$(cat "$state/good") bad=$(cat "$state/bad")
    ls /boot/loader/entries > /dev/console
    [ "$booted" = "$good" ] || fail "after three failed boots of generation $bad, systemd-boot should start generation $good, not $booted"
    [ "$(current)" = "$good" ] || fail "archix should roll back to generation $good after $bad failed to boot"
    [ -f "/boot/loader/entries/archix-$bad+0-3.conf" ] || fail "generation $bad's entry should be marked bad: $(ls /boot/loader/entries)"
    grep -q "generation $bad didn't finish booting" /var/lib/archix/notes || fail "the login note should say generation $bad didn't boot: $(cat /var/lib/archix/notes)"
    grep -q "^preferred" /boot/loader/loader.conf && fail "the rollback to $good should drop preferred: $(cat /boot/loader/loader.conf)"
    say "PASS: generation $bad failed to boot three times, systemd-boot fell back to $good, and archix rolled back to it"
    sed -i 's|, "init=/usr/bin/false", "panic=3" }| }|' "$config"
    # A generation with a UKI of its own, which entries = 1 and two more generations then prune off the ESP; a copy is kept to try booting once it's revoked.
    sed -i 's|"console=ttyS0" }|"console=ttyS0", "archix.revoketest" }|' "$config"
    archix --host archix-vm switch --noconfirm > /dev/console 2>&1 || fail "switch to a generation with a UKI of its own"
    own=$(ls /boot/loader/entries/archix-"$(current)".conf /boot/loader/entries/archix-"$(current)"+*.conf 2> /dev/null | head -1)
    revoked=$(sed -nE 's#^(efi|uki) /#/boot/#p' "$own")
    cp "$revoked" /var/tmp/revoked.efi || fail "copying $revoked"
    sed -i 's|, "archix.revoketest" }| }|; s|protected_entry = true }|protected_entry = true, entries = 1 }|' "$config"
    archix --host archix-vm switch --noconfirm > /dev/console 2>&1 || fail "switch back to the shared UKI"
    sed -i 's|^  files = {|  files = {\n    ["/etc/archix-revoketest"] = { text = "1" },|' "$config"
    archix --host archix-vm switch --noconfirm > /dev/console 2>&1 || fail "switch to a third generation"
    [ -e "$revoked" ] && fail "pruning should take $revoked off the ESP"
    # An entry archix didn't write, standing in for Microsoft's: revoking must leave it in dbx.
    kek=/var/lib/sbctl/keys/KEK
    hash-to-efi-sig-list /usr/lib/systemd/boot/efi/linuxx64.efi.stub /var/tmp/foreign.esl > /dev/null || fail "hashing the stand-in"
    sign-efi-sig-list -a -k "$kek/KEK.key" -c "$kek/KEK.pem" dbx /var/tmp/foreign.esl /var/tmp/foreign.auth > /dev/null || fail "signing the stand-in"
    efi-updatevar -a -f /var/tmp/foreign.auth dbx || fail "appending the stand-in to dbx"
    foreign=$(od -An -tx1 -j44 -N32 /var/tmp/foreign.esl | tr -d ' \n')
    hash-to-efi-sig-list /var/tmp/revoked.efi /var/tmp/revoked.esl > /dev/null || fail "hashing the copy"
    target=$(od -An -tx1 -j44 -N32 /var/tmp/revoked.esl | tr -d ' \n')
    say "efivarfs before revoking: $(stat -f -c 'blocks=%b free=%f size=%S' /sys/firmware/efi/efivars)"
    archix secure-boot revoke --dry-run > /dev/console 2>&1 || fail "revoke --dry-run"
    archix secure-boot revoke --noconfirm > /dev/console 2>&1 || fail "revoke"
    dbx=$(efi-readvar -v dbx)
    echo "$dbx" | grep -qi "$foreign" || fail "the entry archix didn't write should still be in dbx: $dbx"
    echo "$dbx" | grep -qi "$target" || fail "dbx should list the pruned UKI $target: $dbx"
    archix secure-boot revoke --dry-run 2>&1 | grep -q "nothing to revoke" || fail "a second revoke should find nothing left"
    say "PASS: revoke appended the pruned UKIs to dbx and kept the entry it didn't write"
    mkdir -p /boot/EFI/revoketest
    cp /var/tmp/revoked.efi /boot/EFI/revoketest/revoked.efi
    printf 'title revoked\nefi /EFI/revoketest/revoked.efi\n' > /boot/loader/entries/revoketest.conf
    echo "reboot-on-error yes" >> /boot/loader/loader.conf
    current > "$state/before-revoked"
    bootctl set-oneshot revoketest.conf || fail "bootctl set-oneshot revoketest.conf"
    echo 6 > "$state/stage"
    systemctl reboot
    ;;
  6)
    selected=$(tail -c +5 /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f | tr -d '\000')
    [ "$selected" = revoketest.conf ] && fail "the firmware should refuse the revoked UKI"
    [ "$(current)" = "$(cat "$state/before-revoked")" ] || fail "the refused boot shouldn't change the current generation"
    bootctl status 2> /dev/null | grep -q "Secure Boot: enabled (user)" || fail "Secure Boot should be enforcing"
    say "PASS: the firmware refused the revoked UKI and the default entry booted instead ($selected)"
    rm -rf /boot/EFI/revoketest /boot/loader/entries/revoketest.conf
    sed -i '/^reboot-on-error/d' /boot/loader/loader.conf
    archix eject --noconfirm > /dev/console 2>&1 || fail "archix eject"
    ls /boot/loader/entries > /dev/console
    for gone in /etc/systemd/system/archix-boot.service /etc/systemd/system/sysinit.target.wants/archix-boot.service /etc/pacman.d/hooks/archix.hook \
      /etc/profile.d/archix.sh /etc/mkinitcpio.conf.d/archix-rescue.conf /etc/initcpio/hooks/archix-rescue /boot/EFI/archix; do
      [ -e "$gone" ] && fail "eject should remove $gone"
    done
    ls /boot/loader/entries/archix-* > /dev/null 2>&1 && fail "eject should remove archix's boot entries"
    grep -qx "default arch-linux.conf" /boot/loader/loader.conf || fail "eject should make arch-linux.conf the default: $(cat /boot/loader/loader.conf)"
    say "PASS: eject wrote a plain boot entry and took archix's entries, hooks and service off"
    echo 7 > "$state/stage"
    systemctl reboot
    ;;
  7)
    selected=$(tail -c +5 /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f | tr -d '\000')
    [ "$selected" = arch-linux.conf ] || fail "after eject the plain entry should boot, not '$selected'"
    bootctl status 2> /dev/null | grep -q "Secure Boot: enabled (user)" || fail "Secure Boot should still be enforcing after eject"
    systemctl cat archix-boot.service > /dev/null 2>&1 && fail "archix-boot.service should be gone"
    say "PASS: after eject, the plain entry booted with Secure Boot enforcing"
    pacman -Syu --noconfirm > /dev/console 2>&1 || fail "pacman -Syu after eject"
    pacman -U --noconfirm "$(sed -n 's|^Server = \(http://[^/]*\)/.*|\1|p' /etc/pacman.d/mirrorlist | head -1)/packages/l/linux/linux-7.2.8.arch1-2-x86_64.pkg.tar.zst" > /dev/console 2>&1 \
      || fail "plain pacman changing the kernel after eject"
    sbctl verify 2>&1 | grep -q "/boot/vmlinuz-linux is signed" || fail "sbctl's hook should re-sign the new kernel: $(sbctl verify 2>&1 | head -5)"
    echo 8 > "$state/stage"
    systemctl reboot
    ;;
  8)
    selected=$(tail -c +5 /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f | tr -d '\000')
    [ "$selected" = arch-linux.conf ] || fail "the plain entry should still boot, not '$selected'"
    [ "$(uname -r)" = 7.2.8-arch1-2 ] || fail "the kernel plain pacman installed should boot, not $(uname -r)"
    bootctl status 2> /dev/null | grep -q "Secure Boot: enabled (user)" || fail "Secure Boot should be enforcing"
    modprobe dummy || fail "the new kernel's modules should load"
    say "PASS: a kernel plain pacman installed after eject boots from the plain entry with Secure Boot"
    echo done > "$state/stage"
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
