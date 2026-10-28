#!/usr/bin/env bash
# Boots /work/disk.img under OVMF (Secure Boot capable) until the VM test powers off, then checks its report; SUITE=pcr and SUITE=rescue add a swtpm TPM, SUITE=firewall an ssh port forward and a control channel the host answers probes on, SUITE=rescue a console socket that tests/vm/rescue/drive.py types the rescue prompt at.
set -euo pipefail
vars=/work/vars.fd
qmp=/work/qmp.sock
rm -f "$qmp" /work/serial.log
monitor=(-qmp "unix:$qmp,server=on,wait=off")
watchers=()
trap 'kill "${watchers[@]}" 2> /dev/null' EXIT
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
if [ "${SUITE:-}" = pcr ] || [ "${SUITE:-}" = rescue ]; then
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
# Boots until the VM powers off or $1 seconds pass, with its console in $2 and the disks that follow.
vm() {
  local limit=$1 log=$2 console
  shift 2
  console=(-serial file:"$log")
  [ "${SUITE:-}" != rescue ] || console=("${serial[@]}")
  timeout "$limit" qemu-system-x86_64 -enable-kvm -cpu host -m "${VM_MEM:-3072}" -smp "${VM_CPUS:-2}" \
    -machine q35,smm=on -global driver=cfi.pflash01,property=secure,value=on \
    -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd \
    -drive if=pflash,format=raw,unit=1,file="$vars" \
    "$@" \
    -netdev "$net" -device virtio-net-pci,netdev=net0,romfile= \
    -display none "${console[@]}" "${monitor[@]}" || true
}
if [ "${SUITE:-}" = recovery ]; then
  # Four boots: the archix disk breaks itself, then starts no kernel, the rescue disk (first, so it's vda) recovers it, and it boots again.
  archix=(-drive file=/work/disk.img,format=raw,if=none,id=archix -device virtio-blk-pci,drive=archix,bootindex=1)
  rescue=(-drive file=/work/rescue.img,format=raw,if=none,id=rescue -device virtio-blk-pci,drive=rescue,bootindex=0)
  : > /work/serial.log
  phase() {
    local name=$1 limit=$2 want=$3
    shift 3
    vm "$limit" "/work/serial-$name.log" "$@"
    cat "/work/serial-$name.log" >> /work/serial.log
    [ -z "$want" ] || grep -aq "VMTEST: $want" "/work/serial-$name.log" || { grep -a VMTEST /work/serial.log || true; echo "the $name boot didn't print VMTEST: $want" >&2; exit 1; }
  }
  phase broken "${VM_TIMEOUT:-2400}" broken "${archix[@]}"
  phase unbootable "${UNBOOTABLE_TIMEOUT:-180}" "" "${archix[@]}"
  if grep -aq "Linux version" /work/serial-unbootable.log; then
    grep -a VMTEST /work/serial.log || true
    echo "the broken disk should start no kernel" >&2
    exit 1
  fi
  echo "VMTEST: harness: the broken disk started no kernel in ${UNBOOTABLE_TIMEOUT:-180} s" >> /work/serial.log
  phase rescue "${VM_TIMEOUT:-2400}" "rescue: recovered" "${rescue[@]}" "${archix[@]}"
  phase recovered "${VM_TIMEOUT:-2400}" "" "${archix[@]}"
else
  vm "${VM_TIMEOUT:-2400}" /work/serial.log -drive file=/work/disk.img,format=raw,if=virtio "${extra[@]}"
fi
# The console log carries escape and NUL bytes, which make grep call it binary and hide every line.
grep -a VMTEST /work/serial.log || true
[ ! -f /work/probe.log ] || sed 's/^/probe: /' /work/probe.log
grep -aq "VMTEST: OK" /work/serial.log
