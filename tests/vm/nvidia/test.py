# NVIDIA DKMS drivers in place: nvidia-open-dkms added live at 2026-06-01, then the newer snapshot (TO_DATE, 2026-10-10 by default) changing kernel and driver and booting them, the open driver dropped leaving no trace, and the closed nvidia-580xx-dkms from the AUR added and dropped. No GPU, so modules are built and checked, not loaded.
import os

from boot.common import bind, boot, ok, out
from suite import add_packages, drop_packages, lay_out, switch, update

MACHINE = {"mem": "6144", "cpus": "4", "global_timeout": 14400}
TO = os.environ.get("TO_DATE") or "2026-10-10"
DROPINS = "/usr/lib/modprobe.d /etc/modprobe.d /usr/lib/modules-load.d /etc/modules-load.d /usr/lib/udev/rules.d /etc/udev/rules.d /etc/mkinitcpio.conf.d /etc/mkinitcpio.d /usr/lib/initcpio"


def pkg(name):
    said = out(f"pacman -Q {name} 2>/dev/null").split()
    return said[1] if len(said) == 2 else ""


def upstream(version):
    return version.rsplit("-", 1)[0]


def kernel():
    return out("uname -r").strip()


def installed_kernel():
    # Not by directory: a switch that replaces the running kernel leaves that kernel's modules beside the new one.
    return out("pacman -Qlq linux | sed -n 's#^/usr/lib/modules/\\([^/]*\\)/vmlinuz$#\\1#p'").strip()


def built(kernel_release):
    return out(f"modinfo -F version /usr/lib/modules/{kernel_release}/updates/dkms/nvidia.ko* 2>/dev/null; true").strip()


def driver(t, version):
    """The open driver's package at version, its module DKMS built for the running kernel, DKMS's record, its sources, nvidia-utils's firmware and modprobe.d file."""
    want, running = upstream(version), kernel()
    t.check(pkg("nvidia-open-dkms") == version, f"nvidia-open-dkms should be {version}, not {pkg('nvidia-open-dkms')!r}")
    t.check(pkg("nvidia-utils").startswith(want + "-"), f"nvidia-utils should be {want}, not {pkg('nvidia-utils')!r}")
    t.check(out(f"modinfo -k {running} -F version nvidia 2>/dev/null").strip() == want, f"the nvidia module for {running} should be {want}: {out(f'modinfo -k {running} -F version nvidia 2>&1')}")
    t.check(ok(f"dkms status nvidia | grep -q 'nvidia/{want}, {running}, x86_64: installed'"), f"dkms should show nvidia/{want} installed for {running}: {out('dkms status nvidia')}")
    t.check(ok(f"[ -f /usr/src/nvidia-{want}/dkms.conf ]"), f"the source tree /usr/src/nvidia-{want} should be there: {out('ls -d /usr/src/nvidia* 2>&1')}")
    t.check(ok(f"[ -f /usr/lib/firmware/nvidia/{want}/gsp_ga10x.bin ]"), f"nvidia-utils's GSP firmware should be in /usr/lib/firmware/nvidia/{want}: {out('ls -d /usr/lib/firmware/nvidia/[0-9]* 2>&1')}")
    t.check(ok("[ -f /usr/lib/modprobe.d/nvidia-utils.conf ]"), "nvidia-utils's modprobe.d file should be there")
    t.check(ok("modprobe -n nvidia"), "modprobe should find the nvidia module")


def current_image():
    return "/efi/EFI/Linux/arch-linux.efi"


