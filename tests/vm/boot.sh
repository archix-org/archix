#!/usr/bin/env bash
# Boots /work/disk.img under OVMF (Secure Boot capable) until the VM test powers off, then checks its report.
set -euo pipefail
vars=/work/vars.fd
monitor=(-monitor none)
if [ "${SUITE:-boot}" = health ]; then
  sock=/work/monitor.sock
  rm -f "$sock" /work/serial.log
  monitor=(-monitor "unix:$sock,server=on,wait=off")
  # The health suite pulls its network cable and puts it back by asking on the console; QEMU's monitor flips the link.
  (
    cable=on
    while sleep 1; do
      want=on
      grep -aq "VMTEST: cable out" /work/serial.log 2> /dev/null && want=off
      grep -aq "VMTEST: cable in" /work/serial.log 2> /dev/null && want=on
      [ "$want" = "$cable" ] || ! [ -S "$sock" ] && continue
      python -c 'import socket, sys; s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(("set_link net0 %s\n" % sys.argv[2]).encode()); __import__("time").sleep(1)' "$sock" "$want" && cable=$want
    done
  ) &
  cable_pid=$!
  trap 'kill "$cable_pid" 2> /dev/null' EXIT
fi
[ -f "$vars" ] || cp /usr/share/edk2/x64/OVMF_VARS.4m.fd "$vars"
timeout "${VM_TIMEOUT:-2400}" qemu-system-x86_64 -enable-kvm -cpu host -m "${VM_MEM:-3072}" -smp "${VM_CPUS:-2}" \
  -machine q35,smm=on -global driver=cfi.pflash01,property=secure,value=on \
  -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd \
  -drive if=pflash,format=raw,unit=1,file="$vars" \
  -drive file=/work/disk.img,format=raw,if=virtio \
  -netdev user,id=net0 -device virtio-net-pci,netdev=net0,romfile= \
  -display none -serial file:/work/serial.log "${monitor[@]}" || true
# The console log carries escape and NUL bytes, which make grep call it binary and hide every line.
grep -a VMTEST /work/serial.log || true
grep -aq "VMTEST: OK" /work/serial.log
