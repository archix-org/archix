# DKMS with two kernels, linux and linux-lts, in place: the current system gets an entry and image per kernel, a switch adding v4l2loopback-dkms declares both kernels' headers and builds the module for both live, an update (2026-06-01 to 2026-10-10) rebuilds it for both new kernels, and on btrfs the snapshot from before the update tries and keeps with its own builds and DKMS state (/var/lib/dkms bound from the root), after which DKMS builds the update's version again.
from boot.common import HOST, bind, boot, entries, entry, ok, out, trying
from suite import BTRFS, add_packages, images_consistent, lay_out, newest_snapshot, switch, update

MACHINE = {"mem": "4096", "cpus": "4", "global_timeout": 14400}
KERNELS = ("linux", "linux-lts")
OLD, NEW = "0.15.3", "0.15.4"


def kernel_of(package):
    """The kernel release package owns; a switch that replaces the running kernel leaves that kernel's modules beside it until the next one."""
    return out(f"pacman -Qlq {package} | sed -n 's#^/usr/lib/modules/\\([^/]*\\)/vmlinuz$#\\1#p'").strip()


def built(kernel, root=""):
    return out(f"modinfo -F version {root}/usr/lib/modules/{kernel}/updates/dkms/v4l2loopback.ko* 2>/dev/null; true").strip()


def entry_file(package):
    """The file of package's current entry, with its counters."""
    return entry("archix-current.conf" if package == "linux" else f"archix-current-{package}.conf")


def both_kernels(t, want):
    """Each kernel has its own current entry, whose image is built from it, and v4l2loopback want built for it."""
    for package in KERNELS:
        kernel = kernel_of(package)
        t.require(kernel, f"{package} should be installed: {out('ls /usr/lib/modules')}")
        name = entry_file(package)
        t.require(name, f"{package} should have a current entry: {entries()}")
        image = out(f"sed -nE 's#^(efi|uki) /#/efi/#p' /efi/loader/entries/{name}").strip()
        t.check(ok(f"ukify inspect {image} | grep -A6 '^\\.uname' | grep -q {kernel}"), f"{name}'s image should be built from {kernel}: {out(f'ukify inspect {image} | grep -A6 uname')}")
        if want:
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


def boot_entry(m, t, name):
    m.select_entry(name)
    m.reboot()
    started = boot()
    t.check(started == name, f"{name} should start, not {started}")


def run(m, t):
    bind(m, t)
    lay_out(m, t)
    with t.step("1: the current system has an entry and image per kernel"):
        both_kernels(t, "")
        images_consistent(t)

    with t.step("2: a switch adding v4l2loopback-dkms builds it for both kernels, live"):
        add_packages(m, "v4l2loopback-dkms")
        switch(m, t, "the switch adding a DKMS module")
        for headers in ("linux-headers", "linux-lts-headers"):
            t.check(ok(f"grep -q '^{headers} = ' /etc/archix/archix.lock"), f"archix should declare {headers} for the DKMS module: {out('grep -- -headers /etc/archix/archix.lock')}")
        both_kernels(t, OLD)
        loads(t, OLD, "linux")
        if BTRFS:
            t.check(out("findmnt -no FSROOT /var/lib/dkms").strip() == "/@/var/rollback/dkms", f"/var/lib/dkms should be bound from the root's /var/rollback/dkms: {out('findmnt /var/lib/dkms')}")
        images_consistent(t)

    with t.step("3: the linux-lts entry boots and loads its own build"):
        boot_entry(m, t, "archix-current-linux-lts.conf")
        loads(t, OLD, "linux-lts")

    old = {package: kernel_of(package) for package in KERNELS}
    before = None
    with t.step("4: the 2026-10-10 update rebuilds the module for both new kernels"):
        update(m, t, "2026-10-10")
        switch(m, t, "the 2026-10-10 update")
        new = {package: kernel_of(package) for package in KERNELS}
        t.log(f"kernels {old} -> {new}")
        t.check(all(old[p] != new[p] for p in KERNELS), f"the update should change both kernels: {old} -> {new}")
        running = out("uname -r").strip()
        t.check(running == old["linux-lts"], f"the update should run on the old linux-lts: {running}")
        t.check(ok("! grep -q '^dummy ' /proc/modules && modprobe dummy"), f"the running kernel should still load a module it hadn't, as pacman replaced its package: {out(f'ls -la /usr/lib/modules/{running}; modprobe dummy 2>&1')}")
        both_kernels(t, NEW)
        if BTRFS:
            before = newest_snapshot()
            snap = f"/.archix/snapshots/{before}/root"
            for package in KERNELS:
                t.check(built(old[package], snap) == OLD, f"snapshot {before} should keep {OLD} built for {old[package]}, not {built(old[package], snap)!r}")
            t.check(ok(f"[ -d {snap}/var/rollback/dkms/v4l2loopback/{OLD} ] && [ ! -e {snap}/var/rollback/dkms/v4l2loopback/{NEW} ]"), f"snapshot {before}'s DKMS state should hold {OLD} only: {out(f'ls {snap}/var/rollback/dkms/v4l2loopback')}")
        images_consistent(t)

    with t.step("5: after a reboot both kernels' entries load the update's build"):
        m.reboot()
        started = boot()
        t.check(started == "archix-current.conf", f"the current entry should start, not {started}")
        loads(t, NEW, "linux")
        boot_entry(m, t, "archix-current-linux-lts.conf")
        loads(t, NEW, "linux-lts")
        dkms_knows(t, NEW, OLD)

    if not BTRFS:
        return

    with t.step(f"6: trying snapshot {before} boots its own kernel and build, with its DKMS state"):
        name = f"archix-snapshot-{before}.conf"
        t.require(entry(name), f"snapshot {before} should have an entry: {entries()}")
        boot_entry(m, t, name)
        t.check((trying() or {}).get("snapshot") == before, f"a try boot of snapshot {before}: {trying()}")
        loads(t, OLD, "linux")
        dkms_knows(t, OLD, NEW)

    with t.step(f"7: keeping snapshot {before} brings both kernels back with their builds"):
        t.log(m.succeed(f"set -o pipefail; {HOST} rollback {before} --noconfirm 2>&1 | tail -20", 1800, what=f"keeping snapshot {before}"))
        m.reboot()
        started = boot()
        t.check(started == "archix-current.conf" and trying() is None, f"the kept root should boot from the current entry: {started} {trying()}")
        loads(t, OLD, "linux")
        dkms_knows(t, OLD, NEW)
        both_kernels(t, OLD)
        boot_entry(m, t, "archix-current-linux-lts.conf")
        loads(t, OLD, "linux-lts")
        images_consistent(t)

    with t.step("8: after the keep, the update builds the module again for both new kernels"):
        update(m, t, "2026-10-10")
        switch(m, t, "the update again after the keep")
        both_kernels(t, NEW)
        dkms_knows(t, NEW, OLD)
        m.reboot()
        boot()
        loads(t, NEW, "linux")
        images_consistent(t)
