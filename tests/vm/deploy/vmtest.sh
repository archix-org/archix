#!/bin/bash
# Runs at every boot of the deployment prototype VM: switch, boot the new deployment, roll back by boot entry; reports on the console.
set -u
state=/var/lib/archix-proto
mkdir -p "$state"
stage=$(cat "$state/stage" 2> /dev/null || echo 1)
lib=/usr/local/lib/archix-deploy
say() { echo "VMTEST: $*" > /dev/console; }
fail() { say "FAIL: $*"; systemctl poweroff; exit 1; }
booted=$(cat /run/archix/deployment 2> /dev/null)
say "stage $stage: booted $booted, /usr from $(findmnt -n -o SOURCE /usr)"
to=${TO_DATE:-2026-10-10}

case $stage in
  1)
    [ "$booted" = 1 ] || fail "deployment 1 should boot first"
    findmnt -n -o SOURCE /usr | grep -q 'deploy/1' || fail "/usr should come from deployment 1"
    pacman -Q > "$state/before"
    "$lib/check-deployment" 1 > "$state/altered-1"
    getcap /usr/bin/ping > "$state/caps-1" 2>&1
    sha256sum /usr/bin/ping > "$state/ping-1"
    sed -i "s|/repos/[0-9/]*/|/repos/${to//-//}/|" /etc/pacman.d/mirrorlist
    pacman -Sy --noconfirm > /dev/null 2>&1 || fail "pacman -Sy"
    pacman -Suw --noconfirm > /dev/null 2>&1 || fail "downloading the upgrade"
    pacman -Qu > "$state/upgrades"
    say "PASS: deployment 1 booted; $(wc -l < "$state/upgrades") packages to upgrade into deployment 2"
    # In place and metadata-only writes to a file deployment 1 shares must not reach deployment 1.
    DEPLOY_INJECT='echo touched >> /usr/bin/ping && setcap cap_net_admin+ep /usr/bin/ping' \
      "$lib/deploy-switch" -Su > /dev/console 2>&1 || fail "deploy-switch"
    [ "$(readlink /archix/current)" = deploy/2 ] || fail "deployment 2 should be current"
    cmp -s "$state/ping-1" <(sha256sum /archix/deploy/1/usr/bin/ping | sed 's|/archix/deploy/1||') || fail "deployment 1's /usr/bin/ping changed"
    [ "$(getcap /archix/deploy/1/usr/bin/ping 2>&1 | sed 's|/archix/deploy/1||')" = "$(cat "$state/caps-1")" ] || fail "deployment 1's /usr/bin/ping capabilities changed"
    [ "$(cat /run/archix/deployment)" = 1 ] && findmnt -n -o SOURCE /usr | grep -q 'deploy/1' || fail "the running system should still be deployment 1"
    say "PASS: deployment 2 committed; in-place and setcap writes in staging left deployment 1's file alone"
    echo 2 > "$state/stage"
    systemctl reboot
    ;;
  2)
    [ "$booted" = 2 ] || fail "deployment 2 should boot after the switch"
    findmnt -n -o SOURCE /usr | grep -q 'deploy/2' || fail "/usr should come from deployment 2"
    while read -r name _ _ new; do
      [ "$(pacman -Q "$name" | cut -d' ' -f2)" = "$new" ] || fail "$name should be at $new in deployment 2"
    done < "$state/upgrades"
    [ -e /var/lib/archix-deploy/activated-2 ] || fail "deployment 2's /var changes should be applied on its first boot"
    [ "$(systemctl show -P ConditionResult systemd-update-done.service)" = yes ] \
      || fail "the new /usr should be newer than /etc/.updated, so ConditionNeedsUpdate units run: /usr $(stat -c %y /usr), /etc/.updated $(stat -c %y /etc/.updated)"
    "$lib/check-deployment" 1 | cmp -s - "$state/altered-1" || fail "deployment 1 changed: $("$lib/check-deployment" 1 | diff "$state/altered-1" - | head -5)"
    say "PASS: deployment 2 booted with every upgrade, /var activated, deployment 1 unchanged"
    bootctl set-oneshot archix-1.conf || fail "bootctl set-oneshot"
    echo 3 > "$state/stage"
    systemctl reboot
    ;;
  3)
    [ "$booted" = 1 ] || fail "booting entry 1 should boot deployment 1"
    pacman -Q | cmp -s - "$state/before" || fail "deployment 1's packages should be what they were"
    say "PASS: booting deployment 1's entry rolled back to it, with nothing reinstalled"
    echo done > "$state/stage"
    say "OK"
    systemctl poweroff
    ;;
  *) systemctl poweroff ;;
esac
