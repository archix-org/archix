#!/bin/bash
# The rollback torture test on CachyOS: its real repos ahead of Arch's, v3 packages, a power cut, kills and rescue, and rollbacks that reinstall CachyOS packages with its servers unreachable and pacman's cache empty.
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
  { echo "--- diagnostics"; ax status; ax generations; ls /var/lib/archix; tail -15 /var/log/pacman.log; } > /dev/console 2>&1
}
fail() { say "FAIL: $*"; diag; echo failed > "$state/stage"; sync; systemctl poweroff; exit 1; }
current() { ax generations | awk '$1 == "*" { print $2 }'; }
remember() { echo "$2" > "$state/$1"; }
recall() { cat "$state/$1"; }
# systemd-boot names the entry it started in LoaderEntrySelected: 4 attribute bytes, then archix-N.conf in UTF-16.
booted=$(tr -d '\000' < /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
arch_of() { pacman -Qi "$1" 2>/dev/null | sed -n 's/^Architecture *: //p'; }
installed() { pacman -Q "$1" > /dev/null 2>&1; }
journal_open() { [ -e /var/lib/archix/journal.json ]; }
missing_files() { pacman -Qk 2>&1 | grep -c 'No such file or directory'; }
# Output of runs on a full disk goes to tmpfs: a log on that disk would fail to write, and archix with it.
fill_disk() { fallocate -l "$(( $(df --output=avail -B1 / | tail -1) - $1 ))" /var/tmp/archix-fill; }
cut_power() { echo 1 > /proc/sys/kernel/sysrq; echo b > /proc/sysrq-trigger; for _ in 1 2 3 4 5 6 7 8 9 10; do nap; done; }
# Bash builtins only from here: mid-update, a library may be half-replaced and every external command fail.
nap() { read -rt 1 <> <(:) || :; }
kill_comm() { local p c; for p in /proc/[0-9]*; do read -r c < "$p/comm" 2>/dev/null && [ "$c" = "$1" ] && kill -9 "${p#/proc/}" 2>/dev/null; done; }
changed_since() {
  local n=0 line
  mapfile -s "$1" -t lines < /var/log/pacman.log
  for line in "${lines[@]}"; do [[ $line == *'[ALPM] '@(upgraded|downgraded|installed|reinstalled)* ]] && n=$((n + 1)); done
  echo "$n"
}
# Runs archix in the background and cuts the power (or kills it) once pacman has installed $2 packages.
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
# Runs archix in the background and kills it while a pacman hook holds the transaction that installs $1.
kill_after_installing() {
  local target=$1; shift
  printf '[Trigger]\nOperation = Install\nOperation = Upgrade\nType = Package\nTarget = %s\n\n[Action]\nWhen = PostTransaction\nExec = /bin/sh -c "touch /run/archix-vmtest-stalled; sleep 600"\n' "$target" > /etc/pacman.d/hooks/zz-archix-vmtest-stall.hook
  setsid archix --host archix-vm "$@" > "$state/killed.log" 2>&1 < /dev/null &
  local pid=$!
  for _ in $(seq 1200); do
    if [ -e /run/archix-vmtest-stalled ]; then
      kill -9 "$pid"; kill_comm archix; kill_comm pacman; kill_comm sleep
      rm -f /etc/pacman.d/hooks/zz-archix-vmtest-stall.hook /run/archix-vmtest-stalled
      nap
      return 0
    fi
    kill -0 "$pid" 2>/dev/null || { tail -20 "$state/killed.log" > /dev/console; fail "archix $* finished before pacman installed $target"; }
    nap
  done
  fail "pacman never installed $target during archix $*"
}
say "stage $stage: booted generation $booted, current generation $(current), kernel $(uname -r)"

case $stage in
  1)
    case $(uname -r) in *cachyos*) ;; *) fail "generation 1 should boot linux-cachyos, not $(uname -r)" ;; esac
    for pkg in coreutils htop jq; do
      [ "$(arch_of $pkg)" = x86_64_v3 ] || fail "$pkg should be CachyOS's x86_64_v3 build, not $(arch_of $pkg)"
    done
    grep -q '^coreutils = .*repo = "cachyos-core-v3"' "$lock" || fail "the lock should take coreutils from cachyos-core-v3: $(grep '^coreutils' "$lock")"
    grep -q '^htop = .*repo = "cachyos-extra-v3"' "$lock" || fail "the lock should take htop from cachyos-extra-v3: $(grep '^htop' "$lock")"
    ls /var/cache/archix/pkg/htop-*.pkg.tar.zst > /dev/null 2>&1 || fail "archix should keep a copy of htop, which pacstrap installed, for rollbacks"
    remember old "$(current)"
    say "PASS: generation $(recall old) runs $(uname -r) with CachyOS's v3 packages, and archix keeps copies of the CachyOS packages it adopted"

    sed -i 's|"htop", "jq" }, -- packages|"git", "vim" }, -- packages|' "$config"
    ax update --to 2026-10-18 --noconfirm > /dev/console 2>&1 || fail "archix update --to 2026-10-18"
    fill_disk $((20 * 1024 * 1024))
    ax switch --noconfirm > /run/archix-vmtest-full.log 2>&1 && fail "a switch with 20 MB free should fail"
    tail -5 /run/archix-vmtest-full.log > /dev/console
    grep -q "nothing was changed" /run/archix-vmtest-full.log || fail "the full-disk refusal should say nothing was changed"
    rm -f /var/tmp/archix-fill
    journal_open && fail "a switch that can't fit on the disk should fail before changing anything, but it left a journal"
    [ "$(current)" = "$(recall old)" ] || fail "the failed switch on a full disk changed the current generation"
    say "PASS: a full disk stops the switch before it changes anything"

    echo 2 > "$state/stage"; sync
    say "cutting the power in the middle of the switch"
    interrupt_after power 4 switch --noconfirm
    ;;
  2)
    grep -q "^generation $(recall old)," /var/lib/archix/last-rescue || fail "the initramfs should restore generation $(recall old)'s core packages after the cut"
    journal_open || fail "the power cut should leave the switch's journal"
    [ "$booted" = "$(recall old)" ] || fail "the old generation's entry should have booted"
    ax recover --continue > /dev/console 2>&1 || fail "archix recover --continue"
    journal_open && fail "recover --continue left the journal"
    installed git && installed vim || fail "recover --continue should finish installing git and vim"
    installed htop && fail "recover --continue should finish removing htop"
    [ "$(missing_files)" = 0 ] || fail "files are missing after recovering from the power cut: $(pacman -Qk 2>&1 | grep 'No such file' | head -5)"
    remember new "$(current)"
    say "PASS: after a power cut mid-switch the initramfs repaired the boot, and recover --continue finished it (generation $(recall new))"
    echo 3 > "$state/stage"
    systemctl reboot
    ;;
  3)
    [ "$booted" = "$(recall new)" ] || fail "generation $(recall new) should boot by default"
    git --version > /dev/null || fail "git should run"
    # From here on CachyOS's mirror is unreachable and pacman's cache is empty, as when a repo has dropped old builds.
    echo "127.0.0.1 mirror.cachyos.org" >> /etc/hosts
    curl -sf -m 10 -o /dev/null https://mirror.cachyos.org/ && fail "mirror.cachyos.org should be unreachable now"
    rm -f /var/cache/pacman/pkg/*
    say "PASS: generation $(recall new) boots; CachyOS's mirror is now unreachable and pacman's cache empty"
    bootctl set-oneshot "archix-$(recall old).conf" || fail "bootctl set-oneshot"
    echo 4 > "$state/stage"
    systemctl reboot
    ;;
  4)
    [ "$booted" = "$(recall old)" ] && [ "$(current)" = "$(recall old)" ] || fail "booting generation $(recall old)'s entry should roll back to it"
    [ "$(arch_of htop)" = x86_64_v3 ] && [ "$(arch_of jq)" = x86_64_v3 ] || fail "the boot-time rollback should reinstall CachyOS's htop and jq"
    installed git && fail "the boot-time rollback should remove git"
    say "PASS: booting the old entry reinstalled CachyOS's htop and jq from archix's copies, with the mirror unreachable"

    ax rollback "$(recall new)" --noconfirm > /dev/console 2>&1 || fail "archix rollback forward to generation $(recall new) with the mirror unreachable"
    [ "$(arch_of git)" = x86_64_v3 ] || fail "rolling forward should bring CachyOS's git back"
    say "PASS: rolled forward at runtime from archix's copies"

    say "killing a rollback halfway"
    kill_after_installing htop rollback "$(recall old)" --noconfirm
    journal_open || fail "the killed rollback should leave a journal"
    ax recover --rollback > /dev/console 2>&1 || fail "archix recover --rollback after a killed rollback"
    journal_open && fail "recover --rollback left the journal"
    [ "$(current)" = "$(recall new)" ] || fail "recover --rollback should leave generation $(recall new) current"
    [ "$(missing_files)" = 0 ] || fail "files are missing after recovering the killed rollback"
    say "PASS: a rollback killed halfway is undone by recover --rollback, with the mirror unreachable"

    ax rollback "$(recall old)" --noconfirm > /dev/console 2>&1 || fail "archix rollback to generation $(recall old)"
    rm -f /usr/lib/libalpm.so*
    pacman -V > /dev/null 2>&1 && fail "pacman should be broken now"
    bootctl set-oneshot "archix-$(recall old)-rescue.conf" || fail "bootctl set-oneshot rescue"
    echo 5 > "$state/stage"
    systemctl reboot
    ;;
  5)
    pacman -V > /dev/null 2>&1 || fail "the rescue entry should have restored pacman"
    grep -q "^generation $(recall old)," /var/lib/archix/last-rescue || fail "rescue should restore generation $(recall old), not $(cat /var/lib/archix/last-rescue)"
    [ "$(arch_of pacman)" != "" ] || fail "pacman should be registered again"
    [ "$(missing_files)" = 0 ] || fail "files are missing after rescue"
    say "PASS: the rescue entry restored CachyOS's pacman from archix's copies, with the mirror unreachable"
    echo done > "$state/stage"
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
