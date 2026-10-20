#!/bin/bash
# Rollback torture test across deployments: a real base-system update (2026-06-01 to 2026-10-10), a power cut, a kill, a full disk, a broken glibc and a missing key.
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
glibc() { pacman -Q glibc | awk '{print $2}' | cut -d+ -f1; }
kernel_pkg() { pacman -Q linux | awk '{print $2}'; }
# A package's version in a deployment that isn't booted, from that deployment's own pacman database.
deployed() {
  local db; db=$(mktemp -d)
  ln -s "/archix/deploy/$1/pacman" "$db/local"
  pacman --dbpath "$db" -Q "$2" 2>/dev/null | awk '{print $2}'
  rm -rf "$db"
}
# The DKMS module, which must be built for and loadable by the running kernel, at the version the deployment locks.
dkms_module() {
  local want=$1 have
  have=$(modinfo -F version v4l2loopback 2>/dev/null)
  [ "$have" = "$want" ] || fail "v4l2loopback $want should be built by DKMS for $(uname -r), found '${have:-nothing}': $(dkms status 2>&1 | head -5)"
  case $(modinfo -n v4l2loopback) in */updates/dkms/*) ;; *) fail "v4l2loopback should come from DKMS, not $(modinfo -n v4l2loopback)" ;; esac
  modprobe v4l2loopback || fail "v4l2loopback $want should load on $(uname -r)"
  dkms status v4l2loopback | grep -q "v4l2loopback/$want, $(uname -r), x86_64: installed" || fail "dkms should list v4l2loopback/$want as installed for $(uname -r): $(dkms status v4l2loopback)"
}
# Files that reference a GLIBC symbol version the installed glibc lacks; empty when the system is consistent.
needs_newer_glibc() {
  for f in /usr/bin/* /usr/lib/*.so*; do
    [ -f "$f" ] && [ ! -L "$f" ] || continue
    /usr/lib/ld-linux-x86-64.so.2 --list "$f" 2>&1 | grep -q "version .GLIBC_[0-9.]*' not found" && echo "$f"
  done
}
# Files pacman owns that are missing from disk.
missing_files() { pacman -Qk 2>&1 | grep -c 'No such file or directory'; }
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
say "stage $stage: booted $booted, mounted $(cat /run/archix/deployment 2>/dev/null), current $(current), deployments $(deployments), kernel $(uname -r), glibc $(glibc)"

case $stage in
  1)
    [ "$(glibc)" = 2.43 ] || fail "the image should have glibc 2.43 (the 2026-06-01 snapshot)"
    remember image "$(current)"
    sed -i 's|"devtools" }, -- packages|"devtools", "archix-test-hello", "v4l2loopback-dkms" }, -- packages|' "$config"
    ax switch --noconfirm > /dev/console 2>&1 || fail "switch adding the local recipe and a DKMS module"
    remember old "$(current)"
    [ "$(cat "/archix/deploy/$(recall old)/usr/share/archix-test-hello/built-with")" = 2.43 ] || fail "the local recipe should be built against glibc 2.43"
    [ -n "$(deployed "$(recall old)" linux-headers)" ] || fail "archix should declare linux-headers for the DKMS module by itself"
    pacman -Q archix-test-hello > /dev/null 2>&1 && fail "the running deployment shouldn't change before the restart"
    next 2
    systemctl reboot
    ;;
  2)
    [ "$booted" = "$(recall old)" ] || fail "deployment $(recall old) should boot after its switch"
    archix-test-hello || fail "the local recipe's build should run"
    dkms_module 0.15.3
    say "PASS: deployment $(recall old) boots a local recipe and a DKMS module built at the 2026-06-01 snapshot"

    ax update --to 2026-10-10 --noconfirm > /dev/console 2>&1 || fail "archix update --to 2026-10-10"
    kept=$(deployments)
    fill_disk $((20 * 1024 * 1024))
    ax switch --noconfirm > /run/archix-vmtest-full.log 2>&1 && fail "a switch with 20 MB free should fail"
    tail -5 /run/archix-vmtest-full.log > /dev/console
    grep -q "nothing was changed" /run/archix-vmtest-full.log || fail "the full-disk refusal should say nothing was changed"
    rm -f /var/tmp/archix-fill
    [ "$(current)" = "$(recall old)" ] || fail "the failed switch on a full disk changed the current deployment"
    [ "$(deployments)" = "$kept" ] && ! staging_left || fail "the failed switch on a full disk left a deployment or staging tree: $(ls -a /archix/deploy)"
    say "PASS: a full disk stops the switch before it changes anything"

    remember kept "$kept"
    next 3
    say "cutting the power in the middle of the 2026-10-10 update"
    interrupt_after power 40 switch --noconfirm
    ;;
  3)
    [ "$booted" = "$(recall old)" ] && [ "$(current)" = "$(recall old)" ] || fail "after a power cut mid-switch, deployment $(recall old) should still boot and be current"
    [ "$(deployments)" = "$(recall kept)" ] || fail "the cut switch committed a deployment: $(deployments)"
    [ "$(glibc)" = 2.43 ] || fail "the power cut changed the running deployment's glibc"
    [ "$(missing_files)" = 0 ] || fail "files are missing after the power cut: $(pacman -Qk 2>&1 | grep 'No such file' | head -5)"
    [ -z "$(needs_newer_glibc)" ] || fail "after the power cut these need a newer glibc: $(needs_newer_glibc | head -c 400)"
    say "PASS: a power cut mid-update left the running deployment whole"
    ax switch --noconfirm > /dev/console 2>&1 || fail "the update after the power cut"
    staging_left && fail "the next switch should clear the cut switch's staging tree"
    remember new "$(current)"
    [ "$(deployed "$(recall new)" glibc | cut -d+ -f1)" = 2.44 ] || fail "deployment $(recall new) should have glibc 2.44"
    next 4
    systemctl reboot
    ;;
  4)
    [ "$booted" = "$(recall new)" ] || fail "the updated deployment should boot by default"
    case $(uname -r) in 7.2.*) ;; *) fail "deployment $(recall new) should boot kernel 7.2, not $(uname -r)" ;; esac
    [ "$(glibc)" = 2.44 ] || fail "deployment $(recall new) should run glibc 2.44"
    # faac went from libfaac.so.0 to libfaac.so.1 between the snapshots, so the local build had to be rebuilt to keep running.
    archix-test-hello || fail "the local recipe's build should run after the update"
    [ "$(cat /usr/share/archix-test-hello/built-with)" = 2.44 ] || fail "the local recipe should have been rebuilt, since the soname it links went away"
    /usr/lib/ld-linux-x86-64.so.2 --list /usr/bin/archix-test-hello | grep -q libfaac.so.1 || fail "the rebuild should link libfaac.so.1"
    dkms_module 0.15.4
    say "PASS: booted the updated deployment: kernel $(uname -r), glibc $(glibc)"
    bootctl set-oneshot "archix-$(recall old).conf" || fail "bootctl set-oneshot"
    next 5
    systemctl reboot
    ;;
  5)
    old=$(recall old) new=$(recall new) image=$(recall image)
    [ "$booted" = "$old" ] || fail "should have booted deployment $old's entry"
    [ "$(current)" = "$old" ] || fail "booting deployment $old's entry should make it current"
    case $(uname -r) in 7.0.*) ;; *) fail "the old entry should boot kernel 7.0, not $(uname -r)" ;; esac
    [ "$(glibc)" = 2.43 ] || fail "deployment $old should run glibc 2.43, not $(glibc)"
    case $(kernel_pkg) in 7.0.*) ;; *) fail "the linux package should be 7.0, not $(kernel_pkg)" ;; esac
    modprobe dummy || fail "the running kernel's modules should load"
    [ -z "$(needs_newer_glibc)" ] || fail "in deployment $old these need a newer glibc: $(needs_newer_glibc | head -c 400)"
    [ "$(cat /usr/share/archix-test-hello/built-with)" = 2.43 ] || fail "deployment $old should have its own glibc 2.43 build of the local recipe"
    archix-test-hello || fail "deployment $old's own build should run against libfaac.so.0"
    [ "$(missing_files)" = 0 ] || fail "files are missing in deployment $old"
    dkms_module 0.15.3
    say "PASS: booting the old entry brought back kernel, glibc, the local build and the DKMS module, with nothing reinstalled"

    ax rollback "$new" --noconfirm > /dev/console 2>&1 || fail "archix rollback to the newer deployment"
    [ "$(current)" = "$new" ] && [ "$(glibc)" = 2.43 ] || fail "rolling forward should repoint current and leave the running deployment alone"
    ax gc > /dev/console 2>&1 || fail "archix gc"
    [ -f "/efi/loader/entries/archix-$booted.conf" ] || fail "gc removed the entry of deployment $booted, which this boot came from"
    for entry in /efi/loader/entries/archix-*.conf; do
      uki=$(sed -nE 's#^(efi|uki) /#/efi/#p' "$entry")
      [ -f "$uki" ] || fail "after gc, $entry starts $uki, which is gone"
    done
    ls /efi/loader/entries /efi/EFI/archix > /dev/console
    ax rollback "$old" --noconfirm > /dev/console 2>&1 || fail "archix rollback after gc"
    [ "$(current)" = "$old" ] || fail "rolling back to deployment $old should make it current"
    say "PASS: rollbacks between kept deployments repoint them, also after gc"

    [ -d "/archix/deploy/$image" ] && fail "keep = 2 should have dropped the image's deployment $image"
    kept=$(deployments)
    fill_disk $((30 * 1024 * 1024))
    ax rollback "$image" --noconfirm > /run/archix-vmtest-full.log 2>&1 && fail "rebuilding a deployment with 30 MB free should fail"
    tail -5 /run/archix-vmtest-full.log > /dev/console
    rm -f /var/tmp/archix-fill
    [ "$(deployments)" = "$kept" ] && ! staging_left || fail "the failed rebuild on a full disk left something behind: $(ls -a /archix/deploy)"
    [ "$(current)" = "$old" ] || fail "the failed rebuild on a full disk changed the current deployment"
    say "PASS: a full disk stops a rebuilding rollback before it changes anything"

    say "killing a rollback that rebuilds generation $image"
    interrupt_after kill 1 rollback "$image" --noconfirm
    [ "$(current)" = "$old" ] || fail "the killed rollback changed the current deployment"
    [ "$(deployments)" = "$kept" ] || fail "the killed rollback committed a deployment: $(deployments)"
    [ "$(missing_files)" = 0 ] && [ "$(glibc)" = 2.43 ] || fail "the killed rollback changed the running deployment"
    ax rollback "$image" --noconfirm > /dev/console 2>&1 || fail "rebuilding generation $image after the kill"
    [ "$(current)" = "$image" ] || fail "the rebuilt deployment $image should be current"
    staging_left && fail "the rebuild should clear the killed one's staging tree"
    [ -z "$(deployed "$image" archix-test-hello)" ] || fail "the rebuilt deployment $image shouldn't have the local recipe"
    [ "$(deployed "$image" glibc | cut -d+ -f1)" = 2.43 ] || fail "the rebuilt deployment $image should have glibc 2.43"
    say "PASS: a killed rebuild changed nothing, and the next one rebuilt generation $image from the package cache"

    ax rollback "$new" --noconfirm > /dev/console 2>&1 || fail "rolling forward to generation $new"
    [ "$(current)" = "$new" ] || fail "deployment $new should be current"
    [ "$(cat "/archix/deploy/$new/usr/share/archix-test-hello/built-with")" = 2.44 ] || fail "deployment $new should have the glibc 2.44 build of the local recipe"
    next 6
    systemctl reboot
    ;;
  6)
    new=$(recall new)
    [ "$booted" = "$new" ] || fail "the rebuilt deployment $new should boot, not $booted"
    case $(uname -r) in 7.2.*) ;; *) fail "deployment $new should boot kernel 7.2, not $(uname -r)" ;; esac
    archix-test-hello || fail "the local recipe's build should run in the rebuilt deployment"
    dkms_module 0.15.4
    [ "$(missing_files)" = 0 ] || fail "files are missing in the rebuilt deployment"
    say "PASS: a deployment rebuilt from the package cache boots with its kernel, the local build and the DKMS module"

    good=$booted
    remember good "$good"
    # An init that can't run makes the kernel panic, and panic=3 reboots it so systemd-boot counts the try.
    sed -i 's|"console=ttyS0" }|"console=ttyS0", "panic=3" }|' "$config"
    ax switch --noconfirm > /dev/console 2>&1 || fail "switch to a deployment whose glibc gets broken"
    bad=$(current)
    remember bad "$bad"
    [ -f "/efi/loader/entries/archix-$bad+3.conf" ] || fail "the new entry should count three boot tries: $(ls /efi/loader/entries)"
    lib=$(readlink -f "/archix/deploy/$bad/usr/lib/libc.so.6")
    head -c 4096 /dev/urandom > "$lib.broken" && mv "$lib.broken" "$lib" || fail "breaking deployment $bad's glibc"
    [ "$(glibc)" = 2.44 ] && archix --version > /dev/null || fail "breaking deployment $bad's glibc touched the running one"
    say "broke glibc in deployment $bad; rebooting into it"
    next 7
    systemctl reboot
    ;;
  7)
    good=$(recall good) bad=$(recall bad)
    [ "$booted" = "$good" ] || fail "after deployment $bad failed to boot, systemd-boot should start $good, not $booted"
    [ "$(current)" = "$good" ] || fail "archix should make deployment $good current after $bad failed to boot"
    [ -f "/efi/loader/entries/archix-$bad+0-3.conf" ] || fail "deployment $bad's entry should be marked bad: $(ls /efi/loader/entries)"
    grep -q "generation $bad didn't finish booting" /var/lib/archix/notes || fail "the login note should say $bad didn't boot"
    [ "$(glibc)" = 2.44 ] && [ -z "$(needs_newer_glibc)" ] && [ "$(missing_files)" = 0 ] || fail "deployment $good should be whole"
    say "PASS: with glibc destroyed in deployment $bad, systemd-boot fell back to $good and archix made it current"

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
    before=$(current) kept=$(deployments)
    ax switch --noconfirm > "$state/key.log" 2>&1 && fail "a switch to a package signed by a disabled (revoked) key should fail"
    tail -5 "$state/key.log" > /dev/console
    printf 'enable\nsave\n' | "${keyring[@]}" --edit-key "$key" > /dev/null 2>&1
    [ "$(current)" = "$before" ] || fail "the refused switch changed the current deployment"
    [ "$(deployments)" = "$kept" ] && ! staging_left || fail "the refused switch left something behind: $(ls -a /archix/deploy)"
    say "PASS: a package signed by a revoked key is refused before anything changes"

    ax switch --noconfirm > /dev/console 2>&1 || fail "switch once the key is enabled again"
    [ -n "$(deployed "$(current)" jq)" ] || fail "jq should be in the new deployment"
    [ -z "$(deployed "$(current)" archix-test-hello)" ] || fail "archix-test-hello should be gone from the new deployment"
    next done
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
