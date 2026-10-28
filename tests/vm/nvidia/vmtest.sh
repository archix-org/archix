#!/bin/bash
# NVIDIA DKMS drivers across deployments: nvidia-open-dkms at 2026-06-01 and at a newer snapshot (/etc/archix-vmtest-to, 2026-10-10 by default), back by boot menu, a deployment without them, the closed nvidia-580xx-dkms from the AUR, and a dropped deployment rebuilt with its driver. No GPU, so modules are built and checked, not loaded.
set -u
state=/var/lib/archix-vmtest
mkdir -p "$state"
stage=$(cat "$state/stage" 2>/dev/null || echo 1)
config=/etc/archix/init.lua
ax() { archix --host archix-vm "$@"; }
say() { echo "VMTEST: $*" > /dev/console; }
fail() { say "FAIL: $*"; { ax generations; ls /archix/deploy; command -v dkms >/dev/null && dkms status; pacman -Q | grep -i nvidia; } > /dev/console 2>&1; echo failed > "$state/stage"; sync; systemctl poweroff; exit 1; }
current() { ax generations | awk '$1 == "*" { print $2 }'; }
remember() { echo "$2" > "$state/$1"; }
recall() { cat "$state/$1"; }
next() { echo "$1" > "$state/stage"; sync; }
to=$(cat /etc/archix-vmtest-to 2>/dev/null || echo 2026-10-10)
# systemd-boot names the entry it started in LoaderEntrySelected: 4 attribute bytes, then archix-N.conf in UTF-16.
booted=$(tr -d '\000' < /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | grep -ao 'archix-[0-9]*' | head -1 | cut -d- -f2)
pkg() { pacman -Q "$1" 2>/dev/null | awk '{print $2}'; }
# A package's version in a deployment that isn't booted, from that deployment's own pacman database.
deployed() {
  local db; db=$(mktemp -d)
  ln -s "/archix/deploy/$1/pacman" "$db/local"
  pacman --dbpath "$db" -Q "$2" 2>/dev/null | awk '{print $2}'
  rm -rf "$db"
}
# The kernel a deployment boots: the modules directory that holds a vmlinuz.
deployed_kernel() { for k in /archix/deploy/"$1"/usr/lib/modules/*/vmlinuz; do basename "$(dirname "$k")"; done; }
# The version of the nvidia module DKMS built into a deployment for a kernel.
deployed_module() { modinfo -F version /archix/deploy/"$1"/usr/lib/modules/"$2"/updates/dkms/nvidia.ko* 2>/dev/null; }
module_version() { modinfo -k "$1" -F version nvidia 2>/dev/null; }
nvidia_modules() { ls /usr/lib/modules/*/updates/dkms/nvidia* 2>/dev/null; }
nvidia_sources() { ls -d /usr/src/nvidia* 2>/dev/null; }
# nvidia-utils ships its GSP firmware in a directory named by version; linux-firmware's nvidia/ holds chip names.
nvidia_firmware() { ls -d /usr/lib/firmware/nvidia/[0-9]* 2>/dev/null; }
# modprobe.d, modules-load.d, udev and mkinitcpio files named for or mentioning nvidia; nvidia-utils ships the first three, nothing for mkinitcpio.
nvidia_dropins() {
  local dirs=(/usr/lib/modprobe.d /etc/modprobe.d /usr/lib/modules-load.d /etc/modules-load.d /usr/lib/udev/rules.d /etc/udev/rules.d /etc/mkinitcpio.conf.d /etc/mkinitcpio.d /usr/lib/initcpio)
  { find "${dirs[@]}" -iname '*nvidia*' 2>/dev/null; grep -rlis nvidia "${dirs[@]}"; grep -v '^[[:space:]]*#' /etc/mkinitcpio.conf | grep -qi nvidia && echo /etc/mkinitcpio.conf; } | sort -u
}
# The initramfs files in the booted deployment's UKI; lsinitcpio reads a UKI's .initrd section with objcopy.
initramfs_files() {
  local entry uki
  for entry in /efi/loader/entries/archix-"$booted".conf /efi/loader/entries/archix-"$booted"+*.conf; do [ -f "$entry" ] && break; done
  uki=$(sed -nE 's#^(efi|uki) /#/efi/#p' "$entry" 2>/dev/null)
  [ -f "$uki" ] || fail "deployment $booted's entry $entry should start a UKI, not '$uki': $(ls /efi/loader/entries | tr '\n' ' ')"
  lsinitcpio "$uki" || fail "lsinitcpio $uki"
}
# Everything that shows the driver: the packages, the built module for the running kernel and DKMS's own record; $1 is the package version.
driver() {
  local want=${1%-*} kernel; kernel=$(uname -r)
  [ "$(pkg nvidia-open-dkms)" = "$1" ] || fail "nvidia-open-dkms should be $1, not '$(pkg nvidia-open-dkms)'"
  case $(pkg nvidia-utils) in "$want"-*) ;; *) fail "nvidia-utils should be $want, not '$(pkg nvidia-utils)'" ;; esac
  [ "$(module_version "$kernel")" = "$want" ] || fail "the nvidia module built for $kernel should be $want, not '$(module_version "$kernel")'"
  dkms status nvidia | grep -q "nvidia/$want, $kernel, x86_64: installed" || fail "dkms should show nvidia/$want installed for $kernel: $(dkms status nvidia)"
  [ -f "/usr/src/nvidia-$want/dkms.conf" ] || fail "nvidia-open-dkms's source tree /usr/src/nvidia-$want should be there: $(nvidia_sources)"
  [ -f "/usr/lib/firmware/nvidia/$want/gsp_ga10x.bin" ] || fail "nvidia-utils's GSP firmware should be in /usr/lib/firmware/nvidia/$want: $(nvidia_firmware)"
  nvidia_dropins | grep -qx /usr/lib/modprobe.d/nvidia-utils.conf || fail "nvidia-utils's /usr/lib/modprobe.d/nvidia-utils.conf should be found: $(nvidia_dropins)"
  modprobe -n nvidia || fail "modprobe should find the nvidia module for $kernel"
}
# What pacman -Qkk finds wrong in deployment $1's /usr, /opt and /boot, the trees a staged deployment shares with it.
findings() { /opt/archix-vmtest/bin/archix-check-deployment "$1" | grep -E ': /(usr|opt|boot)/' || true; }
baseline() { findings "$1" > "$state/baseline-$1"; }
unchanged() { findings "$2" | diff "$state/baseline-$2" - > "$state/diff-$2" || fail "$1 changed deployment $2's files: $(head -c 600 "$state/diff-$2")"; }
no_driver() {
  for p in nvidia-open-dkms nvidia-utils nvidia-580xx-dkms nvidia-580xx-utils; do
    [ -z "$(pkg $p)" ] || fail "$p should be gone, but $(pkg $p) is installed"
  done
  [ -z "$(nvidia_modules)" ] || fail "nvidia modules are left behind: $(nvidia_modules)"
  [ -z "$(dkms status nvidia 2>/dev/null)" ] || fail "dkms still knows nvidia: $(dkms status nvidia)"
  modinfo -k "$(uname -r)" nvidia > /dev/null 2>&1 && fail "modinfo still finds an nvidia module for $(uname -r)"
  [ -z "$(nvidia_sources)" ] || fail "nvidia sources are left in /usr/src: $(nvidia_sources)"
  [ -z "$(nvidia_firmware)" ] || fail "nvidia-utils's firmware is left behind: $(nvidia_firmware)"
  [ -z "$(nvidia_dropins)" ] || fail "nvidia modprobe, modules-load, udev or mkinitcpio files are left behind: $(nvidia_dropins | tr '\n' ' ')"
  local err; err=$(modprobe nvidia 2>&1) && fail "modprobe nvidia should fail"
  [[ $err == *"Module nvidia not found"* ]] || fail "modprobe nvidia should fail for want of the module, not with: $err"
  [ -d /sys/module/nvidia ] && fail "the nvidia module is loaded"
  local files; files=$(initramfs_files)
  grep -q "usr/lib/modules/$(uname -r)/.*\.ko" <<< "$files" || fail "lsinitcpio should list deployment $booted's initramfs modules for $(uname -r): $(head -c 400 <<< "$files")"
  # The kernel's own hid-nvidia-shield.ko is in every initramfs, so only DKMS modules, the driver's names and nvidia-utils's firmware count.
  local ours; ours=$(grep -E '/updates/|/nvidia[^/]*\.ko|/firmware/nvidia/[0-9]' <<< "$files")
  [ -z "$ours" ] || fail "deployment $booted's UKI has nvidia in its initramfs: $(head -5 <<< "$ours" | tr '\n' ' ')"
  return 0
}
say "stage $stage: booted $booted, current $(current), deployments $(ls /archix/deploy | sort -n | tr '\n' ' '), kernel $(uname -r), $(pacman -Q dkms 2>/dev/null || echo "no dkms")"

