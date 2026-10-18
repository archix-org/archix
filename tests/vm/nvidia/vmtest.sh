#!/bin/bash
# NVIDIA DKMS drivers across rollbacks: nvidia-open-dkms 610.43.02 (2026-06-01) and 615.78.08 (2026-10-10), back by boot menu and at runtime, uninstalled by rolling back to a generation without them, and the closed nvidia-580xx-dkms from the AUR. No GPU, so modules are built and checked, not loaded.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
stage=$(cat "$state/stage" 2>/dev/null || echo 1)
config=/etc/archix/init.lua
ax() { archix --host archix-vm "$@"; }
say() { echo "VMTEST: $*" > /dev/console; }
fail() { say "FAIL: $*"; { ax generations; dkms status; pacman -Q | grep -i nvidia; } > /dev/console 2>&1; echo failed > "$state/stage"; sync; systemctl poweroff; exit 1; }
current() { ax generations | awk '$1 == "*" { print $2 }'; }
remember() { echo "$2" > "$state/$1"; }
recall() { cat "$state/$1"; }
# systemd-boot names the entry it started in LoaderEntrySelected: 4 attribute bytes, then archix-N.conf in UTF-16.
booted=$(tr -d '\000' < /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
pkg() { pacman -Q "$1" 2>/dev/null | awk '{print $2}'; }
module_version() { modinfo -k "$1" -F version nvidia 2>/dev/null; }
nvidia_modules() { ls /usr/lib/modules/*/updates/dkms/nvidia* 2>/dev/null; }
# Everything that shows the driver: the packages, the built module for the running kernel and DKMS's own record.
driver() {
  local want=$1 kernel; kernel=$(uname -r)
  [ "$(pkg nvidia-open-dkms)" = "$want-${2:-1}" ] || fail "nvidia-open-dkms should be $want, not '$(pkg nvidia-open-dkms)'"
  case $(pkg nvidia-utils) in "$want"-*) ;; *) fail "nvidia-utils should be $want, not '$(pkg nvidia-utils)'" ;; esac
  [ "$(module_version "$kernel")" = "$want" ] || fail "the nvidia module built for $kernel should be $want, not '$(module_version "$kernel")'"
  dkms status nvidia | grep -q "nvidia/$want, $kernel, x86_64: installed" || fail "dkms should show nvidia/$want installed for $kernel: $(dkms status nvidia)"
}
no_driver() {
  for p in nvidia-open-dkms nvidia-utils nvidia-580xx-dkms nvidia-580xx-utils; do
    [ -z "$(pkg $p)" ] || fail "$p should be gone, but $(pkg $p) is installed"
  done
  [ -z "$(nvidia_modules)" ] || fail "nvidia modules are left behind: $(nvidia_modules)"
  [ -z "$(dkms status nvidia 2>/dev/null)" ] || fail "dkms still knows nvidia: $(dkms status nvidia)"
  modinfo -k "$(uname -r)" nvidia > /dev/null 2>&1 && fail "modinfo still finds an nvidia module for $(uname -r)"
  return 0
}
say "stage $stage: booted generation $booted, current generation $(current), kernel $(uname -r)"

case $stage in
  1)
    remember none "$(current)"
    no_driver
    sed -i 's|"devtools" }, -- packages|"devtools", "nvidia-open-dkms" }, -- packages|' "$config"
    ax switch --noconfirm > /dev/console 2>&1 || fail "switch adding nvidia-open-dkms"
    driver 610.43.02 2
    remember nv610 "$(current)"
    say "PASS: generation $(recall nv610) installs nvidia-open-dkms 610.43.02, built by DKMS for $(uname -r)"
    ax update --to 2026-10-10 --noconfirm > /dev/console 2>&1 || fail "archix update --to 2026-10-10"
    ax switch --noconfirm > /dev/console 2>&1 || fail "switch to the 2026-10-10 snapshot"
    remember nv615 "$(current)"
    [ "$(module_version 7.2.9-arch1-1)" = 615.78.08 ] || fail "nvidia 615.78.08 should be built for 7.2.9-arch1-1, not '$(module_version 7.2.9-arch1-1)'"
    echo 2 > "$state/stage"
    systemctl reboot
    ;;
  2)
    [ "$booted" = "$(recall nv615)" ] || fail "generation $(recall nv615) should boot by default"
    driver 615.78.08
    say "PASS: generation $(recall nv615) boots kernel $(uname -r) with nvidia-open-dkms 615.78.08"
    bootctl set-oneshot "archix-$(recall nv610).conf" || fail "bootctl set-oneshot"
    echo 3 > "$state/stage"
    systemctl reboot
    ;;
  3)
    [ "$booted" = "$(recall nv610)" ] && [ "$(current)" = "$(recall nv610)" ] || fail "booting generation $(recall nv610)'s entry should roll back to it"
    case $(uname -r) in 7.0.*) ;; *) fail "the old entry should boot kernel 7.0, not $(uname -r)" ;; esac
    driver 610.43.02 2
    dkms status nvidia | grep -q "nvidia/615" && fail "dkms still holds the 615 build: $(dkms status nvidia)"
    say "PASS: booting the old entry rolled the kernel back to $(uname -r) and the driver back to 610.43.02, rebuilt by DKMS"

    ax rollback "$(recall none)" --noconfirm > /dev/console 2>&1 || fail "archix rollback to generation $(recall none), which has no driver"
    no_driver
    say "PASS: rolling back at runtime to generation $(recall none), which had no driver, uninstalls nvidia-open-dkms and nvidia-utils and leaves no module"

    ax rollback "$(recall nv615)" --noconfirm > /dev/console 2>&1 || fail "archix rollback forward to generation $(recall nv615)"
    [ "$(pkg nvidia-open-dkms)" = 615.78.08-1 ] || fail "rolling forward should bring nvidia-open-dkms 615.78.08 back"
    bootctl set-oneshot "archix-$(recall none).conf" || fail "bootctl set-oneshot"
    echo 4 > "$state/stage"
    systemctl reboot
    ;;
  4)
    [ "$booted" = "$(recall none)" ] && [ "$(current)" = "$(recall none)" ] || fail "booting generation $(recall none)'s entry should roll back to it"
    no_driver
    say "PASS: booting the entry of generation $(recall none), which had no driver, uninstalled the 615.78.08 driver at boot"

    sed -i 's|"nvidia-open-dkms" }, -- packages|"nvidia-580xx-dkms" }, -- packages|' "$config"
    ax switch --noconfirm > /dev/console 2>&1 || fail "switch adding the closed driver nvidia-580xx-dkms from the AUR"
    remember closed "$(current)"
    case $(pkg nvidia-580xx-dkms) in 580.*) ;; *) fail "nvidia-580xx-dkms should be installed, not '$(pkg nvidia-580xx-dkms)'" ;; esac
    version=$(pkg nvidia-580xx-dkms | cut -d- -f1)
    [ "$(module_version 7.2.9-arch1-1)" = "$version" ] || fail "the closed nvidia $version should be built for 7.2.9-arch1-1, not '$(module_version 7.2.9-arch1-1)'"
    say "PASS: generation $(recall closed) builds the closed driver nvidia-580xx-dkms $version from the AUR for 7.2.9-arch1-1"
    ax rollback "$(recall none)" --noconfirm > /dev/console 2>&1 || fail "archix rollback from the closed driver to generation $(recall none)"
    no_driver
    say "PASS: rolling back to generation $(recall none) uninstalls the closed driver too"
    echo done > "$state/stage"
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
