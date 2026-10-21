#!/usr/bin/env bash
# Fails unless this user can create a KVM virtual machine, so the VM suites never fall back to TCG.
set -euo pipefail
dev=${1:-/dev/kvm}
fail() {
  echo "::error::$*" >&2
  exit 1
}
[ -e "$dev" ] || fail "$dev doesn't exist: this machine has no KVM, and the VM suites won't run under emulation"
[ -r "$dev" ] && [ -w "$dev" ] || fail "$(id -un) can't open $dev read-write: $(ls -l "$dev")"
python3 - "$dev" << 'PY' || fail "$dev is there but KVM refused to create a virtual machine"
import fcntl, os, sys
kvm = os.open(sys.argv[1], os.O_RDWR | os.O_CLOEXEC)
assert fcntl.ioctl(kvm, 0xAE00) == 12, "KVM_GET_API_VERSION isn't 12"
os.close(fcntl.ioctl(kvm, 0xAE01, 0))
PY
echo "KVM works: $dev creates virtual machines for $(id -un)"
