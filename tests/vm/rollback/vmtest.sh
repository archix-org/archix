#!/bin/bash
# Rollback torture test: a real base-system update (2026-06-01 to 2026-10-10), power cuts, kills, a full disk, a broken glibc and a missing key.
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
newest() { ax generations | awk '{ n = ($1 == "*") ? $2 : $1 } END { print n }'; }
remember() { echo "$2" > "$state/$1"; }
recall() { cat "$state/$1"; }
# systemd-boot names the entry it started in LoaderEntrySelected: 4 attribute bytes, then archix-N.conf in UTF-16.
booted=$(tr -d '\000' < /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
glibc() { pacman -Q glibc | awk '{print $2}' | cut -d+ -f1; }
kernel_pkg() { pacman -Q linux | awk '{print $2}'; }
# The DKMS module, which must be built for and loadable by the running kernel, at the version the generation locks.
dkms_module() {
  local want=$1 have
  have=$(modinfo -F version v4l2loopback 2>/dev/null)
  [ "$have" = "$want" ] || fail "v4l2loopback $want should be built by DKMS for $(uname -r), found '${have:-nothing}': $(dkms status 2>&1 | head -5)"
  case $(modinfo -n v4l2loopback) in */updates/dkms/*) ;; *) fail "v4l2loopback should come from DKMS, not $(modinfo -n v4l2loopback)" ;; esac
  modprobe v4l2loopback || fail "v4l2loopback $want should load on $(uname -r)"
  dkms status v4l2loopback | grep -q "v4l2loopback/$want, $(uname -r), x86_64: installed" || fail "dkms should list v4l2loopback/$want as installed for $(uname -r): $(dkms status v4l2loopback)"
}
journal_open() { [ -e /var/lib/archix/journal.json ]; }
# Files that reference a GLIBC symbol version the installed glibc lacks; empty when the system is consistent.
needs_newer_glibc() {
  for f in /usr/bin/* /usr/lib/*.so*; do
    [ -f "$f" ] && [ ! -L "$f" ] || continue
    /usr/lib/ld-linux-x86-64.so.2 --list "$f" 2>&1 | grep -q "version .GLIBC_[0-9.]*' not found" && echo "$f"
  done
}
# Files pacman owns that are missing from disk.
missing_files() { pacman -Qk 2>&1 | grep -c 'No such file or directory'; }
fill_disk() { fallocate -l "$(( $(df --output=avail -B1 / | tail -1) - $1 ))" /var/tmp/archix-fill; }
cut_power() { echo 1 > /proc/sys/kernel/sysrq; echo b > /proc/sysrq-trigger; for _ in 1 2 3 4 5 6 7 8 9 10; do nap; done; }
# Bash builtins only from here: mid-update, glibc itself may be half-replaced and every external command fail.
nap() { read -rt 1 <> <(:) || :; }
kill_comm() { local p c; for p in /proc/[0-9]*; do read -r c < "$p/comm" 2>/dev/null && [ "$c" = "$1" ] && kill -9 "${p#/proc/}" 2>/dev/null; done; }
changed_since() {
  local n=0 line
  mapfile -s "$1" -t lines < /var/log/pacman.log
  for line in "${lines[@]}"; do [[ $line == *'[ALPM] '@(upgraded|downgraded|installed|reinstalled)* ]] && n=$((n + 1)); done
  echo "$n"
}
# Runs archix in the background and cuts the power (or kills it) once pacman has upgraded or installed $2 packages.
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
say "stage $stage: booted generation $booted, current generation $(current), kernel $(uname -r), glibc $(glibc)"

case $stage in
  1)
    [ "$(glibc)" = 2.43 ] || fail "generation 1 should have glibc 2.43 (the 2026-06-01 snapshot)"
    sed -i 's|"devtools" }, -- packages|"devtools", "archix-test-hello", "v4l2loopback-dkms" }, -- packages|' "$config"
    ax switch --noconfirm > /dev/console 2>&1 || fail "switch adding the local recipe and a DKMS module"
    [ "$(cat /usr/share/archix-test-hello/built-with)" = 2.43 ] || fail "the local recipe should be built against glibc 2.43"
    dkms_module 0.15.3
    pacman -Qq linux-headers > /dev/null 2>&1 || fail "archix should declare linux-headers for the DKMS module by itself"
    remember old "$(current)"
    say "PASS: generation $(recall old) builds a local recipe and a DKMS module at the 2026-06-01 snapshot"

    ax update --to 2026-10-10 --noconfirm > /dev/console 2>&1 || fail "archix update --to 2026-10-10"
    fill_disk $((20 * 1024 * 1024))
    ax switch --noconfirm > "$state/full.log" 2>&1 && fail "a switch with 20 MB free should fail"
    tail -5 "$state/full.log" > /dev/console
    grep -q "nothing was changed" "$state/full.log" || fail "the full-disk refusal should say nothing was changed"
    rm -f /var/tmp/archix-fill
    [ "$(current)" = "$(recall old)" ] || fail "the failed switch on a full disk changed the current generation"
    journal_open && fail "a switch that can't fit on the disk should fail before changing anything, but it left a journal"
    [ "$(glibc)" = 2.43 ] || fail "the failed switch on a full disk changed glibc"
    say "PASS: a full disk stops the switch before it changes anything"

    echo 2 > "$state/stage"; sync
    say "cutting the power in the middle of the 2026-10-10 update"
    interrupt_after power 40 switch --noconfirm
    ;;
  2)
    # pacman doesn't fsync, so the cut leaves half-written binaries; the initramfs has to restore the core packages before systemd starts.
    grep -q "^generation $(recall old)," /var/lib/archix/last-rescue || fail "the initramfs should restore generation $(recall old)'s core packages after the cut"
    [ -e /var/lib/archix/auto-rescue ] || fail "the repair should be the automatic one, not a rescue entry"
    journal_open || fail "the power cut should leave the switch's journal"
    [ "$booted" = "$(recall old)" ] || fail "the old generation's normal entry should have booted"
    ax status > /dev/console 2>&1
    ax status 2>&1 | grep -q 'interrupted\|unfinished' || fail "status should report the interrupted switch"
    say "PASS: after a power cut mid-update the next boot repairs itself in the initramfs, and archix reports the unfinished switch"
    modprobe dummy || fail "the running kernel's modules should still load after the power cut"
    ax recover --continue > /dev/console 2>&1 || fail "archix recover --continue"
    journal_open && fail "recover --continue left the journal"
    [ "$(glibc)" = 2.44 ] || fail "recover --continue should finish the update to glibc 2.44"
    [ "$(missing_files)" = 0 ] || fail "files are missing after recovering from the power cut: $(pacman -Qk 2>&1 | grep 'No such file' | head -5)"
    remember new "$(current)"
    say "PASS: recover --continue finished the update after the power cut (generation $(recall new))"
    echo 3 > "$state/stage"
    systemctl reboot
    ;;
  3)
    [ "$booted" = "$(recall new)" ] || fail "the updated generation should boot by default"
    case $(uname -r) in 7.2.*) ;; *) fail "generation $(recall new) should boot kernel 7.2, not $(uname -r)" ;; esac
    # faac went from libfaac.so.0 to libfaac.so.1 between the snapshots, so the local build had to be rebuilt to keep running.
    archix-test-hello || fail "the local recipe's build should run after the update"
    [ "$(cat /usr/share/archix-test-hello/built-with)" = 2.44 ] || fail "the local recipe should have been rebuilt, since the soname it links went away"
    /usr/lib/ld-linux-x86-64.so.2 --list /usr/bin/archix-test-hello | grep -q libfaac.so.1 || fail "the rebuild should link libfaac.so.1"
    dkms_module 0.15.4
    say "PASS: booted the updated generation: kernel $(uname -r), glibc $(glibc)"
    bootctl set-oneshot "archix-$(recall old).conf" || fail "bootctl set-oneshot"
    echo 4 > "$state/stage"
    systemctl reboot
    ;;
  4)
    [ "$booted" = "$(recall old)" ] || fail "should have booted generation $(recall old)'s entry"
    [ "$(current)" = "$(recall old)" ] || fail "booting generation $(recall old)'s entry should roll back to it"
    case $(uname -r) in 7.0.*) ;; *) fail "the old entry should boot kernel 7.0, not $(uname -r)" ;; esac
    [ "$(glibc)" = 2.43 ] || fail "the boot-time rollback should restore glibc 2.43, not $(glibc)"
    case $(kernel_pkg) in 7.0.*) ;; *) fail "the linux package should be back at 7.0, not $(kernel_pkg)" ;; esac
    modprobe dummy || fail "the running kernel's modules should load"
    newer=$(needs_newer_glibc)
    [ -z "$newer" ] || fail "after rolling glibc back these need a newer one: $(echo $newer | head -c 400)"
    [ "$(cat /usr/share/archix-test-hello/built-with)" = 2.43 ] || fail "the rollback kept the local recipe's glibc 2.44 build instead of generation $(recall old)'s own"
    archix-test-hello || fail "generation $(recall old)'s own build should run against libfaac.so.0 again"
    [ "$(missing_files)" = 0 ] || fail "files are missing after the boot-time rollback"
    dkms_module 0.15.3
    say "PASS: booting the old entry rolled back kernel, glibc, the local build and the DKMS module, and every binary still finds its glibc"

    ax rollback "$(recall new)" --noconfirm > /dev/console 2>&1 || fail "archix rollback to the newer generation at runtime"
    [ "$(glibc)" = 2.44 ] || fail "rolling forward at runtime should bring glibc 2.44 back"
    modprobe -r dummy; modprobe dummy || fail "the running 7.0 kernel's modules should survive installing 7.2 at runtime"
    say "PASS: rolled forward at runtime; the running kernel keeps its modules"

    ax gc > /dev/console 2>&1 || fail "archix gc"
    [ -f "/boot/loader/entries/archix-$booted.conf" ] || fail "gc removed the entry of generation $booted, which this boot came from"
    for entry in /boot/loader/entries/archix-*.conf; do
      uki=$(sed -n 's|^efi /|/boot/|p' "$entry")
      [ -f "$uki" ] || fail "after gc, $entry starts $uki, which is gone"
    done
    ls /boot/loader/entries /boot/EFI/archix > /dev/console
    say "PASS: after a real reboot into generation $booted, gc kept its entry and every entry's UKI"
    ax rollback "$(recall old)" --noconfirm > /dev/console 2>&1 || fail "archix rollback after gc"
    [ "$(glibc)" = 2.43 ] || fail "rollback after gc should restore glibc 2.43"
    say "PASS: rollback still works after gc"

    say "killing a rollback halfway"
    interrupt_after kill 40 rollback "$(recall new)" --noconfirm
    journal_open || fail "the killed rollback should leave a journal"
    if pacman -Qq pacman > /dev/null 2>&1; then
      lib=$(readlink -f /usr/lib/libcurl.so.4)
      head -c 4096 /dev/urandom > "$lib.torn" && mv "$lib.torn" "$lib"
      pacman -Qq pacman > /dev/null 2>&1 && fail "pacman should not start with libcurl torn"
      say "tore libcurl, as if the kill had come while pacman was writing it"
    else
      say "pacman can't start after the kill"
    fi
    ax recover --rollback > "$state/recover.log" 2>&1 || { cat "$state/recover.log" > /dev/console; fail "archix recover --rollback after a killed rollback"; }
    cat "$state/recover.log" > /dev/console
    grep -q "pacman couldn't start" "$state/recover.log" || fail "recover --rollback should have restored pacman's libraries without pacman"
    journal_open && fail "recover --rollback left the journal"
    [ "$(current)" = "$(recall old)" ] || fail "recover --rollback should leave generation $(recall old) current"
    [ "$(glibc)" = 2.43 ] || fail "recover --rollback should put glibc 2.43 back"
    [ "$(missing_files)" = 0 ] || fail "files are missing after recovering the killed rollback"
    say "PASS: a rollback killed halfway, leaving pacman unable to start, is undone cleanly by recover --rollback"

    fill_disk $((30 * 1024 * 1024))
    ax rollback "$(recall new)" --noconfirm > "$state/full.log" 2>&1 && fail "a rollback with 30 MB free should fail"
    tail -5 "$state/full.log" > /dev/console
    rm -f /var/tmp/archix-fill
    if journal_open; then
      ax recover --rollback > /dev/console 2>&1
      fail "a rollback that can't fit on the disk should fail before changing anything, but it left a journal"
    fi
    [ "$(glibc)" = 2.43 ] || fail "the failed rollback on a full disk changed glibc"
    say "PASS: a full disk stops a rollback before it changes anything"

    ax rollback "$(recall new)" --noconfirm > /dev/console 2>&1 || fail "archix rollback to the newer generation"
    bootctl set-oneshot "archix-$(recall old)-rescue.conf" || fail "bootctl set-oneshot rescue"
    echo 5 > "$state/stage"
    sync
    say "breaking glibc on disk, then booting generation $(recall old)'s rescue entry"
    echo 1 > /proc/sys/kernel/sysrq
    head -c 4096 /dev/urandom > /usr/lib/libc.so.6.broken
    mv /usr/lib/libc.so.6.broken /usr/lib/libc.so.6
    echo s > /proc/sysrq-trigger
    cut_power
    ;;
  5)
    grep -q "^generation $(recall old)," /var/lib/archix/last-rescue || fail "rescue should restore generation $(recall old), not $(cat /var/lib/archix/last-rescue)"
    [ "$(glibc)" = 2.43 ] || fail "rescue should leave glibc 2.43, not $(glibc)"
    [ "$(current)" = "$(recall old)" ] || fail "after rescue, generation $(recall old) should be current"
    newer=$(needs_newer_glibc)
    [ -z "$newer" ] || fail "after rescue these need a newer glibc: $(echo $newer | head -c 400)"
    [ "$(missing_files)" = 0 ] || fail "files are missing after rescue"
    say "PASS: with glibc destroyed, the old generation's rescue entry brought the machine back"
    echo 6 > "$state/stage"
    systemctl reboot
    ;;
  6)
    [ "$booted" = "$(recall old)" ] || fail "a plain reboot after rolling back should boot generation $(recall old) again, not $booted"
    [ "$(current)" = "$(recall old)" ] || fail "the rollback should still be current after a reboot"
    say "PASS: the rolled-back generation stays the default entry across a reboot"

    sed -i 's|"archix-test-hello", |"jq", |' "$config"
    ax lock --noconfirm > /dev/console 2>&1 || fail "archix lock with jq"
    date=$(sed -n 's/^date = "\(.*\)"/\1/p' "$lock" | head -1)
    file=$(sed -n 's/^jq = .*file = "\([^"]*\)".*/\1/p' "$lock")
    repo=$(sed -n 's/^jq = .*repo = "\([^"]*\)".*/\1/p' "$lock")
    curl -sfo /tmp/jq.sig "$(cat /var/lib/archix/archive-url)/repos/${date//-//}/$repo/os/x86_64/$file.sig" || fail "fetching jq's signature"
    key=$(gpg --list-packets /tmp/jq.sig 2>/dev/null | sed -n 's/.*issuer fpr v4 \([0-9A-F]*\).*/\1/p' | head -1)
    [ -n "$key" ] || fail "couldn't read the key that signed jq"
    # pacman-key --populate disables revoked packager keys, and pacman refuses packages they signed.
    keyring=(gpg --homedir /etc/pacman.d/gnupg --batch --command-fd 0)
    printf 'disable\nsave\n' | "${keyring[@]}" --edit-key "$key" > /dev/null 2>&1 || fail "disabling $key in pacman's keyring"
    before=$(current)
    ax switch --noconfirm > "$state/key.log" 2>&1 && fail "a switch to a package signed by a disabled (revoked) key should fail"
    tail -5 "$state/key.log" > /dev/console
    printf 'enable\nsave\n' | "${keyring[@]}" --edit-key "$key" > /dev/null 2>&1
    if journal_open; then
      ax recover --rollback > /dev/console 2>&1
      fail "a package signed by a disabled key should be refused before anything changes, but the switch left a journal"
    fi
    [ "$(current)" = "$before" ] || fail "the refused switch changed the current generation"
    pacman -Q archix-test-hello > /dev/null 2>&1 || fail "the refused switch already removed archix-test-hello"
    say "PASS: a package signed by a revoked key is refused before anything changes"

    ax switch --noconfirm > /dev/console 2>&1 || fail "switch once the key is enabled again"
    pacman -Q jq > /dev/null 2>&1 || fail "jq should be installed"
    pacman -Q archix-test-hello > /dev/null 2>&1 && fail "archix-test-hello should be removed"
    echo done > "$state/stage"
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
