#!/usr/bin/env bash
# Boots /work/disk.img under OVMF (Secure Boot capable) until the VM test powers off, then checks its report.
set -euo pipefail
vars=/work/vars.fd
[ -f "$vars" ] || cp /usr/share/edk2/x64/OVMF_VARS.4m.fd "$vars"
timeout "${VM_TIMEOUT:-2400}" qemu-system-x86_64 -enable-kvm -cpu host -m "${VM_MEM:-3072}" -smp "${VM_CPUS:-2}" \
  -machine q35,smm=on -global driver=cfi.pflash01,property=secure,value=on \
  -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd \
  -drive if=pflash,format=raw,unit=1,file="$vars" \
  -drive file=/work/disk.img,format=raw,if=virtio \
  -netdev user,id=net0 -device virtio-net-pci,netdev=net0,romfile= \
  -display none -serial file:/work/serial.log -monitor none || true
grep VMTEST /work/serial.log || true
grep -q "VMTEST: OK" /work/serial.log
