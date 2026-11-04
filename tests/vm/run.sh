#!/usr/bin/env bash
# Boots archix in QEMU with OVMF on the Python driver (docs/vm-testing.md): `run.sh boot [TEST...]` the boot tests side by side from one base (their list is boot_tests below; all but ext4, the lane with rollbacks off, need FS=btrfs), `run.sh powercut [TEST...]` the power cuts at archix's marks (powercut_tests, FS=btrfs, archix built with the test hooks' debug assertions), `--interactive` a REPL on one test; the suites from the deployment era wait to be ported.
set -euo pipefail
suite=${1:-boot}
case $suite in
  nvidia | dkms2 | cachyos | churn | pcr | firewall | selfupgrade | rescue | recovery | secrets) echo "the $suite suite still drives deployments, which are gone; it waits to be ported to the in-place model on the driver" >&2; exit 2 ;;
esac
root=$(cd "$(dirname "$0")/../.." && pwd)
# The driver's suites (tests/vm/driver, docs/vm-testing.md): --interactive gives a REPL with the VM as machine, and a pass is recorded by its inputs' hash and skipped next time unless --no-cache or ARCHIX_VM_CACHE=0.
driver= interactive= cache=1 names=()
[ "${ARCHIX_VM_CACHE:-1}" != 0 ] || cache=
case $suite in boot | powercut) driver=$suite ;; *) echo "unknown suite $suite; run.sh takes boot or powercut" >&2; exit 2 ;; esac
boot_tests=(counting try guard unattended pacman keep hang glibc order health hook layer records strict ptrace ext4)
powercut_tests=(pacman keep varlib rootmove switch)
# The split suites, boot and powercut, run their tests side by side from one base.
split=
case $suite in boot) split=("${boot_tests[@]}") ;; powercut) split=("${powercut_tests[@]}") ;; esac
for arg in "${@:2}"; do
  case $arg in
    --interactive) interactive=1 ;;
    --no-cache) cache= ;;
    -*) echo "unknown option $arg" >&2; exit 2 ;;
    *) [ -n "$split" ] && [[ " ${split[*]} base " == *" $arg "* ]] || { echo "unknown argument $arg; run.sh $suite takes tests of: ${split[*]}" >&2; exit 2; }
      names+=("$arg") ;;
  esac
