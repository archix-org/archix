#!/usr/bin/env bash
# Usage, in the test VM (tests/arch/run.sh starts it): in-vm.sh single CARGO-TEST-ARGS... | build | tests LIST RESULTS; what the test container used to run, with the archive proxy on 127.0.0.1 as before.
set -euo pipefail
umask 022
export USER=root CARGO_TARGET_DIR=/target ARCHIX_TEST_TODAY
if [ -n "${PROXY_PORT:-}" ]; then
  url=http://127.0.0.1:$PROXY_PORT
  systemd-run --quiet --unit=archive-relay --socket-property="ListenStream=127.0.0.1:$PROXY_PORT" \
    /usr/lib/systemd/systemd-socket-proxyd "$PROXY_HOST:$PROXY_PORT"
  if curl -sf -m 60 -o /dev/null "$url/repos/$SNAPSHOT/core/os/x86_64/core.db"; then
    export ARCHIX_TEST_ARCHIVE=$url
    sed -i "s|https://archive.archlinux.org/|$url/|" /etc/pacman.d/mirrorlist
    # A request that misses the proxy then fails instead of passing while the archive happens to be up.
    for a in $(getent ahosts archive.archlinux.org | cut -d" " -f1 | sort -u); do ip route add blackhole "$a" || [ "${a#*:}" != "$a" ] || exit 1; done
  else
    [ -z "${ARCHIX_TEST_OFFLINE:-}" ] || { echo "the offline VM can't reach the archive proxy" >&2; exit 1; }
    echo "warning: the VM can't reach the archive proxy, so the tests use archive.archlinux.org" >&2
  fi
fi
[ -z "${ARCHIX_TEST_OFFLINE:-}" ] || export CARGO_NET_OFFLINE=true
cd /src
mode=$1
shift
case $mode in
  single)
    # The initramfs runs the static musl build, which can lack libc items the glibc build has.
    cargo build --locked --release --target x86_64-unknown-linux-musl
    # Arguments after a -- of their own, such as --nocapture, go to the test binaries with --include-ignored.
    case " $* " in
      *" -- "*) exec cargo test --locked --no-fail-fast "$@" --include-ignored ;;
      *) exec cargo test --locked --no-fail-fast "$@" -- --include-ignored ;;
    esac ;;
  build)
    if [ -n "${PREBUILT:-}" ]; then
      tar -I zstd -xf /prebuilt/target.tar.zst -C /
      cp /prebuilt/tests.tsv /out/tests.tsv
    else
      cargo build --locked --release --target x86_64-unknown-linux-musl
      # The glibc release build is only for the VM suites, which take it from ARCHIX_BUILD_OUT.
      [ -z "${BUILD_OUT:-}" ] || cargo build --locked --release
      /src/tests/arch/list-tests.sh /out/tests.tsv
      # Doc tests have no executable of their own to hand a shard, so the build runs them.
      cargo test --locked --doc -- --include-ignored
    fi
    if [ -n "${BUILD_OUT:-}" ]; then
      tar -I "zstd -T0" -cf /out/target.tar.zst /target/release/archix /target/debug/archix \
        /target/x86_64-unknown-linux-musl/release/archix $(cut -f2 /out/tests.tsv | sort -u)
      # A cached target keeps only the dependencies: archix's own artifacts would go stale against a fresh checkout's mtimes.
      if [ -n "${PRUNE_TARGET:-}" ]; then
        rm -rf /target/*/incremental /target/*/*/incremental
        find /target -path "*/deps/*" \( -name "libarchix-*" -o ! -name "lib*" ! -name "*.d" \) -delete
        find /target -maxdepth 4 \( -path "*/.fingerprint/archix-*" -o -path "*/build/archix-*" \) -prune -exec rm -rf {} +
        find /target -maxdepth 3 -type f -name "archix*" -delete
        du -sh /target /root/.cargo/registry
      fi
    fi ;;
  tests) exec /src/tests/arch/run-tests.sh "$@" ;;
  *) echo "in-vm.sh: unknown mode $mode" >&2; exit 2 ;;
esac
