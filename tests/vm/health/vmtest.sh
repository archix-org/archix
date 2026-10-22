#!/bin/bash
# The health suite: hibernation is refused while a switch waits for its reboot, and boot.health_check falls back from a deployment whose firewall blocks the network or that boots with the cable pulled.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
stage=$(cat "$state/stage" 2>/dev/null || echo start)
say() { echo "VMTEST: $*" > /dev/console; }
fail() { say "FAIL: $*"; systemctl poweroff; exit 1; }
current() { archix generations | awk '$1 == "*" { print $2 }'; }
next() { echo "$1" > "$state/stage"; sync; }
efivar=/sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f
booted=$(tr -d '\000' < "$efivar" 2>/dev/null | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
config=/etc/archix/init.lua
notes=/var/lib/archix/notes
guard=/run/systemd/sleep.conf.d/zz-archix-pending-deployment.conf
say "stage $stage: booted entry $booted, current $(current), entries $(ls /efi/loader/entries | tr '\n' ' ')"
can_hibernate() { busctl call org.freedesktop.login1 /org/freedesktop/login1 org.freedesktop.login1.Manager CanHibernate | awk '{ print $2 }' | tr -d '"'; }
swap_on() { [ -f /swapfile ] || mkswap -q --file /swapfile --size 2G; swapon /swapfile 2> /dev/null || swapon --show | grep -q /swapfile; }
# What the health check must reach: the archive the VM installs from, as host:port.
target() {
  local url hostport
  url=$(sed -n 's/^ *archive_url = "\(.*\)",$/\1/p' "$config")
  hostport=${url#*://}; hostport=${hostport%%/*}
  case $hostport in *:*) ;; *) [ "${url%%:*}" = https ] && hostport=$hostport:443 || hostport=$hostport:80 ;; esac
  echo "$hostport"
}
set_boot() { sed -i "s|^  boot = .*-- boot$|  boot = { loader = \"systemd-boot\"$1 }, -- boot|" "$config"; }
# Waits up to $1 seconds for the health check's unit to settle and prints its state.
health_state() {
  local s
  for _ in $(seq "$1"); do
    s=$(systemctl show -P ActiveState archix-health-check.service)
    case $s in active | failed) echo "$s"; return ;; esac
    sleep 1
  done
  systemctl show -P ActiveState archix-health-check.service
}
carrier() { cat /sys/class/net/en*/carrier 2> /dev/null | head -1; }

