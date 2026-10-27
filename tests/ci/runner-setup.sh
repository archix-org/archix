#!/usr/bin/env bash
# Readies a GitHub-hosted runner: KVM for the runner user (KVM=false skips it), loop devices, NEED_GB free for VM work dirs, and ARCHIX_VM_WORK on the roomiest disk.
set -euo pipefail
[ "${GITHUB_ACTIONS:-}" = true ] || { echo "this reconfigures the machine, so it only runs on a CI runner" >&2; exit 1; }
here=$(cd "$(dirname "$0")" && pwd)
need=${NEED_GB:-20}
if [ "${KVM:-true}" = true ]; then
  echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' | sudo tee /etc/udev/rules.d/99-kvm.rules > /dev/null
  sudo udevadm control --reload-rules
  sudo udevadm trigger --name-match=kvm --settle
  "$here/check-kvm.sh"
fi
# ubuntu-26.04's Docker keeps images in containerd's store, which compresses every layer of the 4.5 GB test image at each build (about 80 s a job).
if docker info -f '{{json .DriverStatus}}' | grep -q containerd.snapshotter; then
  conf=/etc/docker/daemon.json
  { [ -s "$conf" ] && sudo cat "$conf" || echo '{}'; } | jq '.features["containerd-snapshotter"] = false' > "$RUNNER_TEMP/daemon.json"
  sudo cp "$RUNNER_TEMP/daemon.json" "$conf"
  sudo systemctl restart docker
  docker info -f '{{.Driver}}'
fi
# Privileged containers see only the loop nodes that exist when they start, and the image builds and power-cut runs attach several.
for i in $(seq 0 63); do [ -e "/dev/loop$i" ] || sudo mknod -m 0660 "/dev/loop$i" b 7 "$i"; done
free_gb() { df -BG --output=avail "$1" | tail -1 | tr -dc 0-9; }
roomiest() {
  local best='' most=-1 dir
  for dir in /mnt "$RUNNER_TEMP"; do
    [ -d "$dir" ] || continue
    [ "$(free_gb "$dir")" -gt "$most" ] && best=$dir most=$(free_gb "$dir")
  done
  echo "$best"
}
base=$(roomiest)
if [ "$(free_gb "$base")" -lt "$need" ] || [ "$(free_gb /)" -lt 20 ]; then
  echo "freeing space: $(free_gb "$base") GB free on $base, $(free_gb /) GB on /"
  sudo rm -rf /usr/share/dotnet /usr/local/lib/android /opt/ghc /usr/local/.ghcup /opt/hostedtoolcache/CodeQL /usr/share/swift /usr/local/share/boost
  base=$(roomiest)
fi
df -h / "$base"
[ "$(free_gb "$base")" -ge "$need" ] || { echo "::error::only $(free_gb "$base") GB free on $base; this job needs $need GB" >&2; exit 1; }
sudo mkdir -p "$base/archix-ci/work"
sudo chown -R "$(id -u):$(id -g)" "$base/archix-ci"
echo "ARCHIX_VM_WORK=$base/archix-ci/work" >> "$GITHUB_ENV"
# The work-dir guard's defaults are for a shared desktop; a runner is sized to the job by NEED_GB.
echo "ARCHIX_MIN_FREE_GB=$need" >> "$GITHUB_ENV"
echo "ARCHIX_FLOOR_GB=3" >> "$GITHUB_ENV"
echo "VM_CPUS=$(nproc)" >> "$GITHUB_ENV"
