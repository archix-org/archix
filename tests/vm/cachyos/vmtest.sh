#!/bin/bash
# The rollback torture test on CachyOS: its real repos ahead of Arch's, v3 packages, a power cut, a kill, and a dropped deployment rebuilt with its servers unreachable and pacman's cache empty.
set -u
shopt -s extglob
state=/var/lib/archix-vmtest
mkdir -p "$state"
stage=$(cat "$state/stage" 2>/dev/null || echo 1)
config=/etc/archix/init.lua
lock=/etc/archix/archix.lock
ax() { archix --host archix-vm "$@"; }
say() { echo "VMTEST: $*" > /dev/console; }
diag() {
  { echo "--- diagnostics"; ax status; ax generations; ls -a /archix/deploy; ls /var/lib/archix; tail -15 /var/log/pacman.log; } > /dev/console 2>&1
}
fail() { say "FAIL: $*"; diag; echo failed > "$state/stage"; sync; systemctl poweroff; exit 1; }
current() { ax generations | awk '$1 == "*" { print $2 }'; }
deployments() { ls /archix/deploy | sort -n | tr '\n' ' '; }
staging_left() { ls -d /archix/deploy/.staging-* > /dev/null 2>&1; }
remember() { echo "$2" > "$state/$1"; }
recall() { cat "$state/$1"; }
next() { echo "$1" > "$state/stage"; sync; }
# systemd-boot names the entry it started in LoaderEntrySelected: 4 attribute bytes, then archix-N.conf in UTF-16.
booted=$(tr -d '\000' < /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
arch_of() { pacman -Qi "$1" 2>/dev/null | sed -n 's/^Architecture *: //p'; }
installed() { pacman -Q "$1" > /dev/null 2>&1; }
missing_files() { pacman -Qk 2>&1 | grep -c 'No such file or directory'; }
# What pacman -Qkk finds wrong in deployment $1's /usr, /opt and /boot, the trees a staged deployment shares with it.
findings() { archix-check-deployment "$1" | grep -E ': /(usr|opt|boot)/' || true; }
baseline() { findings "$1" > "$state/baseline-$1"; }
unchanged() { findings "$2" | diff "$state/baseline-$2" - > "$state/diff-$2" || fail "$1 changed deployment $2's files: $(head -c 600 "$state/diff-$2")"; }
# Output of runs on a full disk goes to tmpfs: a log on that disk would fail to write, and archix with it.
fill_disk() { fallocate -l "$(( $(df --output=avail -B1 / | tail -1) - $1 ))" /var/tmp/archix-fill; }
cut_power() { echo 1 > /proc/sys/kernel/sysrq; echo b > /proc/sysrq-trigger; for _ in 1 2 3 4 5 6 7 8 9 10; do nap; done; }
nap() { read -rt 1 <> <(:) || :; }
kill_comm() { local p c; for p in /proc/[0-9]*; do read -r c < "$p/comm" 2>/dev/null && [ "$c" = "$1" ] && kill -9 "${p#/proc/}" 2>/dev/null; done; }
changed_since() {
  local n=0 line
  mapfile -s "$1" -t lines < /var/log/pacman.log
  for line in "${lines[@]}"; do [[ $line == *'[ALPM] '@(upgraded|downgraded|installed|reinstalled|removed)* ]] && n=$((n + 1)); done
  echo "$n"
}
# Runs archix in the background and cuts the power (or kills it) once pacman, staging the deployment, has changed $2 packages.
interrupt_after() {
  local how=$1 count=$2; shift 2
  local start; start=$(wc -l < /var/log/pacman.log)
  setsid archix --host archix-vm "$@" > "$state/interrupted.log" 2>&1 < /dev/null &
  local pid=$!
  for _ in $(seq 7200); do
    if [ "$(changed_since "$start")" -ge "$count" ]; then
      if [ "$how" = power ]; then cut_power; fi
      kill_comm pacman; kill -9 "$pid"; kill_comm archix
      nap
      return 0
    fi
    kill -0 "$pid" 2>/dev/null || { tail -20 "$state/interrupted.log" > /dev/console; fail "archix $* finished before it could be interrupted"; }
    nap
  done
  fail "pacman never got to $count packages"
}
say "stage $stage: booted $booted, current $(current), deployments $(deployments), kernel $(uname -r)"

case $stage in
  1)
    case $(uname -r) in *cachyos*) ;; *) fail "the image should boot linux-cachyos, not $(uname -r)" ;; esac
    for pkg in coreutils htop jq; do
      [ "$(arch_of $pkg)" = x86_64_v3 ] || fail "$pkg should be CachyOS's x86_64_v3 build, not $(arch_of $pkg)"
    done
    grep -q '^coreutils = .*repo = "cachyos-core-v3"' "$lock" || fail "the lock should take coreutils from cachyos-core-v3: $(grep '^coreutils' "$lock")"
    grep -q '^htop = .*repo = "cachyos-extra-v3"' "$lock" || fail "the lock should take htop from cachyos-extra-v3: $(grep '^htop' "$lock")"
    ls /var/cache/archix/pkg/htop-*.pkg.tar.zst > /dev/null 2>&1 || fail "archix should keep a copy of htop, which pacstrap installed, for rebuilds"
    remember old "$(current)"
    say "PASS: deployment $(recall old) runs $(uname -r) with CachyOS's v3 packages, and archix keeps copies of the CachyOS packages it adopted"

    sed -i 's|"htop", "jq" }, -- packages|"git", "vim" }, -- packages|' "$config"
    ax update --to 2026-10-18 --noconfirm > /dev/console 2>&1 || fail "archix update --to 2026-10-18"
    kept=$(deployments)
    fill_disk $((20 * 1024 * 1024))
    ax switch --noconfirm > /run/archix-vmtest-full.log 2>&1 && fail "a switch with 20 MB free should fail"
    tail -5 /run/archix-vmtest-full.log > /dev/console
    grep -q "nothing was changed" /run/archix-vmtest-full.log || fail "the full-disk refusal should say nothing was changed"
    rm -f /var/tmp/archix-fill
    [ "$(current)" = "$(recall old)" ] || fail "the failed switch on a full disk changed the current deployment"
    [ "$(deployments)" = "$kept" ] && ! staging_left || fail "the failed switch on a full disk left something behind: $(ls -a /archix/deploy)"
    say "PASS: a full disk stops the switch before it changes anything"

    remember kept "$kept"
    baseline "$(recall old)"
    next 2
    say "cutting the power in the middle of the switch"
    interrupt_after power 4 switch --noconfirm
    ;;
  2)
    [ "$booted" = "$(recall old)" ] && [ "$(current)" = "$(recall old)" ] || fail "after a power cut mid-switch, deployment $(recall old) should still boot and be current"
    [ "$(deployments)" = "$(recall kept)" ] || fail "the cut switch committed a deployment: $(deployments)"
    installed htop && ! installed git || fail "the power cut changed the running deployment's packages"
    [ "$(missing_files)" = 0 ] || fail "files are missing after the power cut: $(pacman -Qk 2>&1 | grep 'No such file' | head -5)"
    unchanged "the power cut mid-switch" "$(recall old)"
    say "PASS: a power cut mid-switch left the running deployment whole, by content"
    ax switch --noconfirm > /dev/console 2>&1 || fail "the switch after the power cut"
    staging_left && fail "the next switch should clear the cut switch's staging tree"
    unchanged "staging the switch after the power cut" "$(recall old)"
    remember new "$(current)"
    next 3
    systemctl reboot
    ;;
  3)
    [ "$booted" = "$(recall new)" ] || fail "deployment $(recall new) should boot by default"
    git --version > /dev/null || fail "git should run"
    [ "$(arch_of git)" = x86_64_v3 ] || fail "git should be CachyOS's v3 build"
    installed htop && fail "htop should be gone in deployment $(recall new)"
    sed -i 's|"git", "vim" }, -- packages|"git", "vim", "tree" }, -- packages|' "$config"
    baseline "$(recall new)"
    ax switch --noconfirm > /dev/console 2>&1 || fail "switch adding tree"
    unchanged "staging the switch adding tree" "$(recall new)"
    [ -d "/archix/deploy/$(recall old)" ] && fail "keep = 2 should have dropped deployment $(recall old)"
    # From here on CachyOS's mirror is unreachable, in every deployment, and pacman's cache is empty, as when a repo has dropped old builds.
    for hosts in /archix/deploy/*/etc/hosts; do echo "127.0.0.1 mirror.cachyos.org" >> "$hosts"; done
    curl -sf -m 10 -o /dev/null https://mirror.cachyos.org/ && fail "mirror.cachyos.org should be unreachable now"
    rm -f /var/cache/pacman/pkg/*
    say "PASS: CachyOS's mirror is now unreachable and pacman's cache empty"

    kept=$(deployments)
    say "killing a rollback that rebuilds generation $(recall old)"
    interrupt_after kill 1 rollback "$(recall old)" --noconfirm
    [ "$(current)" != "$(recall old)" ] && [ "$(deployments)" = "$kept" ] || fail "the killed rollback committed: $(deployments)"
    installed git && [ "$(missing_files)" = 0 ] || fail "the killed rollback changed the running deployment"
    ax rollback "$(recall old)" --noconfirm > /dev/console 2>&1 || fail "rebuilding generation $(recall old) with the mirror unreachable"
    [ "$(current)" = "$(recall old)" ] || fail "the rebuilt deployment $(recall old) should be current"
    staging_left && fail "the rebuild should clear the killed one's staging tree"
    unchanged "the killed rebuild and the rebuild of deployment $(recall old)" "$(recall new)"
    next 4
    systemctl reboot
    ;;
  4)
    [ "$booted" = "$(recall old)" ] && [ "$(current)" = "$(recall old)" ] || fail "the rebuilt deployment $(recall old) should boot"
    case $(uname -r) in *cachyos*) ;; *) fail "the rebuilt deployment should boot linux-cachyos, not $(uname -r)" ;; esac
    [ "$(arch_of htop)" = x86_64_v3 ] && [ "$(arch_of jq)" = x86_64_v3 ] || fail "the rebuild should reinstall CachyOS's htop and jq"
    installed git && fail "the rebuilt deployment shouldn't have git"
    [ "$(missing_files)" = 0 ] || fail "files are missing in the rebuilt deployment"
    say "PASS: a killed rebuild changed nothing, and the next one rebuilt the old deployment from archix's copies of CachyOS's packages, with the mirror unreachable"
    ax rollback "$(recall new)" --noconfirm > /dev/console 2>&1 || fail "archix rollback to deployment $(recall new)"
    [ "$(current)" = "$(recall new)" ] || fail "deployment $(recall new) should be current"
    next done
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
