# Sourced by the test runners: waits for one of ARCHIX_HEAVY_JOBS (default 2) slots, held until the runner exits, so parallel runs can't stall the host's writeback.
heavy_slots=${ARCHIX_HEAVY_JOBS:-2}
heavy_waited=
while :; do
  for heavy_i in $(seq "$heavy_slots"); do
    exec {heavy_fd}> "${XDG_RUNTIME_DIR:-/tmp}/archix-heavy.$heavy_i.lock"
    flock -n "$heavy_fd" && break 2
    exec {heavy_fd}>&-
  done
  [ -n "$heavy_waited" ] || { echo "waiting for one of $heavy_slots test slots" >&2; heavy_waited=1; }
  sleep 5
done
