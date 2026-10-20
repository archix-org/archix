# Sourced by the VM runners: makes $work (or takes ARCHIX_VM_WORK) and removes a made one on exit unless ARCHIX_VM_KEEP=1.
. "$root/tests/heavy-slot.sh"
work=${ARCHIX_VM_WORK:-}
if [ -z "$work" ]; then
  work=$(mktemp -d "/tmp/archix-$1.XXXXXX")
  remove_work() {
    if [ -n "${ARCHIX_VM_KEEP:-}" ]; then
      echo "kept work dir: $work ($(du -sh "$work" 2> /dev/null | cut -f1 || true))"
      return 0
    fi
    # The containers wrote most of it as root.
    docker run --rm -v "$work":/w archix-arch-test find /w -mindepth 1 -delete > /dev/null 2>&1 || true
    rm -rf "$work" 2> /dev/null || true
    [ ! -e "$work" ] || echo "couldn't remove the work dir $work" >&2
    return 0
  }
  trap remove_work EXIT
  trap 'exit 130' INT TERM
fi
echo "work dir: $work"
