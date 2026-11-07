#!/bin/bash
# Turns the test image's tree at $1 into the test VM's root: networking, no logins on the console, and the unit that runs the job the host hands it.
set -euo pipefail
r=$1
echo "LABEL=archix-testvm / ext4 rw,noatime 0 1" > "$r/etc/fstab"
: > "$r/etc/machine-id"
echo archix-testvm > "$r/etc/hostname"
rm -f "$r/etc/resolv.conf" && ln -s /run/systemd/resolve/stub-resolv.conf "$r/etc/resolv.conf"
printf '[Match]\nName=en*\n\n[Network]\nDHCP=yes\n' > "$r/etc/systemd/network/20-wired.network"
install -Dm755 /guest/run "$r/usr/lib/archix-testvm/run"
install -Dm644 /guest/archix-testvm.service "$r/etc/systemd/system/archix-testvm.service"
mkdir -p "$r/etc/systemd/system/multi-user.target.wants" "$r/etc/systemd/system/sockets.target.wants" "$r/etc/systemd/system/network-online.target.wants"
for unit in systemd-networkd.service systemd-resolved.service archix-testvm.service; do ln -sf "/usr/lib/systemd/system/$unit" "$r/etc/systemd/system/multi-user.target.wants/$unit"; done
ln -sf /etc/systemd/system/archix-testvm.service "$r/etc/systemd/system/multi-user.target.wants/archix-testvm.service"
ln -sf /usr/lib/systemd/system/systemd-networkd.socket "$r/etc/systemd/system/sockets.target.wants/systemd-networkd.socket"
ln -sf /usr/lib/systemd/system/systemd-networkd-wait-online.service "$r/etc/systemd/system/network-online.target.wants/systemd-networkd-wait-online.service"
# No logins on the consoles, and /tmp on the disk, as the tests' loop images there would otherwise take the VM's memory.
for unit in serial-getty@ttyS0.service getty@tty1.service console-getty.service serial-getty@hvc0.service tmp.mount; do ln -sf /dev/null "$r/etc/systemd/system/$unit"; done
# Defence in depth: this systemd never arms a hardware watchdog, whatever device it is given.
install -Dm644 /guest/no-watchdog.conf "$r/etc/systemd/system.conf.d/archix-no-watchdog.conf"
