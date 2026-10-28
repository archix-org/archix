#!/usr/bin/env bash
# Boots archix deployments in QEMU with OVMF; `run.sh` (`run.sh boot [TEST...]`) runs the boot tests side by side on the Python driver from one laid-out base (the first layout on a booted system; then layout, deployments and rollback, secureboot with blessing on a read-only ESP and unsigned switches refused, reset-mid-switch, boot counting, revoke under Secure Boot, etc-merge, the fallback UKI without loader/entries and eject back to plain Arch, usr-local: a unit in the shared /usr/local that fails every boot, got past by the fallbacks that leave /usr/local out, and file-systems: a data disk, a LUKS volume, bind and tmpfs mounts, a swap file and zram from file_systems, swap and zram, a switch with swap active and a boot without the data disk), `run.sh boot2` the same checks as one sequential walk, `run.sh rollback` the rollback torture test, `run.sh nvidia` NVIDIA DKMS drivers across deployments (TO_DATE: the newer snapshot, 2026-10-10 by default), `run.sh cachyos` the rollback torture test on CachyOS's real repos, `run.sh health` refusing hibernation while a deployment is pending and boot.health_check's fallbacks (a firewall that blocks the network, a pulled cable), `run.sh pcr` signed PCR policies unlocking a LUKS volume with swtpm, `run.sh dkms2` DKMS with two kernels, `run.sh selfupgrade` archix upgrading itself from ARCHIX_OLD (HEAD~1 by default), `run.sh firewall` the firewall over ssh, `run.sh churn` a year of weekly switches, `run.sh recovery` a live-USB recovery (the ESP wiped and the current deployment damaged; a plain Arch rescue disk binds a kept deployment, chroots in and rolls back to it; it boots and switches again), `run.sh rescue` every deployment broken and a lost root password, each repaired through the rescue entry's `archix rescue` prompt behind its login, then under Secure Boot with root locked wrong and empty passwords refused, a wheel password let in and a TPM-bound LUKS volume asking for its passphrase, `run.sh powercut` the dm-log-writes power-cut replay (tests/vm/powercut/run.sh).
set -euo pipefail
suite=${1:-boot}
case $suite in upgrade | atomic) echo "the $suite suite tests the in-place model; tests/vm/run.sh selfupgrade replaces it" >&2; exit 2 ;; esac
root=$(cd "$(dirname "$0")/../.." && pwd)
[ "$suite" != powercut ] || exec "$root/tests/vm/powercut/run.sh"
# The driver's suites (tests/vm/driver, docs/vm-testing.md): --interactive gives a REPL with the VM as machine, and a pass is recorded by its inputs' hash and skipped next time unless --no-cache or ARCHIX_VM_CACHE=0.
driver= interactive= cache=1 names=()
[ "${ARCHIX_VM_CACHE:-1}" != 0 ] || cache=
case $suite in boot | boot2) driver=$suite ;; esac
boot_tests=(fallback layout revoke secureboot etc-merge counting reset-mid-switch usr-local file-systems)
for arg in "${@:2}"; do
  case $arg in
    --interactive) interactive=1 ;;
    --no-cache) cache= ;;
    -*) echo "unknown option $arg" >&2; exit 2 ;;
    *) [ "$suite" = boot ] && [[ " ${boot_tests[*]} base " == *" $arg "* ]] || { echo "unknown argument $arg; run.sh boot takes tests of: ${boot_tests[*]}" >&2; exit 2; }
      names+=("$arg") ;;
  esac
