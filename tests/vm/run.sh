#!/usr/bin/env bash
# Boots archix deployments in QEMU with OVMF; `run.sh` tests the first layout on a booted system, deployments, boot entries, boot counting (also with archix bless failing on a read-only ESP), Secure Boot, the /etc merge, the fallback UKI without loader/entries and eject back to plain Arch (killed once, then rerun), `run.sh rollback` the rollback torture test, `run.sh nvidia` NVIDIA DKMS drivers across deployments (TO_DATE: the newer snapshot, 2026-10-10 by default), `run.sh cachyos` the rollback torture test on CachyOS's real repos, `run.sh health` refusing hibernation while a deployment is pending and boot.health_check's fallbacks (a firewall that blocks the network, a pulled cable), `run.sh pcr` signed PCR policies unlocking a LUKS volume with swtpm, `run.sh dkms2` DKMS with two kernels, `run.sh selfupgrade` archix upgrading itself from ARCHIX_OLD (HEAD~1 by default), `run.sh firewall` the firewall over ssh, `run.sh churn` a year of weekly switches, `run.sh recovery` a live-USB recovery (the ESP wiped and the current deployment damaged; a plain Arch rescue disk binds a kept deployment, chroots in and rolls back to it; it boots and switches again), `run.sh rescue` every deployment broken and a lost root password, each repaired through the rescue entry's `archix rescue` prompt behind its login, then under Secure Boot with root locked wrong and empty passwords refused, a wheel password let in and a TPM-bound LUKS volume asking for its passphrase, `run.sh powercut` the dm-log-writes power-cut replay (tests/vm/powercut/run.sh).
set -euo pipefail
suite=${1:-boot}
case $suite in upgrade | atomic) echo "the $suite suite tests the in-place model; tests/vm/run.sh selfupgrade replaces it" >&2; exit 2 ;; esac
root=$(cd "$(dirname "$0")/../.." && pwd)
[ "$suite" != powercut ] || exec "$root/tests/vm/powercut/run.sh"
# `run.sh boot2` is the boot suite on the Python driver (tests/vm/driver, docs/vm-testing.md): --interactive gives a REPL with the VM as machine, and a pass is recorded by its inputs' hash and skipped next time unless --no-cache.
driver= interactive= cache=1
case $suite in boot2) driver=boot ;; esac
for arg in "${@:2}"; do
  case $arg in
    --interactive) interactive=1 ;;
    --no-cache) cache= ;;
    *) echo "unknown option $arg" >&2; exit 2 ;;
  esac
done
[ -n "$driver" ] || [ -z "$interactive" ] || { echo "--interactive needs a driver suite (boot2)" >&2; exit 2; }
key=
if [ -n "$driver" ] && [ -z "$interactive" ]; then
  key=$(python3 "$root/tests/vm/driver/cache.py" key "$suite")
  [ -z "$cache" ] || ! python3 "$root/tests/vm/driver/cache.py" hit "$suite" "$key" || exit 0
fi
started=$(date +%s)
# One build volume per checkout, as in tests/arch/run.sh.
target=archix-target-$(printf %s "$root" | sha256sum | cut -c1-12)
. "$root/tests/vm/work-dir.sh" vm
"$root/tests/arch/image.sh"
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
  docker run --rm --network host --device /dev/kvm "${mounts[@]}" archix-arch-test /vm/boot.sh
  exit
fi
tty=() args=()
[ -z "$interactive" ] || tty=(-it) args=(--interactive)
docker run --rm "${tty[@]}" --network host --device /dev/kvm "${mounts[@]}" -e PYTHONPATH=/vm -e VM_TIMEOUT -e VM_MEM -e VM_CPUS archix-arch-test python -m driver "/vm/$driver/test.py" "${args[@]}"
[ -n "$key" ] || exit 0
# A file edited while the suite ran may not be what it tested, so the pass is recorded only for inputs that didn't change.
if [ "$(python3 "$root/tests/vm/driver/cache.py" key "$suite")" = "$key" ]; then
  python3 "$root/tests/vm/driver/cache.py" record "$suite" "$key" "$(($(date +%s) - started))"
else
  echo "the inputs changed during the run; not recording the pass"
fi
