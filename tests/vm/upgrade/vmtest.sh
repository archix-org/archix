#!/bin/bash
# Upgrading archix itself: the previous release switches to the new archix, which rolls back across that upgrade at runtime, killed halfway and from the boot menu.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
stage=$(cat "$state/stage" 2>/dev/null || echo 1)
repo=/srv/archix-repo
lock=/etc/archix/archix.lock
ax() { archix --host archix-vm "$@"; }
say() { echo "VMTEST: $*" > /dev/console; }
fail() {
  say "FAIL: $*"
  { ax status; ax generations; ls /var/lib/archix; tail -15 /var/log/pacman.log; systemctl status archix-boot.service; } > /dev/console 2>&1
  echo failed > "$state/stage"; sync; systemctl poweroff; exit 1
}
current() { ax generations | awk '$1 == "*" { print $2 }'; }
remember() { echo "$2" > "$state/$1"; }
recall() { cat "$state/$1"; }
booted=$(tr -d '\000' < /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
version() { pacman -Q archix | awk '{print $2}'; }
journal_open() { [ -e /var/lib/archix/journal.json ]; }
unit=/etc/systemd/system/archix-boot.service
boot_check_ran() {
  [ "$(systemctl show -p Result --value archix-boot.service)" = success ] && [ "$(systemctl show -p ExecMainStatus --value archix-boot.service)" = 0 ] \
    || fail "archix-boot.service should have run cleanly at this boot: $(grep ExecStart "$unit"); $(journalctl -b -u archix-boot.service | tail -3)"
}
# Puts package version $1 of archix (built from the previous release for 1, from this tree for 2 and up) into the local repo and locks it.
publish() {
  cp "/srv/archix-next/archix-$1-1-x86_64.pkg.tar.zst" "$repo/" && repo-add -q "$repo/archix-self.db.tar.gz" "$repo/archix-$1-1-x86_64.pkg.tar.zst" || fail "publishing archix $1"
  ax update --to 2026-10-10 --noconfirm > /dev/console 2>&1 || fail "locking archix $1"
  grep -q "^archix = .*version = \"$1-1\"" "$lock" || fail "the lock should hold archix $1-1, not: $(grep '^archix' "$lock")"
}
# Runs archix in the background and kills it while pacman's hooks run, once pacman has installed the archix package.
kill_after_install() {
  printf '[Trigger]\nOperation = Install\nOperation = Upgrade\nType = Package\nTarget = archix\n\n[Action]\nWhen = PostTransaction\nExec = /bin/sh -c "touch /run/archix-vmtest-stalled; sleep 600"\n' > /etc/pacman.d/hooks/zz-archix-vmtest-stall.hook
  setsid archix --host archix-vm "$@" > "$state/killed.log" 2>&1 < /dev/null &
  local pid=$!
  for _ in $(seq 1200); do
    if [ -e /run/archix-vmtest-stalled ]; then
      kill -9 "$pid"; pkill -9 -x archix; pkill -9 -x pacman; pkill -9 -x sleep
      rm -f /etc/pacman.d/hooks/zz-archix-vmtest-stall.hook /run/archix-vmtest-stalled
      sleep 1
      return 0
    fi
    kill -0 "$pid" 2>/dev/null || { tail -20 "$state/killed.log" > /dev/console; fail "archix $* finished before pacman installed archix"; }
    sleep 0.5
  done
  fail "pacman never installed archix during archix $*"
}
say "stage $stage: booted generation $booted, current generation $(current), archix package $(version)"

case $stage in
  1)
    [ "$(version)" = 1-1 ] || fail "generation 1 should run archix 1, the previous release"
    remember previous "$(current)"
    publish 2
    ax switch --noconfirm > /dev/console 2>&1 || fail "the previous release switching to the new archix"
    [ "$(version)" = 2-1 ] || fail "the switch should install archix 2"
    remember new "$(current)"
    say "PASS: the previous release switched to the new archix (generation $(recall new))"
    echo 2 > "$state/stage"
    systemctl reboot
    ;;
  2)
    [ "$booted" = "$(recall new)" ] || fail "generation $(recall new) should boot by default"
    boot_check_ran
    ax generations > /dev/console 2>&1 || fail "the new archix listing the previous release's generations"
    ax status > /dev/console 2>&1 || fail "the new archix reading the previous release's state"
    ax switch --noconfirm > /dev/console 2>&1 || fail "the new archix switching with the lock the previous release wrote"
    say "PASS: the new archix boots, and reads the generations, lock and state the previous release wrote"

    publish 3
    ax switch --noconfirm > /dev/console 2>&1 || fail "the new archix switching to a newer build of itself"
    [ "$(version)" = 3-1 ] || fail "the switch should install archix 3"
    grep -qx 'ExecStart=/usr/bin/archix boot-check' "$unit" || fail "after archix replaced itself in a switch, archix-boot.service should still run it: $(grep ExecStart "$unit")"
    remember newer "$(current)"
    say "PASS: archix replaced itself in a switch and archix-boot.service still runs /usr/bin/archix"

    ax rollback "$(recall previous)" --noconfirm > /dev/console 2>&1 || fail "the new archix rolling back to the previous release's generation"
    [ "$(version)" = 1-1 ] || fail "the rollback should put archix 1 back"
    [ "$(current)" = "$(recall previous)" ] || fail "generation $(recall previous) should be current"
    ax generations > /dev/console 2>&1 || fail "the previous release listing generations the new archix made"
    ax status > /dev/console 2>&1 || fail "the previous release reading the new archix's state"
    say "PASS: the new archix rolled back to the previous release at runtime, which reads the new archix's state"

    ax rollback "$(recall newer)" --noconfirm > /dev/console 2>&1 || fail "the previous release rolling forward to the new archix"
    [ "$(version)" = 3-1 ] || fail "rolling forward should install archix 3"
    say "killing a rollback to the previous release once pacman has installed it"
    kill_after_install rollback "$(recall previous)" --noconfirm
    [ "$(version)" = 1-1 ] || fail "the killed rollback should already have installed archix 1"
    journal_open || fail "the killed rollback should leave its journal"
    ax recover --continue > /dev/console 2>&1 || fail "the previous release finishing the rollback the new archix started"
    journal_open && fail "recover --continue left the journal"
    [ "$(current)" = "$(recall previous)" ] || fail "recover --continue should make generation $(recall previous) current"
    say "PASS: the previous release finished a rollback the new archix started and was killed in"

    ax rollback "$(recall newer)" --noconfirm > /dev/console 2>&1 || fail "rolling forward to the new archix again"
    bootctl set-oneshot "archix-$(recall previous).conf" || fail "bootctl set-oneshot"
    echo 3 > "$state/stage"
    systemctl reboot
    ;;
  3)
    [ "$booted" = "$(recall previous)" ] || fail "should have booted generation $(recall previous)'s entry"
    boot_check_ran
    [ "$(current)" = "$(recall previous)" ] || fail "the new archix should roll back to generation $(recall previous) at boot"
    [ "$(version)" = 1-1 ] || fail "the boot-time rollback should put archix 1 back"
    say "PASS: booting the previous release's entry made the new archix roll back to it"
    bootctl set-oneshot "archix-$(recall newer).conf" || fail "bootctl set-oneshot"
    echo 4 > "$state/stage"
    systemctl reboot
    ;;
  4)
    [ "$booted" = "$(recall newer)" ] || fail "should have booted generation $(recall newer)'s entry"
    boot_check_ran
    [ "$(current)" = "$(recall newer)" ] || fail "the previous release should roll forward to generation $(recall newer) at boot"
    [ "$(version)" = 3-1 ] || fail "the boot-time rollback should put archix 3 back"
    ax switch --noconfirm > /dev/console 2>&1 || fail "a switch after rolling forward from the boot menu"
    say "PASS: booting the new archix's entry made the previous release roll forward to it"
    echo done > "$state/stage"
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