case $stage in
  1)
    remember none "$(current)"
    no_driver
    baseline "$(recall none)"
    sed -i 's|"devtools" }, -- packages|"devtools", "nvidia-open-dkms" }, -- packages|' "$config"
    ax switch --noconfirm > /dev/console 2>&1 || fail "switch adding nvidia-open-dkms"
    unchanged "staging the driver and its DKMS build" "$(recall none)"
    remember nv_old "$(current)"
    remember old_driver "$(deployed "$(recall nv_old)" nvidia-open-dkms)"
    [ "$(deployed_module "$(recall nv_old)" "$(uname -r)")" = "$(recall old_driver | sed 's/-[^-]*$//')" ] \
      || fail "deployment $(recall nv_old) should hold nvidia $(recall old_driver) built for $(uname -r)"
    [ -z "$(pkg nvidia-open-dkms)" ] || fail "the running deployment shouldn't change before the restart"
    next 2
    systemctl reboot
    ;;
  2)
    [ "$booted" = "$(recall nv_old)" ] || fail "deployment $(recall nv_old) should boot after its switch"
    remember old_kernel "$(uname -r)"
    driver "$(recall old_driver)"
    say "PASS: deployment $(recall nv_old) boots nvidia-open-dkms $(recall old_driver), built by DKMS for $(uname -r) while staging, with its /usr/src tree, nvidia-utils's firmware and modprobe.d file; no GPU, so build and install checks, not a hardware result"
    ax update --to "$to" --noconfirm > /dev/console 2>&1 || fail "archix update --to $to"
    baseline "$(recall nv_old)"
    ax switch --noconfirm > /dev/console 2>&1 || fail "switch to the $to snapshot"
    unchanged "staging the $to kernel and driver" "$(recall nv_old)"
    nv_new=$(current)
    remember nv_new "$nv_new"
    remember new_driver "$(deployed "$nv_new" nvidia-open-dkms)"
    remember new_kernel "$(deployed_kernel "$nv_new")"
    [ "$(recall new_driver)" != "$(recall old_driver)" ] && [ "$(recall new_kernel)" != "$(recall old_kernel)" ] \
      || fail "the $to snapshot should change both the kernel and the driver, but has $(recall new_kernel) and $(recall new_driver)"
    [ "$(deployed_module "$nv_new" "$(recall new_kernel)")" = "$(recall new_driver | sed 's/-[^-]*$//')" ] \
      || fail "nvidia $(recall new_driver) should be built for $(recall new_kernel), not '$(deployed_module "$nv_new" "$(recall new_kernel)")'"
    driver "$(recall old_driver)"
    next 3
    systemctl reboot
    ;;
  3)
    [ "$booted" = "$(recall nv_new)" ] || fail "deployment $(recall nv_new) should boot by default"
    [ "$(uname -r)" = "$(recall new_kernel)" ] || fail "deployment $(recall nv_new) should boot kernel $(recall new_kernel), not $(uname -r)"
    driver "$(recall new_driver)"
    say "PASS: deployment $(recall nv_new) boots kernel $(uname -r) with nvidia-open-dkms $(recall new_driver)"
    bootctl set-oneshot "archix-$(recall nv_old).conf" || fail "bootctl set-oneshot"
    next 4
    systemctl reboot
    ;;
  4)
    [ "$booted" = "$(recall nv_old)" ] && [ "$(current)" = "$(recall nv_old)" ] || fail "booting deployment $(recall nv_old)'s entry should make it current"
    [ "$(uname -r)" = "$(recall old_kernel)" ] || fail "the old entry should boot kernel $(recall old_kernel), not $(uname -r)"
    driver "$(recall old_driver)"
    new=$(recall new_driver | sed 's/-[^-]*$//')
    dkms status nvidia | grep -q "nvidia/$new" && fail "deployment $(recall nv_old)'s dkms shouldn't know the $new build: $(dkms status nvidia)"
    say "PASS: booting the old entry brought back kernel $(uname -r) and driver $(recall old_driver), with nothing rebuilt"
    ax rollback "$(recall none)" --noconfirm > /dev/console 2>&1 || fail "archix rollback to generation $(recall none), which has no driver"
    [ "$(current)" = "$(recall none)" ] || fail "deployment $(recall none) should be current"
    next 5
    systemctl reboot
    ;;
  5)
    [ "$booted" = "$(recall none)" ] || fail "the rollback should boot deployment $(recall none)"
    no_driver
    say "PASS: deployment $(recall none), which had no driver, boots without any nvidia package, module, /usr/src tree, firmware or modprobe, modules-load, udev or mkinitcpio file, modprobe nvidia finds no module, and its UKI's initramfs has no nvidia; no GPU, so build, install and remove checks, not a hardware result"

    sed -i 's|"devtools", "nvidia-open-dkms" }, -- packages|"devtools", "nvidia-580xx-dkms" }, -- packages|' "$config"
    baseline "$(recall none)"
    ax switch --noconfirm > /dev/console 2>&1 || fail "switch adding the closed driver nvidia-580xx-dkms from the AUR"
    unchanged "staging the closed driver" "$(recall none)"
    closed=$(current)
    case $(deployed "$closed" nvidia-580xx-dkms) in 580.*) ;; *) fail "nvidia-580xx-dkms should be in deployment $closed, not '$(deployed "$closed" nvidia-580xx-dkms)'" ;; esac
    version=$(deployed "$closed" nvidia-580xx-dkms | cut -d- -f1)
    kernel=$(deployed_kernel "$closed")
    [ "$(deployed_module "$closed" "$kernel")" = "$version" ] || fail "the closed nvidia $version should be built for $kernel, not '$(deployed_module "$closed" "$kernel")'"
    say "PASS: deployment $closed builds the closed driver nvidia-580xx-dkms $version from the AUR for $kernel"
    [ -d "/archix/deploy/$(recall nv_old)" ] && fail "keep = 3 should have dropped deployment $(recall nv_old)"
    ax rollback "$(recall nv_old)" --noconfirm > /dev/console 2>&1 || fail "rebuilding generation $(recall nv_old)"
    [ "$(current)" = "$(recall nv_old)" ] || fail "the rebuilt deployment $(recall nv_old) should be current"
    [ "$(deployed_module "$(recall nv_old)" "$(recall old_kernel)")" = "$(recall old_driver | sed 's/-[^-]*$//')" ] \
      || fail "the rebuilt deployment $(recall nv_old) should hold nvidia $(recall old_driver) built for $(recall old_kernel)"
    [ -z "$(deployed "$(recall nv_old)" nvidia-580xx-dkms)" ] || fail "the rebuilt deployment shouldn't have the closed driver"
    unchanged "rebuilding deployment $(recall nv_old)" "$(recall none)"
    next 6
    systemctl reboot
    ;;
  6)
    [ "$booted" = "$(recall nv_old)" ] || fail "the rebuilt deployment $(recall nv_old) should boot"
    [ "$(uname -r)" = "$(recall old_kernel)" ] || fail "the rebuilt deployment should boot kernel $(recall old_kernel), not $(uname -r)"
    driver "$(recall old_driver)"
    [ -z "$(pkg nvidia-580xx-dkms)" ] || fail "the closed driver should be gone"
    say "PASS: a dropped deployment rebuilt from the package cache boots its own driver, built by DKMS again"
    next done
    say "OK"
    systemctl poweroff
    ;;
  *)
    systemctl poweroff
    ;;
esac
