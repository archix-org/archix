#!/bin/bash
# NVIDIA DKMS drivers across rollbacks: nvidia-open-dkms at 2026-06-01 and at a newer snapshot (/etc/archix-vmtest-to, 2026-10-10 by default), back by boot menu and at runtime, uninstalled by rolling back to a generation without them, and the closed nvidia-580xx-dkms from the AUR. No GPU, so modules are built and checked, not loaded.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
stage=$(cat "$state/stage" 2>/dev/null || echo 1)
config=/etc/archix/init.lua
ax() { archix --host archix-vm "$@"; }
say() { echo "VMTEST: $*" > /dev/console; }
fail() { say "FAIL: $*"; { ax generations; command -v dkms >/dev/null && dkms status; pacman -Q | grep -i nvidia; } > /dev/console 2>&1; echo failed > "$state/stage"; sync; systemctl poweroff; exit 1; }
current() { ax generations | awk '$1 == "*" { print $2 }'; }
remember() { echo "$2" > "$state/$1"; }
recall() { cat "$state/$1"; }
# systemd-boot names the entry it started in LoaderEntrySelected: 4 attribute bytes, then archix-N.conf in UTF-16.
to=$(cat /etc/archix-vmtest-to 2>/dev/null || echo 2026-10-10)
booted=$(tr -d '\000' < /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
pkg() { pacman -Q "$1" 2>/dev/null | awk '{print $2}'; }
module_version() { modinfo -k "$1" -F version nvidia 2>/dev/null; }
# The kernel version whose modules directory the installed linux package owns.
linux_kernel() { pacman -Ql linux | sed -n 's|^linux /usr/lib/modules/\([^/]*\)/vmlinuz$|\1|p'; }
nvidia_modules() { ls /usr/lib/modules/*/updates/dkms/nvidia* 2>/dev/null; }
# Everything that shows the driver: the packages, the built module for the running kernel and DKMS's own record; $1 is the package version.
driver() {
  local want=${1%-*} kernel; kernel=$(uname -r)
  [ "$(pkg nvidia-open-dkms)" = "$1" ] || fail "nvidia-open-dkms should be $1, not '$(pkg nvidia-open-dkms)'"
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
say "stage $stage: booted generation $booted, current generation $(current), kernel $(uname -r), $(pacman -Q dkms 2>/dev/null || echo "no dkms")"

case $stage in
  1)
    remember none "$(current)"
    no_driver
    sed -i 's|"devtools" }, -- packages|"devtools", "nvidia-open-dkms" }, -- packages|' "$config"
    ax switch --noconfirm > /dev/console 2>&1 || fail "switch adding nvidia-open-dkms"
    remember old_driver "$(pkg nvidia-open-dkms)"
    remember old_kernel "$(uname -r)"
    driver "$(recall old_driver)"
    remember nv_old "$(current)"
    say "PASS: generation $(recall nv_old) installs nvidia-open-dkms $(recall old_driver), built by DKMS for $(uname -r)"
    ax update --to "$to" --noconfirm > /dev/console 2>&1 || fail "archix update --to $to"
    ax switch --noconfirm > /dev/console 2>&1 || fail "switch to the $to snapshot"
    remember nv_new "$(current)"
    remember new_driver "$(pkg nvidia-open-dkms)"
    remember new_kernel "$(linux_kernel)"
    [ "$(recall new_driver)" != "$(recall old_driver)" ] && [ "$(recall new_kernel)" != "$(recall old_kernel)" ] \
      || fail "the $to snapshot should change both the kernel and the driver, but has $(recall new_kernel) and $(recall new_driver)"
    [ "$(module_version "$(recall new_kernel)")" = "$(recall new_driver | sed 's/-[^-]*$//')" ] \
      || fail "nvidia $(recall new_driver) should be built for $(recall new_kernel), not '$(module_version "$(recall new_kernel)")'"
    echo 2 > "$state/stage"
    systemctl reboot
    ;;
  2)
    [ "$booted" = "$(recall nv_new)" ] || fail "generation $(recall nv_new) should boot by default"
    driver "$(recall new_driver)"
    say "PASS: generation $(recall nv_new) boots kernel $(uname -r) with nvidia-open-dkms $(recall new_driver)"
    bootctl set-oneshot "archix-$(recall nv_old).conf" || fail "bootctl set-oneshot"
    echo 3 > "$state/stage"
    systemctl reboot
    ;;
  3)
    [ "$booted" = "$(recall nv_old)" ] && [ "$(current)" = "$(recall nv_old)" ] || fail "booting generation $(recall nv_old)'s entry should roll back to it"
    [ "$(uname -r)" = "$(recall old_kernel)" ] || fail "the old entry should boot kernel $(recall old_kernel), not $(uname -r)"
    driver "$(recall old_driver)"
    new=$(recall new_driver | sed 's/-[^-]*$//')
    dkms status nvidia | grep -q "nvidia/$new" && fail "dkms still holds the $new build: $(dkms status nvidia)"
    say "PASS: booting the old entry rolled the kernel back to $(uname -r) and the driver back to $(recall old_driver), rebuilt by DKMS"

    ax rollback "$(recall none)" --noconfirm > /dev/console 2>&1 || fail "archix rollback to generation $(recall none), which has no driver"
    no_driver
    say "PASS: rolling back at runtime to generation $(recall none), which had no driver, uninstalls nvidia-open-dkms and nvidia-utils and leaves no module"

    ax rollback "$(recall nv_new)" --noconfirm > /dev/console 2>&1 || fail "archix rollback forward to generation $(recall nv_new)"
    [ "$(pkg nvidia-open-dkms)" = "$(recall new_driver)" ] || fail "rolling forward should bring nvidia-open-dkms $(recall new_driver) back"
    bootctl set-oneshot "archix-$(recall none).conf" || fail "bootctl set-oneshot"
    echo 4 > "$state/stage"
    systemctl reboot
    ;;
  4)
    [ "$booted" = "$(recall none)" ] && [ "$(current)" = "$(recall none)" ] || fail "booting generation $(recall none)'s entry should roll back to it"
    no_driver
    say "PASS: booting the entry of generation $(recall none), which had no driver, uninstalled the $(recall new_driver) driver at boot"

    sed -i 's|"nvidia-open-dkms" }, -- packages|"nvidia-580xx-dkms" }, -- packages|' "$config"
    ax switch --noconfirm > /dev/console 2>&1 || fail "switch adding the closed driver nvidia-580xx-dkms from the AUR"
    remember closed "$(current)"
    case $(pkg nvidia-580xx-dkms) in 580.*) ;; *) fail "nvidia-580xx-dkms should be installed, not '$(pkg nvidia-580xx-dkms)'" ;; esac
    version=$(pkg nvidia-580xx-dkms | cut -d- -f1)
    [ "$(module_version "$(recall new_kernel)")" = "$version" ] || fail "the closed nvidia $version should be built for $(recall new_kernel), not '$(module_version "$(recall new_kernel)")'"
    say "PASS: generation $(recall closed) builds the closed driver nvidia-580xx-dkms $version from the AUR for $(recall new_kernel)"
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