done
[ -z "$split" ] || [ -z "$interactive" ] || [ ${#names[@]} = 1 ] || { echo "run.sh $suite --interactive takes one test, or base" >&2; exit 2; }
# ext4 is the only boot test for an ext4 base, and the only one a btrfs base can't run.
for_fs() {
  [ "$suite" = boot ] && [ "$1" != base ] || return 0
  case ${FS:-ext4} in ext4) [ "$1" = ext4 ] ;; *) [ "$1" != ext4 ] ;; esac
}
if [ -n "$split" ] && [ ${#names[@]} -gt 0 ]; then
  for name in "${names[@]}"; do for_fs "$name" || { echo "run.sh boot $name doesn't run on FS=${FS:-ext4}: ext4 needs FS=ext4, the rest FS=btrfs" >&2; exit 2; }; done
fi
if [ -n "$split" ] && [ ${#names[@]} = 0 ]; then
  for name in "${split[@]}"; do ! for_fs "$name" || names+=("$name"); done
fi
"$root/tests/arch/image.sh"
cache_py=$root/tests/vm/driver/cache.py
# On GitHub each test's outcome is a row of the job's summary, and a failure an annotation naming the test, as one job runs several.
# A failed test's verdict, the ATTRIBUTED: or NOT ATTRIBUTED: line the driver prints after FAIL for docs/CI.md's known flakes and hangs, goes on its row and annotation; without one it's a plain failure.
summary() {
  local result=$2 why=
  [ "$2" != failed ] || [ -z "${4:-}" ] || why=$(grep -aE '^(NOT )?ATTRIBUTED: ' "$4" 2> /dev/null | tail -1 || true)
  [ -z "$why" ] || { result="failed; $why"; echo "${1#"$suite"-}: $why"; }
  [ -n "${GITHUB_STEP_SUMMARY:-}" ] || return 0
  [ -s "$GITHUB_STEP_SUMMARY" ] || printf '| Test | FS | Result | Took |\n| --- | --- | --- | --- |\n' >> "$GITHUB_STEP_SUMMARY"
  echo "| ${1#"$suite"-} | ${FS:-ext4} | $result | ${3:-} |" >> "$GITHUB_STEP_SUMMARY"
  [ "$2" != failed ] || echo "::error title=${1#"$suite"-} failed on ${FS:-ext4}::${1#"$suite"-} failed after $3${why:+ (${why})}; its console and timeline are in this job's logs artifact"
}
# Each driver test by its result-cache name, those still to run and their keys.
declare -A keys=() inputs=()
pending=()
if [ -n "$split" ]; then
  [ -n "$interactive" ] || python3 "$root/tests/vm/driver/hang.py"
  for name in "${names[@]}"; do [ "$name" = base ] || pending+=("$suite-$name"); done
fi
# Exported before the keys are taken, as cache.py hashes it: set after them, it made every power-cut pass's key change during its run.
[ "$suite" != powercut ] || export VM_TIMEOUT="${VM_TIMEOUT:-14400}"
if [ ${#pending[@]} -gt 0 ] && [ -z "$interactive" ]; then
  for name in "${pending[@]}"; do inputs[$name]=$(python3 "$cache_py" inputs "$name"); done
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
# ARCHIX_PREBUILT=DIR takes the binaries from tests/arch/run.sh's ARCHIX_BUILD_OUT=DIR instead of building them; the power-cut suite builds its own, with the test hooks' debug assertions.
if [ "$suite" = powercut ]; then
  docker run --rm "${mounts[@]}" -e CARGO_TARGET_DIR=/target/powercut -e CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true archix-arch-test bash -c 'set -e
    cargo build --locked --release && cp /target/powercut/release/archix /work/archix
    cargo build --locked --release --target x86_64-unknown-linux-musl && cp /target/powercut/x86_64-unknown-linux-musl/release/archix /work/archix-static'
elif [ -n "${ARCHIX_PREBUILT:-}" ]; then
  docker run --rm -v "$ARCHIX_PREBUILT":/prebuilt:ro -v "$work":/work archix-arch-test bash -c 'set -e
    tar -I zstd -xf /prebuilt/target.tar.zst -C /tmp target/release/archix target/x86_64-unknown-linux-musl/release/archix
    cp /tmp/target/release/archix /work/archix && cp /tmp/target/x86_64-unknown-linux-musl/release/archix /work/archix-static'
else
  docker run --rm "${mounts[@]}" archix-arch-test bash -c 'set -e
    cargo build --locked --release && cp /target/release/archix /work/archix
    cargo build --locked --release --target x86_64-unknown-linux-musl && cp /target/x86_64-unknown-linux-musl/release/archix /work/archix-static'
fi
# ARCHIX_TEST_ARCHIVE (e.g. a local caching mirror on http://127.0.0.1:8079) stands in for archive.archlinux.org; the VM reaches the host as 10.0.2.2.
archive=${ARCHIX_TEST_ARCHIVE:-https://archive.archlinux.org}
docker run --rm --privileged --network host "${mounts[@]}" -e ARCHIVE="$archive" -e VM_ARCHIVE="${archive/127.0.0.1/10.0.2.2}" archix-arch-test /vm/build-image.sh
# The power-cut tests of the first layout start from the plain image, and rootmove's from one with the root at the top level of its btrfs.
if [ "$suite" = powercut ]; then
  [[ " ${pending[*]} " != *" powercut-varlib "* ]] || cp --sparse=always "$work/disk.img" "$work/plain.img"
  [[ " ${pending[*]} " != *" powercut-rootmove "* ]] || docker run --rm --privileged --network host "${mounts[@]}" -e ARCHIVE="$archive" -e VM_ARCHIVE="${archive/127.0.0.1/10.0.2.2}" -e IMG=/work/top.img -e ROOT_AT_TOP=1 archix-arch-test /vm/build-image.sh
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
    echo "$name: the inputs changed during the run; not recording the pass. What changed:"
    diff <(printf '%s\n' "${inputs[$name]}") <(python3 "$cache_py" inputs "$name") | sed 's/^/  /' || true
  fi
}
# The runner's own heavy slot, which work-dir.sh took, goes to one VM at a time through own-slot.lock; the others wait for a free one, each holding it until its subshell exits.
take_slot() {
  while :; do
    exec {slot_fd}> "$work/own-slot.lock"
    flock -n "$slot_fd" && return
    exec {slot_fd}>&-
    for i in $(seq "${ARCHIX_HEAVY_JOBS:-2}"); do
      exec {slot_fd}> "${XDG_RUNTIME_DIR:-/tmp}/archix-heavy.$i.lock"
      flock -n "$slot_fd" && return
      exec {slot_fd}>&-
    done
    sleep 2
  done
}
# The split suites: the base lays the image out with this run's archix and powers off; each test then boots a qcow2 overlay of that disk with its own copy of the vars, in a heavy slot of its own.
if [ "${names[*]}" = base ]; then
  "${vm[@]}" python -m driver /vm/boot/base.py "${args[@]}"
  exit
fi
if ! "${vm[@]}" python -m driver /vm/boot/base.py; then
  for name in "${pending[@]}"; do summary "$name" failed "the base, which it needs, failed"; done
  echo "$suite: the base failed, so none of ${pending[*]#boot-} ran on ${FS:-ext4}"
  exit 1
fi
echo "base: laid out in $(($(date +%s) - started)) s from the start"
overlay() { echo "qemu-img create -q -f qcow2 -b /work/disk.img -F raw /work/$1/disk.qcow2 && cp /work/vars.fd /work/$1/vars.fd && exec python -m driver /vm/$suite/$1/test.py --work /work/$1 ${args[*]}"; }
if [ -n "$interactive" ]; then
  mkdir -p "$work/${names[0]}"
  "${vm[@]}" bash -c "$(overlay "${names[0]}")"
  exit
fi
declare -A running=()
for name in "${pending[@]}"; do
  test=${name#"$suite"-}
  mkdir -p "$work/$test"
  (
    take_slot
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
    record "$suite-$test" "$seconds" "$work/$test" || echo "warning: recording $test's pass failed"
    summary "$test" passed "$seconds s"
  else
    failed+=("$test")
    summary "$test" failed "$seconds s" "$work/$test/output.log"
    echo "== $test FAILED (exit $status) after $seconds s, started at +$offset s; its last lines:"
    tail -n 60 "$work/$test/output.log" || true
  fi
done
echo "$suite: ${#pending[@]} test(s) in $(($(date +%s) - started)) s on ${FS:-ext4}${failed[*]:+; failed: ${failed[*]}}"
[ ${#failed[@]} = 0 ]