case $stage in
  start)
    swap_on || fail "making a swap file to hibernate to"
    before=$(can_hibernate)
    echo "$before" > "$state/can-hibernate"
    [ "$before" = yes ] || say "this VM can't hibernate ($before), so only the drop-in is checked"
    archix --host archix-vm install tree --noconfirm > /dev/console 2>&1 || fail "archix install tree"
    pending=$(current)
    [ -f "$guard" ] && grep -qx "AllowHibernation=no" "$guard" || fail "a pending deployment should turn hibernation off: $(cat "$guard" 2>&1)"
    systemd-analyze cat-config systemd/sleep.conf | grep -q "^AllowSuspend=no" && fail "suspend should stay allowed"
    archix status 2>&1 | grep -q "hibernation is off until you reboot into deployment $pending" || fail "status should say hibernation is off: $(archix status 2>&1)"
    if [ "$before" = yes ]; then
      [ "$(can_hibernate)" != yes ] || fail "logind still offers hibernation with deployment $pending pending"
      systemctl hibernate 2> "$state/hibernate.err" && fail "systemctl hibernate went ahead with deployment $pending pending"
      say "systemctl hibernate was refused: $(head -c 200 "$state/hibernate.err")"
    fi
    say "PASS: hibernation is off while deployment $pending waits for the reboot"
    next 2
    systemctl reboot
    ;;
  2)
    [ -e "$guard" ] && fail "the reboot into deployment $(current) should allow hibernation again"
    swap_on || fail "turning the swap file on"
    if [ "$(cat "$state/can-hibernate")" = yes ]; then
      [ "$(can_hibernate)" = yes ] || fail "logind should offer hibernation again after the reboot: $(can_hibernate)"
    fi
    say "PASS: hibernation is allowed again once the pending deployment booted"
    good=$(current)
    echo "$good" > "$state/good"
    target=$(target); port=${target##*:}
    set_boot ", health_check = { units = { \"sshd.service\" }, listening = { 22 }, reachable = { \"$target\" }, timeout = 30, reboot_on_failure = true }"
    sed -i "s|^  hostname = |  firewall = { enable = true, allowed_tcp_ports = { 22 }, extra_rules = \"chain healthtest { type filter hook output priority filter; policy accept; tcp dport $port drop; ip protocol icmp drop; }\" },\n  hostname = |" "$config"
    archix --host archix-vm switch --noconfirm > /dev/console 2>&1 || fail "switch to a deployment whose firewall blocks $target"
    bad=$(current)
    echo "$bad" > "$state/bad"
    [ -f "/efi/loader/entries/archix-$bad+3.conf" ] || fail "deployment $bad's entry should be counted: $(ls /efi/loader/entries)"
    say "PASS: deployment $bad blocks $target in its firewall; rebooting into it"
    next 3
    systemctl reboot
    ;;
  3)
    good=$(cat "$state/good") bad=$(cat "$state/bad")
    if [ "$booted" = "$bad" ]; then
      s=$(health_state 300)
      [ "$s" = failed ] || fail "deployment $bad's health check should fail with its firewall blocking $(target): $s"
      grep -q "deployment $bad failed its health check \"$(target) reachable\"" "$notes" || fail "the failed check should leave a note: $(cat "$notes")"
      say "PASS: deployment $bad failed its health check; waiting for reboot_on_failure"
      sleep 180
      fail "reboot_on_failure should have rebooted the machine"
    fi
    [ "$booted" = "$good" ] || fail "after deployment $bad failed its health check, systemd-boot should start $good, not $booted"
    [ "$(current)" = "$good" ] || fail "archix should make deployment $good current"
    ls /efi/loader/entries | grep -qx "archix-$bad+0-3.conf" || fail "deployment $bad's entry should have no tries left: $(ls /efi/loader/entries)"
    why="deployment $bad fell back to $good because the health check \"$(target) reachable\" failed at "
    grep -qF "$why" "$notes" || fail "the login note should say why: $(cat "$notes")"
    archix status 2>&1 | grep -qF "$why" || fail "status should say why: $(archix status 2>&1)"
    say "PASS: the failed health check rebooted, systemd-boot fell back to $good, and the note and status say why"
    # The deployment is fine; the network goes away for a reason outside it, as with a pulled cable.
    sed -i '/^  firewall = /d' "$config"
    set_boot ", health_check = { units = { \"sshd.service\" }, listening = { 22 }, reachable = { \"$(target)\" }, timeout = 30 }"
    archix --host archix-vm switch --noconfirm > /dev/console 2>&1 || fail "switch to a healthy deployment"
    current > "$state/unplugged"
    say "cable out"
    for _ in $(seq 60); do [ "$(carrier)" = 0 ] && break; sleep 1; done
    [ "$(carrier)" = 0 ] || fail "the host should have pulled the network cable"
    next 4
    systemctl reboot
    ;;
  4)
    good=$(cat "$state/good") unplugged=$(cat "$state/unplugged")
    [ "$booted" = "$unplugged" ] || fail "should boot deployment $unplugged, not $booted"
    s=$(health_state 400)
    [ "$s" = failed ] || fail "with the cable out, deployment $unplugged's health check should fail: $s"
    grep -q "deployment $unplugged failed its health check \"$(target) reachable\"" "$notes" || fail "the failed check should leave a note: $(cat "$notes")"
    ls /efi/loader/entries | grep -qx "archix-$unplugged+0-3.conf" || fail "deployment $unplugged's entry should have no tries left: $(ls /efi/loader/entries)"
    say "PASS: with the cable out, deployment $unplugged failed its health check and keeps running until the next restart"
    say "cable in"
    for _ in $(seq 60); do [ "$(carrier)" = 1 ] && break; sleep 1; done
    [ "$(carrier)" = 1 ] || fail "the host should have put the network cable back"
    next 5
    systemctl reboot
    ;;
  5)
    good=$(cat "$state/good") unplugged=$(cat "$state/unplugged")
    [ "$booted" = "$good" ] && [ "$(current)" = "$good" ] || fail "as documented, a check failing for a reason outside the deployment falls back to $good too, not $booted"
    grep -qF "deployment $unplugged fell back to $good because the health check \"$(target) reachable\" failed at " "$notes" || fail "the note should say why: $(cat "$notes")"
    say "PASS: as documented, the unplugged cable failed the check and fell back to $good, with a note saying why"
    archix --host archix-vm switch --noconfirm > /dev/console 2>&1 || fail "switching again with the network back"
    current > "$state/again"
    next 6
    systemctl reboot
    ;;
  6)
    again=$(cat "$state/again")
    [ "$booted" = "$again" ] || fail "should boot deployment $again, not $booted"
    s=$(health_state 300)
    [ "$s" = active ] || fail "with the network back, deployment $again's health check should pass: $s"
    for _ in $(seq 120); do ls /efi/loader/entries | grep -qx "archix-$again.conf" && break; sleep 1; done
    ls /efi/loader/entries | grep -qx "archix-$again.conf" || fail "deployment $again's entry should count as good: $(ls /efi/loader/entries)"
    say "PASS: switching again once the network was back brought the update back, and its boot counted as good"
    next done
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
