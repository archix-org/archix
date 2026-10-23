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
# systemd-boot names the entry it started in LoaderEntrySelected: 4 attribute bytes, then archix-N.conf or archix-fallback-N.efi in UTF-16.
selected=$(tail -c +5 "$efivar" 2>/dev/null | tr -d '\000')
booted=$(echo "$selected" | grep -aoE 'archix-(fallback-)?[0-9]+' | head -1 | grep -oE '[0-9]+$')
running=$(cat /run/archix/deployment 2>/dev/null)
say "stage $stage: booted entry $selected, mounted deployment $running, current $(current), deployments $(deployments)"
failed_units=$(systemctl --failed --no-legend --plain | awk '{ print $1 }' | tr '\n' ' ')
if [ -n "$failed_units" ]; then
  for unit in $failed_units; do journalctl -b -u "$unit" --no-pager | tail -20 | sed "s|^|VMTEST: $unit: |" > /dev/console; done
  networkctl list --no-pager 2>&1 | sed 's|^|VMTEST: networkctl: |' > /dev/console
  fail "units failed at this boot: $failed_units"
fi
# Nothing in a deployment pulls in the network wait, so a link networkd never configured would go unseen; every stage has a NIC.
if ! /usr/lib/systemd/systemd-networkd-wait-online --timeout=90 > /dev/null 2>&1; then
  networkctl list --no-pager 2>&1 | sed 's|^|VMTEST: networkctl: |' > /dev/console
  journalctl -b -u systemd-networkd --no-pager | tail -30 | sed 's|^|VMTEST: networkd: |' > /dev/console
  fail "networkd didn't configure the NIC's link within 90 s of this stage starting"
