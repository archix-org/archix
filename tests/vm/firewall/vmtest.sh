#!/bin/bash
# The firewall over a real network path: ssh from the host through QEMU's port forward works with the ssh service allowed, is refused once a switch drops it and the new deployment boots, and works again after booting the older deployment's entry; over a second NIC, from LAN and public addresses on IPv4 and IPv6, ssh's port answers everyone, a LAN-only service's only the LAN and an undeclared one no one; the rules are checked in the kernel each time.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
stage=$(cat "$state/stage" 2>/dev/null || echo 1)
config=/etc/archix/init.lua
ax() { archix --host archix-vm "$@"; }
say() { echo "VMTEST: $*" > /dev/console; }
fail() {
  say "FAIL: $*"
  say "diagnostics: ruleset $(nft list ruleset 2>&1 | tr '\n' '|')"
  say "diagnostics: firewall unit $(systemctl status archix-firewall.service --no-pager 2>&1 | tail -5 | tr '\n' '|')"
  echo failed > "$state/stage"; sync; systemctl poweroff; exit 1
}
current() { ax generations | awk '$1 == "*" { print $2 }'; }
remember() { echo "$2" > "$state/$1"; }
recall() { cat "$state/$1"; }
next() { echo "$1" > "$state/stage"; sync; }
# systemd-boot names the entry it started in LoaderEntrySelected: 4 attribute bytes, then archix-N.conf in UTF-16.
booted=$(tr -d '\000' < /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
# Asks the host, over the control channel, to reach sshd through the port forward; prints its answer.
probe_line() {
  local reply=
  exec 3<> /dev/virtio-ports/archix.ctl || fail "the control channel to the host is missing"
  echo "$1 $2" >&3
  read -r -t 180 reply <&3
  exec 3>&-
  echo "$reply"
}
probe() { probe_line probe "$1"; }
ssh_open() {
  local reply; reply=$(probe "$1")
  say "$1: the host's ssh probe says: $reply"
  [[ $reply == "$1 banner=ssh login=$(cat /run/archix/deployment)" ]] || fail "$1: ssh from the host should log in to deployment $(cat /run/archix/deployment): $reply"
}
ssh_refused() {
  local reply; reply=$(probe "$1")
  say "$1: the host's ssh probe says: $reply"
  [[ $reply == "$1 banner=none login=failed"* ]] || fail "$1: ssh from the host should be refused: $reply"
}
# The second NIC's static addresses, IPv6's past duplicate detection.
wait_for_probe_nic() {
  local nic; nic=$(ip -o link | awk -F': ' '/52:54:00:12:34:77/ { print $2 }')
  for _ in $(seq 60); do
    [ -n "$nic" ] && [ "$(ip -o addr show dev "$nic" | grep -c -e 192.168.77.2/ -e 198.51.100.2/ -e 'fd00:77::2/' -e '2001:db8::2/')" = 4 ] \
      && ! ip -o addr show dev "$nic" | grep -q tentative && return 0
    sleep 1
  done
  fail "the second NIC ${nic:-(missing)} should have its four addresses: $(ip -o addr | tr '\n' '|')"
}
# Asks l2.py, through the host, which of sshd's ports 22 (ssh), 2201 (probe-lan, LAN only) and 2202 (undeclared) answer from LAN and public sources on both families.
reach() {
  local reply; reply=$(probe_line reach "$1")
  say "$1: from the second NIC: $reply"
  [ "$reply" = "$1 $2" ] || fail "$1: the second NIC should see $2: $reply"
}
OPEN="lan4/22=open lan4/2201=open lan4/2202=closed lan6/22=open lan6/2201=open lan6/2202=closed wan4/22=open wan4/2201=closed wan4/2202=closed wan6/22=open wan6/2201=closed wan6/2202=closed"
SHUT="lan4/22=closed lan4/2201=closed lan4/2202=closed lan6/22=closed lan6/2201=closed lan6/2202=closed wan4/22=closed wan4/2201=closed wan4/2202=closed wan6/22=closed wan6/2201=closed wan6/2202=closed"
# The archix table as the kernel has it loaded.
loaded() { nft list table inet archix 2>&1; }
rules_loaded() {
  systemctl is-active -q archix-firewall.service || fail "archix-firewall.service should be active"
  loaded | grep -q "type filter hook input priority filter; policy drop;" || fail "the kernel should have archix's input chain with policy drop: $(loaded | tr '\n' '|')"
}
for _ in $(seq 60); do systemctl is-active -q sshd.service && break; sleep 1; done
say "stage $stage: booted $booted, mounted $(cat /run/archix/deployment), current $(current)"
[ "$booted" = "$(cat /run/archix/deployment)" ] || fail "the initramfs mounted deployment $(cat /run/archix/deployment), but systemd-boot started archix-$booted"
systemctl is-active -q sshd.service || fail "sshd should run"

case $stage in
  1)
    rules_loaded
    loaded | grep -q 'tcp dport 22 accept comment "ssh"' || fail "the loaded rules should open port 22 for the ssh service: $(loaded | tr '\n' '|')"
    loaded | grep -q 'ip6 saddr @archix_lan6 tcp dport 2201 accept comment "probe-lan"' || fail "the loaded rules should open port 2201 to the LAN over IPv6: $(loaded | tr '\n' '|')"
    loaded > "$state/rules-open"
    ssh_open allowed
    wait_for_probe_nic
    reach allowed "$OPEN"
    # Another program's table, as Docker's: archix's reload must leave it.
    nft add table ip vmtest-other && nft add chain ip vmtest-other c || fail "adding another table"
    systemctl reload archix-firewall.service || fail "reloading archix-firewall.service"
    nft list tables | grep -q "table ip vmtest-other" || fail "reloading archix's firewall removed another program's table: $(nft list tables | tr '\n' '|')"
    diff <(loaded) "$state/rules-open" > /dev/null || fail "the reload changed archix's rules"
    say "PASS: with the ssh service allowed, ssh from the host logs in, the kernel has archix's rules, ports answer by their services' sources on IPv4 and IPv6, and reloading them leaves another table alone"
    remember open "$(current)"
    sed -i 's|allowed_services = { "ssh", "probe-lan" }|allowed_services = {}|' "$config"
    ax switch --noconfirm > /dev/console 2>&1 || fail "the switch dropping the services"
    remember closed "$(current)"
    [ "$(recall closed)" != "$(recall open)" ] || fail "dropping the services should make a new deployment"
    grep -q "tcp dport" "/archix/deploy/$(recall closed)/etc/nftables-archix.conf" && fail "deployment $(recall closed)'s rules still open a TCP port: $(cat "/archix/deploy/$(recall closed)/etc/nftables-archix.conf" | tr '\n' '|')"
    ssh_open "before-reboot"
    diff <(loaded) "$state/rules-open" > /dev/null || fail "the switch changed the running rules before the reboot"
    say "PASS: the switch dropping the services left the running firewall and ssh as they were"
    next 2
    systemctl reboot
    ;;
  2)
    [ "$booted" = "$(recall closed)" ] || fail "deployment $(recall closed) should boot after its switch"
    rules_loaded
    loaded | grep -q "tcp dport" && fail "the loaded rules should open no TCP port: $(loaded | tr '\n' '|')"
    ssh_refused dropped
    wait_for_probe_nic
    reach dropped "$SHUT"
    ssh-keyscan -T 10 127.0.0.1 2> /dev/null | grep -q ssh- || fail "sshd should still answer on the loopback, which the firewall accepts"
    say "PASS: with the services dropped, the kernel's rules open no port and the host's ssh is refused while sshd answers locally"
    bootctl set-oneshot "archix-$(recall open).conf" || fail "bootctl set-oneshot"
    next 3
    systemctl reboot
    ;;
  3)
    [ "$booted" = "$(recall open)" ] || fail "should have booted deployment $(recall open)'s entry"
    [ "$(current)" = "$(recall open)" ] || fail "booting deployment $(recall open)'s entry should make it current"
    rules_loaded
    diff <(loaded) "$state/rules-open" > "$state/rules-diff" || fail "the rolled-back deployment should load the rules it had: $(tr '\n' '|' < "$state/rules-diff")"
    ssh_open "rolled-back"
    wait_for_probe_nic
    reach rolled-back "$OPEN"
    say "PASS: booting the older deployment's entry brought back its rules in the kernel, and ssh from the host logs in again"
    next done
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