done
[ -n "$driver" ] || [ -z "$interactive" ] || { echo "--interactive needs a driver suite (boot, boot2)" >&2; exit 2; }
[ "$suite" != boot ] || [ -z "$interactive" ] || [ ${#names[@]} = 1 ] || { echo "run.sh boot --interactive takes one test, or base" >&2; exit 2; }
[ "$suite" != boot ] || [ ${#names[@]} -gt 0 ] || names=("${boot_tests[@]}")
"$root/tests/arch/image.sh"
cache_py=$root/tests/vm/driver/cache.py
# On GitHub each test's outcome is a row of the job's summary, and a failure an annotation naming the test, as one job runs several.
summary() {
  [ -n "${GITHUB_STEP_SUMMARY:-}" ] || return 0
  [ -s "$GITHUB_STEP_SUMMARY" ] || printf '| Test | FS | Result | Took |\n| --- | --- | --- | --- |\n' >> "$GITHUB_STEP_SUMMARY"
  echo "| ${1#boot-} | ${FS:-ext4} | $2 | ${3:-} |" >> "$GITHUB_STEP_SUMMARY"
  [ "$2" != failed ] || echo "::error title=${1#boot-} failed on ${FS:-ext4}::${1#boot-} failed after $3; its console and timeline are in this job's logs artifact"
}
# Each driver test by its result-cache name, those still to run and their keys.
declare -A keys=()
pending=()
if [ "$suite" = boot ]; then
  [ -n "$interactive" ] || python3 "$root/tests/vm/boot/checks.py"
  for name in "${names[@]}"; do [ "$name" = base ] || pending+=("boot-$name"); done
elif [ -n "$driver" ]; then
  pending=(boot2)
fi
if [ ${#pending[@]} -gt 0 ] && [ -z "$interactive" ]; then
  while read -r name key; do keys[$name]=$key; done < <(python3 "$cache_py" keys "${pending[@]}")
  if [ -n "$cache" ]; then
    left=()
    for name in "${pending[@]}"; do
      if python3 "$cache_py" hit "$name" "${keys[$name]}"; then
        summary "$name" cached
      else
        left+=("$name")
      fi
    done
    pending=("${left[@]}")
    [ ${#pending[@]} -gt 0 ] || exit 0
  fi
fi
started=$(date +%s)
# One build volume per checkout, as in tests/arch/run.sh.
target=archix-target-$(printf %s "$root" | sha256sum | cut -c1-12)
. "$root/tests/vm/work-dir.sh" vm
mounts=(-v "$root":/src:ro -v "$target":/target -v archix-cargo:/root/.cargo/registry -v "$work":/work -v "$root/tests/vm":/vm:ro -e SUITE="$suite" -e FS="${FS:-ext4}" -e TO_DATE="${TO_DATE:-}")
# ARCHIX_PREBUILT=DIR takes the binaries from tests/arch/run.sh's ARCHIX_BUILD_OUT=DIR instead of building them.
if [ -n "${ARCHIX_PREBUILT:-}" ]; then
  docker run --rm -v "$ARCHIX_PREBUILT":/prebuilt:ro -v "$work":/work archix-arch-test bash -c 'set -e
    tar -I zstd -xf /prebuilt/target.tar.zst -C /tmp target/release/archix target/x86_64-unknown-linux-musl/release/archix
    cp /tmp/target/release/archix /work/archix && cp /tmp/target/x86_64-unknown-linux-musl/release/archix /work/archix-static'
else
  docker run --rm "${mounts[@]}" archix-arch-test bash -c 'set -e
    cargo build --locked --release && cp /target/release/archix /work/archix
    cargo build --locked --release --target x86_64-unknown-linux-musl && cp /target/x86_64-unknown-linux-musl/release/archix /work/archix-static'
fi
if [ "$suite" = selfupgrade ]; then
  old=${ARCHIX_OLD:-HEAD~1}
  rm -rf "$work/old-src" && mkdir -p "$work/old-src" "$work/old"
  git -C "$root" archive "$old" | tar -x -C "$work/old-src"
  git -C "$root" rev-parse --short "$old" > "$work/old/commit"
  # git archive dates every file at its commit, so one target dir shared by two old commits would keep the first one's build.
  old_target=/target/old-$(cat "$work/old/commit")
  docker run --rm -v "$work":/work -v "$target":/target -v archix-cargo:/root/.cargo/registry -w /work/old-src -e CARGO_TARGET_DIR="$old_target" archix-arch-test bash -c 'set -e
    cargo build --locked --release --target x86_64-unknown-linux-musl && cp "$CARGO_TARGET_DIR"/x86_64-unknown-linux-musl/release/archix /work/old/archix-static'
fi
# ARCHIX_TEST_ARCHIVE (e.g. a local caching mirror on http://127.0.0.1:8079) stands in for archive.archlinux.org; the VM reaches the host as 10.0.2.2.
archive=${ARCHIX_TEST_ARCHIVE:-https://archive.archlinux.org}
docker run --rm --privileged --network host "${mounts[@]}" -e ARCHIVE="$archive" -e VM_ARCHIVE="${archive/127.0.0.1/10.0.2.2}" archix-arch-test /vm/build-image.sh
case $suite in
  rollback | nvidia | cachyos | atomic | dkms2 | churn) mounts+=(-e VM_TIMEOUT="${VM_TIMEOUT:-14400}" -e VM_MEM="${VM_MEM:-8192}" -e VM_CPUS="${VM_CPUS:-6}") ;;
  selfupgrade | pcr | firewall) mounts+=(-e VM_TIMEOUT="${VM_TIMEOUT:-7200}" -e VM_MEM="${VM_MEM:-4096}" -e VM_CPUS="${VM_CPUS:-4}") ;;
  health) mounts+=(-e VM_TIMEOUT=5400) ;;
  rescue) mounts+=(-e VM_TIMEOUT=7200) ;;
esac
if [ -z "$driver" ]; then
  if [ "$suite" = firewall ]; then
    # The probe side of the VM's second NIC: a tap in a network namespace of its own, so the host's network is untouched.
    rm -f "$work/reach.sock"
    l2=$(docker run -d --rm --network none --cap-add NET_ADMIN --device /dev/net/tun --sysctl net.ipv6.conf.all.disable_ipv6=0 \
      --sysctl net.ipv6.conf.default.disable_ipv6=0 -v "$work":/work -v "$root/tests/vm":/vm:ro archix-arch-test \
      sh -c 'exec python -u /vm/firewall/l2.py /work/l2.sock /work/reach.sock > /work/l2.log 2>&1')
  fi
  # No EXIT trap of its own: work-dir.sh's frees the heavy slot and the work dir.
  status=0
  docker run --rm --network host --device /dev/kvm "${mounts[@]}" archix-arch-test /vm/boot.sh || status=$?
  [ -z "${l2:-}" ] || docker rm -f "$l2" > /dev/null 2>&1 || true
  exit "$status"
fi
tty=() args=()
[ -z "$interactive" ] || tty=(-it) args=(--interactive)
vm=(docker run --rm "${tty[@]}" --network host --device /dev/kvm "${mounts[@]}" -e PYTHONPATH=/vm -e VM_TIMEOUT -e VM_MEM -e VM_CPUS archix-arch-test)
# Records each pass whose inputs didn't change while it ran, as a file edited meanwhile may not be what it tested, with its logs.
record() {
  local name=$1 seconds=$2 dir=$3 now
  [ -z "$interactive" ] || return 0
  now=$(python3 "$cache_py" keys "$name")
  if [ "${now#* }" = "${keys[$name]}" ]; then
    python3 "$cache_py" record "$name" "${keys[$name]}" "$seconds" "$dir/timeline.log" "$dir/serial.log" "$dir/output.log"
  else
    echo "$name: the inputs changed during the run; not recording the pass"
  fi
}
if [ "$suite" = boot2 ]; then
  "${vm[@]}" python -m driver /vm/boot/test.py "${args[@]}"
  record boot2 "$(($(date +%s) - started))" "$work"
  exit
fi
# The split boot tests: the base lays the image out with this run's archix and powers off; each test then boots a qcow2 overlay of that disk with its own copy of the vars, in a heavy slot of its own.
if [ "${names[*]}" = base ]; then
  "${vm[@]}" python -m driver /vm/boot/base.py "${args[@]}"
  exit
fi
if ! "${vm[@]}" python -m driver /vm/boot/base.py; then
  for name in "${pending[@]}"; do summary "$name" failed "the base, which it needs, failed"; done
  echo "boot: the base failed, so none of ${pending[*]#boot-} ran on ${FS:-ext4}"
  exit 1
fi
echo "base: laid out in $(($(date +%s) - started)) s from the start"
overlay() { echo "qemu-img create -q -f qcow2 -b /work/disk.img -F raw /work/$1/disk.qcow2 && cp /work/vars.fd /work/$1/vars.fd && exec python -m driver /vm/boot/$1/test.py --work /work/$1 ${args[*]}"; }
if [ -n "$interactive" ]; then
  mkdir -p "$work/${names[0]}"
  "${vm[@]}" bash -c "$(overlay "${names[0]}")"
  exit
fi
declare -A running=()
for name in "${pending[@]}"; do
  test=${name#boot-}
  mkdir -p "$work/$test"
  (
    # The runner's own heavy slot, which work-dir.sh took, goes to one test at a time through own-slot.lock; the others wait for a free one.
    while :; do
      exec {slot_fd}> "$work/own-slot.lock"
      flock -n "$slot_fd" && break
      exec {slot_fd}>&-
      for i in $(seq "${ARCHIX_HEAVY_JOBS:-2}"); do
        exec {slot_fd}> "${XDG_RUNTIME_DIR:-/tmp}/archix-heavy.$i.lock"
        flock -n "$slot_fd" && break 2
        exec {slot_fd}>&-
      done
      sleep 2
    done
    began=$(date +%s)
    status=0
    "${vm[@]}" bash -c "$(overlay "$test")" > "$work/$test/output.log" 2>&1 || status=$?
    echo "$status $(($(date +%s) - began)) $((began - started))" > "$work/$test/status"
  ) &
  running[$!]=$test
  sleep "${BOOT_STAGGER:-2}"
done
failed=()
while [ ${#running[@]} -gt 0 ]; do
  wait -n -p done_pid || true
  test=${running[$done_pid]}
  unset "running[$done_pid]"
  # A test whose runner died without its status still counts as failed, and the others still get reported.
  read -r status seconds offset < "$work/$test/status" 2> /dev/null || { status=died seconds=? offset=?; }
  if [ "$status" = 0 ]; then
    echo "== $test passed: $seconds s, started at +$offset s"
    sed -n '/^steps passed/,$p' "$work/$test/output.log"
    record "boot-$test" "$seconds" "$work/$test" || echo "warning: recording $test's pass failed"
    summary "$test" passed "$seconds s"
  else
    failed+=("$test")
    summary "$test" failed "$seconds s"
    echo "== $test FAILED (exit $status) after $seconds s, started at +$offset s; its last lines:"
    tail -n 60 "$work/$test/output.log" || true
  fi
done
echo "boot: ${#pending[@]} test(s) in $(($(date +%s) - started)) s on ${FS:-ext4}${failed[*]:+; failed: ${failed[*]}}"
[ ${#failed[@]} = 0 ]
