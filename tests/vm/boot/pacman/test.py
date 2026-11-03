# A plain pacman transaction that breaks the boot without touching the kernel: archix's hook snapshots the root before it and gives the current entry its tries again, the health check fails each counted boot, the default boot.unblessed_timeout reboots it with nobody there, and systemd-boot falls back to the slot, that snapshot.
import time

from boot.common import bind, boot, entries, entry, ok, out, snapshots, state, trying

PKG = "/root/archix-test-broken-1-1-any.pkg.tar.zst"
BUILD = (
    "set -e; d=/root/archix-test-broken; rm -rf $d; mkdir -p $d/usr/lib/systemd/system/multi-user.target.wants; "
    "printf '[Unit]\\nDescription=a unit a package update broke\\n\\n[Service]\\nType=oneshot\\nRemainAfterExit=yes\\nExecStart=/usr/bin/false\\n' > $d/usr/lib/systemd/system/archix-test-broken.service; "
    "ln -s ../archix-test-broken.service $d/usr/lib/systemd/system/multi-user.target.wants/archix-test-broken.service; "
    "printf 'pkgname = archix-test-broken\\npkgbase = archix-test-broken\\npkgver = 1-1\\npkgdesc = breaks the boot\\narch = any\\nsize = 4096\\n' > $d/.PKGINFO; "
    f"cd $d && bsdtar --zstd -cf {PKG} .PKGINFO usr"
)


def run(m, t):
    bind(m, t)
    with t.step("1: plain pacman installs a package whose unit fails at every boot"):
        boot()
        t.check(state().get("armed") is True, f"the base's good boot should have armed the slot: {state()}")
        m.succeed(BUILD, what="building the broken package")
        before = snapshots()
        installed = out(f"pacman -U --noconfirm {PKG} 2>&1")
        t.log("pacman: " + installed)
        after = snapshots()
        t.require(len(after) == len(before) + 1, f"the hook should take one snapshot before the transaction: {before} -> {after}: {installed}")
        slot = after[-1]
        t.check(f"archix: snapshot {slot} holds the system as it was before this transaction" in installed, f"pacman's output should say so: {installed}")
        t.check("has 2 tries again" in installed, f"the step after should reset the current entry's tries: {installed}")
        t.check("building the boot images" not in installed, f"the kernel and initramfs didn't change, so no image is built: {installed}")
        t.check(state().get("slot") == slot, f"the snapshot after a good boot should be the slot: {state()}")
        t.check(entry("archix-current.conf") == "archix-current+2.conf", f"the current entry should count two tries: {entries()}")
        t.check(entry(f"archix-snapshot-{slot}.conf") == f"archix-snapshot-{slot}+1.conf", f"the slot should have an entry: {entries()}")
        t.check(ok(f"grep -q '^sort-key archix-1$' /efi/loader/entries/archix-snapshot-{slot}+1.conf"), "sorted right after the current entry")
        began = time.monotonic()
        m.reboot()

    with t.step("2: the health check fails, the unblessed timeout reboots, and the slot boots"):
        m.wait_for_console_text(r"rebooting so the boot counter can fall back", timeout=1500)
        # The broken boot's shell answers until the reboot, so wait for the fallback's own notice first.
        m.wait_for_console_text(r"archix: this boot is snapshot", timeout=900)
        m.wait_for_shell(timeout=1800)
        took = time.monotonic() - began
        started = boot()
        t.log(f"from the reboot into the broken system to the fallback's shell: {took:.0f} s")
        t.check(started == f"archix-snapshot-{slot}.conf", f"systemd-boot should fall back to the slot, archix-snapshot-{slot}.conf, not {started}")
        t.check(entry("archix-current.conf") not in (None, "archix-current.conf"), f"the current entry should be spent or marked bad, not blessed: {entries()}")
        t.check((trying() or {}).get("snapshot") == slot, f"in try mode: {trying()}")
        t.check(not ok("[ -e /usr/lib/systemd/system/archix-test-broken.service ]"), "the slot is the system before the broken package")
