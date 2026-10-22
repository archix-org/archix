# Sourced by the VM runners: makes $work (or takes ARCHIX_VM_WORK) and removes a made one on exit unless ARCHIX_VM_KEEP=1.
. "$root/tests/heavy-slot.sh"
work=${ARCHIX_VM_WORK:-}
made_work=
if [ -z "$work" ]; then
  work=$(mktemp -d "/tmp/archix-$1.XXXXXX")
  made_work=1
fi
echo "work dir: $work"
# A run that fills the disk takes the whole host down with it, so it won't start short of ARCHIX_MIN_FREE_GB and is killed below ARCHIX_FLOOR_GB.
free_gb() { df --output=avail -BG "$work" | tail -1 | tr -dc 0-9; }
if [ "$(free_gb)" -lt "${ARCHIX_MIN_FREE_GB:-100}" ]; then
  echo "only $(free_gb) GB free for $work, under ARCHIX_MIN_FREE_GB=${ARCHIX_MIN_FREE_GB:-100}; not starting" >&2
  [ -z "$made_work" ] || rmdir "$work"
  exit 1
fi
(
  while sleep 10; do
    [ "$(free_gb)" -lt "${ARCHIX_FLOOR_GB:-40}" ] || continue
    echo "only $(free_gb) GB left on the disk; killing the run. Largest files in $work:" >&2
    du -ah "$work" 2> /dev/null | sort -h | tail -5 >&2
    # The runner waits on its foreground docker run before it handles a signal, so its containers go first.
    docker ps -q --filter volume="$work" | xargs -r docker kill > /dev/null
    kill -TERM $$
    exit
  done
) &
disk_guard=$!
remove_work() {
  kill "$disk_guard" 2> /dev/null || true
  [ -n "$made_work" ] || return 0
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
