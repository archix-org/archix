#!/usr/bin/env bash
# Boots /work/disk.img under OVMF (Secure Boot capable) until the VM test powers off, then checks its report, with QEMU's view of the NIC when the guest reports one networkd didn't configure; SUITE=pcr adds a swtpm TPM, SUITE=firewall an ssh port forward and a control channel the host answers probes on, SUITE=rescue a console socket that tests/vm/rescue/drive.py types the rescue prompt at.
set -euo pipefail
vars=/work/vars.fd
qmp=/work/qmp.sock
rm -f "$qmp" /work/serial.log /work/qemu-nic.log
monitor=(-qmp "unix:$qmp,server=on,wait=off")
watchers=()
trap 'kill "${watchers[@]}" 2> /dev/null' EXIT
nic_view() {
  local dev
  dev=$(python /vm/qmp.py "$qmp" "info virtio" | grep -a '\[virtio-net\]' | cut -d' ' -f1)
  # Not info virtio-status, which crashes QEMU 11.1.2 on a virtio-net device.
  { echo "== $1, $(date +%T)"; python /vm/qmp.py "$qmp" "info network" "info virtio-queue-status $dev 0" "info virtio-queue-status $dev 1"; } >> /work/qemu-nic.log 2>&1
}
# vmtest.sh's NIC report: QEMU's view of the device as the guest reports it and after its link down/up, then the host's own set_link off/on if that didn't help.
(
  set +e
  step=0
  while sleep 1; do
    log=$(grep -a "VMTEST: nic:" /work/serial.log 2> /dev/null) || continue
    case $step in
      0) nic_view "the guest's first NIC report"; step=1 ;;
      1) grep -q "nic: a link down/up\|nic: still not configured" <<< "$log" || continue
        nic_view "after the guest's link down/up"; step=2
        grep -q "nic: still not configured" <<< "$log" || break
        { echo "== set_link net0 off, then on, $(date +%T)"; python /vm/qmp.py "$qmp" "set_link net0 off"; sleep 2; python /vm/qmp.py "$qmp" "set_link net0 on"; } >> /work/qemu-nic.log 2>&1 ;;
      2) grep -q "nic: after the host's set_link" <<< "$log" || continue
        nic_view "after the host's set_link off/on"; break ;;
    esac
  done
) &
watchers+=($!)
if [ "${SUITE:-boot}" = health ]; then
  sock=/work/monitor.sock
  rm -f "$sock"
  monitor+=(-monitor "unix:$sock,server=on,wait=off")
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
  watchers+=($!)
else
  monitor+=(-monitor none)
fi
[ -f "$vars" ] || cp /usr/share/edk2/x64/OVMF_VARS.4m.fd "$vars"
extra=() net=user,id=net0
need() { command -v "$1" > /dev/null || pacman -S --noconfirm --needed "$2" > /dev/null; }
if [ "${SUITE:-}" = pcr ]; then
  need swtpm swtpm
  mkdir -p /work/swtpm
  swtpm socket --tpm2 --tpmstate dir=/work/swtpm --ctrl type=unixio,path=/work/swtpm/sock --daemon
  for _ in $(seq 50); do [ -S /work/swtpm/sock ] && break; sleep 0.2; done
  extra+=(-chardev socket,id=chrtpm,path=/work/swtpm/sock -tpmdev emulator,id=tpm0,chardev=chrtpm -device tpm-tis,tpmdev=tpm0)
fi
[ ! -f /work/data.img ] || extra+=(-drive file=/work/data.img,format=raw,if=virtio)
serial=(-serial file:/work/serial.log)
if [ "${SUITE:-}" = rescue ]; then
  # The rescue prompt is typed at: the console is a socket drive.py copies into serial.log, and QEMU waits for it to connect.
  rm -f /work/serial.sock
  serial=(-chardev socket,id=console,path=/work/serial.sock,server=on,wait=on -serial chardev:console)
  python /vm/rescue/drive.py /work/serial.sock /work/serial.log "$qmp" &
  watchers+=($!)
fi
if [ "${SUITE:-}" = firewall ]; then
  need ssh openssh
  port=$(python -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
  net+=",hostfwd=tcp:127.0.0.1:$port-:22"
  extra+=(-device virtio-serial-pci -chardev socket,id=ctl,path=/work/ctl.sock,server=on,wait=off -device virtserialport,chardev=ctl,name=archix.ctl)
  python /vm/firewall/probe.py /work/ctl.sock "$port" /work/ssh-key > /work/probe.log 2>&1 &
fi
timeout "${VM_TIMEOUT:-2400}" qemu-system-x86_64 -enable-kvm -cpu host -m "${VM_MEM:-3072}" -smp "${VM_CPUS:-2}" \
  -machine q35,smm=on -global driver=cfi.pflash01,property=secure,value=on \
  -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd \
  -drive if=pflash,format=raw,unit=1,file="$vars" \
  -drive file=/work/disk.img,format=raw,if=virtio \
  "${extra[@]}" \
  -netdev "$net" -device virtio-net-pci,netdev=net0,romfile= \
  -display none "${serial[@]}" "${monitor[@]}" || true
# The console log carries escape and NUL bytes, which make grep call it binary and hide every line.
grep -a VMTEST /work/serial.log || true
[ ! -f /work/probe.log ] || sed 's/^/probe: /' /work/probe.log
[ ! -f /work/qemu-nic.log ] || sed 's/^/qemu: /' /work/qemu-nic.log
grep -aq "VMTEST: OK" /work/serial.log