fi
# Stage 10 boots the fallback of a deployment that doesn't exist, which has to fall back to current.
[ "$stage" = 10 ] || [ "$booted" = "$running" ] || fail "the initramfs mounted deployment $running, but systemd-boot started $selected"
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
    findmnt -no OPTIONS /archix | grep -qw ro || fail "/archix should be a read-only mount: $(findmnt -no OPTIONS /archix)"
    for try in "chmod -R 700 /archix/deploy/1/usr/bin" "chown -R nobody /archix/deploy/1/usr/bin" \
      "setfacl -R -m u:nobody:rwx /archix/deploy/1/usr/bin" "setcap cap_net_raw+ep /archix/deploy/1/usr/bin/pacman"; do
      $try 2> /dev/null && fail "$try got through /archix"
    done
    [ -z "$(findings 1)" ] || fail "deployment 1 changed through /archix: $(findings 1)"
    archix verify > /dev/console 2>&1 || fail "archix verify should find deployments 1 and 2 as their packages installed them"
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
    archix --host archix-vm status | grep -q "waiting for a reboot (deployment 3): tree " || fail "status should name tree as waiting for a reboot: $(archix --host archix-vm status)"
    grep -q "waiting for a reboot (deployment 3)" /var/lib/archix/notes || fail "the login notes should say updates wait for a reboot"
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
    grep -qs "waiting for a reboot" /var/lib/archix/notes && fail "the boot of deployment 3 should take the waiting-updates note away"
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
    [ "$selected" = revoketest.conf ] && fail "the firmware should refuse the revoked UKI"
    [ "$(current)" = "$(cat "$state/before-revoked")" ] || fail "the refused boot shouldn't change the current deployment"
    bootctl status 2> /dev/null | grep -q "Secure Boot: enabled (user)" || fail "Secure Boot should be enforcing"
    say "PASS: the firmware refused the revoked UKI and archix's entry booted instead ($selected)"
    rm -rf /efi/EFI/revoketest /efi/loader/entries/revoketest.conf
    sed -i '/^reboot-on-error/d' "$loader"
    next etc-merge
    systemctl reboot
    ;;
  etc-merge)
    # zsh's install scriptlet appends to /etc/shells in the staged /etc, so a live edit to it is a conflict.
    archix --host archix-vm install zsh --noconfirm > /dev/console 2>&1 || fail "archix install zsh"
    pending=$(current)
    [ "$pending" != "$booted" ] || fail "installing zsh should leave a deployment pending"
    grep -qx /bin/zsh "/archix/deploy/$pending/etc/shells" || fail "zsh's scriptlet should add it to the staged /etc/shells"
    for _ in $(seq 50); do systemctl is-active -q archix-etc-merge.service && break; sleep 0.2; done
    systemctl is-active -q archix-etc-merge.service || fail "the commit should start archix-etc-merge.service: $(systemctl status archix-etc-merge.service --no-pager | tail -5)"
    echo 'root:etc-merge-vmtest' | chpasswd || fail "chpasswd"
    grep '^root:' /etc/shadow > "$state/root-shadow"
    mkdir -p /etc/NetworkManager/system-connections
    (umask 077 && printf '[connection]\nid=vmtest\ntype=wifi\n\n[wifi-security]\npsk=vmtest-secret\n' > /etc/NetworkManager/system-connections/.vmtest.nmconnection.T3MP)
    mv /etc/NetworkManager/system-connections/.vmtest.nmconnection.T3MP /etc/NetworkManager/system-connections/vmtest.nmconnection
    mkdir -p /etc/vmtest-dir/sub && echo "made after the switch" > /etc/vmtest-dir/sub/file
    echo /bin/vmtest-shell >> /etc/shells
    for _ in $(seq 50); do grep -qx /bin/vmtest-shell "/archix/deploy/$pending/etc/shells" && break; sleep 0.2; done
    status=$(archix --host archix-vm status 2>&1)
    if ! { echo "$status" | grep -q "/etc changes carried into the pending deployment $pending" && echo "$status" | grep -q "/etc/shells: changed on the running system"; }; then
      echo "$status" | while IFS= read -r line; do say "status: $line"; done
      journalctl -b -u archix-etc-merge.service --no-pager | tail -30 | while IFS= read -r line; do say "journal: $line"; done
      ls -la /var/lib/archix/etc-merge 2>&1 | while IFS= read -r line; do say "state: $line"; done
      fail "status should count the carried changes and the conflict"
    fi
    say "PASS: the watcher carried a password, a connection, a new directory and a conflicting edit into deployment $pending within seconds"
    echo "$pending" > "$state/pending"
    next etc-merge-2
    systemctl reboot
    ;;
  etc-merge-2)
    pending=$(cat "$state/pending")
    [ "$booted" = "$pending" ] || fail "the restart should boot deployment $pending"
    grep -qxF "$(cat "$state/root-shadow")" /etc/shadow || fail "the password changed after the switch didn't reach deployment $pending"
    nm=/etc/NetworkManager/system-connections/vmtest.nmconnection
    grep -qx psk=vmtest-secret "$nm" && [ "$(stat -c %a "$nm")" = 600 ] || fail "the connection should arrive whole and 0600: $(stat -c %a "$nm" 2>&1)"
    [ "$(cat /etc/vmtest-dir/sub/file 2>/dev/null)" = "made after the switch" ] || fail "the new directory didn't reach deployment $pending"
    grep -qx /bin/vmtest-shell /etc/shells || fail "the live edit to /etc/shells should win"
    grep -qx /bin/zsh /etc/shells.pacnew || fail "deployment $pending's /etc/shells should be /etc/shells.pacnew: $(ls /etc/shells* 2>&1)"
    status=$(archix --host archix-vm status 2>&1)
    echo "$status" | grep -q "were carried from deployment" || fail "status should report the merge after the boot: $status"
    grep -q "didn't shut down cleanly" /var/lib/archix/notes 2>/dev/null && fail "a clean reboot shouldn't leave the power-cut note"
    say "PASS: after a clean reboot deployment $pending has the password, the connection, the directory and the live /etc/shells, with its own as .pacnew"
    archix --host archix-vm install bc --noconfirm > /dev/console 2>&1 || fail "archix install bc"
    pending=$(current)
    for _ in $(seq 50); do systemctl is-active -q archix-etc-merge.service && break; sleep 0.2; done
    echo "$pending" > "$state/pending"
    next etc-merge-3
    echo "written just before the power went" > /etc/vmtest-power-cut
    sync -f /etc
    say "cutting the power inside the /etc merge's window"
    echo 1 > /proc/sys/kernel/sysrq
    echo b > /proc/sysrq-trigger
    ;;
  etc-merge-3)
    pending=$(cat "$state/pending")
    [ "$booted" = "$pending" ] || fail "after the power cut deployment $pending should boot"
    [ "$(cat /etc/vmtest-power-cut 2>/dev/null)" = "written just before the power went" ] || fail "the boot should carry what the old /etc held on disk"
    grep -q "didn't shut down cleanly before deployment $pending first booted" /var/lib/archix/notes || fail "the login note should name the window: $(cat /var/lib/archix/notes)"
    archix --host archix-vm status 2>&1 | grep -q "didn't shut down cleanly" || fail "status should name the window"
    say "PASS: a power cut inside the window left a login note, and the boot carried the old /etc's last change"
    next fallback
    systemctl reboot
    ;;
  fallback)
    c=$(current)
    [ "$(ls /efi/EFI/Linux | grep '^archix-fallback-')" = "archix-fallback-$c.efi" ] \
      || fail "only deployment $c's fallback UKI should be on the ESP: $(ls /efi/EFI/Linux | tr '\n' ' ')"
    sbctl verify 2>&1 | grep "archix-fallback-$c.efi" | grep -q "is signed" || fail "the fallback UKI should be signed: $(sbctl verify 2>&1 | grep fallback)"
    rm -rf /efi/loader/entries
    sync -f /efi
    say "deleted /efi/loader/entries; deployment $c should boot through its fallback UKI"
    next 9
    systemctl reboot
    ;;
  9)
    [ "$selected" = "archix-fallback-$(current).efi" ] || fail "without loader/entries systemd-boot should start the fallback UKI of deployment $(current), not $selected"
    [ "$running" = "$(current)" ] || fail "the fallback UKI should boot deployment $(current), not $running"
    command -v tree > /dev/null || fail "deployment $(current) should have tree"
    say "PASS: with /efi/loader/entries gone, the fallback UKI booted the current deployment $(current)"
    # The fallback of a deployment gc collected: deployment 999 doesn't exist, though archix recorded the UKI.
    cp "/efi/EFI/Linux/archix-fallback-$(current).efi" /efi/EFI/Linux/archix-fallback-999.efi
    printf 'uki /EFI/Linux/archix-fallback-999.efi\n' > /var/lib/archix/boot/entries/archix-fallback-999.efi
    sync -f /efi
    bootctl set-oneshot archix-fallback-999.efi || fail "bootctl set-oneshot archix-fallback-999.efi"
    next 10
    systemctl reboot
    ;;
  10)
    [ "$selected" = archix-fallback-999.efi ] || fail "systemd-boot should have started archix-fallback-999.efi, not $selected"
    [ "$running" = "$(current)" ] || fail "the fallback of a deployment that's gone should boot current, $(current), not $running"
    journalctl -b --no-pager | grep -q "booting deployment $(current) (current;" || fail "deploy-mount should say it booted current instead"
    rm /efi/EFI/Linux/archix-fallback-999.efi /var/lib/archix/boot/entries/archix-fallback-999.efi
    say "PASS: the fallback UKI of a collected deployment booted current"
    pacman -Q > "$state/packages"
    pacman -Qkk > "$state/qkk" 2>&1
    archix eject --noconfirm > /dev/console 2>&1 &
    for _ in $(seq 3000); do grep -qx 'phase copied' /var/lib/archix/eject 2> /dev/null && break; sleep 0.1; done
    grep -qx 'phase copied' /var/lib/archix/eject || fail "the eject never finished copying the trees: $(cat /var/lib/archix/eject 2>&1)"
    pkill -9 -f 'archix eject'
    sync -f /efi
    sync -f /
    [ -d /archix/deploy ] || fail "the killed eject shouldn't have got as far as deleting /archix"
    say "killed the eject after it copied the trees ($(tr '\n' ' ' < /var/lib/archix/eject)); loader.conf: $(tr '\n' ' ' < "$loader")"
    next 11
    systemctl reboot
    ;;
  11)
    say "after the killed eject systemd-boot started $selected"
    archix eject --noconfirm > /dev/console 2>&1 || fail "running eject again should finish it"
    [ -e /archix ] && fail "eject should delete /archix: $(ls -A /archix)"
    findmnt -no SOURCE /usr | grep -q archix && fail "/usr should be the root's own now: $(findmnt -no SOURCE /usr)"
    grep -qx "default arch-linux.efi" "$loader" || fail "eject should make the plain UKI the default: $(cat "$loader")"
    ls /efi/loader/entries/archix-* /efi/EFI/Linux/archix-fallback-* > /dev/null 2>&1 && fail "eject left archix's boot entries"
    [ -e /var/lib/archix/eject ] && fail "the finished eject left its state"
    command -v tree > /dev/null || fail "the running system lost tree"
    say "PASS: eject finished after the kill and the restart"
    next 12
    systemctl reboot
    ;;
  12)
    [ "$selected" = arch-linux.efi ] || fail "plain Arch should boot through arch-linux.efi, not $selected"
    [ -z "$running" ] && [ ! -e /archix ] || fail "a plain boot mounts no deployment"
    findmnt -no SOURCE /usr | grep -q archix && fail "/usr should be the root's own"
    bootctl status 2> /dev/null | grep -q "Secure Boot: enabled (user)" || fail "Secure Boot should still be enforcing"
    pacman -Q | diff "$state/packages" - > "$state/packages-diff" || fail "plain Arch has other packages than the deployment had: $(head -c 600 "$state/packages-diff")"
    pacman -Qkk 2>&1 | diff "$state/qkk" - > "$state/qkk-diff" || fail "pacman -Qkk differs from the deployment's: $(head -c 600 "$state/qkk-diff")"
    systemctl is-active -q archix-protected.service || fail "the ProtectSystem=strict service should run on plain Arch"
    for unit in archix-boot archix-boot-complete archix-deploy-activate archix-flatpak; do
      [ -e "/etc/systemd/system/$unit.service" ] && fail "eject should remove $unit.service"
    done
    say "PASS: plain Arch booted with the deployment's packages, pacman -Qkk as before"
    pacman -Syu --noconfirm > /dev/console 2>&1 || fail "plain pacman -Syu after the eject"
    pacman -S --noconfirm cowsay > /dev/console 2>&1 || fail "plain pacman -S after the eject"
    command -v cowsay > /dev/null || fail "pacman -S cowsay should install it"
    say "PASS: plain pacman -Syu and -S work after the eject"
    next done
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
