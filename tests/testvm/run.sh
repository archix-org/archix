#!/usr/bin/env bash
# Runs a job script in a fresh test VM (tests/testvm/image.sh), the host's only part being QEMU with /dev/kvm in a plain container, as the calling user: nothing in the job reaches the host's devices.
# usage: run.sh [--mem SIZE] [--cpus N] [--share TAG=DIR[:ro] --mount 9p TAG POINT ro|rw]... [--disk SERIAL=FILE[:SIZE][:overlay] --mount disk SERIAL POINT rw]... [--env NAME=VALUE]... [--proxy PORT] [--offline] [--cwd DIR] -- JOB [ARGS...]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
mem=${ARCHIX_TESTVM_MEM:-12G} cpus=${ARCHIX_TESTVM_CPUS:-$(( $(nproc) > 8 ? 8 : $(nproc) ))}
args=() mounts=() envs=() volumes=() proxy=0 offline=
while [ $# -gt 0 ]; do
  case $1 in
    --mem) mem=$2; shift ;;
    --cpus) cpus=$2; shift ;;
    --share) args+=(--share "$2"); dir=${2#*=}; [[ $dir != *:ro ]] || dir=${dir::-3}; volumes+=(-v "$dir:$dir"); shift ;;
    --disk) args+=(--disk "$2"); file=${2#*=}; file=${file%%:*}; volumes+=(-v "$(dirname "$file"):$(dirname "$file")"); shift ;;
    --mount) mounts+=("$2 $3 $4 $5"); shift 4 ;;
    --env) envs+=("$2"); shift ;;
    --proxy) proxy=$2; shift ;;
    --offline) offline=1 ;;
    --cwd) envs+=("TESTVM_CWD=$2"); shift ;;
    --) shift; break ;;
    *) echo "testvm: unknown option $1" >&2; exit 2 ;;
  esac
  shift
done
[ $# -gt 0 ] || { echo "testvm: no job given" >&2; exit 2; }
image=$("$here/image.sh")
# On the disk, not /tmp, which may be RAM: the VM's root overlay grows with what the job writes.
runs=${ARCHIX_TESTVM_CACHE:-$HOME/.cache/archix-testvm}/runs
mkdir -p "$runs"
ctl=$(mktemp -d "$runs/run.XXXXXX")
name=archix-testvm-${ctl##*.}
# A runner stopped or killed takes its VM with it, rather than leaving QEMU running in its container.
trap 'docker rm -f "$name" > /dev/null 2>&1; rm -rf "$ctl"' EXIT
trap 'exit 130' INT TERM
mkdir "$ctl/share" "$ctl/work"
printf '%s\n' "${mounts[@]}" > "$ctl/share/mounts"
: > "$ctl/share/env"
for e in "${envs[@]}"; do printf '%s=%q\n' "${e%%=*}" "${e#*=}" >> "$ctl/share/env"; done
{ printf 'set -e\n'; printf '%q ' "$@"; printf '\n'; } > "$ctl/share/job"
kvm=$(stat -c %g /dev/kvm)
docker run --rm --name "$name" --network host --device /dev/kvm --user "$(id -u):$(id -g)" --group-add "$kvm" \
  -v "$image:$image:ro" -v "$ctl:$ctl" -v "$here:/testvm:ro" "${volumes[@]}" archix-arch-test \
  python3 /testvm/vm.py --image "$image" --ctl "$ctl/share" --work "$ctl/work" --mem "$mem" --cpus "$cpus" \
  ${proxy:+--proxy "$proxy"} ${offline:+--offline} "${args[@]}"
