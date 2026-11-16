# DKMS with two kernels, linux and linux-lts, in place: a switch adding v4l2loopback-dkms declares both kernels' headers and builds the module for both live, each kernel's UKI boots and loads its own build, and an update (2026-06-01 to 2026-10-10) rebuilds it for both new kernels, which boot with it.
from boot.common import bind, boot, ok, out
from suite import add_packages, lay_out, switch, update

MACHINE = {"mem": "4096", "cpus": "4", "global_timeout": 14400}
KERNELS = ("linux", "linux-lts")
OLD, NEW = "0.15.3", "0.15.4"


def kernel_of(package):
    """The kernel release package owns; a switch that replaces the running kernel leaves that kernel's modules beside it until the next one."""
    return out(f"pacman -Qlq {package} | sed -n 's#^/usr/lib/modules/\\([^/]*\\)/vmlinuz$#\\1#p'").strip()


def built(kernel, root=""):
    return out(f"modinfo -F version {root}/usr/lib/modules/{kernel}/updates/dkms/v4l2loopback.ko* 2>/dev/null; true").strip()


def both_kernels(t, want):
    """Both kernels are installed, each with the UKI its preset writes, and v4l2loopback want built for each."""
    for package in KERNELS:
        kernel = kernel_of(package)
        t.require(kernel, f"{package} should be installed: {out('ls /usr/lib/modules')}")
        image = f"/efi/EFI/Linux/arch-{package}.efi"
        t.check(ok(f"ukify inspect {image} | grep -A6 '^\\.uname' | grep -q {kernel}"), f"{image} should be built from {kernel}: {out(f'ukify inspect {image} | grep -A6 uname')}")
        t.check(built(kernel) == want, f"v4l2loopback {want} should be built for {kernel}, not {built(kernel)!r}")
        t.check(ok(f"dkms status v4l2loopback | grep -q 'v4l2loopback/{want}, {kernel}, x86_64: installed'"), f"dkms should show {want} installed for {kernel}: {out('dkms status')}")


def loads(t, want, package):
    """The running kernel is package's, and v4l2loopback want from DKMS loads on it."""
    running = out("uname -r").strip()
    t.check(running == kernel_of(package), f"{package}'s entry should boot {kernel_of(package)}, not {running}")
    t.check(out("modinfo -F version v4l2loopback 2>/dev/null").strip() == want, f"v4l2loopback {want} should be built for {running}: {out('modinfo v4l2loopback 2>&1 | head -3')}")
    t.check("/updates/dkms/" in out("modinfo -n v4l2loopback"), f"v4l2loopback should come from DKMS: {out('modinfo -n v4l2loopback')}")
    t.check(ok("modprobe v4l2loopback && grep -q '^v4l2loopback ' /proc/modules"), f"v4l2loopback {want} should load on {running}: {m_err()}")


def m_err():
    return out("modprobe v4l2loopback 2>&1; dmesg | tail -5")


def dkms_knows(t, want, other):
    said = out("dkms status v4l2loopback 2>&1")
    t.check(f"v4l2loopback/{want}" in said and f"v4l2loopback/{other}" not in said, f"DKMS's state should know {want} and not {other}: {said}")


def boot_kernel(m, t, package):
    """Boots package's UKI once, which systemd-boot lists as arch-PACKAGE.efi."""
    m.select_entry(f"arch-{package}.efi")
    m.reboot()
    started = boot()
    t.check(started == kernel_of(package), f"arch-{package}.efi should boot {kernel_of(package)}, not {started}")


def run(m, t):
    bind(m, t)
    lay_out(m, t)
    with t.step("1: a switch adding v4l2loopback-dkms builds it for both kernels, live"):
        add_packages(m, "v4l2loopback-dkms")
        switch(m, t, "the switch adding a DKMS module")
        for headers in ("linux-headers", "linux-lts-headers"):
            t.check(ok(f"grep -q '^{headers} = ' /etc/archix/archix.lock"), f"archix should declare {headers} for the DKMS module: {out('grep -- -headers /etc/archix/archix.lock')}")
        both_kernels(t, OLD)
        loads(t, OLD, "linux")

    with t.step("2: linux-lts boots and loads its own build"):
        boot_kernel(m, t, "linux-lts")
        loads(t, OLD, "linux-lts")

    old = {package: kernel_of(package) for package in KERNELS}
    with t.step("3: the 2026-10-10 update rebuilds the module for both new kernels"):
        update(m, t, "2026-10-10")
        switch(m, t, "the 2026-10-10 update")
        new = {package: kernel_of(package) for package in KERNELS}
        t.log(f"kernels {old} -> {new}")
        t.check(all(old[p] != new[p] for p in KERNELS), f"the update should change both kernels: {old} -> {new}")
        running = out("uname -r").strip()
        t.check(running == old["linux-lts"], f"the update should run on the old linux-lts: {running}")
        t.check(ok("! grep -q '^dummy ' /proc/modules && modprobe dummy"), f"the running kernel should still load a module it hadn't, as pacman replaced its package: {out(f'ls -la /usr/lib/modules/{running}; modprobe dummy 2>&1')}")
        both_kernels(t, NEW)

    with t.step("4: after a reboot both kernels load the update's build"):
        boot_kernel(m, t, "linux")
        loads(t, NEW, "linux")
        boot_kernel(m, t, "linux-lts")
        loads(t, NEW, "linux-lts")
        dkms_knows(t, NEW, OLD)