def no_driver(t):
    """No nvidia package, module, DKMS record, source tree, firmware or drop-in, modprobe finds none, and linux's UKI has none in its initramfs."""
    for name in ("nvidia-open-dkms", "nvidia-utils", "nvidia-580xx-dkms", "nvidia-580xx-utils"):
        t.check(not pkg(name), f"{name} should be gone, but {pkg(name)} is installed")
    t.check(not out("ls /usr/lib/modules/*/updates/dkms/nvidia* 2>/dev/null").strip(), f"nvidia modules are left behind: {out('ls /usr/lib/modules/*/updates/dkms/')}")
    t.check(not out("dkms status nvidia 2>/dev/null").strip(), f"dkms still knows nvidia: {out('dkms status nvidia')}")
    t.check(not out("ls -d /usr/src/nvidia* /usr/lib/firmware/nvidia/[0-9]* 2>/dev/null").strip(), f"nvidia sources or firmware are left: {out('ls -d /usr/src/nvidia* /usr/lib/firmware/nvidia/[0-9]* 2>&1')}")
    dropins = out(f"{{ find {DROPINS} -iname '*nvidia*' 2>/dev/null; grep -rlis nvidia {DROPINS}; }} | sort -u").strip()
    t.check(not dropins, f"nvidia modprobe, modules-load, udev or mkinitcpio files are left: {dropins}")
    said = out("modprobe nvidia 2>&1; true")
    t.check("not found" in said, f"modprobe nvidia should fail for want of the module: {said}")
    files = out(f"lsinitcpio {current_image()} 2>&1")
    t.check(f"usr/lib/modules/{kernel()}/" in files, f"lsinitcpio should list the current image's modules: {files[:400]}")
    ours = [line for line in files.splitlines() if "/updates/" in line or "/firmware/nvidia/" in line or ("/nvidia" in line and line.endswith((".ko", ".ko.zst", ".ko.xz")))]
    t.check(not ours, f"the current image's initramfs has nvidia: {ours[:5]}")


def run(m, t):
    bind(m, t)
    lay_out(m, t)
    with t.step("1: nvidia-open-dkms added by a switch, live"):
        no_driver(t)
        add_packages(m, "nvidia-open-dkms")
        switch(m, t, "the switch adding nvidia-open-dkms")
        old_driver, old_kernel = pkg("nvidia-open-dkms"), kernel()
        t.log(f"nvidia-open-dkms {old_driver} on {old_kernel}")
        driver(t, old_driver)

    with t.step(f"2: the {TO} update changes kernel and driver, building the new one for the new kernel"):
        update(m, t, TO)
        switch(m, t, f"the switch to the {TO} snapshot")
        new_driver, new_kernel = pkg("nvidia-open-dkms"), installed_kernel()
        t.log(f"now nvidia-open-dkms {new_driver} for {new_kernel}")
        t.require(new_driver != old_driver and new_kernel != old_kernel, f"the {TO} snapshot should change kernel and driver: {new_kernel} {new_driver}")
        t.check(built(new_kernel) == upstream(new_driver), f"nvidia {new_driver} should be built for {new_kernel}, not {built(new_kernel)!r}")
        m.reboot()

    with t.step("3: the update's kernel boots with its driver"):
        started = boot()
        t.check(started == new_kernel, f"the reboot should start {new_kernel}, not {started}")
        driver(t, new_driver)

    with t.step("4: dropping the driver leaves no trace of it"):
        drop_packages(m, "nvidia-open-dkms")
        switch(m, t, "the switch dropping nvidia-open-dkms")
        no_driver(t)

    with t.step("5: the closed nvidia-580xx-dkms from the AUR, added, then dropped"):
        add_packages(m, "nvidia-580xx-dkms")
        switch(m, t, "the switch adding the closed driver from the AUR", timeout=5400)
        closed = pkg("nvidia-580xx-dkms")
        running = installed_kernel()
        t.check(closed.startswith("580."), f"nvidia-580xx-dkms should be installed: {closed!r}")
        t.check(built(running) == upstream(closed), f"the closed nvidia {closed} should be built for {running}, not {built(running)!r}")
        drop_packages(m, "nvidia-580xx-dkms")
        switch(m, t, "the switch dropping the closed driver")
        no_driver(t)
