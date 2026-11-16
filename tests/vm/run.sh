#!/usr/bin/env bash
# Boots archix in QEMU with OVMF on the Python driver (docs/vm-testing.md): `run.sh SUITE [--interactive] [--no-cache]` runs one of suite_tests, a test with an image of its own (tests/vm/SUITE: config.lua, image.env, test.py; ext4_tests also run with FS=ext4): dkms2 DKMS with two kernels, nvidia NVIDIA's DKMS drivers (TO_DATE, 2026-10-10 by default), cachyos stock CachyOS on Limine, firewall the firewall over ssh and a second NIC, secrets declared secrets on real boots.
set -euo pipefail
suite=${1:?run.sh takes one of suite_tests}
root=$(cd "$(dirname "$0")/../.." && pwd)
# The driver's suites (tests/vm/driver, docs/vm-testing.md): --interactive gives a REPL with the VM as machine, and a pass is recorded by its inputs' hash and skipped next time unless --no-cache or ARCHIX_VM_CACHE=0.
interactive= cache=1
[ "${ARCHIX_VM_CACHE:-1}" != 0 ] || cache=
suite_tests=(dkms2 nvidia cachyos firewall secrets)
# Those of suite_tests that also apply with FS=ext4; the release gate runs them on both.
ext4_tests=(dkms2 nvidia firewall secrets)
[[ " ${suite_tests[*]} " == *" $suite "* ]] || { echo "unknown suite $suite; run.sh takes one of: ${suite_tests[*]}" >&2; exit 2; }
for arg in "${@:2}"; do
  case $arg in
    --interactive) interactive=1 ;;
    --no-cache) cache= ;;
    *) echo "unknown argument $arg" >&2; exit 2 ;;
  esac
done
"$root/tests/arch/image.sh"
cache_py=$root/tests/vm/driver/cache.py
# On GitHub the suite's outcome is a row of the job's summary, and a failure an annotation naming it.
# A failed suite's verdict, the ATTRIBUTED: or NOT ATTRIBUTED: line the driver prints after FAIL for docs/CI.md's known flakes and hangs, goes on its row and annotation; without one it's a plain failure.
summary() {
  local result=$2 why=
  [ "$2" != failed ] || [ -z "${4:-}" ] || why=$(grep -aE '^(NOT )?ATTRIBUTED: ' "$4" 2> /dev/null | tail -1 || true)
  [ -z "$why" ] || { result="failed; $why"; echo "$1: $why"; }
  [ -n "${GITHUB_STEP_SUMMARY:-}" ] || return 0
  [ -s "$GITHUB_STEP_SUMMARY" ] || printf '| Test | FS | Result | Took |\n| --- | --- | --- | --- |\n' >> "$GITHUB_STEP_SUMMARY"
  echo "| $1 | ${FS:-ext4} | $result | ${3:-} |" >> "$GITHUB_STEP_SUMMARY"
  [ "$2" != failed ] || echo "::error title=$1 failed on ${FS:-ext4}::$1 failed after $3${why:+ (${why})}; its console and timeline are in this job's logs artifact"
}
[ -n "$interactive" ] || python3 "$root/tests/vm/driver/hang.py"
# Exported before the key is taken, as cache.py hashes it.
export VM_TIMEOUT="${VM_TIMEOUT:-14400}"
key= inputs=
if [ -z "$interactive" ]; then
  inputs=$(python3 "$cache_py" inputs "$suite")
  read -r _ key < <(python3 "$cache_py" keys "$suite")
  if [ -n "$cache" ] && python3 "$cache_py" hit "$suite" "$key"; then
    summary "$suite" cached
    exit 0
  fi
fi
started=$(date +%s)
# One build volume per checkout, as in tests/arch/run.sh.
target=archix-target-$(printf %s "$root" | sha256sum | cut -c1-12)
. "$root/tests/vm/work-dir.sh" vm
mounts=(-v "$root":/src:ro -v "$target":/target -v archix-cargo:/root/.cargo/registry -v "$work":/work -v "$root/tests/vm":/vm:ro -e SUITE="$suite" -e FS="${FS:-ext4}" -e TO_DATE="${TO_DATE:-}" -e IMAGE_DIR="/vm/$suite")
# ARCHIX_PREBUILT=DIR takes the binary from tests/arch/run.sh's ARCHIX_BUILD_OUT=DIR instead of building it.
if [ -n "${ARCHIX_PREBUILT:-}" ]; then
  docker run --rm -v "$ARCHIX_PREBUILT":/prebuilt:ro -v "$work":/work archix-arch-test bash -c 'set -e
    tar -I zstd -xf /prebuilt/target.tar.zst -C /tmp target/release/archix
    cp /tmp/target/release/archix /work/archix'
else
  docker run --rm "${mounts[@]}" archix-arch-test bash -c 'set -e
    cargo build --locked --release && cp /target/release/archix /work/archix'
fi
# ARCHIX_TEST_ARCHIVE (e.g. a local caching mirror on http://127.0.0.1:8079) stands in for archive.archlinux.org; the VM reaches the host as 10.0.2.2.
archive=${ARCHIX_TEST_ARCHIVE:-https://archive.archlinux.org}
# The image is built in a test VM (tests/testvm), which reaches the host's 127.0.0.1 as 10.0.2.2, as the test VMs do.
"$root/tests/testvm/run.sh" --share "work=$work" --mount 9p work /work rw --share "vm=$root/tests/vm:ro" --mount 9p vm /vm ro \
  --env SUITE="$suite" --env FS="${FS:-ext4}" --env TO_DATE="${TO_DATE:-}" --env IMAGE_DIR="/vm/$suite" \
  --env ARCHIVE="${archive/127.0.0.1/10.0.2.2}" --env VM_ARCHIVE="${archive/127.0.0.1/10.0.2.2}" -- /vm/build-image.sh
tty=() args=()
[ -z "$interactive" ] || tty=(-it) args=(--interactive)
vm=(docker run --rm "${tty[@]}" --network host --device /dev/kvm "${mounts[@]}" -e PYTHONPATH=/vm -e VM_TIMEOUT -e VM_MEM -e VM_CPUS -e GITHUB_RUN_ID archix-arch-test)
if [ -n "$interactive" ]; then
  "${vm[@]}" python -m driver "/vm/$suite/test.py" "${args[@]}"
  exit
fi
if [ "$suite" = firewall ]; then
  # The probe side of the VM's second NIC: a tap in a network namespace of its own, so the host's network is untouched.
  rm -f "$work/reach.sock"
  l2=$(docker run -d --rm --network none --cap-add NET_ADMIN --device /dev/net/tun --sysctl net.ipv6.conf.all.disable_ipv6=0 \
    --sysctl net.ipv6.conf.default.disable_ipv6=0 -v "$work":/work -v "$root/tests/vm":/vm:ro archix-arch-test \
    sh -c 'exec python -u /vm/firewall/l2.py /work/l2.sock /work/reach.sock > /work/l2.log 2>&1')
fi
status=0
"${vm[@]}" python -m driver "/vm/$suite/test.py" 2>&1 | tee "$work/output.log" || status=$?
[ -z "${l2:-}" ] || docker rm -f "$l2" > /dev/null 2>&1 || true
seconds=$(($(date +%s) - started))
if [ "$status" = 0 ]; then
  echo "== $suite passed: $seconds s"
  # Records the pass only if its inputs didn't change while it ran, as a file edited meanwhile may not be what it tested.
  read -r _ now < <(python3 "$cache_py" keys "$suite")
  if [ "$now" = "$key" ]; then
    python3 "$cache_py" record "$suite" "$key" "$seconds" "$work/timeline.log" "$work/serial.log" "$work/output.log" || echo "warning: recording $suite's pass failed"
  else
    echo "$suite: the inputs changed during the run; not recording the pass. What changed:"
    diff <(printf '%s\n' "$inputs") <(python3 "$cache_py" inputs "$suite") | sed 's/^/  /' || true
  fi
  summary "$suite" passed "$seconds s"
else
  summary "$suite" failed "$seconds s" "$work/output.log"
  echo "== $suite FAILED (exit $status) after $seconds s on ${FS:-ext4}"
fi
exit "$status"
